//! # Arch-neutral VirtIO device discovery
//!
//! "Which VirtIO devices does this machine have?" is a question with the same *answer
//! shape* on every platform and a completely different *mechanism* on each. On x86_64
//! it is a PCI configuration-space walk driven through the `0xCF8`/`0xCFC` I/O ports;
//! on aarch64 `virt` it is a scan of the 32 fixed MMIO slots [`crate::platform`]
//! describes, each with a `DeviceID` register. There is no port I/O on aarch64 at all,
//! and virtio-mmio has no PCI class code, so the *predicate* differs and not merely the
//! enumeration.
//!
//! Before this module, every caller asked the question in x86 vocabulary:
//!
//! ```text
//! let devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
//! let dev  = devs.iter().find(|d| d.class == 0x01)?;   // 0x01 == mass storage
//! let blk  = VirtioBlk::init_from_pci(dev)?;
//! ```
//!
//! Eighteen call sites did that, sixteen of them in the test suite — and each one bakes
//! in three separate PCI assumptions (a vendor ID, a class code, and a `PciDevice`
//! handle) that have no aarch64 meaning. This module replaces them with:
//!
//! ```text
//! let blk = VirtioBlk::init(&virtio::first_of_kind(VirtioKind::Block)?)?;
//! ```
//!
//! ## What this module is and is not
//!
//! It is **discovery only** — which devices exist, not how to talk to them. The register
//! interface is [`crate::drivers::virtio::transport::VirtioTransport`], a trait with one
//! implementation per transport, and [`VirtioDevice::open_transport`] is where the two
//! meet: discovery is the only place that knows *how* a device was found, so it is the
//! only place that can say which transport speaks to it.
//!
//! The per-transport handle lives in the private [`Handle`] enum, so no caller outside
//! this module can see — or grow a dependency on — how a device was reached.
//!
//! ## Order is stable per platform, and the two platforms disagree
//!
//! [`devices`] returns devices in a stable, documented order. On x86_64 that is PCI
//! bus/slot/function ascending, which is what `pci::devices_by_vendor` yields; on aarch64
//! it is ascending virtio-mmio slot address.
//!
//! **Those two orders are not the same list.** QEMU's `virt` maps `-device` arguments to
//! virtio-mmio slots with *decreasing* base addresses, so "first on the command line" is
//! the *highest* slot, and an identical QEMU invocation enumerates in reverse between the
//! architectures — the harness's `block block block net` on amd64 comes back as
//! `net block block block` here. Any code that infers *which* device it has from a
//! position is therefore wrong on one of the two.
//!
//! So selection is by [`VirtioKind`], and anything destructive selects by *content*:
//! `open_scratch_disk` in the test suite finds its disk by a signature in sector 0
//! precisely because "the first block device" names different disks on the two
//! platforms.

use alloc::boxed::Box;
use alloc::vec::Vec;
use spin::Mutex;

#[cfg(target_arch = "x86_64")]
use crate::drivers::pci::{self, PciDevice};

#[cfg(target_arch = "aarch64")]
use crate::mm::addr::{PhysAddr, VirtAddr};

use super::transport::VirtioTransport;
#[cfg(target_arch = "x86_64")]
use super::PciTransport;
#[cfg(target_arch = "aarch64")]
use super::MmioTransport;
use super::VirtioError;

/// What a discovered VirtIO device *is*, independent of how it was found.
///
/// The discriminants are **VirtIO device types** (VirtIO spec §5), which is the one
/// numbering both transports actually carry:
///
/// - virtio-mmio reports it directly in the `DeviceID` register at offset `0x008`.
/// - Modern virtio-PCI encodes it as `device_id - 0x1040`.
/// - Transitional virtio-PCI uses the legacy `0x1000..=0x103F` block, which needs a
///   small table.
///
/// **Not the PCI class code**, which is what 8.1 first used and which is wrong twice
/// over. It is lossy: class `0x01` is *mass storage*, so virtio-scsi (type 8) and
/// virtio-blk (type 2) are indistinguishable, and `VirtioBlk::init` would happily drive
/// a SCSI device — with the `debug_assert_eq!` on `kind()` passing, because the
/// misclassification happened upstream of the assert. And it is not portable: a PCI
/// class and an mmio `DeviceID` are unrelated numbering schemes running in opposite
/// directions (class `0x01` is block and `0x02` is net; type 1 is *net* and 2 is
/// *block*), so an `Other(n)` keyed on class would mean different devices on the two
/// architectures — inside a type whose entire purpose is to be platform-neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtioKind {
    /// virtio-net, device type 1.
    Net,
    /// virtio-blk, device type 2.
    Block,
    /// A VirtIO device type this kernel has no driver for. Carries the spec §5 type
    /// number, which means the same thing on every transport.
    Other(u32),
}

