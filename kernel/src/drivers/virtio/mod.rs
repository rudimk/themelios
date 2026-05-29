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

use core::ptr::{read_volatile, write_volatile};

use crate::mm::addr::{PhysAddr, VirtAddr};
use crate::mm::{frame, mmio, PAGE_SIZE};

pub mod blk;

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
}

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
    fn new(size: u16, notify_addr: VirtAddr) -> Result<Self, VirtioError> {
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
            desc: desc_phys.to_virt(),
            avail: avail_phys.to_virt(),
            used: used_phys.to_virt(),
            desc_phys,
            avail_phys,
            used_phys,
            notify_addr,
            last_used_idx: 0,
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
    fn submit_and_wait(&mut self, head: u16) {
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
            write_volatile(self.notify_addr.as_u64() as *mut u16, 0);
        }

        // 5. Poll the used ring until its index advances past what we last saw,
        // signalling the device has returned our chain.
        loop {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            let used_idx = unsafe { mmio_read_u16(self.used, RING_IDX) };
            if used_idx != self.last_used_idx {
                self.last_used_idx = used_idx;
                break;
            }
            core::hint::spin_loop();
        }
    }
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

        let queue = Virtqueue::new(size, notify_addr)?;

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
