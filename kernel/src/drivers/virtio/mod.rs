//! # VirtIO PCI transport and split virtqueue
//!
//! VirtIO is the paravirtualised device standard QEMU/KVM (and the cloud
//! hypervisors we target) use for block, network, and console devices. Rather
//! than emulating real hardware register-by-register, the guest and host agree
//! on a small, efficient ring-buffer protocol. This module implements the parts
//! that are **common to every VirtIO device type**:
//!
//! - **Transport discovery**: finding a device's register regions by walking
//!   its PCI vendor capabilities (the modern, VirtIO 1.0+ interface).
//! - **Device initialisation handshake**: the status-bit sequence that brings a
//!   device from reset to "driver OK".
//! - **Feature negotiation**: agreeing on an optional feature set.
//! - **Split virtqueue**: the descriptor table + available ring + used ring
//!   data structure through which buffers are handed to and returned from the
//!   device.
//!
//! The device-specific driver (e.g. `blk.rs` for the block device) builds on
//! top of this: it negotiates its own feature bits, reads its device-specific
//! config, and submits request descriptor chains to a virtqueue.
//!
//! ## Modern (1.0+) vs legacy
//!
//! We speak the modern interface exclusively. A modern VirtIO device advertises
//! up to five register regions through PCI vendor-specific capabilities, each
//! tagged with a `cfg_type`:
//!
//! | cfg_type | region        | purpose                                  |
//! |----------|---------------|------------------------------------------|
//! | 1        | common config | device/driver features, status, queues   |
//! | 2        | notify        | "kick" doorbell — tell the device to run |
//! | 3        | ISR           | interrupt-status byte (read to ack INTx) |
//! | 4        | device config | device-type-specific config (e.g. capacity)|
//!
//! Each capability names a BAR and an offset within it; we map those MMIO
//! windows uncached (see `mm::mmio`) and access them with volatile reads/writes.
//!
//! ## Polling, not interrupts
//!
//! Phase 3 uses **synchronous polling**: after kicking the device we spin on the
//! used ring's index until our buffer comes back. This keeps the block path
//! simple and deterministic. Interrupt-driven completion (MSI-X) is a later
//! optimisation; the VirtIO spec explicitly permits polling.
//!
//! That spin is **bounded**, and giving up is not recoverable. Callers hold the
//! device lock with interrupts disabled for the whole poll, so an unbounded wait
//! freezes the entire kernel rather than just the calling task. When the bound is
//! reached the chain is still device-owned, so the queue disables itself
//! permanently instead of reusing descriptors the device may still touch — see
//! [`Virtqueue::fail`] and [`MAX_COMPLETION_SPINS`].

/// Arch-neutral device discovery — "which VirtIO devices does this machine have?"
///
/// Answers that question without the caller naming PCI, so the eighteen call sites that
/// used to walk PCI config space directly survive the move to virtio-mmio (8.3). The
/// register-level transport stays PCI-shaped until 8.2.
pub mod discovery;

pub use discovery::{devices, devices_of_kind, first_of_kind, VirtioDevice, VirtioKind};

use core::ptr::{read_volatile, write_volatile};

use crate::mm::addr::{PhysAddr, VirtAddr};
use crate::mm::{frame, mmio, PAGE_SIZE};

pub mod blk;
pub mod net;

// --- VirtIO PCI capability layout (fields relative to the capability offset) ---

/// Offset of `cfg_type` within a VirtIO PCI capability (identifies the region).
const VIRTIO_CAP_CFG_TYPE: u8 = 3;
/// Offset of the `bar` index within a VirtIO PCI capability.
const VIRTIO_CAP_BAR: u8 = 4;
/// Offset of the 32-bit `offset` (within the BAR) field.
const VIRTIO_CAP_OFFSET: u8 = 8;
/// Offset of the 32-bit `length` field.
const VIRTIO_CAP_LENGTH: u8 = 12;
/// Offset of the 32-bit `notify_off_multiplier` (only in the notify capability).
const VIRTIO_CAP_NOTIFY_MULT: u8 = 16;

/// `cfg_type` values identifying each VirtIO register region.
const CFG_TYPE_COMMON: u8 = 1;
const CFG_TYPE_NOTIFY: u8 = 2;
const CFG_TYPE_ISR: u8 = 3;
const CFG_TYPE_DEVICE: u8 = 4;

// --- Common configuration structure register offsets ---
//
// These are byte offsets into the "common config" MMIO region (cfg_type 1),
// matching `struct virtio_pci_common_cfg` in the VirtIO 1.x spec.

