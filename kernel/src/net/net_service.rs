//! # Kernel net service — the driver ↔ userspace frame bridge
//!
//! Ring-3 servers cannot touch the NIC directly, so the net service bridges the
//! in-kernel VirtIO-net driver to the ring-3 TCP/IP stack (the net server, added
//! in sub-phase 4.2). It is the networking analogue of the Phase 3
//! [`block_server`](crate::drivers::block_server): a kernel task on an IPC
//! endpoint that owns the device and moves data through shared memory.
//!
//! ## Pull-based, deadlock-free
//!
//! The kernel IPC is synchronous rendezvous only — there is no non-blocking send
//! or notification primitive, so a kernel task **cannot** push an unsolicited
//! message to a ring-3 server that isn't currently parked in `ipc_receive`, and a
//! single task that both pushed RX and served TX would mutually deadlock. So the
//! model is **pull-based**: the ring-3 server always initiates and the service
//! always replies. There are two request types, both `ipc_call`ed by the server:
//!
//! - `MSG_POLL` — "give me the next received frame and the current time." The
//!   service drains the driver into its RX queue, hands back one frame (or none),
//!   and returns the monotonic timestamp for smoltcp's clock.
//! - `MSG_TX_FRAME` — "transmit this frame" (staged in the TX shared region).
//!
//! Because only the ring-3 server ever initiates and the service only ever
//! replies, no send cycle — and therefore no deadlock — is possible.
//!
//! ## RX buffering
//!
//! Received frames arrive unsolicited at the NIC. On every `MSG_POLL` the service
//! first drains *all* frames currently in the driver's receive ring into its own
//! `rx_queue`, which promptly recycles the driver's buffers back to the device,
//! then hands the server one frame per call. The queue is bounded; on overflow it
//! drops the oldest frame and counts it (surfaced by `ifconfig` in sub-phase 4.4).
//! A separate timer-tick drain is unnecessary in Phase 4 because the net server
//! polls continuously; it can be added later if the server ever blocks.
//!
//! ## Single instance
//!
//! Phase 4 has one NIC, so the service is single-instance: its configuration is
//! published once (before the task is spawned) into [`CONFIG`], and the task reads
//! it at startup. If multiple NICs ever need services, adopt the `block_server`
//! per-instance slot-claim pattern.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::arch::time as idt;
use crate::ipc::{self, IpcMessage};
use crate::mm::shared::SharedRegion;
use crate::net::device;
use crate::sched;
use crate::sync::InterruptMutex;

/// Request: hand back the next received frame (if any) plus the current time.
/// Reply: `[status, frame_len, uptime_ms, 0]` (frame in the RX shared region).
pub const MSG_POLL: u64 = 0;
/// Request: transmit the frame staged in the TX shared region.
/// `word1 = frame length`. Reply: `[status, 0, 0, 0]`.
pub const MSG_TX_FRAME: u64 = 1;

/// Request: the ring-3 server reports its acquired IP configuration (sub-phase
/// 4.4). The kernel records it (it does not parse packets — it only stores what
/// the server tells it) so `ifconfig` can display the live configuration.
///
/// Word layout (must match `libthemelios::net_proto`, kept in sync by hand):
/// - `word1` = IPv4 address in bits 0..32 (`a<<24|b<<16|c<<8|d`) | `prefix<<32`
/// - `word2` = default gateway (packed IPv4, 0 = none)
/// - `word3` = primary DNS server (packed IPv4, 0 = none; display only)
///
/// A fully-zero `word1` means "deconfigured". Reply: `[status, 0, 0, 0]`.
pub const MSG_CONFIG: u64 = 2;

/// Reply status: success.
pub const STATUS_OK: u64 = 0;
/// Reply status: failure.
pub const STATUS_ERROR: u64 = 1;

/// The IPv4 configuration the ring-3 net server last reported (via `MSG_CONFIG`).
///
/// The kernel keeps this purely for display (`ifconfig`); it is the server's
/// smoltcp interface that actually holds and uses the configuration.
#[derive(Clone, Copy, Default)]
pub struct AcquiredConfig {
    /// True once an address has been acquired (via DHCP or set statically).
    pub configured: bool,
    /// The interface's IPv4 address.
    pub addr: [u8; 4],
    /// The address prefix length (e.g. 24 for a /24).
    pub prefix: u8,
    /// The default gateway, if any.
    pub gateway: Option<[u8; 4]>,
    /// The primary DNS server, if any (stored for display; no resolver yet).
    pub dns: Option<[u8; 4]>,
}

