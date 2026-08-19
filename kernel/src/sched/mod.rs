//! # Preemptive round-robin scheduler
//!
//! Manages kernel tasks and decides which one runs on the CPU at any given
//! time. The scheduler is driven by the PIT timer interrupt (IRQ0, ~100 Hz)
//! which calls `schedule()` on every tick for preemptive multitasking.
//!
//! ## Design
//!
//! - **Round-robin**: tasks take turns in FIFO order. Each task runs until
//!   the next timer tick (≈10 ms at 100 Hz), then gets moved to the back
//!   of the ready queue.
//! - **Ready queue**: a `VecDeque<TaskId>` of tasks waiting to run. The
//!   scheduler pops from the front and pushes preempted tasks to the back.
//! - **Idle task**: a special task that runs `hlt` in a loop when no other
//!   tasks are ready. It is never placed in the ready queue — it's used as
//!   a fallback when the queue is empty.
//! - **Bootstrap task**: task 0, representing the initial execution context
//!   (kmain's continuation after `sched::init()`). Uses the Limine-provided
//!   boot stack, so it has no allocated stack to free.
//!
//! ## Context switch flow
//!
//! When the timer interrupt fires while task A is running:
//!
//! ```text
//! Timer fires → CPU pushes interrupt frame onto A's stack
//!   → isr_common saves all GPRs → exception_handler → irq_handler(0)
//!   → send EOI → schedule()
//!   → schedule picks task B, calls switch_context(&mut A.ctx, &B.ctx)
//!   → switch_context saves A's callee-saved regs, loads B's RSP
//!   → switch_context pops B's callee-saved regs, ret's into B's schedule()
//!   → B's schedule() returns → B's irq_handler → B's exception_handler
//!   → isr_common restores B's GPRs → iretq → B resumes
//! ```
//!
//! ## Lock management
//!
//! The scheduler state is behind an `InterruptMutex`. The lock is acquired
//! to make scheduling decisions and released BEFORE `switch_context` is
//! called. Raw pointers to the task contexts are extracted while the lock is
//! held and used after release — this is safe because interrupts are disabled
//! (so nothing else can modify the scheduler) and we're single-core.
//!
//! ## Task cleanup
//!
//! When a task's entry function returns, it falls through to `task_exit`,
//! which marks it as Dead and calls `schedule()`. Dead tasks are cleaned
//! up (stack frames freed, slot set to None) during subsequent `schedule()`
//! calls, but only if they're not the currently-executing task (we can't
//! free the stack we're standing on).

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::mm;
use crate::mm::addr::PhysAddr;
use crate::println;
use crate::sync::InterruptMutex;

pub mod task;

#[cfg(target_arch = "x86_64")]
use crate::process::ProcessId;
use crate::arch::context::{self, TaskContext};
use task::{Task, TaskId, TaskState, TOTAL_STACK_PAGES};

/// Whether the scheduler has been initialized. Checked by the timer interrupt
/// handler to avoid calling `schedule()` before the scheduler exists.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// The global scheduler instance, protected by an interrupt-disabling mutex.
///
/// Starts as `None` and is initialized by `init()`. All public functions in
/// this module acquire this lock to read or modify scheduler state.
static SCHEDULER: InterruptMutex<Option<Scheduler>> = InterruptMutex::new(None);

/// The scheduler's internal state.
struct Scheduler {
    /// All tasks, indexed by `TaskId`. Slots are `None` for tasks that have
    /// been cleaned up (dead and stack freed). New tasks may reuse empty slots.
    tasks: Vec<Option<Task>>,

    /// FIFO queue of task IDs waiting to run. Tasks are pushed to the back
    /// when preempted and popped from the front when selected.
    ready_queue: VecDeque<TaskId>,

    /// ID of the currently executing task.
    current_id: TaskId,

    /// ID of the idle task (never in the ready queue — used as fallback).
    idle_id: TaskId,

    /// Next task ID to allocate. Monotonically increasing.
    next_id: TaskId,
}

impl Scheduler {
    /// Allocate a new task ID, either by reusing an empty slot or expanding
    /// the task vector.
    fn alloc_id(&mut self) -> TaskId {
        // Try to reuse an empty slot from a previously cleaned-up task
        for (i, slot) in self.tasks.iter().enumerate() {
            if slot.is_none() && i >= self.next_id.min(self.tasks.len()) {
                return i;
            }
        }
        // No empty slots — allocate at the end
        let id = self.tasks.len();
        self.tasks.push(None);
        id
    }
}