const COMMON_DEVICE_FEATURE_SELECT: u64 = 0x00; // u32
const COMMON_DEVICE_FEATURE: u64 = 0x04; // u32 (read-only)
const COMMON_DRIVER_FEATURE_SELECT: u64 = 0x08; // u32
const COMMON_DRIVER_FEATURE: u64 = 0x0C; // u32
const COMMON_NUM_QUEUES: u64 = 0x12; // u16 (read-only)
const COMMON_DEVICE_STATUS: u64 = 0x14; // u8
const COMMON_QUEUE_SELECT: u64 = 0x16; // u16
const COMMON_QUEUE_SIZE: u64 = 0x18; // u16
const COMMON_QUEUE_MSIX_VECTOR: u64 = 0x1A; // u16
const COMMON_QUEUE_ENABLE: u64 = 0x1C; // u16
const COMMON_QUEUE_NOTIFY_OFF: u64 = 0x1E; // u16 (read-only)
const COMMON_QUEUE_DESC: u64 = 0x20; // u64
const COMMON_QUEUE_DRIVER: u64 = 0x28; // u64 (available ring)
const COMMON_QUEUE_DEVICE: u64 = 0x30; // u64 (used ring)

// --- Device status bits (written to COMMON_DEVICE_STATUS) ---

/// The guest has noticed the device.
const STATUS_ACKNOWLEDGE: u8 = 1;
/// The guest has a driver for the device.
const STATUS_DRIVER: u8 = 2;
/// The driver is set up and ready to drive the device.
const STATUS_DRIVER_OK: u8 = 4;
/// The driver has accepted the feature set it negotiated.
const STATUS_FEATURES_OK: u8 = 8;
/// Something went wrong; the driver has given up on the device.
const STATUS_FAILED: u8 = 128;

/// Feature bit 32: the device conforms to the modern VirtIO 1.0+ spec. The
/// driver MUST negotiate this bit, otherwise the device falls back to legacy.
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Descriptor flag: the buffer continues in the `next` descriptor (a chain).
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// Descriptor flag: the device writes to this buffer (vs. reads from it).
pub const VIRTQ_DESC_F_WRITE: u16 = 2;

/// Maximum virtqueue size we use, regardless of the device's reported maximum.
///
/// 256 descriptors × 16 bytes = exactly one 4 KiB page for the descriptor
/// table, matching the Phase 3 memory budget. Smaller queues from the device
/// are honoured; larger ones are capped to this.
const MAX_QUEUE_SIZE: u16 = 256;

// ----- Volatile MMIO accessors -----
//
// Device registers must be accessed with volatile reads/writes so the compiler
// never caches, reorders, or elides them. The regions are mapped uncached, so
// each access goes straight to the device.

#[inline]
unsafe fn mmio_read_u8(base: VirtAddr, offset: u64) -> u8 {
    unsafe { read_volatile((base.as_u64() + offset) as *const u8) }
}
#[inline]
unsafe fn mmio_read_u16(base: VirtAddr, offset: u64) -> u16 {
    unsafe { read_volatile((base.as_u64() + offset) as *const u16) }
}
#[inline]
unsafe fn mmio_read_u32(base: VirtAddr, offset: u64) -> u32 {
    unsafe { read_volatile((base.as_u64() + offset) as *const u32) }
}
#[inline]
unsafe fn mmio_write_u8(base: VirtAddr, offset: u64, value: u8) {
    unsafe { write_volatile((base.as_u64() + offset) as *mut u8, value) }
}
#[inline]
unsafe fn mmio_write_u16(base: VirtAddr, offset: u64, value: u16) {
    unsafe { write_volatile((base.as_u64() + offset) as *mut u16, value) }
}
#[inline]
unsafe fn mmio_write_u32(base: VirtAddr, offset: u64, value: u32) {
    unsafe { write_volatile((base.as_u64() + offset) as *mut u32, value) }
}
#[inline]
unsafe fn mmio_write_u64(base: VirtAddr, offset: u64, value: u64) {
    unsafe { write_volatile((base.as_u64() + offset) as *mut u64, value) }
}

/// Errors that can occur while bringing up or driving a VirtIO device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VirtioError {
    /// The device is missing a required VirtIO PCI capability (common/notify/ISR).
    MissingCapability,
    /// The device did not accept the negotiated features (FEATURES_OK cleared).
    FeatureNegotiationFailed,
    /// The selected virtqueue does not exist (queue size reported as 0).
    QueueUnavailable,
    /// Out of physical frames for the virtqueue rings.
    OutOfMemory,
    /// The device signalled an error during initialisation.
    DeviceError,
    /// The device did not complete a submitted descriptor chain within the bounded
    /// poll window (see [`MAX_COMPLETION_SPINS`]). Usually means the device was
    /// reset or re-initialised underneath an in-flight request, so the used ring
    /// will never advance for that chain.
    DeviceTimeout,
    /// The queue's view of the rings no longer matches the device's, so completions
    /// can no longer be attributed to the requests that produced them. Permanent
    /// until the device is re-initialised; see [`Virtqueue::fail`].
    QueueDesynchronised,
}