/// A snapshot of the network interface's status, for the `ifconfig` command and
/// the DHCP integration test. Populated by the net service task.
#[derive(Clone, Copy)]
pub struct NetStatus {
    /// Whether a net service is running (a NIC was found and bound).
    pub present: bool,
    /// The NIC's hardware (MAC) address.
    pub mac: [u8; 6],
    /// The NIC's MTU in bytes.
    pub mtu: usize,
    /// Count of RX frames dropped because the service's RX queue overflowed.
    pub rx_dropped: u64,
    /// The IPv4 configuration the ring-3 stack last reported.
    pub config: AcquiredConfig,
}

impl Default for NetStatus {
    fn default() -> Self {
        Self {
            present: false,
            mac: [0; 6],
            mtu: 0,
            rx_dropped: 0,
            config: AcquiredConfig::default(),
        }
    }
}

/// Live interface status, updated by the net service task and read by
/// [`status`]. Guarded by an `InterruptMutex` because the shell (or a test) may
/// read it concurrently with the service task's writes.
static STATUS: InterruptMutex<NetStatus> = InterruptMutex::new(NetStatus {
    present: false,
    mac: [0; 6],
    mtu: 0,
    rx_dropped: 0,
    config: AcquiredConfig {
        configured: false,
        addr: [0; 4],
        prefix: 0,
        gateway: None,
        dns: None,
    },
});

/// Read a snapshot of the current network interface status.
///
/// Returns a `NetStatus` with `present == false` if no net service is running
/// (no NIC was discovered at boot).
pub fn status() -> NetStatus {
    *STATUS.lock()
}

/// Decode a packed IPv4 address from the low 32 bits of a `MSG_CONFIG` word.
/// Mirrors `libthemelios::net_proto::unpack_ipv4` (duplicated by hand — the
/// kernel does not depend on the userspace support crate).
fn unpack_ipv4(word: u64) -> [u8; 4] {
    [
        (word >> 24) as u8,
        (word >> 16) as u8,
        (word >> 8) as u8,
        word as u8,
    ]
}

/// Size of each frame shared region: one maximum Ethernet frame, rounded up.
const REGION_BYTES: u64 = 2048;

/// Maximum frames buffered in the service's RX queue before the oldest is dropped.
const RX_QUEUE_CAP: usize = 64;

/// Published service configuration, read by the spawned task at startup.
#[derive(Clone)]
struct Config {
    /// Registry index of the NIC this service drives.
    nic_index: usize,
    /// IPC endpoint the service listens on.
    endpoint: u64,
    /// Shared region the service writes received frames into (server reads).
    rx_region: SharedRegion,
    /// Shared region the server stages TX frames into (service reads).
    tx_region: SharedRegion,
}

/// Single published configuration slot (Phase 4 has one NIC / one service).
static CONFIG: InterruptMutex<Option<Config>> = InterruptMutex::new(None);

/// A handle to a started net service, returned to whoever spawned it.
///
/// Carries the IPC endpoint the ring-3 server sends requests to and the two
/// shared regions (RX/TX) so the spawner can map them into the server's address
/// space — or, in a kernel-side test, access them directly via the HHDM.
#[derive(Clone, Copy)]
pub struct NetServiceHandle {
    /// Endpoint the server sends `MSG_POLL`/`MSG_TX_FRAME` to.
    pub endpoint: u64,
    /// Shared region carrying received frames (service → server).
    pub rx_region: SharedRegion,
    /// Shared region carrying frames to transmit (server → service).
    pub tx_region: SharedRegion,
}

/// Start the net service for the NIC at registry `nic_index`.
///
/// Allocates the RX/TX shared regions, creates the IPC endpoint, publishes the
/// configuration, and spawns the service task. Returns a handle, or `None` if a
/// shared region could not be allocated.
pub fn start(nic_index: usize) -> Option<NetServiceHandle> {
    let rx_region = SharedRegion::alloc(REGION_BYTES)?;
    let tx_region = SharedRegion::alloc(REGION_BYTES)?;
    let endpoint = ipc::create_endpoint("net-service");

    // Reset the acquired-config view: this interface has not (re)configured yet.
    // Without this, a caller polling `status().config.configured` could observe a
    // stale `true` left by a previous service instance and act (e.g. TCP connect)
    // before this interface has an address. The new service task republishes
    // MAC/MTU at startup and the ring-3 stack re-reports its lease.
    {
        let mut st = STATUS.lock();
        st.config = AcquiredConfig::default();
    }

    *CONFIG.lock() = Some(Config {
        nic_index,
        endpoint,
        rx_region,
        tx_region,
    });

    sched::spawn("net-service", service_loop);

    Some(NetServiceHandle {
        endpoint,
        rx_region,
        tx_region,
    })
}

