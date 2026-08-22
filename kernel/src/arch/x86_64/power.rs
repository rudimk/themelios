//! # x86_64 power control — stopping and restarting the machine
//!
//! The aarch64 side of this has a single, standard, *real* answer: PSCI. Ask firmware to
//! power off, and it does, on QEMU and on a Graviton alike. x86 has no such thing, and
//! this module is mostly an honest account of that gap.
//!
//! ## Shutdown is guessed; reset is real
//!
//! **Soft-off (S5) on x86 is an ACPI operation.** The correct sequence is: find the RSDP,
//! walk to the FADT, learn where `PM1a_CNT`/`PM1b_CNT` live and what `SLP_TYPa`/`SLP_TYPb`
//! values the `\_S5` package specifies — and `\_S5` is an *AML object*, so reading it
//! means interpreting ACPI Machine Language. That is an interpreter this kernel does not
//! have and is not close to having.
//!
//! What `power_off` does instead is write the `SLP_EN` bit to the `PM1a_CNT` port that
//! each hypervisor is *known to place at a fixed address*, in turn. That works on the
//! emulators and on nothing else. It is not "shutdown on x86"; it is "shutdown on the
//! three virtual machines we know the magic number for", and the boot log says so rather
//! than letting a silent hang stand in for the difference.
//!
//! **Reset is a different story.** The Reset Control Register at port `0xCF9` is genuine
//! chipset hardware — Intel ICH/PCH and everything that clones it — and QEMU implements
//! it. So `reset` is real on hardware in a way `power_off` is not, and its fallbacks
//! (the 8042 pulse, then a deliberate triple fault) are progressively cruder but work
//! essentially everywhere x86 does.
//!
//! ## Why not reuse `exit_qemu`?
//!
//! Because [`super::cpu::exit_qemu`] is not shutdown. It writes to `isa-debug-exit`, a
//! device that exists only when the test harness passes
//! `-device isa-debug-exit,iobase=0xf4`, and its whole purpose is to hand an exit *code*
//! to the harness. On an interactive boot that port is unmapped and the write does
//! nothing. Wiring the shell's `shutdown` to it would produce a command that works under
//! `cargo xtask test` and silently fails under `cargo xtask run` — the exact inversion of
//! what a user would expect.

use super::cpu::{halt, outb, outw};
use crate::arch::irq;

/// ACPI `PM1a_CNT` addresses that hypervisors are known to fix in place, with the value
/// that requests soft-off.
///
/// The value is `SLP_EN` (bit 13) — `0x2000` — with `SLP_TYP` (bits 10..12) left at 0,
/// except on VirtualBox where `SLP_TYP = 5` is required, giving `0x3400`.
///
/// **On real hardware the FADT names this address and it is not any of these.** Writing a
/// word to whatever ISA port happens to live there is harmless in practice only because
/// these addresses are inside the ACPI PM block on the machines that implement them.
/// Extending this table is not the fix for a physical machine; an ACPI parser is.
const SOFT_OFF_PORTS: &[(u16, u16, &str)] = &[
    // QEMU 2.0+ (PIIX4 and Q35 both map the ACPI PM block here by default).
    (0x604, 0x2000, "QEMU"),
    // Bochs, and QEMU before 2.0.
    (0xB004, 0x2000, "Bochs / QEMU < 2.0"),
    // VirtualBox, which requires SLP_TYP = 5 as well as SLP_EN.
    (0x4004, 0x3400, "VirtualBox"),
];

/// The chipset Reset Control Register (Intel ICH/PCH and compatibles).
const RESET_CONTROL_PORT: u16 = 0xCF9;
/// `SYS_RST` — request a system reset.
const RCR_SYS_RST: u8 = 0x02;
/// `RST_CPU | SYS_RST` — perform it. Writing `SYS_RST` first and then this pair is the
/// documented two-step; a single write to `0x06` also works on most parts, but the
/// two-step is what the datasheet specifies.
const RCR_FULL_RESET: u8 = 0x06;

/// The 8042 keyboard controller's command port, and the pulse that drives the CPU's
/// `RESET#` line low. Predates ACPI and every chipset register here; on machines with no
/// real 8042 the port is usually still emulated for exactly this reason.
const KBD_COMMAND_PORT: u16 = 0x64;
const KBD_PULSE_RESET: u8 = 0xFE;