// ========================================================================
// Public API
// ========================================================================

/// Initialize the scheduler.
///
/// Creates the bootstrap task (representing the current execution context)
/// and the idle task. After this call, `spawn()` can be used to create new
/// tasks and the timer interrupt will call `schedule()` for preemption.
///
/// Must be called exactly once, after the heap is initialized (we use Vec
/// and VecDeque) and before interrupts are enabled.
pub fn init() {
    let mut sched = Scheduler {
        tasks: Vec::new(),
        ready_queue: VecDeque::new(),
        current_id: 0,
        idle_id: 0,
        next_id: 0,
    };

    // --- Task 0: Bootstrap (the current execution context) ---
    //
    // This task represents kmain's continuation. It doesn't need a stack
    // allocation — it's already running on the Limine-provided boot stack.
    // Its context starts empty and will be filled in by the first call to
    // switch_context (when the timer preempts it).
    let bootstrap_id = 0;
    sched.tasks.push(Some(Task {
        id: bootstrap_id,
        name: String::from("main"),
        state: TaskState::Running,
        context: TaskContext::empty(),
        stack_phys_base: None,
        // Ring-3 / Linux-thread state, x86_64 only — see `task::Task`.
        #[cfg(target_arch = "x86_64")]
        kernel_stack_top: 0, // Bootstrap uses Limine's stack; never returns from ring 3
        #[cfg(target_arch = "x86_64")]
        process_id: ProcessId::KERNEL,
        #[cfg(target_arch = "x86_64")]
        fs_base: 0,
        #[cfg(target_arch = "x86_64")]
        clone_entry: None,
        #[cfg(target_arch = "x86_64")]
        clear_child_tid: 0,
    }));
    sched.current_id = bootstrap_id;
    sched.next_id = 1;

    // --- Task 1: Idle task ---
    //
    // Runs when no other tasks are ready. Executes `hlt` in a loop with
    // interrupts enabled so the timer can wake the CPU and preempt it.
    // Never placed in the ready queue — only selected as a fallback.
    let idle_id = create_task(&mut sched, "idle", idle_entry);
    sched.idle_id = idle_id;
    // Remove idle from ready queue (create_task adds it)
    sched.ready_queue.retain(|&id| id != idle_id);

    println!("Scheduler initialized (bootstrap task 0, idle task {})", idle_id);

    *SCHEDULER.lock() = Some(sched);
    INITIALIZED.store(true, Ordering::Release);
}

/// Check whether the scheduler has been initialized.
///
/// The timer interrupt handler checks this before calling `schedule()` so
/// that timer ticks during early boot (before the scheduler exists) are
/// handled gracefully.
pub fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::Acquire)
}

/// Spawn a new task with the given name and entry function.
///
/// Allocates a kernel stack, sets up the initial context so it looks like
/// `switch_context` just saved the task's state, and places the task in
/// the ready queue.
///
/// Returns the new task's ID.
pub fn spawn(name: &str, entry: fn()) -> TaskId {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("Scheduler not initialized");
    let id = create_task(sched, name, entry);
    println!("  Spawned task {} (\"{}\")", id, name);
    id
}

#[cfg(target_arch = "x86_64")]
/// Set the current task's `IA32_FS_BASE` (Linux TLS) and apply it immediately.
///
/// Called by `arch_prctl(ARCH_SET_FS)` (Phase 5.1): records the base on the task
/// so the context switch restores it, and writes the MSR now so `%fs`-relative
/// accesses work for the rest of this time slice without waiting for a switch.
pub fn set_current_fs_base(fs_base: u64) {
    {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("Scheduler not initialized");
        let id = sched.current_id;
        if let Some(Some(task)) = sched.tasks.get_mut(id) {
            task.fs_base = fs_base;
        }
    }
    #[cfg(target_arch = "x86_64")]
    crate::arch::x86_64::syscall::write_fs_base(fs_base);
}

#[cfg(target_arch = "x86_64")]
/// Set another task's FS base (Linux TLS) by id — used by `clone(CLONE_SETTLS)`
/// to seed a freshly-created thread's `%fs` before it first runs.
pub fn set_task_fs_base(id: TaskId, fs_base: u64) {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("Scheduler not initialized");
    if let Some(Some(task)) = sched.tasks.get_mut(id) {
        task.fs_base = fs_base;
    }
}

