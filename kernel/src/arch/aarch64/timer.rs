//! # ARM generic timer (virtual counter, `CNTV`)
//!
//! The aarch64 analog of the x86 PIT: a periodic interrupt at 100 Hz that drives
//! [`crate::arch::time::tick_count`].
//!
//! ## Why the *virtual* timer
//!
//! The architecture provides both a physical (`CNTP`) and a virtual (`CNTV`) timer at
//! EL1. We use `CNTV`, on **PPI 27**, because the physical timer can be trapped away
//! from EL1 by `CNTHCTL_EL2.EL1PCEN` when EL2 is present — which it is on the cloud
//! ARM hardware Phase 8 targets. The virtual timer is the trap-free guest choice, and
//! behaves identically for our purposes. `CNTP` would be PPI 30.
//!
//! ## How the countdown works
//!
//! `CNTV_TVAL_EL0` is a *down-counter*, decremented at the rate in `CNTFRQ_EL0`. The
//! interrupt asserts when it reaches zero, and stays asserted until the timer is
//! re-armed — so the handler must write `TVAL` again on every tick. Failing to re-arm
//! is not a missed tick but a permanently pending interrupt: the core would take the
//! same interrupt forever and make no progress. That, and forgetting the GIC EOI, are
//! the two ways this reliably wedges.
//!
//! The interval is `CNTFRQ_EL0 / TICK_HZ`. `CNTFRQ_EL0` is *not* a constant across
//! platforms (QEMU `virt` reports 62.5 MHz), so it is read rather than assumed.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::println;

/// Interrupt ID of the EL1 virtual timer: PPI 27. Private to each core.
pub const TIMER_INTID: u32 = 27;

/// Tick rate, matching the x86 PIT so `arch::time` means the same on both
/// architectures: one tick ≈ 10 ms.
const TICK_HZ: u64 = 100;

/// Countdown reload value, computed from `CNTFRQ_EL0` at [`init`].
static RELOAD: AtomicU64 = AtomicU64::new(0);

/// Read the timer frequency in Hz.
#[inline]
fn read_cntfrq() -> u64 {
    let v: u64;
    // SAFETY: reading CNTFRQ_EL0 has no side effects.
    unsafe { asm!("mrs {}, CNTFRQ_EL0", out(reg) v, options(nomem, nostack)) };
    v
}

/// Arm the virtual timer to fire after `ticks` counter decrements.
#[inline]
fn set_tval(ticks: u64) {
    // SAFETY: writing CNTV_TVAL_EL0 reloads the down-counter. No memory effect.
    unsafe { asm!("msr CNTV_TVAL_EL0, {}", in(reg) ticks, options(nomem, nostack)) };
}

/// Enable or disable the virtual timer.
///
/// `CNTV_CTL_EL0`: bit 0 = ENABLE, bit 1 = IMASK (interrupt mask — set means the
/// timer still counts but does not signal), bit 2 = ISTATUS (read-only, condition met).
#[inline]
fn set_ctl(enable: bool) {
    let v: u64 = if enable { 1 } else { 0 };
    // SAFETY: writing CNTV_CTL_EL0 controls timer signalling. No memory effect.
    unsafe { asm!("msr CNTV_CTL_EL0, {}", in(reg) v, options(nomem, nostack)) };
}

/// Start the periodic tick.
///
/// Requires the GIC to be up (this enables the timer's PPI through it) and should run
/// with interrupts still masked; the caller unmasks when it is ready to take them.
pub fn init() {
    let freq = read_cntfrq();
    assert!(
        freq != 0,
        "CNTFRQ_EL0 reads 0 — the platform did not program the timer frequency, so \
         no sensible tick interval can be derived"
    );

    let reload = freq / TICK_HZ;
    RELOAD.store(reload, Ordering::Relaxed);

    // Route the timer's PPI through the interrupt controller before enabling the
    // timer, so the first expiry has somewhere to go.
    super::gic::enable_intid(TIMER_INTID);

    set_tval(reload);
    set_ctl(true);

    println!(
        "[timer] CNTV armed: {} Hz counter, reload {} → {} Hz tick (PPI {})",
        freq, reload, TICK_HZ, TIMER_INTID
    );
}

/// Service one timer expiry: re-arm and advance the tick counter.
///
/// Called from the GIC dispatch path. Re-arming first keeps the window in which the
/// interrupt is still asserted as short as possible.
///
/// Phase 7.2 is **tick only** — `schedule()` is wired in here in 7.3, matching the
/// x86 split where the PIT ISR bumps the tick and separately calls the scheduler.
pub fn handle_tick() {
    set_tval(RELOAD.load(Ordering::Relaxed));
    super::time::bump();
}