/// Upper bound on how many times [`Virtqueue::submit_and_wait`] polls the used ring
/// before declaring the device stuck.
///
/// This window is time during which the **entire kernel is frozen** — the caller
/// holds the device lock, so interrupts are disabled for its whole duration — so it
/// is sized against measurements, not estimates. All figures below were measured
/// under QEMU TCG in a debug build, the configuration CI runs.
///
/// What a *healthy* completion costs (per-queue high-water mark over a full suite):
///
/// | condition        | max spins observed | ≈ wall |
/// |------------------|--------------------|--------|
/// | idle             | 1,000 – 6,000      | 1–7 ms |
/// | 4× host CPU load | ~10,000            | ~11 ms |
///
/// So completions take **milliseconds, not microseconds**, and the high-water mark
/// had not converged across four runs (1.0k → 3.1k → 6.0k → 10.0k) — it is a fat
/// tail measured from few samples. 2,000,000 leaves ~200× headroom over the observed
/// tail, at a worst-case freeze of ~2.3 s (measured: 200,000 spins ≈ 230–250 ms idle,
/// ~440 ms under load). That is the trade being made: a *spurious* timeout is safe
/// but permanently disables the queue ([`Virtqueue::fail`]), which turns a passing
/// run red, whereas the freeze only ever costs that 2.3 s on a device that has
/// genuinely wedged.
///
/// Note this bound also sets the runtime of the fault-injection test that exercises
/// the timeout path, so raising it further lengthens the suite proportionally.
///
/// A real deadline would be better than a raw count. `rdtsc` keeps advancing with
/// interrupts disabled (unlike the monotonic tick, which stalls), and the 8254 PIT
/// counter can be latched and read over ports 0x43/0x40 for a genuine 10 ms-grained
/// deadline with no calibration at all. Either is a separate change.
const MAX_COMPLETION_SPINS: u32 = 2_000_000;

/// A single split-virtqueue descriptor (16 bytes, matches the VirtIO spec).
///
/// Describes one physically-contiguous buffer: its guest-physical address, its
/// length, flags (chain/write), and the index of the next descriptor in a chain.
#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// A split virtqueue: the ring buffers through which buffers are exchanged.
///
/// Three regions, each in its own physically-contiguous allocation so the
/// device can DMA to/from them:
/// - **Descriptor table**: the pool of buffer descriptors.
/// - **Available ring**: indices the driver makes available for the device.
/// - **Used ring**: indices the device has finished and returns to the driver.
///
/// All three are accessed via the HHDM (they are ordinary RAM frames); the
/// physical addresses are programmed into the device's common config.
pub struct Virtqueue {
    /// Number of descriptors (queue size).
    size: u16,
    /// This queue's index (0-based). Written to the notify doorbell to tell the
    /// device which queue has new buffers — queue 0 for a block device, or the
    /// receive/transmit queues (0/1) of a network device.
    index: u16,
    /// HHDM virtual base of the descriptor table.
    desc: VirtAddr,
    /// HHDM virtual base of the available ring.
    avail: VirtAddr,
    /// HHDM virtual base of the used ring.
    used: VirtAddr,
    /// Physical address of the descriptor table (programmed into the device).
    desc_phys: PhysAddr,
    /// Physical address of the available ring.
    avail_phys: PhysAddr,
    /// Physical address of the used ring.
    used_phys: PhysAddr,
    /// MMIO doorbell address: writing the queue index here kicks the device.
    notify_addr: VirtAddr,
    /// Last used-ring index we have observed (for polling completions).
    last_used_idx: u16,
    /// Set once this queue has desynchronised from the device — a submitted chain
    /// was abandoned on timeout, or the device returned a completion for a chain we
    /// did not submit. See [`Virtqueue::fail`] for why the queue must then be
    /// refused rather than reused.
    failed: bool,
}

// --- Available / used ring field offsets (within their respective regions) ---
//
// Available ring:  { flags: u16, idx: u16, ring: [u16; size], used_event: u16 }
// Used ring:       { flags: u16, idx: u16, ring: [{id:u32,len:u32}; size], avail_event: u16 }

const RING_FLAGS: u64 = 0;
const RING_IDX: u64 = 2;
const RING_ENTRIES: u64 = 4;

