//! # virtio-mmio transport
//!
//! The transport QEMU's aarch64 `virt` machine uses to present VirtIO devices, and the
//! second implementation of [`super::transport::VirtioTransport`].
//!
//! ## How it differs from virtio-PCI
//!
//! **No enumeration, no capability walk.** A `virt` machine has a fixed bank of 32 slots
//! at `0x0a00_0000`, 0x200 bytes apart, described by [`crate::platform`]. Every register
//! is at a fixed offset from its slot base — there is no BAR to program and no vendor
//! capability chain to follow.
//!
//! **One shared doorbell.** `QueueNotify` (0x050) serves every queue, where virtio-PCI
//! gives each queue its own. This is why [`super::transport::Notifier`] resolves an
//! address rather than the virtqueue computing one.
//!
//! **No `NumQueues` register.** virtio-PCI answers "how many queues?" in one read; the
//! mmio register map has no such field, only a per-selected-queue `QueueNumMax`. The
//! trait's provided `num_queues` probes upward, and this implementation deliberately does
//! not override it.
//!
//! ## Every control register is 32 bits, and getting that wrong is silent
//!
//! QEMU's model rejects any access to a register below `0x100` whose size is not four
//! bytes: it logs `"wrong size access to register!"` and then **drops the write** or
//! returns zero for a read — no fault, nothing the guest can observe. A byte-wide status
//! write would leave the device in reset while the driver believed it had acknowledged;
//! a 16-bit doorbell write would make every kick a no-op and every request time out.
//!
//! That is why this module has its own [`r32`]/[`w32`] rather than using the parent
//! module's six-width helpers, and why they assert alignment: `mm::mmio::map` produces
//! Device-`nGnRnE` memory on aarch64, where an unaligned access raises an Alignment fault
//! that Phase 7.2's handler treats as fatal. The same access is silently fine on x86.
//!
//! Device *config* space (`0x100` and above) is the exception — 1, 2 and 4 byte accesses
//! are all legal there, and an 8-byte access **calls `abort()` in QEMU**, which is why
//! [`VirtioTransport::config_read_raw`] copies bytes.

use crate::mm::addr::{PhysAddr, VirtAddr};

use super::transport::{Notifier, NotifyWidth, SelectedQueue, VirtioTransport};
use super::VirtioError;

// --- Register offsets (VirtIO 1.x §4.2.2; `include/uapi/linux/virtio_mmio.h`) ---

const MAGIC_VALUE: u64 = 0x000;
const VERSION: u64 = 0x004;
const DEVICE_ID: u64 = 0x008;
const DEVICE_FEATURES: u64 = 0x010;
const DEVICE_FEATURES_SEL: u64 = 0x014;
const DRIVER_FEATURES: u64 = 0x020;
const DRIVER_FEATURES_SEL: u64 = 0x024;
const QUEUE_SEL: u64 = 0x030;
const QUEUE_NUM_MAX: u64 = 0x034;
const QUEUE_NUM: u64 = 0x038;
const QUEUE_READY: u64 = 0x044;
const QUEUE_NOTIFY: u64 = 0x050;
const STATUS: u64 = 0x070;
const QUEUE_DESC_LOW: u64 = 0x080;
const QUEUE_DESC_HIGH: u64 = 0x084;
const QUEUE_AVAIL_LOW: u64 = 0x090;
const QUEUE_AVAIL_HIGH: u64 = 0x094;
const QUEUE_USED_LOW: u64 = 0x0a0;
const QUEUE_USED_HIGH: u64 = 0x0a4;
const CONFIG_GENERATION: u64 = 0x0fc;
const CONFIG: u64 = 0x100;

/// `"virt"` little-endian — every slot answers this, populated or not.
const MAGIC: u32 = 0x7472_6976;

/// The version this kernel speaks. QEMU defaults to **1** (legacy) unless the machine is
/// given `-global virtio-mmio.force-legacy=false`.
const VERSION_MODERN: u32 = 2;

