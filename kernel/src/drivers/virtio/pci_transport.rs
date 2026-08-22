//! # virtio-PCI (modern) transport
//!
//! The VirtIO 1.0+ PCI transport: the implementation of [`crate::drivers::virtio::transport::VirtioTransport`]
//! used on x86_64, where VirtIO devices arrive as PCI functions.
//!
//! ## What makes this transport distinctive
//!
//! **Register regions are found by walking PCI vendor capabilities.** A modern virtio-PCI
//! device advertises up to five regions through capabilities tagged with a `cfg_type`:
//! common config (1), notify (2), ISR (3), and device config (4). Each names a BAR and an
//! offset within it, which `init` maps uncached. virtio-mmio needs none of this — its
//! registers sit at fixed offsets from a slot base.
//!
//! **The doorbell is per queue**, at `notify_base + queue_notify_off * notify_off_multiplier`,
//! where `queue_notify_off` is a register of the *selected* queue. virtio-mmio has a single
//! shared `QueueNotify`. This difference is why [`crate::drivers::virtio::transport::Notifier`] exists: the
//! transport resolves an address and the virtqueue merely rings it.
//!
//! ## Why the offsets live here
//!
//! The `COMMON_*` constants below are byte offsets into `struct virtio_pci_common_cfg`
//! (VirtIO 1.x spec). They were module-level constants in `virtio/mod.rs`, which made them
//! look like shared vocabulary — they are not. They describe one transport's
//! register layout and mean nothing to virtio-mmio, whose registers are at entirely
//! different offsets with different widths. Keeping them beside the only code that may
//! use them is what stops 8.3 from accidentally inheriting them.


use crate::drivers::pci::PciDevice;
use crate::mm::addr::{PhysAddr, VirtAddr};
use crate::mm::mmio;

use super::transport::{Notifier, NotifyWidth, SelectedQueue, VirtioTransport};
use super::{
    mmio_read_u16, mmio_read_u32, mmio_read_u8, mmio_write_u16, mmio_write_u32, mmio_write_u64,
    mmio_write_u8, VirtioError,
};

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
const COMMON_CONFIG_GENERATION: u64 = 0x15; // u8
const COMMON_QUEUE_SELECT: u64 = 0x16; // u16
const COMMON_QUEUE_SIZE: u64 = 0x18; // u16
const COMMON_QUEUE_MSIX_VECTOR: u64 = 0x1A; // u16
const COMMON_QUEUE_ENABLE: u64 = 0x1C; // u16
const COMMON_QUEUE_NOTIFY_OFF: u64 = 0x1E; // u16 (read-only)
const COMMON_QUEUE_DESC: u64 = 0x20; // u64
const COMMON_QUEUE_DRIVER: u64 = 0x28; // u64 (available ring)
const COMMON_QUEUE_DEVICE: u64 = 0x30; // u64 (used ring)

/// A VirtIO device's discovered register regions and negotiated state.
///
/// Constructed by [`PciTransport::init`], which walks the PCI capabilities,
/// maps the MMIO regions, and runs the initialisation handshake up to (but not
/// including) `DRIVER_OK`. The device-specific driver then negotiates features,
/// sets up virtqueues, and finally calls `set_driver_ok`.
pub struct PciTransport {
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
    /// Size of the device-config region in bytes.
    ///
    /// `init` reads this from the capability and used to discard it, so `device_config()`
    /// handed drivers an unbounded pointer. Keeping it lets `read_config` reject an
    /// over-long read instead of walking off the end of the mapping.
    device_cfg_len: usize,
}

impl PciTransport {
    /// Locate a VirtIO PCI device's register regions and map them.
    ///
    /// Walks the device's PCI vendor capabilities, mapping each register region
    /// (common / notify / ISR / device config) into uncached MMIO. That is the whole job:
    /// **the reset-and-acknowledge handshake is not done here.** It is spec sequencing
    /// identical on every transport, so it lives once as
    /// [`VirtioTransport::begin_init`] and is driven from
    /// [`crate::drivers::virtio::discovery::VirtioDevice::open_transport`], which is the
    /// one place both transports pass through.
    ///
    /// The caller continues with `negotiate_features`, `setup_queue`, and
    /// `set_driver_ok`.
    pub fn init(dev: &PciDevice) -> Result<Self, VirtioError> {
        let mut common: Option<VirtAddr> = None;
        let mut notify_base: Option<VirtAddr> = None;
        let mut notify_off_multiplier: u32 = 0;
        let mut isr: Option<VirtAddr> = None;
        let mut device_cfg: Option<VirtAddr> = None;
        let mut device_cfg_len: usize = 0;

        for cap in dev.capabilities() {
            // Only vendor-specific capabilities describe VirtIO regions.
            if cap.id != crate::drivers::pci::CAP_ID_VENDOR {
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
                CFG_TYPE_DEVICE => {
                    device_cfg = Some(mapped);
                    device_cfg_len = region_len as usize;
                }
                _ => {}
            }
        }

        let common = common.ok_or(VirtioError::MissingCapability)?;
        let notify_base = notify_base.ok_or(VirtioError::MissingCapability)?;
        let isr = isr.ok_or(VirtioError::MissingCapability)?;

        Ok(Self {
            common,
            notify_base,
            notify_off_multiplier,
            isr,
            device_cfg,
            device_cfg_len,
        })
    }

}