/// Base of the modern virtio-PCI device-ID range: `0x1040 + device_type`.
const VIRTIO_PCI_MODERN_BASE: u16 = 0x1040;
/// First and last device IDs of the transitional (legacy) virtio-PCI range.
const VIRTIO_PCI_LEGACY_FIRST: u16 = 0x1000;
const VIRTIO_PCI_LEGACY_LAST: u16 = 0x103F;

impl VirtioKind {
    /// Human-readable name, for boot lines and test failures.
    pub fn name(self) -> &'static str {
        match self {
            VirtioKind::Net => "net",
            VirtioKind::Block => "block",
            // Named for the common types so a boot line stays readable when we meet a
            // device we do not drive.
            VirtioKind::Other(3) => "console",
            VirtioKind::Other(4) => "rng",
            VirtioKind::Other(5) => "balloon",
            VirtioKind::Other(8) => "scsi",
            VirtioKind::Other(9) => "9p",
            VirtioKind::Other(_) => "unknown",
        }
    }

    /// Classify a VirtIO device type (spec §5) — the arch-neutral form.
    pub fn from_device_type(ty: u32) -> Self {
        match ty {
            1 => VirtioKind::Net,
            2 => VirtioKind::Block,
            other => VirtioKind::Other(other),
        }
    }

    /// Classify a virtio-PCI function from its `device_id`.
    ///
    /// Handles both the modern (`0x1040 +`) and transitional (`0x1000..=0x103F`)
    /// encodings. The harness launches every device with `disable-legacy=on`, so in
    /// practice only the modern path runs — the legacy table is there so a device that
    /// slips through is classified rather than silently mislabelled.
    fn from_pci_device_id(device_id: u16) -> Self {
        if device_id >= VIRTIO_PCI_MODERN_BASE {
            return Self::from_device_type((device_id - VIRTIO_PCI_MODERN_BASE) as u32);
        }
        if (VIRTIO_PCI_LEGACY_FIRST..=VIRTIO_PCI_LEGACY_LAST).contains(&device_id) {
            // Transitional IDs are not `base + type`; they are their own table.
            return match device_id {
                0x1000 => VirtioKind::Net,
                0x1001 => VirtioKind::Block,
                0x1002 => VirtioKind::Other(5), // balloon
                0x1003 => VirtioKind::Other(3), // console
                0x1004 => VirtioKind::Other(8), // scsi
                0x1005 => VirtioKind::Other(4), // rng
                0x1009 => VirtioKind::Other(9), // 9p
                other => VirtioKind::Other(other as u32),
            };
        }
        VirtioKind::Other(device_id as u32)
    }
}

/// A VirtIO device the platform has presented to us.
///
/// Deliberately opaque: the transport handle inside is not part of the public surface,
/// so a caller cannot accidentally grow a dependency on PCI. Drivers reach it through
/// the crate-private [`VirtioDevice::pci`].
#[derive(Debug, Clone)]
pub struct VirtioDevice {
    kind: VirtioKind,
    handle: Handle,
}

/// How to reach a discovered device — the one genuinely per-transport piece of a
/// [`VirtioDevice`].
///
/// A PCI function is identified by bus/slot/function and reached by walking capabilities;
/// a virtio-mmio device is a *mapped register window* and nothing else. The mmio variant
/// carries the mapped address rather than the physical one because discovery had to map
/// the window to read `DeviceID` in the first place — re-mapping it at
/// [`VirtioDevice::open_transport`] time would burn a fresh page of virtual address space
/// on every call, through a bump allocator with no unmap.
#[derive(Debug, Clone)]
enum Handle {
    #[cfg(target_arch = "x86_64")]
    Pci(PciDevice),
    #[cfg(target_arch = "aarch64")]
    Mmio { base: VirtAddr },
}

