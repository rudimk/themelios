//! # PSCI — Power State Coordination Interface
//!
//! The aarch64 answer to the question x86 solves with QEMU's `isa-debug-exit` device:
//! how does the guest *stop* the machine?
//!
//! x86_64's test harness writes a byte to I/O port `0xf4`, and QEMU turns that into a
//! process exit code — success and failure are distinguishable from outside without
//! reading a single line of output. The `virt` machine has no such device, and aarch64
//! has no I/O ports at all, so the port trick has no analog.
//!
//! What it does have is **PSCI**, the ARM-standard firmware interface for power
//! control. `SYSTEM_OFF` asks the firmware to power the machine down; QEMU implements
//! it directly (there is no real firmware here) and terminates. That gives a clean,
//! prompt shutdown — but *only one* exit status, because a machine that has been
//! switched off cannot say why. The pass/fail verdict therefore travels over the serial
//! console as a sentinel line, and this module's job is only to make QEMU exit
//! afterwards rather than sit idle until the harness times out.
//!
//! That split matters for what the harness can conclude:
//!
//! | Observation                          | Verdict                              |
//! |--------------------------------------|--------------------------------------|
//! | sentinel says PASS, QEMU exits       | pass                                 |
//! | sentinel says FAIL, QEMU exits       | fail, with the failing test named    |
//! | QEMU exits with no sentinel          | fail — the kernel died mid-suite     |
//! | no exit before the deadline          | fail — hang                          |
//!
//! The third row is the one worth having: without a shutdown the "died mid-suite" and
//! "hung" cases are indistinguishable, and both look like a timeout.
//!
//! ## Conduit
//!
//! PSCI calls reach the implementation through either `HVC` (to EL2) or `SMC` (to EL3),
//! and which one is correct depends on the platform — it is advertised in the device
//! tree, not fixed by the architecture. QEMU `virt` defaults to **HVC**, but boots with
//! EL3 firmware in some configurations, where `SMC` is the conduit instead.
//!
//! Rather than depend on a boot-time DTB parse we do not otherwise need, [`system_off`]
//! issues `HVC` and then `SMC`. On QEMU `virt` the first never returns, which is the
//! point.
//!
//! Be exact about what the second instruction is worth, because an earlier version of
//! this comment oversold it. It covers one case: a platform whose PSCI implementation
//! *returns* `NOT_SUPPORTED` from the HVC conduit. It does **not** cover a machine
//! where `HVC` is undefined — that raises a synchronous exception, and the Phase 7.2
//! handler ends an unrecognised syndrome at `panic!`, so control never reaches the
//! `SMC`. There, the fallback is dead code.
//!
//! Worse, in a `--features test` build that panic calls [`shutdown_or_hang`], which
//! calls back into here — unbounded recursion through the exception path. Neither is
//! reachable on `virt` (QEMU intercepts PSCI-over-HVC regardless of EL2), but the
//! reasoning originally written here did not hold, and a platform that genuinely needs
//! the `SMC` conduit will need a DTB parse rather than this.

use core::arch::asm;

/// `PSCI SYSTEM_OFF`, SMC32 calling convention.
///
/// SMC64 has its own function ID space, but `SYSTEM_OFF` takes no arguments and
/// returns nothing, so the 32-bit ID is the one to use and is what every PSCI
/// implementation supports.
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;

/// Power the machine off. Does not return if PSCI is available.
///
/// Falls through to the caller only if neither conduit is implemented, which is why
/// [`shutdown_or_hang`] exists to give that case a defined outcome.
///
/// # Safety
///
/// Stops the machine. Every caller must be prepared for execution to end here — in
/// particular, anything that must reach the serial console has to have been flushed
/// already. Ordering output before this call is necessary but, on real hardware, not
/// sufficient: `Pl011::putc` polls the TX FIFO for *space* before writing, never for a
/// character having left, and nothing drains `FR.BUSY`/`FR.TXFE` before the power-off.
/// On QEMU it holds because `pl011_write` forwards `DR` to the chardev immediately; on
/// a real part the tail of a line could be cut. A prior version of this comment called
/// the writes "synchronous", which they are not.
pub unsafe fn system_off() {
    // SAFETY: `HVC`/`SMC` with a PSCI function ID in x0. A conduit that is implemented
    // but does not recognise the call returns an error in x0; one that is not
    // implemented raises a synchronous exception the installed vectors report. Neither
    // corrupts state.
    //
    // The ID goes through x0 directly rather than a scratch register. The previous
    // form (`in(reg)` plus an x0-x3 clobber list) excused itself with "harmless because
    // a successful call never returns" — which is self-cancelling, since the second
    // block exists *only* for the case where the first one does return. Under SMCCC 1.0
    // a callee may clobber x4-x17, which is exactly where `in(reg)` could have put the
    // ID, and the compiler would have assumed it survived.
    unsafe {
        asm!(
            "hvc #0",
            inout("x0") PSCI_SYSTEM_OFF => _,
            out("x1") _, out("x2") _, out("x3") _,
            clobber_abi("C"),
            options(nostack),
        );
        asm!(
            "smc #0",
            inout("x0") PSCI_SYSTEM_OFF => _,
            out("x1") _, out("x2") _, out("x3") _,
            clobber_abi("C"),
            options(nostack),
        );
    }
}

/// Power off, or park the CPU forever if PSCI will not oblige.
///
/// The loop is not a fallback so much as an honest ending: if neither conduit works
/// there is nothing further this kernel can do, and spinning with interrupts masked is
/// preferable to returning into a caller that believed the machine had stopped. The
/// harness sees no exit, times out, and reports a hang — which is exactly what has
/// happened.
pub fn shutdown_or_hang() -> ! {
    // SAFETY: this is the intended end of execution; nothing after it needs to run.
    unsafe { system_off() };

    crate::arch::irq::disable();
    loop {
        crate::arch::irq::halt();
    }
}
