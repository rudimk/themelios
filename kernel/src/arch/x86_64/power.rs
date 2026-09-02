//! # x86_64 power control — stopping and restarting the machine
//!
//! The aarch64 side of this has a single, standard answer: PSCI, an ARM-specified
//! firmware interface with a fixed function ID. x86 has no such thing — the equivalent
//! information is discoverable, but only by parsing ACPI tables — and this module is
//! mostly an honest account of that gap.
//!
//! ## Shutdown is guessed; reset is real
//!
//! **Soft-off (S5) on x86 is an ACPI operation.** The correct sequence is: find the RSDP,
//! walk to the FADT, read `PM1a_CNT_BLK`/`PM1b_CNT_BLK` to learn where the control
//! registers live, and learn which `SLP_TYP` values mean S5 on this machine.
//!
//! That last part is the only genuinely awkward one, and it is *not* as awkward as an
//! earlier version of this comment claimed. `\_S5` is an AML object, but in practice it is
//! a static `Name(_S5, Package(){...})`, and the usual approach is to scan the DSDT byte
//! stream for the `_S5_` signature and decode the small fixed package encoding after it —
//! a byte scan, not an interpreter. ACPI 5.0's hardware-reduced profile skips even that,
//! specifying S5 entry through the FADT's `SLEEP_CONTROL_REG`/`SLEEP_STATUS_REG` with no
//! `\_S5` lookup at all. What this kernel is missing is an **ACPI table parser**, which is
//! a much smaller thing than an AML interpreter, and the same parser would supply
//! `RESET_REG` for `reset` too.
//!
//! What `power_off` does instead is write the `SLP_EN` bit to the `PM1a_CNT` addresses the
//! emulators are known to end up with, in turn. That works on the emulators and is
//! actively unwise anywhere else — see `SOFT_OFF_PORTS`. It is not "shutdown on x86",
//! and the boot log says so rather than letting a silent hang stand in for the difference.
//!
//! **Reset is a different story.** The Reset Control Register at port `0xCF9` is genuine
//! chipset hardware — Intel ICH/PCH and everything that clones it — and QEMU implements
//! it. So `reset` is real on hardware in a way `power_off` is not, and its fallbacks
//! (the 8042 pulse, then a deliberate triple fault) are progressively cruder but work
//! essentially everywhere x86 does.
//!
//! ## Why not reuse `exit_qemu`?
//!
//! Because `cpu::exit_qemu` is not shutdown. It writes to `isa-debug-exit`, a
//! device that exists only when the test harness passes
//! `-device isa-debug-exit,iobase=0xf4`, and its whole purpose is to hand an exit *code*
//! to the harness. On an interactive boot that port is unmapped and the write does
//! nothing. Wiring the shell's `shutdown` to it would produce a command that works under
//! `cargo xtask test` and silently fails under `cargo xtask run` — the exact inversion of
//! what a user would expect.

use super::cpu::{halt, outb, outw};
use crate::arch::irq;

/// `PM1a_CNT` addresses seen on the emulators, with the value that requests soft-off.
///
/// The value is `SLP_EN` (bit 13) — `0x2000` — with `SLP_TYP` (bits 10..12) left at 0,
/// which QEMU maps to soft-off. VirtualBox needs `SLP_TYP = 5` as well, giving `0x3400`.
///
/// **Neither address is chosen by the hypervisor.** `PMBASE` is programmed by *firmware*,
/// and the FADT then tells the OS where it landed. SeaBIOS defaults `acpi_pm_base` to
/// `0xb000` and moves it to `0x0600` only when QEMU exposes `etc/table-loader` — i.e. only
/// when QEMU is generating ACPI tables at all. So `0x604` is the common case and `0xb004`
/// is *current* QEMU with `-machine acpi=off`, not some antique: booting
/// `q35,acpi=off` is exactly how the fall-through below was observed.
///
/// ## This is unsafe on real hardware, and the list is not the reason
///
/// On a physical machine the FADT names an address that is very unlikely to be either of
/// these, and `0xb000`/`0x4000` are ordinary low PCI/LPC I/O space — routinely assigned to
/// an SMBus controller, a SuperIO or EC config window, or a PCI I/O BAR. Blind-writing
/// `0x2000` there pokes whatever *is* there and then falls through to a hang.
///
/// An unmapped x86 I/O write at CPL 0 is discarded rather than faulting, so this cannot
/// crash a machine that has nothing at these ports. That is the only guarantee available,
/// and it is not the same as "harmless". Adding entries to this table is not the fix; the
/// fix is reading `PM1a_CNT_BLK` out of the FADT before writing anything, and until that
/// exists `power_off` should be understood as an emulator convenience.
const SOFT_OFF_PORTS: &[(u16, u16, &str)] = &[
    // QEMU/SeaBIOS with ACPI tables (the default), and OVMF on Q35.
    (0x604, 0x2000, "QEMU (PMBASE 0x600)"),
    // SeaBIOS's own default, which survives when QEMU generates no ACPI tables
    // (`-machine acpi=off`); also OVMF + i440fx, whose PIIX4 PMBASE is 0xb000.
    (0xB004, 0x2000, "SeaBIOS default / acpi=off (PMBASE 0xb000)"),
    // VirtualBox: PM_PORT_BASE 0x4000 + PM1a_CTL 0x04, and it decodes SLP_TYP 5.
    (0x4004, 0x3400, "VirtualBox"),
];

