//! # aarch64 context switching
//!
//! The aarch64 counterpart to [`crate::arch::x86_64::context`]: save the outgoing
//! task's callee-saved state, swap stacks, restore the incoming task's, and return
//! into wherever it left off.
//!
//! ## What has to be saved
//!
//! AAPCS64 splits the general-purpose registers into caller-saved (`x0`-`x18`) and
//! callee-saved (**`x19`-`x28`**, plus `x29` the frame pointer and `x30` the link
//! register). `switch_context` is called as an ordinary function, so the compiler has
//! already spilled anything caller-saved it cared about; only the callee-saved set
//! must survive the switch.
//!
//! The ABI *also* makes the low 64 bits of `v8`-`v15` callee-saved. Those are not
//! saved here, and that is sound only because the kernel is built for
//! **`aarch64-unknown-none-softfloat`** and emits no vector instructions at all (Phase
//! 7.2 verified: 301 vector instructions in the hardfloat image, zero in the softfloat
//! one). If the kernel ever returns to a hardfloat target, this file must grow a
//! 64-byte FP save area or context switches will silently corrupt floating-point state
//! across tasks — with no fault to point at it.
//!
//! ## Returning is `ret`, and `ret` reads `x30`
//!
//! x86's `switch_context` ends in `ret`, which *pops* a return address off the stack.
//! aarch64's `ret` branches to whatever is in `x30`. Since `x30` is part of the
//! callee-saved set restored from the incoming task's stack, the effect is the same —
//! but it means the initial stack frame of a *new* task must place the bootstrap
//! address in the `x30` slot rather than at a "return address" position.
//!
//! ## Stack frame layout
//!
//! `switch_context` pushes twelve registers (96 bytes, which keeps SP 16-byte aligned
//! as the ABI requires):
//!
//! ```text
//! [higher addresses]
//!   x30 (link register)   ← `ret` branches here
//!   x29 (frame pointer)
//!   x28  x27  x26  x25
//!   x24  x23  x22  x21
//!   x20  x19              ← SP stored in TaskContext points here
//! [lower addresses]
//! ```

use core::arch::naked_asm;

/// Saved CPU context for a task: just the stack pointer.
///
/// Everything else lives *on* that stack, pushed by [`switch_context`]. The struct is
/// `#[repr(C)]` because the assembly loads and stores through it by offset.
#[repr(C)]
pub struct TaskContext {
    /// Stack pointer to this task's saved callee-saved registers.
    pub sp: u64,
}

impl TaskContext {
    /// A context whose saved stack pointer is `stack_pointer`.
    ///
    /// The pointer must address a frame built by [`setup_initial_stack`] (for a task
    /// that has never run) or written by [`switch_context`].
    pub const fn new(stack_pointer: u64) -> Self {
        Self { sp: stack_pointer }
    }

    /// An empty (zeroed) context.
    ///
    /// Used for the bootstrap task, which represents the execution context already
    /// running when the scheduler starts; its real context is written by the first
    /// [`switch_context`] that saves *out of* it.
    pub const fn empty() -> Self {
        Self { sp: 0 }
    }
}

/// Switch CPU context from the current task to another.
///
/// Saves the callee-saved registers onto the current stack, stores SP into `old`,
/// loads SP from `new`, restores that task's registers, and `ret`s into wherever it
/// was when it last switched out — or into [`task_bootstrap`] for a task that has
/// never run.
///
/// Arguments arrive per AAPCS64: `old` in `x0`, `new` in `x1`.
///
/// # Safety
///
/// - Both pointers must be valid, aligned, and distinct.
/// - `new` must describe a stack containing a frame this function created, or one
///   built by [`setup_initial_stack`].
/// - Interrupts must be masked — the caller owns that.
#[unsafe(naked)]
pub unsafe extern "C" fn switch_context(_old: *mut TaskContext, _new: *const TaskContext) {
    naked_asm!(
        // Push the callee-saved set. The pre-index on the first `stp` moves SP down by
        // the whole 96 bytes at once, so SP never sits at an unaligned intermediate.
        "stp x19, x20, [sp, #-96]!",
        "stp x21, x22, [sp, #16]",
        "stp x23, x24, [sp, #32]",
        "stp x25, x26, [sp, #48]",
        "stp x27, x28, [sp, #64]",
        "stp x29, x30, [sp, #80]",

        // old->sp = current SP. SP cannot be an operand to `str`, so it goes via x2 —
        // which is caller-saved and therefore free to clobber.
        "mov x2, sp",
        "str x2, [x0]",

        // SP = new->sp.
        "ldr x2, [x1]",
        "mov sp, x2",

        // Restore the incoming task's callee-saved set.
        "ldp x21, x22, [sp, #16]",
        "ldp x23, x24, [sp, #32]",
        "ldp x25, x26, [sp, #48]",
        "ldp x27, x28, [sp, #64]",
        "ldp x29, x30, [sp, #80]",
        "ldp x19, x20, [sp], #96",

        // Branch to the restored x30: back into `schedule()` for a resumed task, or
        // into `task_bootstrap` for a new one.
        "ret",
    );
}