impl Virtqueue {
    /// Allocate and zero a split virtqueue of `size` descriptors.
    ///
    /// Each of the three regions gets its own contiguous physical allocation
    /// (page-aligned, which satisfies all VirtIO alignment requirements) and is
    /// zeroed. Returns `OutOfMemory` if any allocation fails.
    fn new(size: u16, index: u16, notify_addr: VirtAddr) -> Result<Self, VirtioError> {
        let n = size as usize;
        // Region sizes per the spec.
        let desc_bytes = 16 * n;
        let avail_bytes = 6 + 2 * n;
        let used_bytes = 6 + 8 * n;

        let desc_phys = alloc_zeroed(desc_bytes).ok_or(VirtioError::OutOfMemory)?;
        let avail_phys = alloc_zeroed(avail_bytes).ok_or(VirtioError::OutOfMemory)?;
        let used_phys = alloc_zeroed(used_bytes).ok_or(VirtioError::OutOfMemory)?;

        Ok(Self {
            size,
            index,
            desc: desc_phys.to_virt(),
            avail: avail_phys.to_virt(),
            used: used_phys.to_virt(),
            desc_phys,
            avail_phys,
            used_phys,
            notify_addr,
            last_used_idx: 0,
            failed: false,
        })
    }

    /// Write a descriptor into the table at `index`.
    fn write_desc(&self, index: u16, desc: VirtqDesc) {
        // SAFETY: index < size; the descriptor table has `size` 16-byte slots
        // in HHDM-mapped RAM.
        unsafe {
            let ptr = (self.desc.as_u64() + (index as u64) * 16) as *mut VirtqDesc;
            write_volatile(ptr, desc);
        }
    }

    /// Submit a chain of descriptors (already written via `write_desc`) starting
    /// at `head`, then kick the device and poll until it completes the chain.
    ///
    /// This is the synchronous request primitive: the driver lays out a chain
    /// (e.g. request header → data buffer → status byte for a block request),
    /// places `head` in the available ring, increments the available index,
    /// rings the doorbell, and spins on the used ring.
    ///
    /// Returns [`VirtioError::DeviceTimeout`] if the device does not return the
    /// chain within [`MAX_COMPLETION_SPINS`]. Callers must treat that as a failed
    /// request rather than retrying indefinitely — see the comment on the poll loop
    /// for why the wait is bounded at all.
    fn submit_and_wait(&mut self, head: u16) -> Result<(), VirtioError> {
        // Refuse work on a queue that has already desynchronised (see `fail`).
        if self.failed {
            return Err(VirtioError::QueueDesynchronised);
        }

        // 1. Publish `head` into the available ring at the current avail index.
        // SAFETY: avail ring is HHDM-mapped RAM of the correct size.
        let avail_idx = unsafe { mmio_read_u16(self.avail, RING_IDX) };
        let ring_slot = avail_idx % self.size;
        unsafe {
            let entry_ptr =
                (self.avail.as_u64() + RING_ENTRIES + (ring_slot as u64) * 2) as *mut u16;
            write_volatile(entry_ptr, head);
        }

        // 2. A write barrier ensures the descriptor and ring entry are visible
        // before we advance the index the device reads.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        // 3. Advance the available index so the device sees the new entry.
        unsafe {
            mmio_write_u16(self.avail, RING_IDX, avail_idx.wrapping_add(1));
        }

        // 4. Barrier, then kick the device by writing the queue index to the
        // notify doorbell.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        unsafe {
            write_volatile(self.notify_addr.as_u64() as *mut u16, self.index);
        }

        // 5. Poll the used ring until its index advances past what we last saw,
        // signalling the device has returned our chain — but never forever.
        //
        // Both callers (the block driver and the NIC's TX path) hold the device's
        // `InterruptMutex` across this call, so **interrupts are disabled while we
        // spin**. An unbounded spin here therefore does not merely stall the calling
        // task: it stops the timer, so nothing preempts us and the whole kernel is
        // dead with no output. That is a real failure mode, not a theoretical one —
        // it is what wedged the test suite when one net-service was mid-transmit
        // while another bring-up reset the same shared VirtIO device out from under
        // it, leaving a used ring that would never advance.
        for _ in 0..MAX_COMPLETION_SPINS {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            let used_idx = unsafe { mmio_read_u16(self.used, RING_IDX) };
            if used_idx == self.last_used_idx {
                core::hint::spin_loop();
                continue;
            }

            // Consume exactly ONE entry — the one at our cursor — rather than
            // jumping the cursor to the device's index. Jumping would silently
            // swallow any extra entry, and (worse) after an abandoned request it
            // would let this call adopt the *previous* request's late completion as
            // its own. Reading the element also lets us assert the device returned
            // the chain we actually submitted, which the index alone cannot tell us.
            let slot = (self.last_used_idx % self.size) as u64;
            // SAFETY: used ring is HHDM-mapped RAM; entries start at RING_ENTRIES
            // and are 8 bytes each ({ id: u32, len: u32 }), slot < size.
            let id = unsafe { read_volatile((self.used.as_u64() + RING_ENTRIES + slot * 8) as *const u32) };
            self.last_used_idx = self.last_used_idx.wrapping_add(1);

            if id as u16 != head {
                // The device completed a chain we did not just submit. Our view of
                // the ring no longer matches the device's, so every later completion
                // would be attributed to the wrong request.
                self.fail("completion id mismatch");
                return Err(VirtioError::QueueDesynchronised);
            }
            return Ok(());
        }

