//! # VirtIO network device driver
//!
//! Drives a VirtIO network device (QEMU's virtual NIC) on top of the generic
//! VirtIO transport in the parent module. Implements the kernel's
//! [`NetDevice`](crate::net::device::NetDevice) trait so the higher layers — the
//! kernel net service and, above it, the ring-3 TCP/IP stack — see a plain "send
//! this Ethernet frame / here's a received one" interface.
//!
//! ## Two queues
//!
//! Unlike the block device (one request queue), a network device has **two**
//! virtqueues:
//!
//! - **Queue 0 — receiveq**: the driver pre-posts empty, device-writable buffers.
//!   The device fills one whenever a frame arrives; the driver polls the used ring
//!   to discover completed buffers, copies the frame out, and re-posts the buffer.
//! - **Queue 1 — transmitq**: to send, the driver writes a frame into a buffer and
//!   posts it device-readable, then waits for the device to consume it.
//!
//! RX is inherently asynchronous — frames arrive unsolicited — so it uses the
//! transport's non-blocking `publish`/`kick`/`poll_used` primitives rather than
//! the block driver's synchronous `submit_and_wait`. TX is low-rate and done one
//! frame at a time under the device lock, so it reuses `submit_and_wait`.
//!
//! ## The VirtIO-net header
//!
//! Every frame on the wire is prefixed, in the buffer, by a fixed
//! `struct virtio_net_hdr`. For a modern (VirtIO 1.0+) device this header is
//! **12 bytes** (it always carries the trailing `num_buffers` field). We negotiate
//! no offload features, so on TX we zero the header, and on RX we skip it — the
//! `NetDevice` trait only ever exposes the pure Ethernet frame.

use alloc::boxed::Box;

use super::discovery::{VirtioDevice, VirtioKind};
use crate::mm::addr::PhysAddr;
use crate::mm::frame;
use crate::net::device::{NetDevice, NetError};
use crate::sync::InterruptMutex;

use super::{
    VirtioError, VirtioTransport, VirtqDesc, Virtqueue,
    VIRTQ_DESC_F_WRITE,
};

/// Feature bit 3: the device reports its MTU in the device config region.
const VIRTIO_NET_F_MTU: u64 = 1 << 3;
/// Feature bit 5: the device provides its MAC address in the device config.
const VIRTIO_NET_F_MAC: u64 = 1 << 5;

/// The VirtIO-net header length for a modern (VirtIO 1.0+) device: 12 bytes
/// (`flags, gso_type, hdr_len, gso_size, csum_start, csum_offset, num_buffers`).
const VIRTIO_NET_HDR_LEN: usize = 12;

/// Default MTU (Ethernet payload) when the device does not advertise one.
const DEFAULT_MTU: usize = 1500;

/// Maximum Ethernet frame we handle: MTU payload + 14-byte L2 header.
const MAX_FRAME: usize = DEFAULT_MTU + 14;

/// Size of each pre-posted receive buffer. Comfortably holds the 12-byte header
/// plus a full 1514-byte Ethernet frame, rounded up.
const RX_BUF_SIZE: usize = 2048;

/// Number of receive buffers pre-posted to the device (the driver's RX ring).
const N_RX_BUFS: usize = 16;

/// Contiguous frames backing the RX buffer region (16 × 2 KiB = 32 KiB = 8 pages).
const RX_REGION_FRAMES: usize = (N_RX_BUFS * RX_BUF_SIZE) / 4096;

// --- Device config region offsets (struct virtio_net_config) ---
const CFG_MAC: u64 = 0; // mac[6]
const CFG_MTU: u64 = 10; // mtu: u16

/// Per-device mutable hardware state, guarded by a mutex so the `&self` trait
/// methods can drive the two hardware queues safely.
struct Inner {
    /// The VirtIO transport (kept alive; holds the MMIO mappings).
    ///
    /// A trait object, so this driver names no transport in particular: 8.3's virtio-mmio
    /// implementation drops in here without the driver changing.
    #[allow(dead_code)]
    transport: Box<dyn VirtioTransport>,
    /// The receive virtqueue (queue 0).
    rx_queue: Virtqueue,
    /// The transmit virtqueue (queue 1).
    tx_queue: Virtqueue,
    /// Physical base of the contiguous RX buffer region (N_RX_BUFS × RX_BUF_SIZE).
    rx_bufs_phys: PhysAddr,
    /// Physical base of the single TX staging buffer.
    tx_buf_phys: PhysAddr,
}

