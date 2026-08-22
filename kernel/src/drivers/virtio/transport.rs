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
//! - [`Virtqueue`] held its own doorbell address, so the queue — a data structure
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
//! ## This surface was validated against the second implementation, not derived from the first
//!
//! The trait's first draft was written by reading the virtio-PCI code, and review found
//! four places where that produced a PCI shape under a neutral name: a 16-bit doorbell
//! write (virtio-mmio's `QueueNotify` is 32-bit, and QEMU *silently drops* narrower
//! accesses), a `num_queues` method with no mmio register behind it at all, a
//! `device_config()` returning a raw pointer with neither a length nor a generation
//! counter, and byte-wide status accesses that mmio would discard.
//!
//! The surface below was then checked by drafting a throwaway `MmioTransport` against it
//! and compiling it — the technique 8.spike used, which 8.2 should have repeated from the
//! start. Every method maps to a real register: `status` to `Status` (0x070) as a 32-bit
//! access, `queue_max_size` to `QueueSel`/`QueueNumMax`, `configure_queue` to the split
//! low/high ring pairs plus `QueueNum`/`QueueReady`, `queue_notifier` to the shared
//! `QueueNotify` at 32 bits, and `num_queues` to no register at all — which is why it is a
//! provided method that probes rather than a required one. The draft needed no change to
//! this file, which is the evidence the shape is right.
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
    Virtqueue, VirtioError, MAX_QUEUE_SIZE, STATUS_ACKNOWLEDGE, STATUS_DRIVER, STATUS_DRIVER_OK,
    STATUS_FAILED, STATUS_FEATURES_OK, VIRTIO_F_VERSION_1,
};

/// Access width of a doorbell register.
///
/// **Not cosmetic.** QEMU's virtio-mmio model rejects any access to a control register
/// (offset < `0x100`) whose size is not 4 bytes: `virtio_mmio.c` logs
/// `"wrong size access to register!"` and then *drops the write* — no fault, no
/// diagnostic the guest can see. A 16-bit write to the 32-bit `QueueNotify` (0x050)
/// would therefore turn every kick into a silent no-op, the device would never see any
/// work, and `submit_and_wait` would spin to its bound and permanently fail the queue.
///
/// virtio-PCI's per-queue doorbell genuinely is 16 bits, so the width is a property of
/// the transport and has to travel with the address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // U32 is constructed only by a virtio-mmio transport (8.3)
pub enum NotifyWidth {
    /// 16-bit — virtio-PCI's per-queue notify doorbell.
    U16,
    /// 32-bit — virtio-mmio's shared `QueueNotify` register.
    U32,
}

/// A resolved doorbell: where to write, how wide, to tell the device a queue has work.
///
/// Produced by [`VirtioTransport::queue_notifier`] at queue-setup time and held by the
/// [`Virtqueue`]. The queue rings it without knowing whether the address is one of many
/// (virtio-PCI) or shared with every other queue (virtio-mmio).
#[derive(Debug, Clone, Copy)]
pub struct Notifier {
    addr: VirtAddr,
    width: NotifyWidth,
}

impl Notifier {
    /// Build a notifier for an already-resolved doorbell address.
    ///
    /// Only transport implementations call this — resolving the address *and knowing the
    /// register's width* are both precisely the parts that differ between them.
    pub fn at(addr: VirtAddr, width: NotifyWidth) -> Self {
        Self { addr, width }
    }

    /// Ring the doorbell for `queue_index`.
    ///
    /// The value written is the queue index on both transports: virtio-PCI expects the
    /// *vqn* at the queue's own doorbell, virtio-mmio at the shared `QueueNotify`. Only
    /// the width differs, and getting it wrong is silent — see [`NotifyWidth`].
    ///
    /// The caller is responsible for the release barrier beforehand: the device must not
    /// observe this write before the ring updates it announces.
    pub fn ring(self, queue_index: u16) {
        // SAFETY: `addr` is a doorbell register mapped by the transport that produced
        // this notifier, written at the width that transport documents.
        unsafe {
            match self.width {
                NotifyWidth::U16 => {
                    core::ptr::write_volatile(self.addr.as_u64() as *mut u16, queue_index)
                }
                NotifyWidth::U32 => {
                    core::ptr::write_volatile(self.addr.as_u64() as *mut u32, queue_index as u32)
                }
            }
        }
    }
}

