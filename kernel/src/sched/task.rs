//! # Task data structures
//!
//! Defines the core types for the scheduler: tasks, task IDs, task states,
//! and the saved CPU context used for context switching.
//!
//! ## Task lifecycle
//!
//! A task is created via `sched::spawn()`, which allocates a kernel stack,
//! sets up an initial CPU context (so it looks like the task was just
//! context-switched out), and places the task in the Ready state.
//!
//! State transitions:
//! ```text
//! spawn() → Ready → Running → Ready     (preempted by timer)
//!                  → Running → Dead      (entry function returned)
//!                  → Running → Blocked   (future: waiting for I/O)
//!                              Blocked → Ready (future: I/O completed)
//! ```
//!
//! ## Per-task kernel stack
//!
//! Each task gets its own kernel stack allocated from the physical frame
//! allocator. The stack is accessed via the HHDM (same as all physical
//! memory in the kernel). The layout is:
//!
//! ```text
//! [padding page (4 KiB)] [usable stack (16 KiB, 4 pages)]
//!  ^                      ^                               ^
//!  phys_base              usable bottom                   stack_top (initial RSP)
//! ```
//!
//! The padding page is NOT a true guard page (it's still mapped and writable —
//! unmapping requires page table modification, deferred to Phase 2). It just
//! provides a buffer between adjacent allocations to reduce the blast radius
//! of a stack overflow.

use alloc::string::String;
use crate::mm::addr::PhysAddr;

/// Unique identifier for each task.
///
/// Tasks are stored in a `Vec<Option<Task>>` indexed by their ID, so IDs
/// are sequential non-negative integers starting from 0. When a task dies
/// and is cleaned up, its slot becomes `None` and may be reused.
pub type TaskId = usize;

/// The possible states a task can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Eligible to run — sitting in the scheduler's ready queue waiting
    /// for its turn.
    Ready,

    /// Currently executing on the CPU. Exactly one task is Running at any
    /// time on a single-core system.
    Running,

    /// Waiting for an external event (I/O, IPC message, timer sleep).
    /// Not yet used in Phase 1 — included for completeness and future use.
    Blocked,

    /// Finished execution. The task's stack will be freed on the next
    /// scheduling pass and its slot in the task table reclaimed.
    Dead,
}

/// Saved CPU context for context switching.
///
/// Only stores the stack pointer — all callee-saved registers (rbx, rbp,
/// r12-r15) are pushed onto the task's own kernel stack by `switch_context`.
/// When switching away from a task, `switch_context` pushes registers and
/// stores RSP here. When switching back, it loads RSP from here and pops
/// the saved registers.
///
/// For a newly created task, `rsp` points to a pre-built stack frame that
/// mimics a `switch_context` save: callee-saved register slots (with r12
/// holding the entry function address) and a return address pointing to
/// `task_bootstrap`. When `switch_context` "restores" this context, it
/// pops the initial values and `ret`s into `task_bootstrap`.
#[repr(C)]
pub struct TaskContext {
    /// Stack pointer pointing to the saved callee-saved registers on this
    /// task's kernel stack.
    pub rsp: u64,
}

impl TaskContext {
    /// Create an empty (zeroed) context.
    ///
    /// Used for the bootstrap task (task 0) which represents the current
    /// execution context at the time `sched::init()` is called. Its context
    /// will be filled in by the first call to `switch_context` when the
    /// scheduler preempts it.
    pub const fn empty() -> Self {
        Self { rsp: 0 }
    }
}

/// Number of 4 KiB pages for the usable stack area.
/// 4 pages = 16 KiB — enough for kernel task call stacks in Phase 1.
pub const STACK_PAGES: usize = 4;

/// Number of padding pages allocated below the stack.
/// This is NOT a true guard page (it's still mapped) — just a buffer
/// to reduce the chance of stack overflow corrupting adjacent allocations.
/// True guard pages (unmapped) require page table modification (Phase 2).
pub const PADDING_PAGES: usize = 1;

/// Total pages allocated per task stack (usable + padding).
pub const TOTAL_STACK_PAGES: usize = STACK_PAGES + PADDING_PAGES;

/// A schedulable unit of execution (kernel thread).
///
/// Each task has its own kernel stack, saved CPU context, and metadata.
/// The scheduler maintains a `Vec<Option<Task>>` and switches between
/// tasks by saving/restoring their `context` field via `switch_context`.
pub struct Task {
    /// Unique identifier — also the index into the scheduler's task Vec.
    pub id: TaskId,

    /// Human-readable name for debug output (e.g., "idle", "shell", "test-3").
    pub name: String,

    /// Current execution state (Ready, Running, Blocked, Dead).
    pub state: TaskState,

    /// Saved CPU context (stack pointer). Updated by `switch_context` each
    /// time the task is switched out, read when switching back in.
    pub context: TaskContext,

    /// Physical base address of the stack allocation (including padding page).
    /// `None` for the bootstrap task which uses the Limine-provided boot stack.
    /// Used to free the stack frames when the task is cleaned up.
    pub stack_phys_base: Option<PhysAddr>,
}