/// A VirtIO network device implementing [`NetDevice`].
pub struct VirtioNet {
    /// Mutable hardware state (transport, queues, DMA buffers).
    inner: InterruptMutex<Inner>,
    /// The device's MAC address (read from device config, immutable).
    mac: [u8; 6],
    /// The interface MTU in bytes (payload, excluding the L2 header).
    mtu: usize,
}

impl VirtioNet {
    /// Bring up a net device found by [`crate::drivers::virtio::discovery`].
    ///
    /// The transport-neutral entry point: the caller hands over a [`VirtioDevice`] and
    /// discovery decides which transport speaks to it, so this driver names none.
    pub fn init(dev: &VirtioDevice) -> Result<Self, VirtioError> {
        debug_assert_eq!(
            dev.kind(),
            VirtioKind::Net,
            "VirtioNet::init handed a {} device",
            dev.kind().name()
        );
        let transport = dev.open_transport()?;
        // Request MAC and MTU reporting; we negotiate no offload/mergeable
        // features, so every frame fits in one buffer with a 12-byte header.
        let feats = transport.negotiate_features(VIRTIO_NET_F_MAC | VIRTIO_NET_F_MTU)?;

        // Queue 0 = receiveq, queue 1 = transmitq.
        let rx_queue = transport.setup_queue(0)?;
        let tx_queue = transport.setup_queue(1)?;

        // Read MAC and MTU from the device-specific config region, through the transport.
        //
        // The MAC is six separate byte reads, which is exactly the shape a device-side
        // update can tear; taking them under one generation check is the reason
        // `read_config` exists. Valid because we negotiated `VIRTIO_NET_F_MAC`.
        let mut mac = [0u8; 6];
        transport.read_config(CFG_MAC as usize, &mut mac)?;

        let mtu = if feats & VIRTIO_NET_F_MTU != 0 {
            let mut buf = [0u8; 2];
            transport.read_config(CFG_MTU as usize, &mut buf)?;
            u16::from_le_bytes(buf) as usize
        } else {
            DEFAULT_MTU
        };

        // Allocate the RX buffer region and the single TX staging buffer.
        let rx_bufs_phys = frame::allocate_contiguous_frames(RX_REGION_FRAMES)
            .ok_or(VirtioError::OutOfMemory)?;
        let tx_buf_phys = frame::allocate_frame().ok_or(VirtioError::OutOfMemory)?;

        // Pre-post every RX buffer: descriptor `i` points at buffer `i`, marked
        // device-writable, so the device can fill it as frames arrive.
        for i in 0..N_RX_BUFS {
            let buf_phys = rx_bufs_phys.as_u64() + (i as u64) * RX_BUF_SIZE as u64;
            rx_queue.write_desc(
                i as u16,
                VirtqDesc {
                    addr: buf_phys,
                    len: RX_BUF_SIZE as u32,
                    flags: VIRTQ_DESC_F_WRITE,
                    next: 0,
                },
            );
            rx_queue.publish(i as u16);
        }
        rx_queue.kick();

        // The device is fully set up — let it run.
        transport.set_driver_ok();

        Ok(Self {
            inner: InterruptMutex::new(Inner {
                transport,
                rx_queue,
                tx_queue,
                rx_bufs_phys,
                tx_buf_phys,
            }),
            mac,
            mtu,
        })
    }
}