#[cfg(target_arch = "x86_64")]
/// Record a `clone`-created thread's ring-3 entry `(rip, rsp)` (Phase 5.3). The
/// [`thread_trampoline`](crate::linux::thread::thread_trampoline) reads it back.
pub fn set_task_clone_entry(id: TaskId, rip: u64, rsp: u64) {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("Scheduler not initialized");
    if let Some(Some(task)) = sched.tasks.get_mut(id) {
        task.clone_entry = Some((rip, rsp));
    }
}

#[cfg(target_arch = "x86_64")]
/// Set a task's `CLONE_CHILD_CLEARTID` futex word address (Phase 5.3).
pub fn set_task_clear_child_tid(id: TaskId, addr: u64) {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("Scheduler not initialized");
    if let Some(Some(task)) = sched.tasks.get_mut(id) {
        task.clear_child_tid = addr;
    }
}

#[cfg(target_arch = "x86_64")]
/// Read the current task's `clone` entry `(rip, rsp)`, if it was created by
/// `clone` (used by the thread trampoline to enter ring 3).
pub fn current_clone_entry() -> Option<(u64, u64)> {
    let guard = SCHEDULER.lock();
    let sched = guard.as_ref().expect("Scheduler not initialized");
    sched
        .tasks
        .get(sched.current_id)
        .and_then(|t| t.as_ref())
        .and_then(|t| t.clone_entry)
}

#[cfg(target_arch = "x86_64")]
/// Read (and clear) the current task's `clear_child_tid` address — called on
/// thread exit so the kernel can zero + futex-wake the join word exactly once.
pub fn take_current_clear_child_tid() -> u64 {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("Scheduler not initialized");
    if let Some(Some(task)) = sched.tasks.get_mut(sched.current_id) {
        let addr = task.clear_child_tid;
        task.clear_child_tid = 0;
        addr
    } else {
        0
    }
}

#[cfg(target_arch = "x86_64")]
/// Set the current task's `clear_child_tid` (Linux `set_tid_address`).
pub fn set_current_clear_child_tid(addr: u64) {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("Scheduler not initialized");
    if let Some(Some(task)) = sched.tasks.get_mut(sched.current_id) {
        task.clear_child_tid = addr;
    }
}

#[cfg(target_arch = "x86_64")]
/// Read the current task's `IA32_FS_BASE` value (for `arch_prctl(ARCH_GET_FS)`).
pub fn current_fs_base() -> u64 {
    let guard = SCHEDULER.lock();
    let sched = guard.as_ref().expect("Scheduler not initialized");
    sched
        .tasks
        .get(sched.current_id)
        .and_then(|t| t.as_ref())
        .map(|t| t.fs_base)
        .unwrap_or(0)
}