/// Proof that a particular queue is currently selected, plus what selecting it revealed.
///
/// Both transports address queue registers through a *selected queue*: virtio-PCI has
/// `COMMON_QUEUE_SELECT`, virtio-mmio has `QueueSel` (0x030). So the statefulness is
/// portable — but "call `queue_max_size` first, then these two, and do not interleave"
/// was an ordering contract stated only in prose, with the affected methods taking an
/// `index` they ignored.
///
/// That is a trap for a second implementation rather than a defect in the first: nothing
/// in-tree gets the order wrong today, but a virtio-mmio implementation would plausibly
/// *use* its `index` argument, giving the two impls different sensitivity to call order —
/// the classic shape of "works on amd64, corrupts on arm64". Making selection return a
/// token the dependent methods consume turns the contract into something the compiler
/// enforces, and costs no MMIO access at all.
#[derive(Debug, Clone, Copy)]
pub struct SelectedQueue {
    /// The queue this token selected.
    pub index: u16,
    /// The largest size the device supports for it; `0` means the queue does not exist.
    pub max_size: u16,
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

    /// The device-specific configuration region's size in bytes; `0` when absent.
    fn config_len(&self) -> usize;

    /// Copy `out.len()` bytes out of device-config space starting at `offset`.
    ///
    /// Implementations must use only the access widths their transport permits.
    /// virtio-mmio allows 1, 2 and 4 bytes in config space — and **`abort()`s QEMU** on an
    /// 8-byte access, which is why no `u64` helper reaches this directly.
    fn config_read_raw(&self, offset: usize, out: &mut [u8]);

    /// The device-config generation counter, or `0` on a transport without one.
    ///
    /// virtio-mmio exposes it at `0x0fc` and virtio-PCI in its common config. It is how a
    /// driver detects that a multi-access config read was torn by a concurrent device-side
    /// update. Defaulting to `0` makes [`read_config`](Self::read_config)'s generation
    /// check trivially succeed on a transport that has none, rather than forcing every
    /// implementation to invent one.
    fn config_generation(&self) -> u32 {
        0
    }

    /// Select queue `index`, and report what that revealed.
    ///
    /// The returned [`SelectedQueue`] is the only way to reach
    /// [`configure_queue`](Self::configure_queue) and
    /// [`queue_notifier`](Self::queue_notifier), so the ordering they depend on cannot be
    /// got wrong.
    fn select_queue(&self, index: u16) -> SelectedQueue;

    /// Program the selected queue's size and ring addresses, then enable it.
    fn configure_queue(
        &self,
        queue: &SelectedQueue,
        size: u16,
        desc: PhysAddr,
        avail: PhysAddr,
        used: PhysAddr,
    );

    /// Resolve the doorbell for the selected queue.
    ///
    /// virtio-PCI reads a per-queue `queue_notify_off` register whose value depends on
    /// which queue is selected, which is why this takes the token rather than an index.
    fn queue_notifier(&self, queue: &SelectedQueue) -> Notifier;


    // --- Provided: spec sequencing, identical on every transport -------------------

    /// How many virtqueues this device has.
    ///
    /// **Probing is the default because virtio-mmio has no such register.** virtio-PCI
    /// answers in one read (`COMMON_NUM_QUEUES`) and overrides this; the mmio register
    /// map has `QueueNumMax` *per selected queue* and nothing that reports a count, so the
    /// only portable answer is to select upward until a queue reports size 0.
    ///
    /// Because the probe selects queues as a side effect, callers must not assume a queue
    /// stays selected across it — which is why [`queue_notifier`](Self::queue_notifier)
    /// and [`configure_queue`](Self::configure_queue) take a fresh token.
    #[allow(dead_code)] // live only under `--features test`
    fn num_queues(&self) -> u16 {
        const MAX_PROBED_QUEUES: u16 = 64;
        let mut n = 0;
        while n < MAX_PROBED_QUEUES && self.select_queue(n).max_size != 0 {
            n += 1;
        }
        n
    }

