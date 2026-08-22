//! # The VirtIO transport trait
//!
//! A VirtIO device is reached through a *transport*: the register interface that carries
//! the device status handshake, feature negotiation, queue configuration, and the
//! doorbell. Everything above it — the split virtqueue, the block and network drivers —
//! is transport-agnostic by design, and the VirtIO spec says so explicitly.
//!
//! ThemeliOS spoke exactly one transport until now: **virtio-PCI (modern, 1.0+)**, whose
//! registers are found by walking PCI vendor capabilities and whose doorbell is
//! *per queue*, at `notify_base + queue_notify_off * notify_off_multiplier`. That shape
//! had leaked into two places it does not belong:
//!
//! - `VirtioTransport` was a concrete struct holding five PCI-derived `VirtAddr`s.
//! - [`super::Virtqueue`] held its own doorbell address, so the queue — a data structure
//!   the spec defines without reference to any transport — knew how PCI computes one.
//!
//! virtio-mmio, which is how QEMU's aarch64 `virt` machine presents VirtIO, has neither
//! property: its registers are at fixed offsets from a slot base (no capability walk),
//! and it has **one shared `QueueNotify` register** rather than a doorbell per queue.
//!
//! ## What is in the trait, and what is not
//!
//! The trait carries **primitives** — the operations whose *implementation* genuinely
//! differs per transport. The handshake built on top of them ([`VirtioTransport::negotiate_features`],
//! [`VirtioTransport::setup_queue`], [`VirtioTransport::set_driver_ok`]) is spec-defined
//! sequencing that is identical everywhere, so it lives here once as provided methods
//! rather than being duplicated into each implementation.
//!
//! **Interrupt acknowledgement is deliberately absent.** The VirtIO stack polls: queue
//! setup writes `0xFFFF` (NO_VECTOR) to the MSI-X vector register, and the net service
//! polls continuously. The PCI transport's ISR byte has never been read by anything. An
//! `ack_interrupt` on this trait would be an interface with no callers on either side,
//! and inventing one would mean 8.3 implementing it for mmio to satisfy a contract
//! nothing exercises.
//!
//! ## Notification
//!
//! [`Notifier`] is the one piece of per-queue state the transport hands to a queue. Both
//! transports ultimately *do* the same thing — write the queue index to an MMIO register
//! — and differ only in **which address** that is: PCI computes one per queue, mmio uses
//! the same one for all of them. So `Notifier` is a resolved address plus the act of
//! ringing it, and the computation stays behind the transport where it belongs. It is
//! deliberately not a trait: there is no third behaviour to abstract over, and inventing
//! one would be generality with no second implementation to justify it.

use crate::mm::addr::{PhysAddr, VirtAddr};

use super::{
    Virtqueue, VirtioError, MAX_QUEUE_SIZE, STATUS_DRIVER_OK, STATUS_FAILED, STATUS_FEATURES_OK,
    VIRTIO_F_VERSION_1,
};

/// A resolved doorbell: where to write, to tell the device a queue has work.
///
/// Produced by [`VirtioTransport::queue_notifier`] at queue-setup time and held by the
/// [`Virtqueue`]. The queue rings it without knowing whether the address is one of many
/// (virtio-PCI) or shared with every other queue (virtio-mmio).
#[derive(Debug, Clone, Copy)]
pub struct Notifier {
    addr: VirtAddr,
}

impl Notifier {
    /// Build a notifier for an already-resolved doorbell address.
    ///
    /// Only transport implementations call this — resolving the address is precisely the
    /// part that differs between them.
    pub fn at(addr: VirtAddr) -> Self {
        Self { addr }
    }

    /// Ring the doorbell for `queue_index`.
    ///
    /// The value written is the queue index in both transports: virtio-PCI expects the
    /// *vqn* at the queue's own doorbell, and virtio-mmio expects it at the shared
    /// `QueueNotify`. The caller is responsible for the release barrier beforehand — the
    /// device must not observe this write before the ring updates it announces.
    pub fn ring(self, queue_index: u16) {
        // SAFETY: `addr` is a doorbell register mapped by the transport that produced
        // this notifier, and the device's contract is a 16-bit write of the queue index.
        unsafe {
            core::ptr::write_volatile(self.addr.as_u64() as *mut u16, queue_index);
        }
    }
}