/// First code a newly created task executes.
///
/// [`switch_context`] "returns" here because [`setup_initial_stack`] placed this
/// address in the `x30` slot. By then the initial frame has been restored, so `x19`
/// holds the task's entry function.
///
/// 1. Unmask IRQs. The scheduler runs with them masked; a task needs them on or the
///    timer can never preempt it.
/// 2. Call the entry function.
/// 3. If it returns, fall into the exit path rather than branching to garbage.
#[unsafe(naked)]
pub unsafe extern "C" fn task_bootstrap() {
    naked_asm!(
        // The x86 analog is `sti`. `#2` is the I bit of the {D=8,A=4,I=2,F=1} mask.
        "msr DAIFClr, #2",

        // Call the entry function, whose address `switch_context` just restored into
        // x19. SP is 16-byte aligned here — `blr` pushes nothing, unlike x86's `call`,
        // so the callee sees the alignment the ABI requires.
        "blr x19",

        // Entry returned: clean the task up. Does not come back.
        "bl {exit}",

        // Unreachable. `brk #1` raises a synchronous exception the Phase 7.2 handler
        // decodes and reports, which is a far better outcome than running off into
        // whatever follows.
        "brk #1",

        exit = sym task_exit,
    );
}

/// Catches a task whose entry function returned.
///
/// Without it the task would `ret` to an undefined `x30` and jump somewhere arbitrary.
/// Marks the task Dead and schedules away; it is never switched back to, so this
/// diverges.
extern "C" fn task_exit() -> ! {
    crate::sched::exit_current_task();
}

/// Build the initial stack frame for a new task.
///
/// Lays out exactly what [`switch_context`]'s restore sequence expects, so the first
/// switch to this task lands in [`task_bootstrap`] with the entry function in `x19`.
///
/// Layout, from `stack_top` downward:
///
/// ```text
/// stack_top (16-byte aligned)
///   -0x08: x30 = task_bootstrap   ← `ret` branches here
///   -0x10: x29 = 0                (frame pointer; zero terminates backtraces)
///   -0x18: x28 = 0
///   -0x20: x27 = 0
///   -0x28: x26 = 0
///   -0x30: x25 = 0
///   -0x38: x24 = 0
///   -0x40: x23 = 0
///   -0x48: x22 = 0
///   -0x50: x21 = 0
///   -0x58: x20 = 0
///   -0x60: x19 = entry            ← `blr x19` calls this
///          ↑ initial SP points here (96 bytes below stack_top, still 16-aligned)
/// ```
///
/// # Safety
///
/// `stack_top` must be the 16-byte-aligned top of at least 96 bytes of writable,
/// mapped stack owned by the new task.
pub unsafe fn setup_initial_stack(stack_top: u64, entry: fn()) -> u64 {
    debug_assert!(stack_top % 16 == 0, "task stack top must be 16-byte aligned");

    let sp = stack_top - 96;
    // Slot index within the frame, matching `switch_context`'s offsets: x19 and x20 at
    // +0/+8, then pairs upward, with x30 last at +88.
    let slot = |i: usize| (sp + (i as u64) * 8) as *mut u64;

    // SAFETY: the caller guarantees 96 writable bytes below `stack_top`; each slot is
    // an 8-byte aligned word inside that range.
    unsafe {
        core::ptr::write(slot(0), entry as *const () as u64); // x19 = entry fn
        for i in 1..11 {
            core::ptr::write(slot(i), 0); // x20-x29
        }
        core::ptr::write(slot(11), task_bootstrap as *const () as u64); // x30
    }

    sp
}