/// The service task entry point: receive requests, serve them, reply. Runs
/// forever.
fn service_loop() {
    // Read the published configuration and resolve the NIC.
    let cfg = match CONFIG.lock().clone() {
        Some(c) => c,
        None => return, // never started properly
    };
    let nic = match device::get(cfg.nic_index) {
        Some(n) => n,
        None => return,
    };

    // Publish the interface's static properties (MAC, MTU) so `ifconfig` can
    // show them even before the ring-3 stack has acquired an address.
    {
        let mut st = STATUS.lock();
        st.present = true;
        st.mac = nic.mac();
        st.mtu = nic.mtu();
    }

    // Local mutable state: the RX frame queue and the overflow counter.
    let mut rx_queue: VecDeque<Vec<u8>> = VecDeque::new();
    let mut dropped: u64 = 0;
    let mut scratch = [0u8; REGION_BYTES as usize];

    loop {
        let request = match ipc::ipc_receive(cfg.endpoint) {
            Ok(msg) => msg,
            // Endpoint torn down — exit.
            Err(_) => return,
        };

        let reply = match request.words[0] {
            MSG_POLL => {
                // Drain every frame currently in the driver's receive ring into
                // our queue, recycling the driver's buffers back to the device.
                loop {
                    match nic.receive(&mut scratch) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if rx_queue.len() >= RX_QUEUE_CAP {
                                // Bounded queue: drop the oldest and count it.
                                rx_queue.pop_front();
                                dropped = dropped.wrapping_add(1);
                                // Surface the running drop count for `ifconfig`.
                                STATUS.lock().rx_dropped = dropped;
                            }
                            rx_queue.push_back(scratch[..n].to_vec());
                        }
                    }
                }

                let now = idt::tick_count().wrapping_mul(10);
                if let Some(frame) = rx_queue.pop_front() {
                    // SAFETY: rx_region is HHDM-mapped RAM of REGION_BYTES; the
                    // request/reply protocol serialises access to it.
                    let dst = unsafe { cfg.rx_region.as_slice_mut() };
                    let n = frame.len().min(dst.len());
                    dst[..n].copy_from_slice(&frame[..n]);
                    IpcMessage::new([STATUS_OK, n as u64, now, 0])
                } else {
                    IpcMessage::new([STATUS_OK, 0, now, 0])
                }
            }
            MSG_TX_FRAME => {
                let len = (request.words[1] as usize).min(cfg.tx_region.size as usize);
                // Copy the frame out of the shared region before transmitting.
                // SAFETY: tx_region is HHDM-mapped RAM of REGION_BYTES.
                let frame = {
                    let src = unsafe { cfg.tx_region.as_slice_mut() };
                    src[..len].to_vec()
                };
                let status = match nic.transmit(&frame) {
                    Ok(()) => STATUS_OK,
                    Err(_) => STATUS_ERROR,
                };
                IpcMessage::new([status, 0, 0, 0])
            }
            MSG_CONFIG => {
                // The ring-3 stack reports its acquired IP configuration. Record
                // it for `ifconfig`; the kernel never inspects packet contents.
                let addr_word = request.words[1];
                let addr = unpack_ipv4(addr_word);
                let prefix = (addr_word >> 32) as u8;
                let gateway = match request.words[2] {
                    0 => None,
                    w => Some(unpack_ipv4(w)),
                };
                let dns = match request.words[3] {
                    0 => None,
                    w => Some(unpack_ipv4(w)),
                };
                // A fully-zero address word means "deconfigured" (lease lost).
                let configured = addr_word != 0;
                STATUS.lock().config = AcquiredConfig {
                    configured,
                    addr,
                    prefix,
                    gateway,
                    dns,
                };
                IpcMessage::new([STATUS_OK, 0, 0, 0])
            }
            _ => IpcMessage::new([STATUS_ERROR, 0, 0, 0]),
        };

        // Reply to the (ring-3) caller. A failed reply (caller gone) is non-fatal.
        let _ = ipc::ipc_reply(cfg.endpoint, request.reply_token, reply);
    }
}