/// Preemptive scheduling entry point — called from the timer interrupt.
///
/// Picks the next Ready task from the round-robin queue, switches to it
/// via `switch_context`. If no other tasks are ready, switches to idle
/// (or stays on the current task if it's idle).
///
/// Must be called with interrupts disabled (which is guaranteed when
/// called from an interrupt handler).
pub fn schedule() {
    // Extract the context pointers while holding the scheduler lock,
    // then release the lock before doing the actual context switch.
    // We release the lock so the new task doesn't inherit a held lock
    // (which would cause deadlock if it tries to acquire the scheduler
    // lock, e.g., when it gets preempted later).
    let switch_info = {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("Scheduler not initialized");

        let current_id = sched.current_id;

        // Clean up dead tasks (free their stacks). We can only clean up
        // tasks that aren't the current one — we might be on their stack!
        // Cleanup is done here (inside the lock) because deallocate_frame
        // acquires its own InterruptMutex, and since interrupts are already
        // disabled, the nested lock works correctly.
        cleanup_dead_tasks(sched, current_id);

        // If the current task is still Running (preempted by timer),
        // move it to Ready and put it back in the queue.
        // If it's Dead (called exit), don't re-queue it.
        let current_task = sched.tasks[current_id].as_mut().unwrap();
        if current_task.state == TaskState::Running {
            current_task.state = TaskState::Ready;
            // Don't put idle back in the ready queue — it's the fallback only
            if current_id != sched.idle_id {
                sched.ready_queue.push_back(current_id);
            }
        }

        // Pick the next task: pop from ready queue, fall back to idle.
        let next_id = sched.ready_queue.pop_front()
            .unwrap_or(sched.idle_id);

        // If we'd switch to ourselves, just mark Running and return.
        if next_id == current_id {
            sched.tasks[current_id].as_mut().unwrap().state = TaskState::Running;
            return;
        }

        // Mark the next task as Running and update current_id.
        sched.tasks[next_id].as_mut().unwrap().state = TaskState::Running;
        sched.current_id = next_id;

        // Re-establish KERNEL_GS_BASE = &PER_CPU on EVERY switch — including
        // switches to kernel-only tasks (idle, bootstrap) with no ring-3 kernel
        // stack. Skipping it for those leaves KERNEL_GS_BASE stale after a
        // ring-3 task blocks mid-syscall, and the next ring-3 syscall then loads
        // RSP=0 and double-faults. See `syscall::refresh_kernel_gs_base`.
        #[cfg(target_arch = "x86_64")]
        crate::arch::x86_64::syscall::refresh_kernel_gs_base();

        // Restore the next task's FS base (Linux TLS, Phase 5.1). Like the GS
        // base, IA32_FS_BASE is a global register `switch_context` does not save,
        // so a Linux thread's `%fs`-relative TLS accesses would use whatever base
        // the previous task left behind. Reload it from the task on every switch.
        //
        // aarch64's analog is TPIDR_EL0, which arrives with the EL0 port.
        #[cfg(target_arch = "x86_64")]
        crate::arch::x86_64::syscall::write_fs_base(
            sched.tasks[next_id].as_ref().unwrap().fs_base,
        );

        #[cfg(target_arch = "x86_64")]
        {
            // Update TSS.RSP0 and PerCpu.kernel_stack_top for the new task.
            // This must happen BEFORE the context switch so that if the new task
            // is in ring 3 and gets interrupted (or does a syscall), the CPU uses
            // the correct kernel stack. On a single-core system with interrupts
            // disabled, there's no race — the values are set before the switch.
            // (For kernel tasks with kernel_stack_top == 0 we leave the stack alone;
            // they never enter `syscall_entry`, and the next ring-3 switch sets it.)
            let next_stack_top = sched.tasks[next_id].as_ref().unwrap().kernel_stack_top;
            if next_stack_top != 0 {
                #[cfg(target_arch = "x86_64")]
                {
                    crate::arch::x86_64::gdt::set_tss_rsp0(next_stack_top);
                    crate::arch::x86_64::syscall::set_kernel_stack(next_stack_top);
                }
            }
        }

        #[cfg(target_arch = "x86_64")]
        {
            // Switch CR3 if the next task belongs to a different process.
            // Changing CR3 flushes the user-half TLB entries (kernel-half entries
            // are shared across all address spaces via the same PML4 upper-half
            // entries, but the CPU flushes everything on CR3 write). We skip the
            // CR3 write if both tasks are in the same process (same address space)
            // or if both are in the kernel process (PID 0, which has no separate
            // address space).
            let current_pid = sched.tasks[current_id].as_ref().unwrap().process_id;
            let next_pid = sched.tasks[next_id].as_ref().unwrap().process_id;
            if current_pid != next_pid {
                if let Some(pml4_phys) = crate::process::process_pml4(next_pid) {
                    #[cfg(target_arch = "x86_64")]
                    unsafe { crate::arch::x86_64::cpu::write_cr3(pml4_phys); }
                }
            }
        }

        #[cfg(target_arch = "aarch64")]
        {
            // Re-point TPIDR_EL1 at the per-CPU block and describe the incoming task.
            // The aarch64 analog of `refresh_kernel_gs_base` above, in the same place
            // and for the same reason: TPIDR_EL1 is a global register that
            // `switch_context` does not save, so the only way it cannot go stale is to
            // rewrite it on every switch rather than on the switches that seem to need
            // it. That "seem to need it" is exactly what produced the Phase 4.5 stale
            // GS-base bug.
            let (top, limit) = sched.tasks[next_id]
                .as_ref()
                .unwrap()
                .kernel_stack_bounds()
                .unwrap_or((0, 0));
            // SAFETY: interrupts are masked for the whole of `schedule()`, which is
            // what `on_context_switch` requires so that an exception cannot observe
            // the block half-updated.
            unsafe {
                crate::arch::aarch64::percpu::on_context_switch(next_id as u64, top, limit);
            }
        }

        // Get raw pointers to the task contexts. These point into the Vec's
        // buffer, which won't be reallocated because:
        // 1. Interrupts are disabled — no other code can push to the Vec.
        // 2. We're about to drop the lock and immediately use the pointers.
        let old_ctx = &mut sched.tasks[current_id].as_mut().unwrap().context
            as *mut TaskContext;
        let new_ctx = &sched.tasks[next_id].as_ref().unwrap().context
            as *const TaskContext;

        Some((old_ctx, new_ctx))
        // Guard dropped here — lock released
    };

    if let Some((old_ctx, new_ctx)) = switch_info {
        // SAFETY: Both pointers are valid (checked above), point to stable
        // memory (Vec buffer with interrupts disabled), and the new task's
        // stack contains valid saved registers.
        unsafe { context::switch_context(old_ctx, new_ctx); }
    }

    // When this task resumes (switched back by a future schedule() call),
    // execution continues here. The lock has been released, and interrupts
    // are still disabled (the caller — timer handler or yield_now — will
    // handle re-enabling them).
}

