//! # Arch-neutral VirtIO device discovery
//!
//! "Which VirtIO devices does this machine have?" is a question with the same *answer
//! shape* on every platform and a completely different *mechanism* on each. On x86_64
//! it is a PCI configuration-space walk driven through the `0xCF8`/`0xCFC` I/O ports;
//! on aarch64 `virt` it will be a scan of 32 fixed MMIO slots, each with a `DeviceID`
//! register. There is no port I/O on aarch64 at all, and virtio-mmio has no PCI class
//! code, so the *predicate* differs and not merely the enumeration.
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
//! It is **discovery only**. The register-level transport — feature negotiation, queue
//! configuration, the notify doorbell — is still PCI-shaped and still lives in
//! [`super::VirtioTransport`]; turning that into a trait with two implementations is
//! sub-phase 8.2's job. Keeping the two apart is deliberate: this change moves ~18 call
//! sites and touches working amd64 storage and networking, and rewriting the register
//! layer in the same commit would make "amd64 is unchanged" a claim about two things at
//! once instead of one.
//!
//! Accordingly [`VirtioDevice`] still *carries* a `PciDevice` today. What matters is
//! that no caller outside this module and the drivers can see it.
//!
//! ## Ordering is part of the contract
//!
//! [`devices`] returns devices in a stable, documented order, because callers select by
//! position ("the first block device") and because the amd64 acceptance test for this
//! sub-phase is that the discovered set is byte-identical to what the PCI walk produced
//! before. On x86_64 that order is PCI bus/slot/function ascending, which is what
//! `pci::devices_by_vendor` already yields.
//!
//! It is worth writing down now that **aarch64 will not be able to preserve this
//! ordering convention naively**: QEMU's `virt` maps `-device` arguments to virtio-mmio
//! slots with *decreasing* base addresses, so "first on the command line" is the
//! *highest* slot. Any code that assumes command-line order equals discovery order is
//! already wrong there. Selecting by [`VirtioKind`] rather than by index is the habit
//! that survives the move.

use alloc::vec::Vec;
use spin::Mutex;

use crate::drivers::pci::{self, PciDevice};

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
    pci: PciDevice,
}

impl VirtioDevice {
    /// What kind of device this is.
    pub fn kind(&self) -> VirtioKind {
        self.kind
    }

    /// The underlying PCI handle.
    ///
    /// Scoped to `crate::drivers::virtio` on purpose — **not** `pub(crate)`. The kernel
    /// is a binary crate, so `pub(crate)` is effectively fully public and the "no caller
    /// outside the drivers can see PCI" property would be a convention rather than a
    /// compiler guarantee. This restricts it to the three intended callers. It disappears
    /// in 8.2 when the transport becomes a trait, rather than gaining an aarch64 sibling.
    pub(in crate::drivers::virtio) fn pci(&self) -> &PciDevice {
        &self.pci
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
    let found: Vec<VirtioDevice> = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID)
        .into_iter()
        .map(|pci| VirtioDevice {
            kind: VirtioKind::from_pci_device_id(pci.device_id),
            pci,
        })
        .collect();
    *slot = Some(found);
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