/// virtio-PCI (modern, 1.0+) implementation of the transport interface.
///
/// Every method here is the register access the PCI transport defines; the spec
/// sequencing built on top of them lives once on the trait. The `COMMON_*` offsets these
/// read and write are the `virtio_pci_common_cfg` layout and mean nothing to any other
/// transport.
impl VirtioTransport for PciTransport {
    fn status(&self) -> u8 {
        // SAFETY: common config is a mapped MMIO region with a status byte.
        unsafe { mmio_read_u8(self.common, COMMON_DEVICE_STATUS) }
    }

    fn set_status(&self, value: u8) {
        // SAFETY: writing the status register is the defined way to drive the
        // init state machine.
        unsafe { mmio_write_u8(self.common, COMMON_DEVICE_STATUS, value) }
    }

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

    fn write_driver_features(&self, features: u64) {
        // SAFETY: feature select/write registers are in the common config.
        unsafe {
            mmio_write_u32(self.common, COMMON_DRIVER_FEATURE_SELECT, 0);
            mmio_write_u32(self.common, COMMON_DRIVER_FEATURE, features as u32);
            mmio_write_u32(self.common, COMMON_DRIVER_FEATURE_SELECT, 1);
            mmio_write_u32(self.common, COMMON_DRIVER_FEATURE, (features >> 32) as u32);
        }
    }

    fn num_queues(&self) -> u16 {
        // SAFETY: num_queues is a read-only register in the common config.
        unsafe { mmio_read_u16(self.common, COMMON_NUM_QUEUES) }
    }

    fn select_queue(&self, index: u16) -> SelectedQueue {
        // SAFETY: queue_select and queue_size are common-config registers.
        let max_size = unsafe {
            mmio_write_u16(self.common, COMMON_QUEUE_SELECT, index);
            mmio_read_u16(self.common, COMMON_QUEUE_SIZE)
        };
        SelectedQueue { index, max_size }
    }

    fn queue_notifier(&self, _queue: &SelectedQueue) -> Notifier {
        // virtio-PCI gives each queue its own doorbell:
        //   notify_base + queue_notify_off * notify_off_multiplier
        // `queue_notify_off` is a register of the *selected* queue, which is why this
        // must follow `queue_max_size` for the same index. virtio-mmio has no analogue —
        // one shared register serves every queue — which is the whole reason the address
        // is resolved here rather than in `Virtqueue`.
        // SAFETY: queue_notify_off is a common-config register.
        let notify_off = unsafe { mmio_read_u16(self.common, COMMON_QUEUE_NOTIFY_OFF) };
        Notifier::at(
            VirtAddr::new(
                self.notify_base.as_u64()
                    + (notify_off as u64) * (self.notify_off_multiplier as u64),
            ),
            // virtio-PCI's per-queue doorbell is a 16-bit register.
            NotifyWidth::U16,
        )
    }

    fn configure_queue(
        &self,
        queue: &SelectedQueue,
        size: u16,
        desc: PhysAddr,
        avail: PhysAddr,
        used: PhysAddr,
    ) {
        // The token proves the right queue is selected; assert the size we were handed
        // actually fits it, which is the one thing the token cannot encode.
        debug_assert!(
            size <= queue.max_size,
            "queue {} programmed with size {} > device maximum {}",
            queue.index,
            size,
            queue.max_size
        );

        // Program the queue: size, ring physical addresses, disable MSI-X
        // (0xFFFF = NO_VECTOR — we poll), then enable.
        // SAFETY: all are common-config registers for the selected queue.
        unsafe {
            mmio_write_u16(self.common, COMMON_QUEUE_SIZE, size);
            mmio_write_u16(self.common, COMMON_QUEUE_MSIX_VECTOR, 0xFFFF);
            mmio_write_u64(self.common, COMMON_QUEUE_DESC, desc.as_u64());
            mmio_write_u64(self.common, COMMON_QUEUE_DRIVER, avail.as_u64());
            mmio_write_u64(self.common, COMMON_QUEUE_DEVICE, used.as_u64());
            mmio_write_u16(self.common, COMMON_QUEUE_ENABLE, 1);
        }
    }

    fn config_len(&self) -> usize {
        self.device_cfg_len
    }

    fn config_read_raw(&self, offset: usize, out: &mut [u8]) {
        let Some(base) = self.device_cfg else {
            // `read_config` bounds-checks against `config_len`, which is 0 without a
            // region, so this is unreachable rather than a silent zero-fill.
            debug_assert!(out.is_empty(), "config read with no device-config region");
            return;
        };
        // Byte-at-a-time. virtio-PCI permits wider accesses here, but the narrow form is
        // the one both transports allow, and device config is read once at init — there
        // is nothing to gain from a faster path that only works on one transport.
        for (i, byte) in out.iter_mut().enumerate() {
            // SAFETY: `read_config` has already bounded `offset + out.len()` by
            // `config_len`, which is the length of this mapped region.
            *byte = unsafe { mmio_read_u8(base, (offset + i) as u64) };
        }
    }

    fn config_generation(&self) -> u32 {
        // SAFETY: config_generation is a common-config register.
        unsafe { mmio_read_u8(self.common, COMMON_CONFIG_GENERATION) as u32 }
    }
}
