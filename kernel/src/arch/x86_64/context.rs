//! # x86_64 context switching
//!
//! Saves the outgoing task's callee-saved registers, swaps stacks, restores the
//! incoming task's, and `ret`s into wherever it left off. The aarch64 counterpart is
//! [`crate::arch::aarch64::context`]; both are reached through
//! [`crate::arch::context`].
//!
//! ## Why only callee-saved registers?
//!
//! The System V ABI splits registers into caller-saved (rax, rcx, rdx, rsi, rdi,
//! r8-r11) and callee-saved (rbx, rbp, r12-r15). `switch_context` is called as an
//! ordinary function, so the compiler has already spilled anything caller-saved it
//! needed; only the callee-saved set has to survive.
//!
//! ## Stack frame layout
//!
//! After `switch_context` pushes registers:
//!
//! ```text
//! [higher addresses]
//!   return address   ← popped by `ret`
//!   rbx              ← pushed first, popped last
//!   rbp
//!   r12
//!   r13
//!   r14
//!   r15              ← pushed last, popped first
//! [lower addresses]  ← RSP stored in TaskContext points here
//! ```

/// Saved CPU context for a task: just the stack pointer.
///
/// Everything else lives *on* that stack, pushed by [`switch_context`].
#[repr(C)]
pub struct TaskContext {
    /// Stack pointer to this task's saved callee-saved registers.
    pub rsp: u64,
}

impl TaskContext {
    /// A context whose saved stack pointer is `stack_pointer`.
    ///
    /// The pointer must address a frame built by [`setup_initial_stack`] (for a task
    /// that has never run) or written by [`switch_context`].
    pub const fn new(stack_pointer: u64) -> Self {
        Self { rsp: stack_pointer }
    }

    /// An empty (zeroed) context.
    ///
    /// Used for the bootstrap task, which represents the execution context already
    /// running when the scheduler starts; its real context is written by the first
    /// [`switch_context`] that saves *out of* it.
    pub const fn empty() -> Self {
        Self { rsp: 0 }
    }
}


/// Switch CPU context from the current task to a new task.
///
/// Saves callee-saved registers onto the current stack, saves RSP into `old`,
/// loads RSP from `new`, restores callee-saved registers, and `ret`s — which
/// jumps to wherever the new task was when it last called `switch_context`
/// (or to `task_bootstrap` for new tasks).
///
/// Parameters arrive via System V ABI: `old` in RDI, `new` in RSI.
/// `naked_asm!` only allows `sym` and `const` operands — we rely on the
/// calling convention to place arguments in the correct registers.
///
/// # Safety
///
/// - Both pointers must be valid and properly aligned.
/// - The new task's stack must contain valid saved registers.
/// - Interrupts must be disabled (caller's responsibility).
#[unsafe(naked)]
pub unsafe extern "C" fn switch_context(
    _old: *mut TaskContext,
    _new: *const TaskContext,
) {
    core::arch::naked_asm!(
        // Save callee-saved registers onto the current (old) task's stack.
        // Caller-saved registers (rax, rcx, rdx, rsi, rdi, r8-r11) are the
        // caller's responsibility per the System V ABI — the compiler handles them.
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",

        // Save the current stack pointer into old->rsp (offset 0 of TaskContext).
        // RDI = first parameter = old: *mut TaskContext.
        "mov [rdi], rsp",

        // Load the new task's stack pointer from new->rsp (offset 0 of TaskContext).
        // RSI = second parameter = new: *const TaskContext.
        "mov rsp, [rsi]",

        // Restore callee-saved registers from the new task's stack (reverse order).
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",

        // Return to the new task's saved return address.
        // For a resumed task: returns into schedule() → irq_handler → iretq.
        // For a new task: returns into task_bootstrap.
        "ret",
    );
}

/// Bootstrap function for newly created tasks.
///
/// When `switch_context` switches to a new task for the first time, it
/// "returns" into this function (the address was placed on the initial stack).
/// By this point, `switch_context` has already restored callee-saved registers
/// from the initial stack frame, so r12 contains the task's entry function
/// address (it was placed in the r12 slot during initial stack setup).
///
/// Sequence:
/// 1. Enable interrupts — the scheduler runs with interrupts disabled, but
///    the task needs them enabled for timer preemption to work.
/// 2. Call the entry function (address in r12).
/// 3. If the entry function returns, call `task_exit` to clean up.
#[unsafe(naked)]
pub unsafe extern "C" fn task_bootstrap() {
    core::arch::naked_asm!(
        // Enable interrupts. We entered from the scheduler which had interrupts
        // disabled (either from the timer interrupt handler or explicit cli).
        // The task needs interrupts enabled so the timer can preempt it.
        "sti",

        // Call the task's entry function. Its address was stored in the r12
        // callee-saved register slot of the initial stack frame and was
        // restored by switch_context's `pop r12`.
        //
        // Stack alignment: after switch_context's `ret`, RSP = stack_top
        // (16-byte aligned). `call r12` pushes an 8-byte return address,
        // so the entry function sees RSP ≡ 8 mod 16 — correct per System V ABI.
        "call r12",

        // If the entry function returns normally, fall through to task_exit.
        // task_exit marks the task as Dead and calls schedule() to switch
        // to the next available task. It never returns.
        "call {exit}",

        // Unreachable — task_exit diverges. UD2 triggers an Invalid Opcode
        // exception (#UD) if we somehow get here, making the bug obvious
        // instead of silently corrupting state.
        "ud2",

        exit = sym task_exit,
    );
}

/// Called when a task's entry function returns.
///
/// This is the "trampoline" that catches tasks returning from their entry
/// point. Without it, a returning task would `ret` to a garbage address
/// and crash. Instead, we cleanly mark the task as Dead and switch to the
/// next ready task.
///
/// This function never returns — `exit_current_task` calls `schedule()`,
/// which switches to a different task and never comes back to a dead one.
extern "C" fn task_exit() -> ! {
    crate::sched::exit_current_task();
}

/// Build the initial stack frame for a new task.
///
/// Lays out exactly what [`switch_context`]'s restore sequence expects, so the first
/// switch to this task `ret`s into [`task_bootstrap`] with the entry function in r12.
///
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
///
/// # Safety
///
/// `stack_top` must be the 16-byte-aligned top of at least 56 bytes of writable,
/// mapped stack owned by the new task.
pub unsafe fn setup_initial_stack(stack_top: u64, entry: fn()) -> u64 {
    let mut sp = stack_top;
    let mut push = |v: u64| {
        sp -= 8;
        // SAFETY: caller guarantees writable mapped stack below `stack_top`.
        unsafe { core::ptr::write(sp as *mut u64, v) };
    };

    push(task_bootstrap as *const () as u64); // return address for `ret`
    push(0); // rbx
    push(0); // rbp (zero terminates stack traces)
    push(entry as *const () as u64); // r12 — task_bootstrap does `call r12`
    push(0); // r13
    push(0); // r14
    push(0); // r15

    sp
}