        // Timed out. The chain is STILL published and device-owned: the descriptors
        // and their buffers may be read or written by the device at any later point.
        // Reusing them would race the device, and a late completion would be picked
        // up by whichever request polls next. Poison the queue instead.
        self.fail("completion timeout");
        Err(VirtioError::DeviceTimeout)
    }

    /// Whether this queue has desynchronised and been disabled ([`Virtqueue::fail`]).
    ///
    /// Callers stage data into device-owned memory (descriptors, bounce/staging
    /// buffers) *before* submitting, so they must check this first: once a chain has
    /// been abandoned, those buffers may still be read by the device, and overwriting
    /// them races it. Checking only at submit time is too late — the writes have
    /// already happened.
    fn is_failed(&self) -> bool {
        self.failed
    }

    /// Mark this queue permanently unusable and say so once.
    ///
    /// Called when the queue and the device disagree about ring state. Recovery is
    /// not possible from here: the abandoned descriptors are still device-owned, so
    /// we can neither reclaim them nor safely resynchronise the cursor — only a full
    /// device re-initialisation (which reprograms the rings) can clear this. Failing
    /// every later request is deliberate: a queue that keeps accepting work after
    /// desynchronising returns other requests' data as if it were yours, which is far
    /// worse than a loud, permanent I/O error.
    fn fail(&mut self, reason: &str) {
        if !self.failed {
            self.failed = true;
            crate::println!(
                "[virtio] queue {} failed: {reason} (last_used_idx={}); queue disabled",
                self.index,
                self.last_used_idx
            );
        }
    }

    // ----- Asynchronous (non-blocking) primitives for networking -----
    //
    // Block I/O is request/response and uses `submit_and_wait`. Networking is
    // different: RX buffers are pre-posted and filled by the device whenever
    // frames arrive, and the driver polls for completions without blocking. These
    // three primitives — publish, kick, poll_used — provide that pattern.

    /// Publish descriptor chain head `head` into the available ring, WITHOUT
    /// kicking the device or waiting. Used to (re)post RX buffers and to queue a
    /// TX frame before a single `kick`.
    fn publish(&self, head: u16) {
        // A desynchronised queue must not be handed more buffers — see `fail`.
        if self.failed {
            return;
        }
        // SAFETY: avail ring is HHDM-mapped RAM of the correct size.
        let avail_idx = unsafe { mmio_read_u16(self.avail, RING_IDX) };
        let slot = avail_idx % self.size;
        unsafe {
            let entry = (self.avail.as_u64() + RING_ENTRIES + (slot as u64) * 2) as *mut u16;
            write_volatile(entry, head);
        }
        // Ensure the ring entry is visible before we advance the index.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        unsafe {
            mmio_write_u16(self.avail, RING_IDX, avail_idx.wrapping_add(1));
        }
    }

    /// Kick the device: tell it this queue has newly-available buffers to process.
    fn kick(&self) {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        // SAFETY: notify_addr is this queue's mapped MMIO doorbell; the value is
        // the queue index (the vqn the device expects).
        unsafe {
            write_volatile(self.notify_addr.as_u64() as *mut u16, self.index);
        }
    }

    /// Non-blocking used-ring poll. If the device has returned a buffer since our
    /// last observation, returns `(descriptor head id, bytes written by device)`
    /// and advances our used cursor by one; otherwise returns `None`.
    fn poll_used(&mut self) -> Option<(u16, u32)> {
        // Completions on a desynchronised queue cannot be attributed to the right
        // request, so report "nothing waiting" forever rather than hand back a
        // buffer id we can no longer trust.
        if self.failed {
            return None;
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        // SAFETY: used ring is HHDM-mapped RAM of the correct size.
        let used_idx = unsafe { mmio_read_u16(self.used, RING_IDX) };
        if used_idx == self.last_used_idx {
            return None;
        }
        // Read the used element at our cursor. Used-ring entries start at
        // RING_ENTRIES and are 8 bytes each: { id: u32, len: u32 }.
        let slot = (self.last_used_idx % self.size) as u64;
        let (id, len) = unsafe {
            let base = self.used.as_u64() + RING_ENTRIES + slot * 8;
            let id = read_volatile(base as *const u32);
            let len = read_volatile((base + 4) as *const u32);
            (id, len)
        };
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Some((id as u16, len))
    }
}