impl Inner {
    /// Transmit one Ethernet frame: stage it (with a zeroed VirtIO-net header)
    /// into the TX buffer, post it device-readable, and wait for completion.
    fn transmit(&mut self, frame: &[u8]) -> Result<(), NetError> {
        if frame.len() > MAX_FRAME || VIRTIO_NET_HDR_LEN + frame.len() > RX_BUF_SIZE {
            return Err(NetError::TooLarge);
        }
        // Bail out BEFORE staging anything. The TX path is single-buffered
        // (`tx_buf_phys`, descriptor 0), so if a previous chain was abandoned the
        // device may still be reading that buffer and that descriptor. The staging
        // writes below would race it; checking at submit time would be too late.
        if self.tx_queue.is_failed() {
            return Err(NetError::DeviceError);
        }
        // Stage: zeroed 12-byte header, then the frame.
        // SAFETY: tx_buf_phys backs one HHDM-mapped frame we own; the total
        // length is bounded by the check above and access is serialised by the
        // enclosing device mutex.
        let staged = VIRTIO_NET_HDR_LEN + frame.len();
        unsafe {
            let buf = core::slice::from_raw_parts_mut(
                self.tx_buf_phys.to_virt().as_mut_ptr::<u8>(),
                staged,
            );
            buf[..VIRTIO_NET_HDR_LEN].fill(0);
            buf[VIRTIO_NET_HDR_LEN..].copy_from_slice(frame);
        }
        // One device-readable descriptor (no NEXT, no WRITE): the device reads
        // the staged bytes out and transmits them.
        self.tx_queue.write_desc(
            0,
            VirtqDesc {
                addr: self.tx_buf_phys.as_u64(),
                len: staged as u32,
                flags: 0,
                next: 0,
            },
        );
        // Submit and poll until the device has consumed the frame. If the device
        // never returns the chain — which happens when the shared NIC is reset or
        // re-initialised while this transmit is in flight — the queue disables
        // itself and we report a device error so the caller drops the frame.
        // Spinning here would freeze the kernel: we hold the device lock, so
        // interrupts are disabled.
        //
        // The staging buffer and descriptor are protected by the `is_failed` check at
        // the top of this function, which runs before either is written.
        if let Err(e) = self.tx_queue.submit_and_wait(0) {
            // Log the first failure per queue only; `Virtqueue::fail` already
            // reports the underlying cause, and a chatty TX path under a wedged
            // device would bury it.
            if !matches!(e, VirtioError::QueueDesynchronised) {
                crate::println!("[virtio-net] transmit failed: {e:?}; frame dropped");
            }
            return Err(NetError::DeviceError);
        }
        Ok(())
    }

    /// Poll for one received frame; copy it (minus the VirtIO-net header) into
    /// `buf` and re-post the buffer. Returns bytes copied, or 0 if none waiting.
    fn receive(&mut self, buf: &mut [u8]) -> Result<usize, NetError> {
        let (id, len) = match self.rx_queue.poll_used() {
            Some(entry) => entry,
            None => return Ok(0), // no frame waiting
        };

        // The buffer for descriptor `id` holds: 12-byte header, then the frame.
        let total = len as usize;
        let frame_len = total.saturating_sub(VIRTIO_NET_HDR_LEN);
        let n = frame_len.min(buf.len());
        if n > 0 {
            // SAFETY: descriptor `id` (< N_RX_BUFS) addresses buffer `id` in the
            // HHDM-mapped RX region; the device wrote `total` bytes into it.
            unsafe {
                let base = self
                    .rx_bufs_phys
                    .to_virt()
                    .as_mut_ptr::<u8>()
                    .add(id as usize * RX_BUF_SIZE + VIRTIO_NET_HDR_LEN);
                buf[..n].copy_from_slice(core::slice::from_raw_parts(base, n));
            }
        }

        // Re-post the buffer so the device can reuse it for the next frame.
        let buf_phys = self.rx_bufs_phys.as_u64() + id as u64 * RX_BUF_SIZE as u64;
        self.rx_queue.write_desc(
            id,
            VirtqDesc {
                addr: buf_phys,
                len: RX_BUF_SIZE as u32,
                flags: VIRTQ_DESC_F_WRITE,
                next: 0,
            },
        );
        self.rx_queue.publish(id);
        self.rx_queue.kick();

        Ok(n)
    }
}

impl NetDevice for VirtioNet {
    fn transmit(&self, frame: &[u8]) -> Result<(), NetError> {
        self.inner.lock().transmit(frame)
    }

    fn receive(&self, buf: &mut [u8]) -> Result<usize, NetError> {
        self.inner.lock().receive(buf)
    }

    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn mtu(&self) -> usize {
        self.mtu
    }
}
