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
//! [padding page (4 KiB, RESERVED)] [usable stack (16 KiB, 4 pages)]
//!  ^                                ^                               ^
//!  phys_base                        usable bottom                   stack_top (initial RSP)
//! ```
//!
//! The padding page is **reserved, not unmapped**. It is owned by the task, so nothing
//! else is placed there and a small overflow runs into the task's own memory rather
//! than a neighbour's — but it does not fault.
//!
//! It used to be described as an unmapped guard page giving "true stack overflow
//! detection". That was never true. Kernel stacks live in the HHDM, which the
//! bootloader maps with 2 MiB/1 GiB huge pages, so the 4 KiB unmap walked into a huge
//! directory entry and cleared eight bytes of unrelated data while leaving the page
//! mapped (see the note in `sched::create_task`). Real detection requires kernel stacks
//! in a dedicated virtual window built from 4 KiB mappings — the same treatment the
//! kernel heap received — tracked as follow-up scheduler work.

use alloc::string::String;
use crate::mm::addr::PhysAddr;
#[cfg(target_arch = "x86_64")]
use crate::process::ProcessId;

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
/// Saved CPU context for a task, supplied by the active architecture.
///
/// Re-exported so `Task` and the scheduler name one type regardless of whether the
/// stack pointer underneath is called `rsp` or `sp`.
pub use crate::arch::context::TaskContext;


/// Number of 4 KiB pages for the usable stack area.
/// 4 pages = 16 KiB — enough for kernel task call stacks in Phase 1.
pub const STACK_PAGES: usize = 4;

/// Number of padding pages allocated below the usable stack.
///
/// Reserved (owned by the task) but still mapped — see the module docs. This bounds
/// the damage of a small overflow; it does not detect one.
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

    /// Top of this task's kernel stack (highest valid address, stack grows down).
    ///
    /// Written to TSS.RSP0 and PerCpu.kernel_stack_top on every context switch
    /// so that ring 3 → ring 0 transitions (syscall, interrupt) land on the
    /// correct kernel stack. Zero for the bootstrap task (which uses Limine's
    /// boot stack and never transitions from ring 3).
    ///
    /// x86_64 only — it exists to feed TSS.RSP0, and aarch64 has no TSS. The
    /// equivalent there is `SP_EL1`, which the CPU switches to on exception entry
    /// without the kernel having to stage an address anywhere, so this field arrives
    /// with the EL0 port rather than with the scheduler.
    #[cfg(target_arch = "x86_64")]
    pub kernel_stack_top: u64,

    /// The process this task belongs to. All boot-time tasks (main, idle, shell)
    /// belong to PID 0 (the kernel process). User tasks belong to the process
    /// that spawned them. The scheduler uses this to determine whether a CR3
    /// switch is needed on context switch (different process → different address space).
    #[cfg(target_arch = "x86_64")]
    pub process_id: ProcessId,

    /// The x86-64 `IA32_FS_BASE` value for this task (Phase 5.1). Linux thread-
    /// local storage is addressed via `%fs`, set by `arch_prctl(ARCH_SET_FS)`.
    /// FS base is a global register not saved by `switch_context`, so — exactly
    /// like the GS base — the scheduler restores it from here on every context
    /// switch. 0 for tasks that don't use TLS (kernel tasks, native servers).
    #[cfg(target_arch = "x86_64")]
    pub fs_base: u64,

    /// Ring-3 entry for a task created by Linux `clone` (Phase 5.3): the child
    /// resumes at the parent's post-`syscall` RIP on its own stack. `(rip, rsp)`;
    /// `None` for tasks that enter ring 3 by other means (ELF exec, servers).
    #[cfg(target_arch = "x86_64")]
    pub clone_entry: Option<(u64, u64)>,

    /// The Linux `CLONE_CHILD_CLEARTID` / `set_tid_address` futex word (Phase
    /// 5.3): on thread exit the kernel writes 0 here and futex-wakes it, which is
    /// how `pthread_join` observes a thread has finished. 0 if unset.
    #[cfg(target_arch = "x86_64")]
    pub clear_child_tid: u64,
}

impl Task {
    /// Virtual bounds of this task's usable kernel stack, as `(top, limit)`.
    ///
    /// `top` is one past the highest usable byte (the stack grows down from it) and
    /// `limit` is the lowest address the stack may reach before it runs into the
    /// padding page reserved beneath it. Both are HHDM addresses, matching what
    /// `create_task` hands to `setup_initial_stack`.
    ///
    /// `None` for the bootstrap task, which runs on the stack Limine handed over and
    /// whose extent the kernel therefore does not know. Callers must treat that as
    /// "unknown", not as "zero-sized" — a bounds check that fires on every address is
    /// worse than no bounds check at all.
    ///
    /// The arithmetic is architecture-neutral; the `cfg` is about the *consumer*. Only
    /// the aarch64 per-CPU block records stack bounds today, so on x86_64 this is dead
    /// code rather than shared code. Gated by consumer, the same way the ring-3 fields
    /// above are.
    #[cfg(target_arch = "aarch64")]
    pub fn kernel_stack_bounds(&self) -> Option<(u64, u64)> {
        let base = self.stack_phys_base?;
        let top = base.as_u64() + (TOTAL_STACK_PAGES as u64) * crate::mm::PAGE_SIZE;
        let limit = base.as_u64() + (PADDING_PAGES as u64) * crate::mm::PAGE_SIZE;
        Some((
            PhysAddr::new(top).to_virt().as_u64(),
            PhysAddr::new(limit).to_virt().as_u64(),
        ))
    }
}