/// Power the machine off, or park it forever if nothing here is understood.
///
/// Tries each known hypervisor `PM1a_CNT` address in turn. A machine that implements one
/// of them stops inside that write and never reaches the next; one that implements none
/// falls through to [`park`], having changed nothing that matters — the ports written are
/// either the ACPI PM block or unmapped ISA space, and an unmapped write is discarded.
///
/// Never returns. Callers must have flushed anything they need on the console first.
pub fn power_off() -> ! {
    crate::println!("[power] requesting soft-off (ACPI S5)");

    for &(port, value, who) in SOFT_OFF_PORTS {
        crate::println!("[power]   trying {who} PM1a_CNT at {port:#x} <- {value:#x}");
        // SAFETY: a word write to an ACPI PM control port. On a machine that implements
        // it this does not return; on one that does not, the port is unmapped and the
        // write is discarded.
        unsafe { outw(port, value) };
    }

    crate::println!(
        "[power] no known soft-off port responded — this machine needs real ACPI \
         (FADT + \\_S5 via an AML interpreter), which this kernel does not implement"
    );
    park();
}

/// Reset the machine, or park it forever if every method fails.
///
/// Three attempts, most to least civilised:
///
/// 1. **Reset Control Register (`0xCF9`).** Real chipset hardware, and what a modern
///    x86 machine actually wants.
/// 2. **8042 pulse.** The original PC reset, still emulated nearly everywhere precisely
///    because so much software depends on it.
/// 3. **Triple fault.** Not a mechanism so much as a consequence: with no usable IDT, a
///    fault cannot be delivered, the fault handling that failure triggers cannot be
///    delivered either, and the CPU gives up and resets. Architecturally guaranteed, and
///    the reason it is last is that it goes through no firmware at all.
///
/// Never returns.
pub fn reset() -> ! {
    crate::println!("[power] resetting");

    // SAFETY: the documented two-step on the chipset reset register. On a part that
    // implements it, the second write does not return.
    unsafe {
        outb(RESET_CONTROL_PORT, RCR_SYS_RST);
        outb(RESET_CONTROL_PORT, RCR_FULL_RESET);
    }

    crate::println!("[power]   reset control register did not take; pulsing the 8042");
    // SAFETY: the 8042 command port; `0xFE` pulses the CPU reset line.
    unsafe { outb(KBD_COMMAND_PORT, KBD_PULSE_RESET) };

    crate::println!("[power]   8042 did not take; forcing a triple fault");
    triple_fault();
}

/// The operand `lidt` takes: a limit and a base, packed with no padding.
///
/// Declared here rather than borrowed from [`super::idt`] because the *point* of this one
/// is to be invalid — a zero limit means the table holds no descriptors at all.
#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Reset by making exception delivery impossible.
///
/// Load an IDT with zero limit, then raise a breakpoint. The CPU cannot deliver `#BP`
/// (there is no descriptor), tries to raise `#DF`, cannot deliver that either, and a
/// fault during double-fault delivery is by definition a triple fault: the processor
/// enters shutdown, and every platform's response to shutdown is to assert reset.
fn triple_fault() -> ! {
    let null_idt = DescriptorTablePointer { limit: 0, base: 0 };

    // Mask interrupts across the `lidt`/`int3` pair. Not for correctness of the outcome —
    // a timer interrupt arriving after the `lidt` would triple-fault just as well, which
    // is the goal — but so that what resets the machine is the instruction this function
    // executes rather than whatever happened to fire first. A reset is not the place to
    // leave the proximate cause up to timing.
    irq::disable();

    // SAFETY: deliberately destroying interrupt delivery, immediately before relying on
    // that destruction. Nothing after this point is expected to execute, and interrupts
    // are masked above, so no other CPU state matters.
    unsafe {
        core::arch::asm!(
            "lidt [{}]",
            "int3",
            in(reg) &null_idt,
            options(nostack)
        );
    }

    // Architecturally unreachable.
    park();
}

/// Stop this CPU for good.
///
/// Reached only when every mechanism above was ignored — which means the machine is one
/// this kernel does not know how to stop. Masking interrupts and halting is the honest
/// ending: returning to a caller that believed the machine had stopped would be worse,
/// and under the test harness a hang is reported as a hang rather than as a pass.
fn park() -> ! {
    irq::disable();
    loop {
        halt();
    }
}
