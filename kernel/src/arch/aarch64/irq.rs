//! # aarch64 local-interrupt control (`DAIF`)
//!
//! The aarch64 side of the [`arch::irq`](crate::arch::irq) facade. On aarch64 the
//! interrupt mask lives in the `DAIF` process-state field: the **I** bit (IRQ mask)
//! is the analog of the x86 interrupt flag. `msr DAIFSet/DAIFClr, #2` sets/clears the
//! I bit (the `#2` immediate is the `{D=8,A=4,I=2,F=1}` mask), and `wfi` idles the
//! core until an interrupt — the analog of x86 `hlt`.

use core::arch::asm;

/// Mask IRQs (the x86 `cli` analog): set `DAIF.I`.
#[inline(always)]
pub fn disable() {
    // SAFETY: DAIFSet is an unprivileged-safe PSTATE write; masks IRQs only.
    unsafe { asm!("msr DAIFSet, #2", options(nomem, nostack, preserves_flags)) };
}

/// Unmask IRQs (the x86 `sti` analog): clear `DAIF.I`.
#[inline(always)]
pub fn enable() {
    // SAFETY: DAIFClr unmasks IRQs; no memory effect.
    unsafe { asm!("msr DAIFClr, #2", options(nomem, nostack, preserves_flags)) };
}

/// Whether IRQs are currently unmasked (`DAIF.I == 0`).
#[inline(always)]
pub fn are_enabled() -> bool {
    let daif: u64;
    // SAFETY: reading DAIF has no side effects.
    unsafe { asm!("mrs {}, DAIF", out(reg) daif, options(nomem, nostack, preserves_flags)) };
    // In the DAIF read, the I (IRQ mask) bit is bit 7. Interrupts are *enabled* when
    // it is clear.
    daif & (1 << 7) == 0
}

/// Idle the core until the next interrupt (the x86 `hlt` analog).
#[inline(always)]
pub fn halt() {
    // SAFETY: `wfi` is a hint that suspends until an interrupt/event; always safe.
    unsafe { asm!("wfi", options(nomem, nostack, preserves_flags)) };
}