/// Fault-injection coverage for the queue-failure paths (test builds only).
///
/// These paths are unreachable from the normal suite — a healthy QEMU device always
/// completes — so without injection the timeout and desynchronisation handling would
/// ship untested. That matters here because the *previous* version of this code
/// looked correct and silently returned one request's data as another's; the
/// assertions below are exactly what pins that behaviour down.
///
/// Drives a queue with no device behind it: the used ring is plain RAM that nobody
/// ever advances, and the doorbell is a scratch page rather than real MMIO.
#[cfg(feature = "test")]
pub fn test_queue_failure_paths() -> Result<(), &'static str> {
    const SIZE: u16 = 8;

    /// Build a standalone queue backed by RAM, with no device attached.
    fn fresh() -> Option<Virtqueue> {
        let desc = alloc_zeroed(SIZE as usize * 16)?;
        let avail = alloc_zeroed(6 + SIZE as usize * 2)?;
        let used = alloc_zeroed(6 + SIZE as usize * 8)?;
        // No device is listening, so the kick must land on harmless RAM.
        let doorbell = alloc_zeroed(PAGE_SIZE as usize)?;
        Some(Virtqueue {
            size: SIZE,
            index: 0xFF,
            desc: desc.to_virt(),
            avail: avail.to_virt(),
            used: used.to_virt(),
            desc_phys: desc,
            avail_phys: avail,
            used_phys: used,
            notify_addr: doorbell.to_virt(),
            last_used_idx: 0,
            failed: false,
        })
    }

    // --- 1. A device that never completes must time out, not spin forever ---
    let mut q = fresh().ok_or("queue alloc failed")?;
    match q.submit_and_wait(0) {
        Err(VirtioError::DeviceTimeout) => {}
        _ => return Err("submit_and_wait should time out when the used ring never advances"),
    }
    if !q.failed {
        return Err("a timeout must disable the queue");
    }

    // --- 2. A disabled queue must refuse everything afterwards ---
    // This is the assertion that matters most: the abandoned chain is still
    // device-owned, so accepting more work is what would let a late completion be
    // handed to the wrong request.
    match q.submit_and_wait(0) {
        Err(VirtioError::QueueDesynchronised) => {}
        _ => return Err("a disabled queue must refuse further submissions"),
    }
    if q.poll_used().is_some() {
        return Err("a disabled queue must report no completions");
    }

    // --- 3. A completion for a chain we did not submit must disable the queue ---
    // Simulate the device returning descriptor 5 while we wait on head 0, which is
    // what a late completion from an abandoned request looks like.
    let mut q2 = fresh().ok_or("queue alloc failed")?;
    unsafe {
        let base = q2.used.as_u64() + RING_ENTRIES;
        write_volatile(base as *mut u32, 5u32); // id
        write_volatile((base + 4) as *mut u32, 0u32); // len
        mmio_write_u16(q2.used, RING_IDX, 1);
    }
    match q2.submit_and_wait(0) {
        Err(VirtioError::QueueDesynchronised) => {}
        _ => return Err("a mismatched completion id must desynchronise the queue"),
    }
    if !q2.failed {
        return Err("an id mismatch must disable the queue");
    }

    Ok(())
}

/// Allocate `bytes` of physically-contiguous, zeroed memory.
///
/// Rounds up to whole frames and zeroes them (freshly allocated frames may hold
/// stale data, which would corrupt virtqueue state). Returns the physical base.
fn alloc_zeroed(bytes: usize) -> Option<PhysAddr> {
    let pages = ((bytes as u64 + PAGE_SIZE - 1) / PAGE_SIZE) as usize;
    let phys = frame::allocate_contiguous_frames(pages.max(1))?;
    // SAFETY: the frames are HHDM-mapped and owned by us for the device's life.
    unsafe {
        core::ptr::write_bytes(phys.to_virt().as_mut_ptr::<u8>(), 0, pages * PAGE_SIZE as usize);
    }
    Some(phys)
}

/// A VirtIO device's discovered register regions and negotiated state.
///
/// Constructed by `VirtioTransport::init`, which walks the PCI capabilities,
/// maps the MMIO regions, and runs the initialisation handshake up to (but not
/// including) `DRIVER_OK`. The device-specific driver then negotiates features,
/// sets up virtqueues, and finally calls `set_driver_ok`.
pub struct VirtioTransport {
    /// Common configuration MMIO region.
    common: VirtAddr,
    /// Notify region base MMIO address.
    notify_base: VirtAddr,
    /// Multiplier applied to a queue's `notify_off` to find its doorbell.
    notify_off_multiplier: u32,
    /// ISR status MMIO byte (read to acknowledge a legacy interrupt).
    #[allow(dead_code)]
    isr: VirtAddr,
    /// Device-specific config MMIO region (e.g. block capacity), if present.
    device_cfg: Option<VirtAddr>,
}