/// A slot's whole register window on QEMU `virt` is 0x200 bytes, so device-config space is
/// the 256 bytes from `0x100`. Bounding reads against this is what stops an over-long
/// config read reaching into the *next slot's* registers.
const CONFIG_LEN: usize = 0x100;

/// 32-bit MMIO read — the only legal control-register access width.
///
/// # Safety
/// `base + off` must be a mapped Device-`nGnRnE` register.
#[inline]
unsafe fn r32(base: VirtAddr, off: u64) -> u32 {
    debug_assert_eq!(off % 4, 0, "unaligned Device-memory read at +{off:#x}");
    // SAFETY: caller guarantees a mapped register; `debug_assert` covers alignment.
    unsafe { core::ptr::read_volatile((base.as_u64() + off) as *const u32) }
}

/// 32-bit MMIO write — the only legal control-register access width.
///
/// # Safety
/// `base + off` must be a mapped Device-`nGnRnE` register.
#[inline]
unsafe fn w32(base: VirtAddr, off: u64, val: u32) {
    debug_assert_eq!(off % 4, 0, "unaligned Device-memory write at +{off:#x}");
    // SAFETY: caller guarantees a mapped register; `debug_assert` covers alignment.
    unsafe { core::ptr::write_volatile((base.as_u64() + off) as *mut u32, val) }
}

/// What a slot probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotProbe {
    /// The slot is populated with a modern device of this VirtIO device type.
    Device(u32),
    /// The slot exists but holds no device (`DeviceID == 0`).
    Empty,
}

/// A VirtIO device reached over virtio-mmio.
pub struct MmioTransport {
    /// Mapped base of this slot's 0x200-byte register window.
    base: VirtAddr,
}

impl MmioTransport {
    /// Probe an already-mapped slot without disturbing it.
    ///
    /// Reads only `MagicValue`, `Version` and `DeviceID` — no writes, so this is safe to
    /// call on every slot during discovery without resetting devices a driver may already
    /// be using.
    ///
    /// # Safety
    /// `base` must be a mapped Device-`nGnRnE` window of at least 0x200 bytes.
    pub unsafe fn probe(base: VirtAddr) -> Result<SlotProbe, VirtioError> {
        // SAFETY: caller guarantees the mapping.
        let (magic, device_id, version) = unsafe {
            (
                r32(base, MAGIC_VALUE),
                r32(base, DEVICE_ID),
                r32(base, VERSION),
            )
        };

        if magic != MAGIC {
            return Err(VirtioError::BadMagic);
        }
        // An empty slot answers magic and version but reads 0 for DeviceID, and ignores
        // every write. That makes DeviceID the clean emptiness test — and it must be
        // checked *before* the version check, because an empty slot on a legacy-configured
        // machine would otherwise be reported as a legacy device.
        if device_id == 0 {
            return Ok(SlotProbe::Empty);
        }
        if version != VERSION_MODERN {
            return Err(VirtioError::LegacyTransport);
        }
        Ok(SlotProbe::Device(device_id))
    }

    /// Wrap a slot that [`probe`](Self::probe) has already validated.
    ///
    /// # Safety
    /// `base` must be a mapped window for a slot that probed as [`SlotProbe::Device`].
    pub unsafe fn new(base: VirtAddr) -> Self {
        Self { base }
    }

    /// Select a queue. Every queue-addressed register acts on the selection.
    fn select(&self, index: u16) {
        // SAFETY: QueueSel is a 32-bit control register in a mapped window.
        unsafe { w32(self.base, QUEUE_SEL, index as u32) }
    }
}

impl VirtioTransport for MmioTransport {
    fn status(&self) -> u8 {
        // A 32-bit read, narrowed. A byte read would return 0 — see the module docs.
        // SAFETY: Status is a control register in this slot's mapped window.
        (unsafe { r32(self.base, STATUS) }) as u8
    }

    fn set_status(&self, value: u8) {
        // A 32-bit write. A byte write would be silently dropped.
        // SAFETY: as above.
        unsafe { w32(self.base, STATUS, value as u32) }
    }