impl VirtioDevice {
    /// What kind of device this is.
    pub fn kind(&self) -> VirtioKind {
        self.kind
    }
}

/// The discovered device list, populated once by [`init`].
///
/// **Discovery is cached, and that is a portability requirement rather than an
/// optimisation.** On x86 `pci::devices_by_vendor` is a filtered clone of a `Vec` that
/// `pci::scan()` already built, so re-enumerating is nearly free and callers may treat
/// it as such — `devices_of_kind` and `first_of_kind` both call `devices()`, and some
/// tests call it several times.
///
/// virtio-mmio cannot honour that contract. Answering "what devices are there" means
/// mapping and reading 32 MMIO windows, and `mm::mmio::map` is a bump allocator with no
/// de-duplication and **no unmap** — every call burns fresh virtual address space and
/// page-table frames. An uncached `devices()` would leak 32 mappings per call across
/// every call site in the suite, and the symptom would be an exhausted allocator in a
/// much later sub-phase, nowhere near the cause.
///
/// Caching here means each transport is enumerated exactly once, on either
/// architecture, and gives an aarch64 implementation somewhere to keep the mapped
/// address rather than re-deriving it.
static DEVICES: Mutex<Option<Vec<VirtioDevice>>> = Mutex::new(None);

/// Enumerate the platform's VirtIO devices, once.
///
/// Called from `kmain` after the bus/transport layer is ready — on x86 that means after
/// `pci::scan()`, since this filters the table that populates. Idempotent: a second call
/// is a no-op, so a test that runs before boot finished cannot double-enumerate.
pub fn init() {
    let mut slot = DEVICES.lock();
    if slot.is_some() {
        return;
    }
    *slot = Some(enumerate());
}

/// Enumerate this platform's VirtIO devices.
#[cfg(target_arch = "x86_64")]
fn enumerate() -> Vec<VirtioDevice> {
    pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID)
        .into_iter()
        .map(|pci| VirtioDevice {
            kind: VirtioKind::from_pci_device_id(pci.device_id),
            handle: Handle::Pci(pci),
        })
        .collect()
}

/// Enumerate this platform's VirtIO devices by scanning the virtio-mmio slot bank.
///
/// **Every slot is scanned, and each is classified by its `DeviceID` register — never by
/// its position.** QEMU maps `-device` arguments to slots with *decreasing* base
/// addresses, so command-line order and scan order run opposite ways; a kernel that
/// inferred "the first disk" from a slot index would pick the wrong one, and on the amd64
/// harness's layout that is the difference between the scratch disk and the ext2 volume.
///
/// A populated slot is mapped once, here, and the mapping travels in the [`Handle`] — so
/// a device is mapped exactly once no matter how many times it is opened.
///
/// A legacy slot is reported and skipped rather than silently omitted: the difference
/// between "no disk attached" and "the disk is there but QEMU is presenting it the old
/// way" is one command-line flag, and a driver that only ever says "no VirtIO block
/// device" gives no hint which.
#[cfg(target_arch = "aarch64")]
fn enumerate() -> Vec<VirtioDevice> {
    use super::mmio_transport::SlotProbe;

    let mut found = Vec::new();
    let mut legacy = 0usize;
    let mut empty = 0usize;

    for dev in crate::platform::info().virtio_mmio {
        // One page covers a 0x200-byte slot window.
        let base = crate::mm::mmio::map(PhysAddr::new(dev.base), SLOT_WINDOW_BYTES);

        // SAFETY: `base` is the window just mapped, of the required size.
        match unsafe { MmioTransport::probe(base) } {
            Ok(SlotProbe::Device(device_type)) => {
                let kind = VirtioKind::from_device_type(device_type);
                crate::println!(
                    "[virtio]   slot @ {:#x}: type {} ({})",
                    dev.base,
                    device_type,
                    kind.name()
                );
                found.push(VirtioDevice {
                    kind,
                    handle: Handle::Mmio { base },
                });
            }
            Ok(SlotProbe::Empty) => empty += 1,
            Err(VirtioError::LegacyTransport) => legacy += 1,
            Err(_) => {}
        }
    }

    if legacy > 0 {
        crate::println!(
            "[virtio] {legacy} slot(s) present a LEGACY (v1) device and were skipped — \
             start QEMU with `-global virtio-mmio.force-legacy=false`"
        );
    }
    crate::println!(
        "[virtio] scanned {} mmio slots: {} device(s), {} empty, {} legacy",
        crate::platform::info().virtio_mmio.len(),
        found.len(),
        empty,
        legacy
    );
    found
}

