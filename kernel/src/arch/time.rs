//! # `arch::time` — architecture-neutral monotonic tick
//!
//! The kernel's coarse monotonic time source: a free-running counter incremented by
//! the periodic timer interrupt (100 Hz, so one tick ≈ 10 ms). Arch-neutral code
//! (audit timestamps, the container registry, the shell, the Linux `clock_gettime`
//! and `getrandom` shims) reads it through this facade rather than naming a specific
//! timer driver.
//!
//! On **x86_64** it is driven by the PIT ISR ([`arch::x86_64::idt::tick_count`]); on
//! **aarch64** (Phase 7.2+) by the ARM generic-timer ISR. Like [`arch::irq`], this is
//! a `pub use` re-export — no runtime dispatch.
//!
//! [`arch::x86_64::idt::tick_count`]: crate::arch::x86_64::idt::tick_count
//! [`arch::irq`]: crate::arch::irq

/// Ticks since boot (100 Hz; one tick ≈ 10 ms). Monotonic, wrapping.
#[cfg(target_arch = "x86_64")]
pub use crate::arch::x86_64::idt::tick_count;

// aarch64: driven by the generic-timer ISR (Phase 7.2). cfg-stripped on x86_64.
#[cfg(target_arch = "aarch64")]
pub use crate::arch::aarch64::time::tick_count;
