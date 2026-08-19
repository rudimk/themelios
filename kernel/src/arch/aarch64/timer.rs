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

/// Tick rate: one tick ≈ 10 ms, the same nominal rate as the x86 PIT so `arch::time`
/// is comparable across architectures.
///
/// "Comparable", not identical. The 8253 in mode 3 auto-reloads in hardware from the
/// previous expiry; here the handler re-arms in software. `handle_tick` advances an
/// absolute deadline and credits skipped periods precisely so the two stay equivalent
/// for elapsed-time purposes, but the mechanisms differ and anything comparing tick
/// deltas across arches should know it.
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

/// Read the virtual counter. Free-running, monotonic, and — crucially — advancing
/// whether or not interrupts are being delivered, which makes it the only sound basis
/// for timing out a wait on the timer itself.
#[inline]
pub fn read_cntvct() -> u64 {
    let v: u64;
    // SAFETY: reading CNTVCT_EL0 has no side effects.
    unsafe { asm!("mrs {}, CNTVCT_EL0", out(reg) v, options(nomem, nostack)) };
    v
}

/// Read the current absolute compare value.
#[inline]
fn read_cval() -> u64 {
    let v: u64;
    // SAFETY: reading CNTV_CVAL_EL0 has no side effects.
    unsafe { asm!("mrs {}, CNTV_CVAL_EL0", out(reg) v, options(nomem, nostack)) };
    v
}

/// Set the absolute deadline at which the timer next fires.
#[inline]
fn set_cval(deadline: u64) {
    // SAFETY: writing CNTV_CVAL_EL0 sets the compare value. No memory effect.
    unsafe { asm!("msr CNTV_CVAL_EL0, {}", in(reg) deadline, options(nomem, nostack)) };
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
        freq >= TICK_HZ,
        "CNTFRQ_EL0 reads {} Hz, below the {} Hz tick rate — the reload would round to \
         0, which re-expires immediately and produces an unbreakable interrupt storm",
        freq,
        TICK_HZ
    );

    let reload = freq / TICK_HZ;
    RELOAD.store(reload, Ordering::Relaxed);

    // Route the timer's PPI through the interrupt controller before enabling the
    // timer, so the first expiry has somewhere to go.
    super::gic::enable_intid(TIMER_INTID);

    // Arm the first deadline relative to now; every subsequent one is relative to the
    // previous deadline (see `handle_tick`).
    set_cval(read_cntvct() + reload);
    set_ctl(true);

    println!(
        "[timer] CNTV armed: {} Hz counter, reload {} → {} Hz tick (PPI {})",
        freq, reload, TICK_HZ, TIMER_INTID
    );
}

/// The counter units per tick, as computed at [`init`] from `CNTFRQ_EL0`.
pub fn reload() -> u64 {
    RELOAD.load(Ordering::Relaxed)
}

/// Service one timer expiry: re-arm and advance the tick counter.
///
/// Called from the GIC dispatch path. Re-arming first keeps the window in which the
/// interrupt is still asserted as short as possible — and re-arming *at all* is
/// mandatory, because the timer drives a **level** line into the GIC. If the compare
/// condition stays met, the GIC re-pends the interrupt immediately after every `EOIR`
/// and the core does nothing but service timer interrupts.
///
/// ## Absolute deadlines, not a fixed reload
///
/// The obvious implementation writes `CNTV_TVAL_EL0 = reload`, which sets the deadline
/// to *now* + reload. That anchors each period to when the handler happened to run, so
/// every tick permanently absorbs interrupt-entry and dispatch latency and the clock
/// drifts. Advancing `CNTV_CVAL_EL0` by exactly `reload` instead anchors to the
/// previous deadline, so the period is exact regardless of how long servicing took.
///
/// Ticks can still be *lost*: if interrupts are masked for longer than a period, the
/// line is already asserted and several expiries collapse into one interrupt. The loop
/// below therefore advances the deadline past `now` and credits every period it skips,
/// so `tick_count` tracks elapsed time rather than interrupts serviced.
///
/// Phase 7.2 is **tick only** — `schedule()` is wired in here in 7.3, matching the
/// x86 split where the PIT ISR bumps the tick and separately calls the scheduler.
pub fn handle_tick() {
    let reload = RELOAD.load(Ordering::Relaxed);
    let now = read_cntvct();
    let mut deadline = read_cval();

    // Advance past `now`, crediting one tick per elapsed period. Normally this runs
    // exactly once; it runs more only if we were masked through one or more periods.
    loop {
        deadline = deadline.wrapping_add(reload);
        super::time::bump();
        if deadline > now {
            break;
        }
    }
    set_cval(deadline);
}