/// Voluntarily yield the current task's time slice.
///
/// The current task is moved to the back of the ready queue and the next
/// task is scheduled. Use this for cooperative multitasking (e.g., a task
/// that has no work to do right now).
pub fn yield_now() {
    // Disable interrupts for the scheduling critical section.
    // schedule() expects interrupts to be disabled on entry.
    crate::arch::irq::disable();

    schedule();

    // Re-enable interrupts after returning from schedule.
    // (When the task resumes, we're back here with interrupts still disabled.)
    crate::arch::irq::enable();
}

/// Mark the current task as Dead and switch to the next task.
///
/// Called by `task_exit` (the trampoline) when a task's entry function
/// returns. This function never returns — the dead task will never be
/// scheduled again.
pub fn exit_current_task() -> ! {
    crate::arch::irq::disable();

    {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("Scheduler not initialized");
        let current = sched.tasks[sched.current_id].as_mut().unwrap();
        current.state = TaskState::Dead;
    }

    schedule();

    // schedule() switched to a different task. If we get here, something
    // is very wrong — a dead task was resumed.
    panic!("exit_current_task: dead task was resumed");
}

/// Get the current task's ID.
pub fn current_task_id() -> TaskId {
    SCHEDULER.lock()
        .as_ref()
        .expect("Scheduler not initialized")
        .current_id
}

#[cfg(target_arch = "x86_64")]
/// Get the current task's process ID.
pub fn current_process_id() -> ProcessId {
    let guard = SCHEDULER.lock();
    let sched = guard.as_ref().expect("Scheduler not initialized");
    sched.tasks[sched.current_id].as_ref().unwrap().process_id
}

#[cfg(target_arch = "x86_64")]
/// Spawn a new task within a specific process.
///
/// Like `spawn()`, but assigns the task to the given process instead of
/// the kernel process (PID 0). Also adds the task ID to the process's
/// task list so the process tracks ownership.
pub fn spawn_in_process(name: &str, entry: fn(), pid: ProcessId) -> TaskId {
    let id = {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("Scheduler not initialized");
        let id = create_task(sched, name, entry);
        // Override the default kernel PID with the target process
        sched.tasks[id].as_mut().unwrap().process_id = pid;
        id
    };
    crate::process::add_task_to_process(pid, id);
    println!("  Spawned task {} (\"{}\") in {}", id, name, pid);
    id
}

/// Get the number of live tasks (non-None, non-Dead).
pub fn active_task_count() -> usize {
    let guard = SCHEDULER.lock();
    let sched = guard.as_ref().expect("Scheduler not initialized");
    sched.tasks.iter()
        .filter(|t| t.as_ref().is_some_and(|task| task.state != TaskState::Dead))
        .count()
}

/// Get the total number of tasks ever created (including dead/cleaned-up).
pub fn total_task_slots() -> usize {
    SCHEDULER.lock()
        .as_ref()
        .expect("Scheduler not initialized")
        .tasks
        .len()
}

/// Info about a task, returned by `task_list()` for display purposes.
/// Copied out of the scheduler lock so callers can format at leisure.
pub struct TaskInfo {
    pub id: TaskId,
    pub name: String,
    pub state: TaskState,
}