    fn read_device_features(&self) -> u64 {
        // SAFETY: the feature select/value pair are control registers.
        unsafe {
            w32(self.base, DEVICE_FEATURES_SEL, 0);
            let low = r32(self.base, DEVICE_FEATURES) as u64;
            w32(self.base, DEVICE_FEATURES_SEL, 1);
            let high = r32(self.base, DEVICE_FEATURES) as u64;
            (high << 32) | low
        }
    }

    fn write_driver_features(&self, features: u64) {
        // SAFETY: as above.
        unsafe {
            w32(self.base, DRIVER_FEATURES_SEL, 0);
            w32(self.base, DRIVER_FEATURES, features as u32);
            w32(self.base, DRIVER_FEATURES_SEL, 1);
            w32(self.base, DRIVER_FEATURES, (features >> 32) as u32);
        }
    }

    fn select_queue(&self, index: u16) -> SelectedQueue {
        self.select(index);
        // SAFETY: QueueNumMax is a control register, read after selection.
        let max_size = (unsafe { r32(self.base, QUEUE_NUM_MAX) }) as u16;
        SelectedQueue { index, max_size }
    }

    fn queue_notifier(&self, _queue: &SelectedQueue) -> Notifier {
        // One register for every queue — the value written says which. 32 bits wide; a
        // narrower write is dropped, which would make every kick a silent no-op.
        Notifier::at(
            VirtAddr::new(self.base.as_u64() + QUEUE_NOTIFY),
            NotifyWidth::U32,
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
        debug_assert!(
            size <= queue.max_size,
            "queue {} programmed with size {} > device maximum {}",
            queue.index,
            size,
            queue.max_size
        );

        // Re-select rather than trusting the token's provenance: the token proves *a*
        // selection happened, and re-asserting it costs one 32-bit write.
        self.select(queue.index);

        // SAFETY: all control registers in this slot's mapped window.
        unsafe {
            w32(self.base, QUEUE_NUM, size as u32);

            // Ring addresses go out as low/high 32-bit halves. virtio-PCI writes each as
            // one 64-bit register; there is no 64-bit access here, and an 8-byte write
            // would be dropped like any other non-4-byte access.
            w32(self.base, QUEUE_DESC_LOW, desc.as_u64() as u32);
            w32(self.base, QUEUE_DESC_HIGH, (desc.as_u64() >> 32) as u32);
            w32(self.base, QUEUE_AVAIL_LOW, avail.as_u64() as u32);
            w32(self.base, QUEUE_AVAIL_HIGH, (avail.as_u64() >> 32) as u32);
            w32(self.base, QUEUE_USED_LOW, used.as_u64() as u32);
            w32(self.base, QUEUE_USED_HIGH, (used.as_u64() >> 32) as u32);

            // QEMU latches the size and all three ring addresses on this write, so every
            // value above must already be in place. This is the mmio spelling of
            // virtio-PCI's `QUEUE_ENABLE`.
            w32(self.base, QUEUE_READY, 1);
        }
    }

    fn config_len(&self) -> usize {
        CONFIG_LEN
    }

    fn config_read_raw(&self, offset: usize, out: &mut [u8]) {
        // Byte at a time. Config space permits 1/2/4-byte accesses — and an 8-byte access
        // aborts QEMU outright — so the narrow form is the one that is always correct.
        for (i, byte) in out.iter_mut().enumerate() {
            // SAFETY: `read_config` bounded `offset + out.len()` by `config_len`, which is
            // the size of the config region inside this slot's mapped window.
            *byte = unsafe {
                core::ptr::read_volatile(
                    (self.base.as_u64() + CONFIG + (offset + i) as u64) as *const u8,
                )
            };
        }
    }

    fn config_generation(&self) -> u32 {
        // SAFETY: ConfigGeneration is a 32-bit control register.
        unsafe { r32(self.base, CONFIG_GENERATION) }
    }

    // num_queues: deliberately not overridden. virtio-mmio has no such register; the
    // trait's default probes QueueSel/QueueNumMax upward, which is the only way to answer.
}
