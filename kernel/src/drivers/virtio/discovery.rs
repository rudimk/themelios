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

use crate::drivers::pci::{self, PciDevice};

/// What a discovered VirtIO device *is*, independent of how it was found.
///
/// On PCI this is derived from the class code; on virtio-mmio it will come from the
/// `DeviceID` register, whose values are the VirtIO device-type numbers rather than PCI
/// classes. Callers care about neither — they ask for a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtioKind {
    /// Block device (virtio-blk). PCI class `0x01` (mass storage).
    Block,
    /// Network interface (virtio-net). PCI class `0x02` (network controller).
    Net,
    /// A VirtIO device this kernel has no driver for. Carries the raw
    /// platform-specific type byte purely for diagnostics.
    Other(u8),
}

impl VirtioKind {
    /// Human-readable name, for boot lines and test failures.
    pub fn name(self) -> &'static str {
        match self {
            VirtioKind::Block => "block",
            VirtioKind::Net => "net",
            VirtioKind::Other(_) => "unknown",
        }
    }

    /// Classify a PCI class code.
    ///
    /// x86-specific by nature: the aarch64 provider will classify a virtio-mmio
    /// `DeviceID` instead, and the two numbering schemes are unrelated.
    fn from_pci_class(class: u8) -> Self {
        match class {
            0x01 => VirtioKind::Block,
            0x02 => VirtioKind::Net,
            other => VirtioKind::Other(other),
        }
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
    /// Crate-private on purpose. Only the VirtIO drivers call this, and only until 8.2
    /// replaces it with a transport trait — at which point this method disappears
    /// rather than gaining an aarch64 sibling.
    pub(crate) fn pci(&self) -> &PciDevice {
        &self.pci
    }
}

/// Every VirtIO device on this machine, in a stable order.
///
/// See the module docs on why the order is part of the contract.
pub fn devices() -> Vec<VirtioDevice> {
    pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID)
        .into_iter()
        .map(|pci| VirtioDevice {
            kind: VirtioKind::from_pci_class(pci.class),
            pci,
        })
        .collect()
}

/// Every VirtIO device of one kind, in discovery order.
pub fn devices_of_kind(kind: VirtioKind) -> Vec<VirtioDevice> {
    devices().into_iter().filter(|d| d.kind == kind).collect()
}

/// The first VirtIO device of one kind, or `None`.
///
/// The overwhelmingly common case: there is exactly one disk and one NIC.
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