impl VirtioTransport {
    /// Discover and reset a VirtIO PCI device, leaving it ready for feature
    /// negotiation.
    ///
    /// Steps:
    /// 1. Walk the device's PCI vendor capabilities, mapping each register
    ///    region (common / notify / ISR / device config) into uncached MMIO.
    /// 2. Reset the device (write 0 to the status register).
    /// 3. Set ACKNOWLEDGE then DRIVER status bits.
    ///
    /// The caller continues with `negotiate_features`, `setup_queue`, and
    /// `set_driver_ok`.
    /// Bring up the transport for a device found by [`discovery`].
    ///
    /// The transport-neutral entry point, mirroring `VirtioBlk::init`. Only the
    /// transport self-test calls this directly; 8.2 replaces the body when
    /// `VirtioTransport` becomes a trait with a virtio-mmio implementation.
    pub fn init_for(dev: &VirtioDevice) -> Result<Self, VirtioError> {
        Self::init(dev.pci())
    }

    pub fn init(dev: &super::pci::PciDevice) -> Result<Self, VirtioError> {
        let mut common: Option<VirtAddr> = None;
        let mut notify_base: Option<VirtAddr> = None;
        let mut notify_off_multiplier: u32 = 0;
        let mut isr: Option<VirtAddr> = None;
        let mut device_cfg: Option<VirtAddr> = None;

        for cap in dev.capabilities() {
            // Only vendor-specific capabilities describe VirtIO regions.
            if cap.id != super::pci::CAP_ID_VENDOR {
                continue;
            }
            let cfg_type = dev.read_config_u8(cap.offset + VIRTIO_CAP_CFG_TYPE);
            let bar_index = dev.read_config_u8(cap.offset + VIRTIO_CAP_BAR);
            let region_offset = dev.read_config_u32(cap.offset + VIRTIO_CAP_OFFSET);
            let region_len = dev.read_config_u32(cap.offset + VIRTIO_CAP_LENGTH);

            // Resolve the BAR this region lives in to a physical base, then map
            // the (offset, length) window uncached.
            let bar = match dev.bar(bar_index) {
                Some(b) if b.is_memory => b,
                _ => continue, // skip I/O BARs and unimplemented slots
            };
            let region_phys = PhysAddr::new(bar.address + region_offset as u64);
            let mapped = mmio::map(region_phys, region_len.max(4) as usize);

            match cfg_type {
                CFG_TYPE_COMMON => common = Some(mapped),
                CFG_TYPE_NOTIFY => {
                    notify_base = Some(mapped);
                    notify_off_multiplier =
                        dev.read_config_u32(cap.offset + VIRTIO_CAP_NOTIFY_MULT);
                }
                CFG_TYPE_ISR => isr = Some(mapped),
                CFG_TYPE_DEVICE => device_cfg = Some(mapped),
                _ => {}
            }
        }

        let common = common.ok_or(VirtioError::MissingCapability)?;
        let notify_base = notify_base.ok_or(VirtioError::MissingCapability)?;
        let isr = isr.ok_or(VirtioError::MissingCapability)?;

        let transport = Self {
            common,
            notify_base,
            notify_off_multiplier,
            isr,
            device_cfg,
        };

        // Reset: write 0 to the status register and wait for it to read back 0.
        transport.set_status(0);
        // Acknowledge the device and announce we have a driver.
        transport.set_status(STATUS_ACKNOWLEDGE);
        transport.add_status(STATUS_DRIVER);

        Ok(transport)
    }

    /// Read the current device status byte.
    fn status(&self) -> u8 {
        // SAFETY: common config is a mapped MMIO region with a status byte.
        unsafe { mmio_read_u8(self.common, COMMON_DEVICE_STATUS) }
    }

    /// Overwrite the device status byte.
    fn set_status(&self, value: u8) {
        // SAFETY: writing the status register is the defined way to drive the
        // init state machine.
        unsafe { mmio_write_u8(self.common, COMMON_DEVICE_STATUS, value) }
    }

    /// OR additional bits into the device status byte.
    fn add_status(&self, bits: u8) {
        let cur = self.status();
        self.set_status(cur | bits);
    }

    /// Read the 64-bit device feature bitmap (two 32-bit halves).
    fn read_device_features(&self) -> u64 {
        // SAFETY: feature select/read registers are in the common config.
        unsafe {
            mmio_write_u32(self.common, COMMON_DEVICE_FEATURE_SELECT, 0);
            let low = mmio_read_u32(self.common, COMMON_DEVICE_FEATURE) as u64;
            mmio_write_u32(self.common, COMMON_DEVICE_FEATURE_SELECT, 1);
            let high = mmio_read_u32(self.common, COMMON_DEVICE_FEATURE) as u64;
            (high << 32) | low
        }
    }

