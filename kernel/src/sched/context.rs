//! # Context switching
//!
//! Implements the low-level CPU context switch and task bootstrap/exit
//! functions. These are the most architecture-specific and safety-critical
//! parts of the scheduler.
//!
//! ## How context switching works
//!
//! When the scheduler decides to switch from task A to task B:
//!
//! 1. `switch_context` is called with pointers to A's and B's `TaskContext`
//! 2. It pushes A's callee-saved registers (rbx, rbp, r12-r15) onto A's stack
//! 3. It saves A's RSP into A's `TaskContext.rsp`
//! 4. It loads B's RSP from B's `TaskContext.rsp`
//! 5. It pops B's callee-saved registers from B's stack
//! 6. It executes `ret`, which pops B's return address and jumps there
//!
//! For a previously-running task, `ret` goes back into `schedule()`, which
//! returns through the interrupt handler chain and eventually `iretq` restores
//! the full task state (including caller-saved registers and RFLAGS).
//!
//! For a new task, `ret` goes to `task_bootstrap`, which enables interrupts
//! and calls the task's entry function.
//!
//! ## Why only callee-saved registers?
//!
//! The System V ABI divides registers into caller-saved (rax, rcx, rdx, rsi,
//! rdi, r8-r11) and callee-saved (rbx, rbp, r12-r15). Since `switch_context`
//! is called as a regular function, the compiler has already saved any
//! caller-saved registers it needs. We only need to save the callee-saved
//! ones that the compiler expects to survive across the call.
//!
//! ## Stack frame layout
//!
//! After `switch_context` pushes registers, the stack looks like:
//!
//! ```text
//! [higher addresses]
//!   return address   ← will be popped by `ret`
//!   rbx              ← pushed first, popped last
//!   rbp
//!   r12
//!   r13
//!   r14
//!   r15              ← pushed last, popped first
//! [lower addresses]  ← RSP stored in TaskContext points here
//! ```

use super::task::TaskContext;

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
fn task_exit() -> ! {
    super::exit_current_task();
}
