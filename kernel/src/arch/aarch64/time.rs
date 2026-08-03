//! # aarch64 monotonic tick
//!
//! The aarch64 side of the [`arch::time`](crate::arch::time) facade. A free-running
//! counter that the ARM generic-timer ISR bumps once per tick (100 Hz). The timer
//! itself lands in Phase 7.2; until then this reads a counter that stays at 0 (the
//! boot-to-banner milestone of 7.0b takes no timer interrupts), so time-dependent
//! callers see a monotonic-but-frozen clock rather than a link error.

use core::sync::atomic::{AtomicU64, Ordering};

/// Ticks since boot. Bumped by the generic-timer ISR (Phase 7.2).
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Ticks since boot (100 Hz; one tick ≈ 10 ms). Monotonic, wrapping.
#[inline]
pub fn tick_count() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Advance the tick counter by one. Called from the generic-timer ISR (Phase 7.2).
#[inline]
#[allow(dead_code)] // wired up when the timer ISR lands in 7.2
pub fn bump() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}