/// The register interface of a VirtIO device.
///
/// Implementations are per *transport*, not per device type: one for virtio-PCI, and one
/// for virtio-mmio in 8.3. See the module docs for what belongs here and what does not.
///
/// `Send` is required because drivers own a transport inside an `InterruptMutex`, which
/// is only `Sync` when its contents can move between contexts.
pub trait VirtioTransport: Send {
    // --- Primitives: these differ per transport -----------------------------------

    /// Read the device status byte.
    fn status(&self) -> u8;

    /// Write the device status byte. Writing `0` resets the device.
    fn set_status(&self, value: u8);

    /// Read the 64-bit device feature bits.
    fn read_device_features(&self) -> u64;

    /// Write the 64-bit driver feature bits (the subset we accept).
    fn write_driver_features(&self, features: u64);

    /// How many virtqueues this device has.
    fn num_queues(&self) -> u16;

    /// Select queue `index` and report the largest size the device supports for it.
    ///
    /// Returns `0` when the queue does not exist. Selection is a side effect the
    /// subsequent [`configure_queue`](Self::configure_queue) and
    /// [`queue_notifier`](Self::queue_notifier) rely on, which is why they are documented
    /// as a sequence rather than independent calls.
    fn queue_max_size(&self, index: u16) -> u16;

    /// Program the currently selected queue's size and ring addresses, then enable it.
    ///
    /// Must follow a [`queue_max_size`](Self::queue_max_size) call for the same `index`.
    fn configure_queue(
        &self,
        index: u16,
        size: u16,
        desc: PhysAddr,
        avail: PhysAddr,
        used: PhysAddr,
    );

    /// Resolve the doorbell for the currently selected queue.
    ///
    /// Must follow a [`queue_max_size`](Self::queue_max_size) call for the same `index`:
    /// virtio-PCI reads a per-queue `queue_notify_off` register whose value depends on
    /// which queue is selected.
    fn queue_notifier(&self, index: u16) -> Notifier;

    /// The device-specific configuration region, if this device has one.
    ///
    /// Block devices report capacity here; network devices report MAC and MTU.
    fn device_config(&self) -> Option<VirtAddr>;

    // --- Provided: spec sequencing, identical on every transport -------------------

    /// Set additional status bits without disturbing the ones already set.
    fn add_status(&self, bits: u8) {
        let current = self.status();
        self.set_status(current | bits);
    }

    /// Negotiate features: accept the intersection of what the device offers and what we
    /// want, then confirm the device accepts our selection.
    ///
    /// `VIRTIO_F_VERSION_1` is always requested and always included in the accepted set —
    /// a modern device that does not offer it is not one we can speak to, and the device
    /// is marked FAILED before the error returns so it is left in a defined state.
    fn negotiate_features(&self, wanted: u64) -> Result<u64, VirtioError> {
        let device_features = self.read_device_features();

        // We require VERSION_1; intersect the rest of what we want with what the
        // device offers.
        let negotiated = (device_features & (wanted | VIRTIO_F_VERSION_1)) | VIRTIO_F_VERSION_1;

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

    /// Allocate, program and enable queue `index`, returning it ready for use.
    ///
    /// The ordering here is the spec's and is load-bearing: select the queue and learn
    /// its maximum size, allocate rings sized to fit, then hand the device their physical
    /// addresses and enable it. Resolving the notifier happens while the queue is still
    /// selected, for the reason given on [`queue_notifier`](Self::queue_notifier).
    fn setup_queue(&self, index: u16) -> Result<Virtqueue, VirtioError> {
        let max_size = self.queue_max_size(index);
        if max_size == 0 {
            return Err(VirtioError::QueueUnavailable);
        }
        let size = max_size.min(MAX_QUEUE_SIZE);

        let notifier = self.queue_notifier(index);
        let queue = Virtqueue::new(size, index, notifier)?;

        self.configure_queue(
            index,
            size,
            queue.desc_phys,
            queue.avail_phys,
            queue.used_phys,
        );
        Ok(queue)
    }

    /// Tell the device the driver is fully set up and it may begin operating.
    fn set_driver_ok(&self) {
        self.add_status(STATUS_DRIVER_OK);
    }
}