/// Bytes of register window per virtio-mmio slot on QEMU `virt`.
#[cfg(target_arch = "aarch64")]
const SLOT_WINDOW_BYTES: usize = 0x200;

impl VirtioDevice {
    /// Open this device's transport, reset it, and leave it ready for the feature
    /// handshake.
    ///
    /// **This is the seam where the two transports meet.** Discovery is the only place
    /// that knows *how* a device was found, so it is the only place that can say which
    /// transport speaks to it: a PCI function gets a [`PciTransport`], an mmio slot gets
    /// an [`MmioTransport`]. Drivers receive a `Box<dyn VirtioTransport>` and never learn
    /// which, which is what lets `blk.rs` and `net.rs` contain no reference to PCI at all.
    ///
    /// Because it is the single common path, it is also where
    /// [`VirtioTransport::begin_init`] runs — the reset-and-acknowledge handshake the
    /// spec defines identically for both. Constructing a transport therefore cannot
    /// leave a device un-reset, which is a mistake the virtio-PCI path made for four
    /// phases while a comment claimed otherwise.
    pub fn open_transport(&self) -> Result<Box<dyn VirtioTransport>, VirtioError> {
        let transport: Box<dyn VirtioTransport> = match &self.handle {
            #[cfg(target_arch = "x86_64")]
            Handle::Pci(pci) => Box::new(PciTransport::init(pci)?),
            #[cfg(target_arch = "aarch64")]
            // SAFETY: the window was mapped and probed during discovery, and probing
            // established that this slot holds a modern device.
            Handle::Mmio { base } => Box::new(unsafe { MmioTransport::new(*base) }),
        };
        transport.begin_init()?;
        Ok(transport)
    }
}

/// Every VirtIO device on this machine, in a stable order.
///
/// See the module docs on why the order is documented, and [`DEVICES`] on why the
/// enumeration is cached. Enumerates on first use if [`init`] has not run, so a caller
/// that arrives early still gets an answer rather than an empty list.
pub fn devices() -> Vec<VirtioDevice> {
    {
        let slot = DEVICES.lock();
        if let Some(devs) = slot.as_ref() {
            return devs.clone();
        }
    }
    init();
    DEVICES.lock().as_ref().cloned().unwrap_or_default()
}

/// Every VirtIO device of one kind, in discovery order.
pub fn devices_of_kind(kind: VirtioKind) -> Vec<VirtioDevice> {
    devices().into_iter().filter(|d| d.kind == kind).collect()
}

/// The first VirtIO device of one kind, or `None`.
///
/// Convenient, and safe for anything read-only — but **never** use it to pick a target
/// for a destructive operation. "First" is a property of this platform's enumeration
/// order, not of the device, and the two disagree between architectures. A test that
/// writes must identify its disk by content; see `open_scratch_disk` in the test suite.
pub fn first_of_kind(kind: VirtioKind) -> Option<VirtioDevice> {
    devices().into_iter().find(|d| d.kind == kind)
}

/// A one-line summary of what discovery found, for the boot log.
///
/// This is the human-readable half of the sub-phase's acceptance criterion: the same
/// devices, in the same order, before and after the seam went in. It is printed rather
/// than merely asserted so that a change in the *set* is visible in CI logs even when
/// no test happens to cover it.
pub fn describe() -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    let devs = devices();
    let _ = write!(s, "{} device(s):", devs.len());
    for d in &devs {
        let _ = write!(s, " {}", d.kind.name());
    }
    if devs.is_empty() {
        let _ = write!(s, " (none)");
    }
    s
}