/// Get a snapshot of all live tasks (non-None slots).
///
/// Returns a Vec of `TaskInfo` structs that can be printed outside the
/// scheduler lock. Used by the shell's `tasks` command.
pub fn task_list() -> Vec<TaskInfo> {
    let guard = SCHEDULER.lock();
    let sched = guard.as_ref().expect("Scheduler not initialized");
    sched.tasks.iter()
        .filter_map(|slot| {
            slot.as_ref().map(|task| TaskInfo {
                id: task.id,
                name: task.name.clone(),
                state: task.state,
            })
        })
        .collect()
}

/// Kill a task by ID, marking it as Dead.
///
/// The task's stack will be freed on the next scheduling pass. Returns
/// `true` if the task was found and killed, `false` if the ID is invalid
/// or the task is already dead/cleaned-up.
///
/// Cannot kill the idle task or the currently running task.
pub fn kill_task(id: TaskId) -> bool {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("Scheduler not initialized");

    if id == sched.idle_id || id == sched.current_id {
        return false;
    }

    if let Some(Some(task)) = sched.tasks.get_mut(id) {
        if task.state == TaskState::Dead {
            return false;
        }
        task.state = TaskState::Dead;
        // Remove from ready queue if present
        sched.ready_queue.retain(|&tid| tid != id);
        true
    } else {
        false
    }
}

/// Block the current task until `wake_task()` is called with its ID.
///
/// Marks the current task as Blocked (so the scheduler won't put it in
/// the ready queue) and calls `schedule()` to switch to another task.
/// When another task (or an IRQ handler) calls `wake_task(id)`, the
/// blocked task is moved back to Ready and will be scheduled again.
///
/// Must NOT be called from an interrupt handler — only from task context.
pub fn block_current_task() {
    crate::arch::irq::disable();

    {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("Scheduler not initialized");
        let current = sched.tasks[sched.current_id].as_mut().unwrap();
        current.state = TaskState::Blocked;
    }

    schedule();

    // When we return here, the task has been woken up.
    crate::arch::irq::enable();
}

/// Wake a blocked task, moving it back to the Ready state.
///
/// If the task is Blocked, moves it to Ready and adds it to the ready
/// queue. Called from IRQ handlers (e.g., IRQ4 waking the shell task
/// when serial input arrives) or from other tasks.
///
/// Safe to call from interrupt context (acquires the scheduler lock
/// which disables interrupts).
pub fn wake_task(id: TaskId) {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("Scheduler not initialized");

    if let Some(Some(task)) = sched.tasks.get_mut(id) {
        if task.state == TaskState::Blocked {
            task.state = TaskState::Ready;
            sched.ready_queue.push_back(id);
        }
    }
}

// ========================================================================
// Internal helpers
// ========================================================================

