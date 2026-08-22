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

// Each provider populates the whole struct, but each architecture reads only the parts
// that apply to it: `GicV2` and the mmio bank are aarch64-only, `X86Port` is x86-only.
// The unread remainder is the price of one shared description rather than two divergent
// ones, which is the entire point of the module. Marked rather than papered over, so a
// field that is dead on *both* architectures still shows up in review.
#![allow(dead_code)]

/// Which UART programming model a console needs.
///
/// The ACPI **SPCR** table names this, and it decides which driver binds: PL011, 16550
/// and the SBSA Generic UART are not one driver. `PlatformInfo` carried only base/size/irq
/// at first, which meant an ACPI provider would have had to discard the one field that
/// answers the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UartKind {
    /// ARM PL011.
    Pl011,
    /// NS16550-compatible, memory-mapped.
    Ns16550,
    /// SBSA Generic UART (a PL011 subset).
    SbsaGeneric,
    /// x86 port-I/O 8250/16550 — `base` is a port number, not an address.
    X86Port,
}

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

/// Build a bank of virtio-mmio transport descriptors at compile time.
///
/// 8.1 first modelled this as a `VirtioMmioWindow { base, stride, count, first_irq }`,
/// on the stated grounds that a window is "the form firmware actually reports". **That
/// was wrong, and this plan's own decision 12 refutes it:** QEMU `virt` emits 32
/// individual `virtio_mmio@<base>` device-tree nodes, each with its own `reg` and
/// `interrupts` — which is why it can set `dma-coherent` on *every node*. A property
/// emitted per node is evidence of a list. ACPI is no better: SR-class servers have no
/// virtio-mmio at all, and the bindings that exist are per-device. No firmware anywhere
/// produces a window.
///
/// The real objection to a list was ergonomic — nobody wants 32 near-identical struct
/// literals. A `const fn` answers that: four authored numbers, a list-shaped field, and
/// a device-tree provider drops straight in. It also stops silently asserting the two
/// things a window bakes in — uniform stride, and interrupts both contiguous *and*
/// co-ordered with bases — which hold on `virt` and are guaranteed nowhere else.
const fn mmio_bank(base: u64, stride: u64, first_irq: u32) -> [PlatformDevice; 32] {
    let mut bank = [PlatformDevice { base: 0, size: 0, irq: IRQ_NONE }; 32];
    let mut i = 0;
    while i < 32 {
        bank[i] = PlatformDevice {
            base: base + (i as u64) * stride,
            size: stride,
            irq: first_irq + i as u32,
        };
        i += 1;
    }
    bank
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
    /// The console UART: where it is, and which programming model it needs.
    pub uart: PlatformDevice,
    /// Which UART driver the console needs — SPCR's field, in ACPI terms.
    pub uart_kind: UartKind,
    /// The interrupt controller.
    pub intc: InterruptController,
    /// Interrupt the periodic timer raises.
    pub timer_irq: u32,
    /// The virtio-mmio transports, if this platform has any. Empty on PCI platforms,
    /// where VirtIO is enumerated rather than described.
    pub virtio_mmio: &'static [PlatformDevice],
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
    uart_kind: UartKind::X86Port,
    intc: InterruptController::Pic8259,
    timer_irq: 0,
    virtio_mmio: &[],
};

/// QEMU `virt` (aarch64), GICv2. Every address here was previously a `const` beside the
/// driver that used it.
#[cfg(target_arch = "aarch64")]
static QEMU_VIRT: PlatformInfo = PlatformInfo {
    name: "QEMU virt (aarch64, GICv2)",
    // PL011. SPI 1 -> INTID 33.
    uart: PlatformDevice { base: 0x0900_0000, size: 0x1000, irq: 33 },
    uart_kind: UartKind::Pl011,
    intc: InterruptController::GicV2 { gicd: 0x0800_0000, gicc: 0x0801_0000 },
    // CNTV virtual timer, PPI 27.
    timer_irq: 27,
    // 32 transports at 0x200 stride; SPI 16..47 -> INTID 48..79.
    virtio_mmio: &VIRT_MMIO_BANK,
};

/// QEMU `virt`'s 32 virtio-mmio transports: `0x0a00_0000`, 0x200 stride, SPI 16..47
/// (INTID 48..79). Four authored numbers; the list is expanded at compile time.
#[cfg(target_arch = "aarch64")]
static VIRT_MMIO_BANK: [PlatformDevice; 32] = mmio_bank(0x0a00_0000, 0x200, 48);
