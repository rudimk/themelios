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

use crate::arch::x86_64::idt;
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

/// Reply status: success.
pub const STATUS_OK: u64 = 0;
/// Reply status: failure.
pub const STATUS_ERROR: u64 = 1;

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
            _ => IpcMessage::new([STATUS_ERROR, 0, 0, 0]),
        };

        // Reply to the (ring-3) caller. A failed reply (caller gone) is non-fatal.
        let _ = ipc::ipc_reply(cfg.endpoint, request.reply_token, reply);
    }
}