    /// Read device-config bytes under a generation check, so a multi-access read cannot
    /// tear against a concurrent device-side update.
    ///
    /// The device may change its config while we read it — capacity on a resizable disk,
    /// link state on a NIC. Both transports expose a generation counter for exactly this;
    /// neither this kernel nor its predecessor had ever consulted one, because
    /// `device_config()` handed drivers a raw pointer and there was nowhere to put the
    /// check. Bounds are enforced here too: on virtio-mmio the config region is the tail
    /// of a `0x200`-byte slot window, so an over-long read walks into the *next device's
    /// registers*.
    fn read_config(&self, offset: usize, out: &mut [u8]) -> Result<(), VirtioError> {
        const CONFIG_READ_RETRIES: usize = 8;

        let end = offset.checked_add(out.len()).ok_or(VirtioError::ConfigOutOfRange)?;
        if end > self.config_len() {
            return Err(VirtioError::ConfigOutOfRange);
        }
        for _ in 0..CONFIG_READ_RETRIES {
            let before = self.config_generation();
            self.config_read_raw(offset, out);
            if self.config_generation() == before {
                return Ok(());
            }
        }
        Err(VirtioError::ConfigUnstable)
    }

    /// Read a little-endian `u64` from device config.
    ///
    /// Assembled from bytes rather than issued as an 8-byte access: virtio-mmio permits
    /// only 1-, 2- and 4-byte accesses to config space and **`abort()`s QEMU** on anything
    /// wider. Both halves come from one generation-checked read, so the two words cannot
    /// be torn relative to each other.
    fn config_u64(&self, offset: usize) -> Result<u64, VirtioError> {
        let mut buf = [0u8; 8];
        self.read_config(offset, &mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    /// Set additional status bits without disturbing the ones already set.
    fn add_status(&self, bits: u8) {
        let current = self.status();
        self.set_status(current | bits);
    }

    /// Reset the device and open the handshake, leaving it ready for
    /// [`negotiate_features`](Self::negotiate_features).
    ///
    /// **The reset is asynchronous, and this kernel used to assume it was not.** The spec
    /// (§4.1.4.3.2 for PCI, §4.2.3.1 for mmio) says writing `0` to `Status` *requests* a
    /// reset and the driver must then wait for the register to read back `0`. The
    /// virtio-PCI path wrote `0` and immediately wrote `ACKNOWLEDGE`, under a comment that
    /// claimed it waited — so for four phases the handshake raced a device that was still
    /// tearing down its previous queue configuration.
    ///
    /// On amd64 that race was invisible, because nothing ever re-initialised a device that
    /// had already been driven. 8.3's aarch64 suite does: several tests open the same
    /// block device in turn, and the second driver's `ACKNOWLEDGE` landed while the device
    /// was still resetting, so its queue programming was discarded and every request timed
    /// out on a used ring that would never advance.
    ///
    /// The spin bound is generous and the failure is reported rather than ignored: a
    /// device that will not reset cannot be driven, and pressing on would produce the
    /// silent timeout this exists to prevent.
    fn begin_init(&self) -> Result<(), VirtioError> {
        /// Reads of `Status` to allow before declaring the device stuck. Emulated devices
        /// reset within one; the bound only has to be large enough that a slow host
        /// cannot reach it, since reaching it fails the device permanently.
        const RESET_SPINS: usize = 100_000;

        self.set_status(0);
        for _ in 0..RESET_SPINS {
            if self.status() == 0 {
                // Acknowledge the device and announce we have a driver.
                self.set_status(STATUS_ACKNOWLEDGE);
                self.add_status(STATUS_DRIVER);
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(VirtioError::ResetTimeout)
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
        let selected = self.select_queue(index);
        if selected.max_size == 0 {
            return Err(VirtioError::QueueUnavailable);
        }
        let size = selected.max_size.min(MAX_QUEUE_SIZE);

        let notifier = self.queue_notifier(&selected);
        let queue = Virtqueue::new(size, index, notifier)?;

        self.configure_queue(
            &selected,
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
