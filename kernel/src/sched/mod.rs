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

pub mod context;
pub mod task;

use task::{Task, TaskContext, TaskId, TaskState, TOTAL_STACK_PAGES};

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
    #[cfg(target_arch = "x86_64")]
    crate::arch::x86_64::cpu::cli();

    schedule();

    // Re-enable interrupts after returning from schedule.
    // (When the task resumes, we're back here with interrupts still disabled.)
    #[cfg(target_arch = "x86_64")]
    crate::arch::x86_64::cpu::sti();
}

/// Mark the current task as Dead and switch to the next task.
///
/// Called by `task_exit` (the trampoline) when a task's entry function
/// returns. This function never returns — the dead task will never be
/// scheduled again.
pub fn exit_current_task() -> ! {
    #[cfg(target_arch = "x86_64")]
    crate::arch::x86_64::cpu::cli();

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

    // The usable stack starts after the padding page(s) and extends to the top.
    // Stack grows downward, so the initial RSP points to the top (highest address).
    let stack_top_phys = PhysAddr::new(
        phys_base.as_u64() + (TOTAL_STACK_PAGES as u64) * mm::PAGE_SIZE
    );
    let stack_top_virt = stack_top_phys.to_virt();

    // Set up the initial stack frame to look like switch_context just saved
    // the task's state. See context.rs for the expected stack layout.
    let initial_rsp = setup_initial_stack(stack_top_virt.as_u64(), entry);

    let task = Task {
        id,
        name: String::from(name),
        state: TaskState::Ready,
        context: TaskContext { rsp: initial_rsp },
        stack_phys_base: Some(phys_base),
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
/// 2. `ret` into `task_bootstrap` (the return address we place on the stack)
///
/// Returns the initial RSP value to store in the task's `TaskContext`.
///
/// Stack layout (addresses decreasing = stack growing down):
/// ```text
/// stack_top (16-byte aligned):
///   -0x08: task_bootstrap address   (return addr for switch_context's `ret`)
///   -0x10: 0                        (initial rbx)
///   -0x18: 0                        (initial rbp)
///   -0x20: entry function address   (initial r12 — called by task_bootstrap)
///   -0x28: 0                        (initial r13)
///   -0x30: 0                        (initial r14)
///   -0x38: 0                        (initial r15)
///          ↑ initial RSP points here
/// ```
fn setup_initial_stack(stack_top: u64, entry: fn()) -> u64 {
    let mut sp = stack_top;

    // The return address for switch_context's `ret` — enters task_bootstrap.
    sp -= 8;
    unsafe { *(sp as *mut u64) = context::task_bootstrap as *const () as u64; }

    // rbx = 0 (no specific initial value needed)
    sp -= 8;
    unsafe { *(sp as *mut u64) = 0; }

    // rbp = 0 (frame pointer — zero to terminate stack traces)
    sp -= 8;
    unsafe { *(sp as *mut u64) = 0; }

    // r12 = entry function address (task_bootstrap will `call r12`)
    sp -= 8;
    unsafe { *(sp as *mut u64) = entry as *const () as u64; }

    // r13 = 0
    sp -= 8;
    unsafe { *(sp as *mut u64) = 0; }

    // r14 = 0
    sp -= 8;
    unsafe { *(sp as *mut u64) = 0; }

    // r15 = 0
    sp -= 8;
    unsafe { *(sp as *mut u64) = 0; }

    sp
}

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
/// Runs in an infinite loop, halting the CPU between timer interrupts.
/// Interrupts must be enabled (they are — `task_bootstrap` calls `sti`
/// before jumping to the entry function) so the timer can wake the CPU
/// from `hlt` and preempt the idle task when a real task becomes ready.
fn idle_entry() {
    loop {
        #[cfg(target_arch = "x86_64")]
        crate::arch::x86_64::cpu::halt();

        #[cfg(not(target_arch = "x86_64"))]
        core::hint::spin_loop();
    }
}