    /// Write the 64-bit driver feature bitmap (the features we accept).
    fn write_driver_features(&self, features: u64) {
        // SAFETY: feature select/write registers are in the common config.
        unsafe {
            mmio_write_u32(self.common, COMMON_DRIVER_FEATURE_SELECT, 0);
            mmio_write_u32(self.common, COMMON_DRIVER_FEATURE, features as u32);
            mmio_write_u32(self.common, COMMON_DRIVER_FEATURE_SELECT, 1);
            mmio_write_u32(self.common, COMMON_DRIVER_FEATURE, (features >> 32) as u32);
        }
    }

    /// Negotiate features with the device.
    ///
    /// Always negotiates `VIRTIO_F_VERSION_1` (required for the modern
    /// interface) plus whatever `wanted` device-specific bits the device also
    /// offers. Sets FEATURES_OK and verifies the device accepts the set.
    /// Returns the features actually negotiated.
    pub fn negotiate_features(&self, wanted: u64) -> Result<u64, VirtioError> {
        let device_features = self.read_device_features();

        // We require VERSION_1; intersect the rest of what we want with what the
        // device offers.
        let negotiated = (device_features & (wanted | VIRTIO_F_VERSION_1))
            | VIRTIO_F_VERSION_1;

        // The device must offer VERSION_1, or it isn't a modern device.
        if device_features & VIRTIO_F_VERSION_1 == 0 {
            self.add_status(STATUS_FAILED);
            return Err(VirtioError::FeatureNegotiationFailed);
        }

        self.write_driver_features(negotiated);
        self.add_status(STATUS_FEATURES_OK);

        // The device clears FEATURES_OK if it cannot support our selection.
        if self.status() & STATUS_FEATURES_OK == 0 {
            self.add_status(STATUS_FAILED);
            return Err(VirtioError::FeatureNegotiationFailed);
        }

        Ok(negotiated)
    }

    /// Read the number of queues the device exposes.
    #[allow(dead_code)]
    pub fn num_queues(&self) -> u16 {
        // SAFETY: num_queues is a read-only register in the common config.
        unsafe { mmio_read_u16(self.common, COMMON_NUM_QUEUES) }
    }

    /// Set up virtqueue `index`, allocating its rings and programming the device.
    ///
    /// Selects the queue, reads its maximum size (capped to `MAX_QUEUE_SIZE`),
    /// allocates the descriptor/available/used regions, programs their physical
    /// addresses, computes the queue's notify doorbell, and enables the queue.
    /// Returns the ready-to-use `Virtqueue`.
    pub fn setup_queue(&self, index: u16) -> Result<Virtqueue, VirtioError> {
        // Select the queue we want to configure.
        // SAFETY: queue_select and friends are common-config registers.
        unsafe {
            mmio_write_u16(self.common, COMMON_QUEUE_SELECT, index);
        }

        let max_size = unsafe { mmio_read_u16(self.common, COMMON_QUEUE_SIZE) };
        if max_size == 0 {
            return Err(VirtioError::QueueUnavailable);
        }
        let size = max_size.min(MAX_QUEUE_SIZE);

        // Compute this queue's notify doorbell address:
        //   notify_base + queue_notify_off * notify_off_multiplier
        let notify_off = unsafe { mmio_read_u16(self.common, COMMON_QUEUE_NOTIFY_OFF) };
        let notify_addr = VirtAddr::new(
            self.notify_base.as_u64()
                + (notify_off as u64) * (self.notify_off_multiplier as u64),
        );

        let queue = Virtqueue::new(size, index, notify_addr)?;

        // Program the queue: size, ring physical addresses, disable MSI-X
        // (0xFFFF = NO_VECTOR — we poll), then enable.
        // SAFETY: all are common-config registers for the selected queue.
        unsafe {
            mmio_write_u16(self.common, COMMON_QUEUE_SIZE, size);
            mmio_write_u16(self.common, COMMON_QUEUE_MSIX_VECTOR, 0xFFFF);
            mmio_write_u64(self.common, COMMON_QUEUE_DESC, queue.desc_phys.as_u64());
            mmio_write_u64(self.common, COMMON_QUEUE_DRIVER, queue.avail_phys.as_u64());
            mmio_write_u64(self.common, COMMON_QUEUE_DEVICE, queue.used_phys.as_u64());
            mmio_write_u16(self.common, COMMON_QUEUE_ENABLE, 1);
        }

        Ok(queue)
    }

    /// Signal that the driver is fully set up — the device may now run.
    pub fn set_driver_ok(&self) {
        self.add_status(STATUS_DRIVER_OK);
    }

    /// Access the device-specific config region, if the device has one.
    pub fn device_config(&self) -> Option<VirtAddr> {
        self.device_cfg
    }
}
