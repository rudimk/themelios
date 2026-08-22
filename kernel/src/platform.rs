//! # Platform description
//!
//! Where this machine's fixed devices live, and how they interrupt.
//!
//! ## Why this exists now, when nothing discovers anything yet
//!
//! Every MMIO address in the aarch64 port is currently a `const` next to the driver that
//! uses it — `PL011_PHYS` in `boot.rs`, `GICD_PHYS`/`GICC_PHYS` in `gic.rs` — and each
//! driver's init function takes no arguments because it already knows where its device
//! is. That is fine on QEMU `virt`, whose memory map is fixed and documented.
//!
//! The debt is not the constants. **It is the shape.** Real firmware answers
//! "where is the UART?" with a `(compatible, base, size, irq)` tuple pulled from ACPI or
//! a device tree, and answers "what devices are there?" with a *list*. A driver whose
//! init takes no arguments cannot consume either. So the phase that finally parses ACPI
//! would not merely add a parser — it would rewrite every driver signature in the port,
//! after a suite of tests had frozen the current ones.
//!
//! This module pays that cost now, while it is one struct and one call site per driver:
//! the *description* is a value passed to drivers from the start, and there is exactly
//! one hard-coded provider per architecture. A future discovery phase then adds
//! providers — an ACPI one, a device-tree one — and changes nothing else.
//!
//! ## Deviation from the plan's sketch
//!
//! Pinned decision 11 sketched `virtio_slots: &[PlatformDevice]`. A slice of 32
//! identically-shaped entries is a worse description than the thing it describes: QEMU
//! `virt` exposes a *window* — a base, a stride, a count, and a contiguous IRQ range —
//! and the aarch64 provider would have to spell out 32 entries that differ only by
//! arithmetic. [`VirtioMmioWindow`] says the same thing in the form firmware actually
//! reports it, so 8.3 can scan it directly.

/// A fixed device: where it is, how big its register window is, and how it interrupts.
///
/// `base`/`size` are physical. A device that is not memory-mapped (an x86 port-I/O UART)
/// reports `base` as its port number and `size` as the port count — the field names stay
/// honest because "where is it" is the question either way, and no arch-neutral code
/// dereferences these.
#[derive(Debug, Clone, Copy)]
pub struct PlatformDevice {
    /// Physical base address, or I/O port number on a port-I/O platform.
    pub base: u64,
    /// Size of the register window in bytes, or port count.
    pub size: u64,
    /// Platform interrupt number, or [`IRQ_NONE`].
    pub irq: u32,
}

/// "This device does not interrupt, or is polled."
pub const IRQ_NONE: u32 = u32::MAX;

/// A contiguous bank of virtio-mmio transports.
///
/// QEMU `virt` places 32 of them at a fixed stride with consecutive interrupts. Real
/// SBSA-class hardware has none of these at all and puts VirtIO (when it has any) on
/// PCIe, which is why this is an `Option` on [`PlatformInfo`] rather than a required
/// field.
#[derive(Debug, Clone, Copy)]
pub struct VirtioMmioWindow {
    /// Physical base of slot 0.
    pub base: u64,
    /// Bytes between consecutive slots.
    pub stride: u64,
    /// Number of slots in the bank.
    pub count: usize,
    /// Interrupt of slot 0; slot *n* is `first_irq + n`.
    pub first_irq: u32,
}

impl VirtioMmioWindow {
    /// Physical base of slot `n`, or `None` if out of range.
    pub fn slot_base(&self, n: usize) -> Option<u64> {
        (n < self.count).then(|| self.base + (n as u64) * self.stride)
    }

    /// Interrupt number of slot `n`, or `None` if out of range.
    pub fn slot_irq(&self, n: usize) -> Option<u32> {
        (n < self.count).then(|| self.first_irq + n as u32)
    }
}

/// What kind of interrupt controller this platform has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptController {
    /// Legacy 8259 pair, programmed through I/O ports.
    Pic8259,
    /// ARM GICv2: a distributor and a CPU interface, both memory-mapped.
    GicV2 { gicd: u64, gicc: u64 },
}

/// The machine, as the kernel understands it before it discovers anything.
#[derive(Debug, Clone, Copy)]
pub struct PlatformInfo {
    /// Name for the boot log — this is the field that makes a wrong provider obvious.
    pub name: &'static str,
    /// The console UART.
    pub uart: PlatformDevice,
    /// The interrupt controller.
    pub intc: InterruptController,
    /// Interrupt the periodic timer raises.
    pub timer_irq: u32,
    /// The virtio-mmio bank, if this platform has one.
    pub virtio_mmio: Option<VirtioMmioWindow>,
}

/// This machine.
///
/// One hard-coded provider per architecture today. A discovery phase adds ACPI and
/// device-tree providers behind this same call, and every caller is already shaped for
/// it.
pub fn info() -> &'static PlatformInfo {
    #[cfg(target_arch = "x86_64")]
    {
        &X86_64_PC
    }
    #[cfg(target_arch = "aarch64")]
    {
        &QEMU_VIRT
    }
}

/// x86_64 PC: COM1 on a port, the 8259 pair, PIT on IRQ 0, and no virtio-mmio — VirtIO
/// arrives over PCI, which is enumerated rather than described.
#[cfg(target_arch = "x86_64")]
static X86_64_PC: PlatformInfo = PlatformInfo {
    name: "x86_64 PC (ports, 8259, PIT)",
    uart: PlatformDevice { base: 0x3F8, size: 8, irq: 4 },
    intc: InterruptController::Pic8259,
    timer_irq: 0,
    virtio_mmio: None,
};

/// QEMU `virt` (aarch64), GICv2. Every address here was previously a `const` beside the
/// driver that used it.
#[cfg(target_arch = "aarch64")]
static QEMU_VIRT: PlatformInfo = PlatformInfo {
    name: "QEMU virt (aarch64, GICv2)",
    // PL011. SPI 1 -> INTID 33.
    uart: PlatformDevice { base: 0x0900_0000, size: 0x1000, irq: 33 },
    intc: InterruptController::GicV2 { gicd: 0x0800_0000, gicc: 0x0801_0000 },
    // CNTV virtual timer, PPI 27.
    timer_irq: 27,
    // 32 transports at 0x200 stride; SPI 16..47 -> INTID 48..79.
    virtio_mmio: Some(VirtioMmioWindow {
        base: 0x0a00_0000,
        stride: 0x200,
        count: 32,
        first_irq: 48,
    }),
};
