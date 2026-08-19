//! # Context-switch facade
//!
//! Re-exports the active architecture's task context primitives, so the scheduler is
//! written once. Like the other `arch::*` facades this is a compile-time `pub use`,
//! not a trait object.
//!
//! ## The contract
//!
//! | Item | Meaning |
//! |------|---------|
//! | `TaskContext` | saved CPU state for a task (the stack pointer; everything else lives on that stack) |
//! | `switch_context` | save the outgoing task's callee-saved registers, swap stacks, restore the incoming task's |
//! | `task_bootstrap` | first code a new task runs: unmask interrupts, call its entry function, catch a return |
//! | `setup_initial_stack` | build the frame that makes the first switch land in `task_bootstrap` |
//!
//! The two differ in more than register names: x86_64's `ret` pops a return address
//! while aarch64's branches to `x30`, so the initial frames are laid out differently,
//! and the callee-saved sets have different sizes. Those details live in the
//! per-architecture modules.

#[cfg(target_arch = "x86_64")]
pub use crate::arch::x86_64::context::*;

#[cfg(target_arch = "aarch64")]
pub use crate::arch::aarch64::context::*;
