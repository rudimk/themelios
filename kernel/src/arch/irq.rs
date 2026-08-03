//! # `arch::irq` — architecture-neutral local-interrupt control
//!
//! A thin, zero-cost facade over the active architecture's local-interrupt-flag
//! primitives, so arch-neutral kernel code (the scheduler, IPC, [`sync`](crate::sync),
//! boot) can mask/unmask interrupts and idle the CPU **without naming a specific
//! architecture**. This is the seam that lets those subsystems compile unchanged on
//! both x86_64 and aarch64 (Phase 7).
//!
//! On **x86_64** these map to `cli`/`sti`/`interrupts_enabled`/`hlt`; on **aarch64**
//! (landing in Phase 7.2+) they map to `DAIF` masking and `wfi`. The facade is a
//! `pub use` re-export of the active arch's implementation — no runtime dispatch,
//! identical codegen to calling the arch function directly.
//!
//! ## Naming
//!
//! Deliberately architecture-neutral verbs (`disable`/`enable`/`are_enabled`/`halt`)
//! rather than the x86 mnemonics (`cli`/`sti`), so a reader of `sched`/`ipc`/`sync`
//! isn't misled into thinking that code is x86-specific.

// x86_64: re-export the hand-rolled CPU primitives under neutral names. Renaming in
// the `pub use` keeps a single source of truth (the impl lives in `arch::x86_64::cpu`)
// while presenting the arch-neutral vocabulary to callers.
#[cfg(target_arch = "x86_64")]
pub use crate::arch::x86_64::cpu::{
    cli as disable, halt, interrupts_enabled as are_enabled, sti as enable,
};

// aarch64: the DAIF/`wfi` implementations land with the interrupt-controller and
// timer work (Phase 7.2). This branch is cfg-stripped on x86_64 builds, so it does
// not need `arch::aarch64::irq` to exist yet.
#[cfg(target_arch = "aarch64")]
pub use crate::arch::aarch64::irq::{are_enabled, disable, enable, halt};
