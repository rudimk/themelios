//! # Process scheduler
//!
//! Manages all executable tasks (kernel threads and userspace processes) and
//! decides which one runs on which CPU core at any given time.
//!
//! ## Responsibilities
//!
//! - **Task lifecycle**: Create, run, suspend, resume, and terminate tasks.
//! - **Scheduling policy**: Decide which task runs next. Initial implementation
//!   will use a simple round-robin scheduler; later phases may add priority-based
//!   or fair scheduling.
//! - **Context switching**: Save the current task's CPU state and restore the
//!   next task's state. The actual register save/restore is architecture-specific
//!   (in `arch::{x86_64,aarch64}`), but the scheduler decides *when* to switch.
//! - **SMP support** (future): Run tasks across multiple CPU cores.
//!
//! ## Design notes
//!
//! In ThemeliOS, containers map to groups of tasks. The scheduler will eventually
//! be capability-aware — a container's CPU time allocation is controlled by
//! capabilities granted to it.