/// The chipset Reset Control Register (Intel ICH/PCH and compatibles).
const RESET_CONTROL_PORT: u16 = 0xCF9;
/// `SYS_RST` (bit 1) — arm a system reset.
const RCR_SYS_RST: u8 = 0x02;

/// `RST_CPU | SYS_RST` (bits 2 and 1) — perform a **hard** reset.
///
/// Deliberately *not* named `FULL_RESET`, which an earlier draft called it: bit 3 of
/// `RST_CNT` is `FULL_RST`, and this value leaves it clear. A hard reset is what is wanted
/// — `FULL_RST` additionally cycles platform power, which is more than "reboot" means.
///
/// The two-step (`0x02` then `0x06`) is copied from Linux's
/// `native_machine_emergency_restart`, not from a datasheet: the ICH/PCH documentation
/// describes the bits, not a write sequence. Linux reads-modifies-writes so it preserves
/// the register's other bits, and delays between the two writes; this does neither,
/// because at this point nothing else in the machine is going to observe `RST_CNT` again.
const RCR_HARD_RESET: u8 = 0x06;

/// The 8042 keyboard controller's command port, and the pulse that drives the CPU's
/// `RESET#` line low. Predates ACPI and every chipset register here; on machines with no
/// real 8042 the port is usually still emulated for exactly this reason.
const KBD_COMMAND_PORT: u16 = 0x64;
const KBD_PULSE_RESET: u8 = 0xFE;

/// Power the machine off, or park it forever if nothing here is understood.
///
/// Tries each known hypervisor `PM1a_CNT` address in turn. A machine that implements one
/// of them stops inside that write and never reaches the next; one that implements none
/// falls through to [`park`]. On an emulator that is harmless; on real hardware it is not
/// necessarily so, and [`SOFT_OFF_PORTS`] says why.
///
/// Never returns. Callers must have flushed anything they need on the console first.
pub fn power_off() -> ! {
    crate::println!("[power] requesting soft-off (ACPI S5)");

    for &(port, value, who) in SOFT_OFF_PORTS {
        crate::println!("[power]   trying {who} PM1a_CNT at {port:#x} <- {value:#x}");
        // SAFETY: a word write to an I/O port. At CPL 0 an x86 I/O write to an unclaimed
        // port is discarded rather than faulting, so this cannot trap; on a machine that
        // *does* decode the port as PM1a_CNT it does not return. What it is not is
        // guaranteed inert on real hardware — see `SOFT_OFF_PORTS`.
        unsafe { outw(port, value) };
    }

    crate::println!(
        "[power] no known soft-off port responded — this machine needs the FADT's \
         PM1a_CNT_BLK, which needs an ACPI table parser this kernel does not have"
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
/// 3. **Triple fault.** Not a mechanism so much as a consequence — see
///    [`triple_fault`]. It is last because it goes through no firmware at all, so it is
///    the one option that cannot be declined; what it *architecturally* guarantees is
///    that the processor enters shutdown state, and turning that into a reset is the
///    platform's response, not the CPU's.
///
/// Never returns.
pub fn reset() -> ! {
    crate::println!("[power] resetting");

    // SAFETY: the documented two-step on the chipset reset register. On a part that
    // implements it, the second write does not return.
    unsafe {
        outb(RESET_CONTROL_PORT, RCR_SYS_RST);
        outb(RESET_CONTROL_PORT, RCR_HARD_RESET);
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
/// Load an IDT with zero limit, then execute `int3`. The chain that follows is four
/// exceptions long, not two (Intel SDM Vol. 3 §6.14.2 and Table 6-5):
///
/// 1. Vector 3 is beyond the IDT limit, so delivery raises **`#GP`** — not `#DF`. `#BP` is
///    a *benign* exception (and a trap, not a fault), and failing to deliver a benign
///    exception never yields `#DF` directly.
/// 2. Delivering that `#GP` is also beyond the limit, raising a second `#GP`.
/// 3. `#GP` is *contributory*, and contributory-during-contributory is the definition of
///    **`#DF`**.
/// 4. `#DF` cannot be delivered either, and a fault during double-fault delivery is a
///    triple fault: the processor enters **shutdown state**.
///
/// What happens next is up to the platform. Hardware asserts reset; QEMU without
/// `-no-reboot` resets, and *with* it shuts the VM down instead — which is why the
/// harness's crash behaviour and a deliberate `reboot` look alike under `cargo xtask run`.
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