/// Create a new task with an allocated stack and initial context.
///
/// The task is added to the scheduler's task Vec and placed in the ready
/// queue. Returns the new task's ID.
fn create_task(sched: &mut Scheduler, name: &str, entry: fn()) -> TaskId {
    // Allocate a task ID (reuse empty slot or extend the Vec).
    let id = sched.alloc_id();

    // Allocate contiguous physical frames for the kernel stack.
    // TOTAL_STACK_PAGES = PADDING_PAGES + STACK_PAGES.
    let phys_base = mm::frame::allocate_contiguous_frames(TOTAL_STACK_PAGES)
        .expect("Failed to allocate stack for new task");

    // The first frame of the stack allocation is a padding page. It is *reserved*
    // (the allocator owns it, so nothing else is placed there) but it is not, at
    // present, an unmapped guard page.
    //
    // This used to call `unmap_page()` on the padding page's HHDM address, claiming
    // "true stack overflow detection". That never worked, and was actively harmful.
    // Kernel stacks live in the HHDM, which Limine maps with 2 MiB/1 GiB huge pages,
    // so the 4 KiB walk reached a huge directory entry and treated the data frame
    // beneath it as a page table — clearing eight bytes of unrelated memory instead
    // of unmapping anything. The page remained mapped throughout, so a stack overflow
    // silently ran into the padding page exactly as it would have without the call.
    // (Phase 7.1 added an assertion to `ensure_table`, which is what surfaced this.)
    //
    // Reserving the padding page still bounds the damage of a small overflow, so that
    // behaviour is kept. Restoring real detection requires kernel stacks to live in a
    // dedicated virtual window built from 4 KiB mappings — the same fix the kernel
    // heap received — rather than in the HHDM. Tracked as follow-up work; it belongs
    // with the scheduler, not with the MMU port.

    // The usable stack starts after the padding page(s) and extends to the top.
    // Stack grows downward, so the initial SP points to the top (highest address).
    let stack_top_phys = PhysAddr::new(
        phys_base.as_u64() + (TOTAL_STACK_PAGES as u64) * mm::PAGE_SIZE
    );
    let stack_top_virt = stack_top_phys.to_virt();

    // Set up the initial stack frame to look like switch_context just saved
    // the task's state. See context.rs for the expected stack layout.
    // SAFETY: `stack_top_virt` is the top of freshly allocated, mapped stack frames
    // owned by this task; nothing else refers to them yet.
    let initial_sp = unsafe { context::setup_initial_stack(stack_top_virt.as_u64(), entry) };

    let task = Task {
        id,
        name: String::from(name),
        state: TaskState::Ready,
        context: TaskContext::new(initial_sp),
        stack_phys_base: Some(phys_base),
        // Ring-3 / Linux-thread state, x86_64 only — see `task::Task`.
        #[cfg(target_arch = "x86_64")]
        kernel_stack_top: stack_top_virt.as_u64(),
        #[cfg(target_arch = "x86_64")]
        process_id: ProcessId::KERNEL, // Default to kernel process; caller can override
        #[cfg(target_arch = "x86_64")]
        fs_base: 0, // set by arch_prctl(ARCH_SET_FS) for Linux TLS (Phase 5.1)
        #[cfg(target_arch = "x86_64")]
        clone_entry: None, // set by Linux clone() for thread tasks (Phase 5.3)
        #[cfg(target_arch = "x86_64")]
        clear_child_tid: 0,
    };

    // Place the task in its slot (either reusing an empty one or the new end)
    if id < sched.tasks.len() {
        sched.tasks[id] = Some(task);
    } else {
        sched.tasks.push(Some(task));
    }

    // Add to the ready queue
    sched.ready_queue.push_back(id);

    // Update next_id if needed
    if id >= sched.next_id {
        sched.next_id = id + 1;
    }

    id
}

/// Set up the initial stack frame for a new task.
///
/// Builds a stack frame that mimics the state after `switch_context` has
/// pushed all callee-saved registers. When `switch_context` later "restores"
/// from this frame, it will:
/// 1. Pop r15-r12, rbp, rbx (r12 gets the entry function address)

/// Clean up dead tasks by freeing their stacks and clearing their slots.
///
/// Only cleans up tasks that are Dead AND not the currently executing task
/// (we can't free the stack we're standing on — it'll be cleaned up next
/// time schedule() runs when a different task is current).
fn cleanup_dead_tasks(sched: &mut Scheduler, current_id: TaskId) {
    for task_opt in sched.tasks.iter_mut() {
        let should_clean = task_opt.as_ref().is_some_and(|task| {
            task.state == TaskState::Dead && task.id != current_id
        });
        if should_clean {
            let task = task_opt.take().unwrap();
            if let Some(phys_base) = task.stack_phys_base {
                // No guard-page re-map is needed here: `create_task` no longer unmaps
                // the padding page (see the comment there — the unmap never worked and
                // corrupted memory). The HHDM mapping was never actually disturbed, so
                // the frames can go straight back to the allocator.

                // Free all stack frames (padding page + usable pages).
                for i in 0..TOTAL_STACK_PAGES {
                    mm::frame::deallocate_frame(
                        PhysAddr::new(phys_base.as_u64() + i as u64 * mm::PAGE_SIZE)
                    );
                }
            }
        }
    }
}

/// Entry function for the idle task.
///
/// Runs in an infinite loop, halting the CPU between timer interrupts
/// (`hlt` on x86_64, `wfi` on aarch64 — both resume on the next interrupt).
/// Interrupts must be enabled (they are — `task_bootstrap` unmasks them
/// before jumping to the entry function) so the timer can wake the CPU
/// and preempt the idle task when a real task becomes ready.
///
/// If interrupts were ever masked here the halt would never return, so the
/// idle task would wedge the whole system rather than merely spin.
fn idle_entry() {
    loop {
        crate::arch::irq::halt();
    }
}
