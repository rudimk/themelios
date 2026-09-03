//! # aarch64 exception vectors (`VBAR_EL1`)
//!
//! The aarch64 analog of the x86 IDT, and the first thing in the port that makes a
//! fault *reportable* rather than an unrecoverable silent hang.
//!
//! ## The vector table is a table of code, not of pointers
//!
//! x86's IDT holds 256 descriptors, each naming a handler address. aarch64's
//! `VBAR_EL1` instead points at 2 KiB of **executable code**: sixteen entries of 128
//! bytes each, and the CPU *branches directly into* the slot rather than loading an
//! address from it. So each slot has to be a stub small enough to fit in 32
//! instructions, which is why the ones below immediately jump to shared code.
//!
//! The sixteen slots are four groups of four, selected by where the exception came
//! from, and within a group by kind:
//!
//! ```text
//! offset  taken from                          kinds (in order)
//! ------  ---------------------------------   -----------------------------
//! 0x000   current EL, using SP_EL0            Sync, IRQ, FIQ, SError
//! 0x200   current EL, using SP_ELx            Sync, IRQ, FIQ, SError
//! 0x400   lower EL, AArch64                   Sync, IRQ, FIQ, SError
//! 0x600   lower EL, AArch32                   Sync, IRQ, FIQ, SError
//! ```
//!
//! The kernel runs at EL1 on `SP_EL1`, so **the `0x200` group is the one that fires**
//! in Phase 7 — but only because early boot makes it so. Limine hands off with
//! `SPSel = 0`, which would route exceptions to the `0x000` group *and* land them on an
//! uninitialised `SP_EL1`; `arch::aarch64::boot::use_sp_el1` establishes the invariant
//! this table assumes before anything can fault. The `0x400` group becomes live when EL0 lands with the
//! deferred ring-3 port; until then it is wired to the same reporting path so that an
//! unexpected entry is diagnosed instead of silently executing whatever follows.
//!
//! ## Why every slot is populated
//!
//! An unpopulated slot is not inert — the CPU branches into it regardless and executes
//! whatever bytes are there. Leaving one empty turns a diagnosable fault into
//! arbitrary code execution. All sixteen are therefore filled, even the ones that
//! "cannot" be taken.
//!
//! ## Syndrome decoding
//!
//! Synchronous exceptions carry a reason in `ESR_EL1.EC` (bits 31:26) and, for aborts,
//! a faulting address in `FAR_EL1`. The cases that matter in this phase:
//!
//! | `EC`   | Meaning                                     |
//! |--------|---------------------------------------------|
//! | `0x15` | `SVC` from AArch64 — the syscall path (EL0) |
//! | `0x20` | Instruction abort, lower EL                 |
//! | `0x21` | Instruction abort, current EL               |
//! | `0x24` | Data abort, lower EL                        |
//! | `0x25` | Data abort, current EL                      |
//! | `0x3C` | `BRK` — the software-breakpoint / `int3`    |

use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

use super::syscall;
use crate::println;

/// Number of general-purpose registers saved on exception entry (`x0`-`x30`).
const SAVED_GPRS: usize = 31;

/// Register state captured on exception entry, in the order the stub pushes it.
///
/// `#[repr(C)]` because the assembly stub builds this layout by hand; changing the
/// field order without changing the stub silently misattributes every register.
#[repr(C)]
#[derive(Debug)]
pub struct ExceptionFrame {
    /// `x0`-`x30`. `x30` is the link register.
    pub x: [u64; SAVED_GPRS],
    /// Exception Link Register — the address execution resumes at.
    pub elr: u64,
    /// Saved Program Status Register — the interrupted PSTATE.
    pub spsr: u64,
    /// Exception Syndrome Register — why we are here.
    pub esr: u64,
    /// Fault Address Register — meaningful for aborts.
    pub far: u64,
    /// The interrupted context's `SP_EL0`.
    ///
    /// **Per-task state living in a CPU-global register**, which is the whole reason it is
    /// in this frame. `SP_EL0` is banked by exception *level*, not by task: taking an
    /// exception sets `PSTATE.SP = 1` so the handler runs on `SP_EL1` and leaves `SP_EL0`
    /// untouched — but every task at EL0 shares that one register.
    ///
    /// Leave a task's user stack pointer live in it across a syscall and the Phase 4.5
    /// bug returns verbatim with one register substituted for one memory slot: task A
    /// takes `svc`, a timer IRQ preempts it, `schedule()` runs task B which is also
    /// mid-syscall, B's exit writes its own `SP_EL0` and `eret`s; A is later resumed,
    /// reaches its exit tail, and `eret`s onto **B's user stack**.
    ///
    /// The invariant: saved on entry, restored from here with interrupts masked
    /// immediately before `eret`, never read back live after any window where preemption
    /// could occur. The file already reasons this way about `ELR_EL1`/`SPSR_EL1` — "single
    /// system registers, not per-task storage" — and this is the same argument.
    pub sp_el0: u64,
    /// The interrupted context's `TPIDR_EL0` — userspace's thread pointer (TLS).
    ///
    /// Same hazard and same fix as [`sp_el0`](Self::sp_el0). 7.3 gave `TPIDR_EL1` the
    /// structural treatment (rewritten on every context switch); this is its EL0
    /// counterpart, and the context switch saves neither today — it preserves x19-x30 and
    /// nothing else.
    pub tpidr_el0: u64,
}

/// Bytes the entry stub reserves for an [`ExceptionFrame`].
///
/// Kept in sync with the `sub sp, sp, #304` in the assembly below by the assertions
/// that follow — mechanically, not by comment. This file already has one war story
/// about hand-maintained assembly geometry drifting out of step (see the note on slot
/// size); adding a field to `ExceptionFrame` should be a build error, not a handler
/// that silently reads a neighbouring register's value.
const EXC_FRAME_RESERVE: usize = 304;

const _: () = assert!(core::mem::size_of::<ExceptionFrame>() <= EXC_FRAME_RESERVE);
// SP must stay 16-byte aligned across the `bl` into Rust (AAPCS64).
const _: () = assert!(EXC_FRAME_RESERVE % 16 == 0);

/// Count of `BRK` exceptions handled, so the self-test can prove the handler ran
/// rather than inferring it from "we did not crash".
static BRK_COUNT: AtomicU64 = AtomicU64::new(0);

/// Count of `SVC` exceptions dispatched as syscalls, so the self-test can prove the path
/// ran rather than inferring it from "we did not crash".
static SVC_COUNT: AtomicU64 = AtomicU64::new(0);

/// How many syscalls have been dispatched from EL0.
pub fn svc_count() -> u64 {
    SVC_COUNT.load(Ordering::Relaxed)
}

// --- Syndrome constants (ESR_EL1.EC, bits 31:26) ---

const EC_SVC64: u64 = 0x15;
const EC_INSTR_ABORT_LOWER: u64 = 0x20;
const EC_INSTR_ABORT_CURRENT: u64 = 0x21;
const EC_DATA_ABORT_LOWER: u64 = 0x24;
const EC_DATA_ABORT_CURRENT: u64 = 0x25;
/// Illegal Execution State. Raised when `PSTATE.IL` is set — most usefully, after an
/// `eret` whose `SPSR_EL1.M` held a *reserved* mode: `SetPSTATEFromPSR` takes the
/// illegal-PSR branch, sets `IL` and **skips** the EL/SP assignment, so the PE stays at
/// EL1 on `SP_EL1` and the very next fetch traps here. 8.spike reproduced it deliberately
/// (`ESR 0x3a000000`, slot 4, `SPSR 0x1003c5`); before that this printed "unclassified"
/// for the one syndrome an illegal exception return produces.
const EC_ILLEGAL_STATE: u64 = 0x0E;
const EC_BRK: u64 = 0x3C;

/// Human-readable name for an exception class, for diagnostics.
fn ec_name(ec: u64) -> &'static str {
    match ec {
        0x00 => "unknown reason",
        0x07 => "SVE/SIMD/FP access trapped (CPACR_EL1.FPEN)",
        EC_SVC64 => "SVC (AArch64)",
        EC_INSTR_ABORT_LOWER => "instruction abort from a lower EL",
        EC_INSTR_ABORT_CURRENT => "instruction abort at EL1",
        0x22 => "PC alignment fault",
        EC_DATA_ABORT_LOWER => "data abort from a lower EL",
        EC_DATA_ABORT_CURRENT => "data abort at EL1",
        0x26 => "SP alignment fault",
        EC_ILLEGAL_STATE => "Illegal Execution State (PSTATE.IL)",
        EC_BRK => "BRK (software breakpoint)",
        _ => "unclassified",
    }
}

/// Decode the Data/Instruction Fault Status Code (`ESR_EL1.ISS[5:0]`) for aborts.
///
/// The common cases are the ones worth naming: a translation fault means the mapping
/// is absent, a permission fault means it exists but the access was not allowed, and
/// an access-flag fault means `AF` was clear — which is the mistake
/// [`crate::arch::aarch64::paging`] guards against by always setting it.
fn fault_status(iss: u64) -> &'static str {
    match iss & 0x3f {
        0b000000..=0b000011 => "address size fault",
        0b000100..=0b000111 => "translation fault (no mapping)",
        0b001000..=0b001011 => "access flag fault (AF clear)",
        0b001100..=0b001111 => "permission fault",
        0b010000 => "synchronous external abort",
        0b100001 => "alignment fault",
        _ => "other",
    }
}

// The vector table itself. Each of the sixteen slots is 128 bytes (`.align 7`), and
// the table as a whole must be 2 KiB aligned (`.align 11`) because `VBAR_EL1` ignores
// the low 11 bits.
//
// Every slot saves the full register state and calls the same Rust entry point with a
// tag identifying which slot fired, so an exception arriving somewhere unexpected is
// reported as such rather than mishandled as something else.
global_asm!(
    r#"
.section .text

// A vector slot is exactly 128 bytes, and the CPU branches *into* it. Anything that
// does not fit silently overflows into the next slot — and because `.align 7` then
// pushes that slot to the following boundary, the whole table shifts and exceptions
// land in the wrong handler. (That is not hypothetical: the first version of this
// file saved all 31 GPRs inline, needed 188 bytes per slot, and delivered a
// synchronous data abort to the FIQ stub.)
//
// So each slot holds four instructions: reserve the frame, save the two registers
// needed as scratch, load the slot tag, and branch to the shared body below.
.macro VECTOR_STUB tag
    sub sp, sp, #304
    stp x0, x1, [sp, #(0 * 8)]
    mov x1, #\tag
    b   aarch64_exc_common
.endm

// Shared entry: completes the ExceptionFrame the stub started, calls Rust, and
// returns. x1 holds the vector tag on arrival; x0/x1 are already saved.
aarch64_exc_common:
    stp x2,  x3,  [sp, #(2 * 8)]
    stp x4,  x5,  [sp, #(4 * 8)]
    stp x6,  x7,  [sp, #(6 * 8)]
    stp x8,  x9,  [sp, #(8 * 8)]
    stp x10, x11, [sp, #(10 * 8)]
    stp x12, x13, [sp, #(12 * 8)]
    stp x14, x15, [sp, #(14 * 8)]
    stp x16, x17, [sp, #(16 * 8)]
    stp x18, x19, [sp, #(18 * 8)]
    stp x20, x21, [sp, #(20 * 8)]
    stp x22, x23, [sp, #(22 * 8)]
    stp x24, x25, [sp, #(24 * 8)]
    stp x26, x27, [sp, #(26 * 8)]
    stp x28, x29, [sp, #(28 * 8)]
    str x30,      [sp, #(30 * 8)]

    mrs x2, ELR_EL1
    mrs x3, SPSR_EL1
    stp x2, x3, [sp, #(31 * 8)]
    mrs x2, ESR_EL1
    mrs x3, FAR_EL1
    stp x2, x3, [sp, #(33 * 8)]

    // SP_EL0 and TPIDR_EL0: per-task state in CPU-global registers. Saved here so a
    // preempting context switch cannot leak one task's user stack or thread pointer into
    // another's `eret`. See the field docs on ExceptionFrame.
    mrs x2, SP_EL0
    mrs x3, TPIDR_EL0
    stp x2, x3, [sp, #(35 * 8)]

    mov x0, sp              // &mut ExceptionFrame
                            // x1 already holds the vector tag
    bl  aarch64_exception_entry

    // Restore ELR_EL1/SPSR_EL1 from the frame. This is NOT merely "in case the handler
    // moved ELR to step over a BRK" — since 7.3 it is load-bearing for every single
    // preemption, and must not be optimised into a conditional write-back.
    //
    // ELR_EL1 and SPSR_EL1 are single system registers, not per-task storage. When the
    // handler calls schedule(), this task is switched away and some other task runs —
    // taking its own exceptions, each of which overwrites both registers. By the time
    // this task is resumed and reaches here, they describe whichever exception ran
    // last, on whatever task. Only reloading them from *this* task's frame, which the
    // entry sequence filled and which lives on this task's own stack, makes the `eret`
    // below return to the right place with the right PSTATE.
    //
    // The window between these two writes and the `eret` is the aarch64 analog of the
    // x86 syscall-exit tail: an exception taken inside it would clobber both registers
    // again. It is closed because DAIF.I is masked from exception entry all the way
    // through here — nothing in the handler path, schedule() included, unmasks it —
    // which `aarch64_exception_entry` asserts on the way out.
    ldp x2, x3, [sp, #(31 * 8)]
    msr ELR_EL1, x2
    msr SPSR_EL1, x3

    // Restore the two per-task registers inside the same interrupts-masked window as
    // ELR/SPSR, and for the same reason: between here and the `eret` nothing may run that
    // could switch tasks, or all four would describe some other task's context.
    ldp x2, x3, [sp, #(35 * 8)]
    msr SP_EL0, x2
    msr TPIDR_EL0, x3

    ldp x0,  x1,  [sp, #(0 * 8)]
    ldp x2,  x3,  [sp, #(2 * 8)]
    ldp x4,  x5,  [sp, #(4 * 8)]
    ldp x6,  x7,  [sp, #(6 * 8)]
    ldp x8,  x9,  [sp, #(8 * 8)]
    ldp x10, x11, [sp, #(10 * 8)]
    ldp x12, x13, [sp, #(12 * 8)]
    ldp x14, x15, [sp, #(14 * 8)]
    ldp x16, x17, [sp, #(16 * 8)]
    ldp x18, x19, [sp, #(18 * 8)]
    ldp x20, x21, [sp, #(20 * 8)]
    ldp x22, x23, [sp, #(22 * 8)]
    ldp x24, x25, [sp, #(24 * 8)]
    ldp x26, x27, [sp, #(26 * 8)]
    ldp x28, x29, [sp, #(28 * 8)]
    ldr x30,      [sp, #(30 * 8)]
    add sp, sp, #304
    eret

.align 11
.global aarch64_vector_table
aarch64_vector_table:
    // --- Current EL, SP_EL0 (not used: we run on SP_EL1) ---
    .align 7
    VECTOR_STUB 0
    .align 7
    VECTOR_STUB 1
    .align 7
    VECTOR_STUB 2
    .align 7
    VECTOR_STUB 3

    // --- Current EL, SP_ELx (this is the live group at EL1) ---
    .align 7
    VECTOR_STUB 4     // synchronous
    .align 7
    VECTOR_STUB 5     // IRQ
    .align 7
    VECTOR_STUB 6     // FIQ
    .align 7
    VECTOR_STUB 7     // SError

    // --- Lower EL, AArch64 (live once EL0 lands) ---
    .align 7
    VECTOR_STUB 8
    .align 7
    VECTOR_STUB 9
    .align 7
    VECTOR_STUB 10
    .align 7
    VECTOR_STUB 11

    // --- Lower EL, AArch32 (never: we do not run 32-bit code) ---
    .align 7
    VECTOR_STUB 12
    .align 7
    VECTOR_STUB 13
    .align 7
    VECTOR_STUB 14
    .align 7
    VECTOR_STUB 15
"#
);

/// Vector-slot tags, matching the `VECTOR_STUB` arguments above.
/// Vector slot 8: synchronous exception from a **lower** EL in AArch64 — the syscall
/// path. Dispatch keys on this *before* looking at `ESR_EL1.EC`, so an `svc` executed at
/// EL1 (which arrives at slot 4) stays fatal instead of being serviced as a syscall.
const TAG_LOWER_A64_SYNC: u64 = 8;

const TAG_CUR_SPX_SYNC: u64 = 4;

/// The IRQ slot, dispatched to the interrupt controller.
const TAG_CUR_SPX_IRQ: u64 = 5;

/// Vector slot 9: IRQ from a **lower** EL in AArch64 — a device interrupt taken while
/// userspace was running.
///
/// The same event as [`TAG_CUR_SPX_IRQ`], arriving through a different door purely because
/// of the EL it interrupted. Phase 7 handled only slot 5 because EL1 was the only level
/// that existed; the moment 8.4 dropped to EL0, the first timer tick landed here instead
/// and fell through to the fatal arm.
///
/// The failure was doubly misleading, which is worth recording: an IRQ does **not** write
/// `ESR_EL1`, so the fatal reporter printed the syndrome left over from the previous
/// `svc` — `EC = 0x15`, "SVC (AArch64)" — and the crash read as a syscall problem rather
/// than an unhandled interrupt slot.
const TAG_LOWER_A64_IRQ: u64 = 9;

/// Name a vector slot for diagnostics.
fn slot_name(tag: u64) -> &'static str {
    match tag {
        0..=3 => "current EL / SP_EL0",
        4 => "current EL / SP_ELx, synchronous",
        5 => "current EL / SP_ELx, IRQ",
        6 => "current EL / SP_ELx, FIQ",
        7 => "current EL / SP_ELx, SError",
        8 => "lower EL (AArch64), synchronous",
        9 => "lower EL (AArch64), IRQ",
        10 => "lower EL (AArch64), FIQ",
        11 => "lower EL (AArch64), SError",
        _ => "lower EL (AArch32)",
    }
}

/// Common Rust entry point for every vector slot.
///
/// # Safety
///
/// Called only from the assembly stubs above, with `frame` pointing at the register
/// state they just pushed onto the current stack.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aarch64_exception_entry(frame: &mut ExceptionFrame, tag: u64) {
    let ec = (frame.esr >> 26) & 0x3f;

    // First thing, before any formatting: record where the frame landed. If the entry
    // stub is running off the end of the stack this is the only value that says so,
    // and it must be captured before println! consumes several KiB more.
    let frame_at = frame as *const _ as u64;

    match tag {
        // IRQ — the controller says what actually happened.
        //
        // Both slots: 5 when the interrupt hit kernel code, 9 when it hit userspace. The
        // handling is identical — the GIC does not care which EL was running, and neither
        // does the scheduler — so they share an arm rather than duplicating it.
        TAG_CUR_SPX_IRQ | TAG_LOWER_A64_IRQ => {
            let reschedule = crate::arch::aarch64::gic::dispatch_irq();

            // Preempt *after* the EOI issued inside `dispatch_irq`, never before.
            // `schedule()` switches stacks and does not return until this task runs
            // again, so scheduling with the interrupt still active would leave it
            // active in the controller for the whole intervening period — and a GICv2
            // CPU interface delivers nothing further while an interrupt is active.
            // The x86_64 timer ISR sequences it the same way: send the EOI, then
            // `schedule()`.
            //
            // Doing this from the handler is sound because the exception frame lives
            // on the *interrupted task's own stack*: switching away leaves it intact,
            // and when this task is resumed it unwinds back through here on that same
            // stack and `eret`s exactly where it left off.
            if reschedule && crate::sched::is_initialized() {
                crate::sched::schedule();
            }

            // The stub is about to write ELR_EL1/SPSR_EL1 back from the frame and
            // `eret`. That tail is only atomic while interrupts are masked — see the
            // long note beside the write-back. Nothing above unmasks them, including
            // the schedule() that just ran, but "nothing does" is an invariant no line
            // of source states, so check it here rather than discover it as a rare and
            // impossible-looking wrong return.
            debug_assert_irqs_masked("IRQ vector exit");
            return;
        }

        // Synchronous from EL0 — the syscall path.
        //
        // **Keyed on the slot, then the EC**, in that order and deliberately. An `svc`
        // executed at EL1 arrives at slot 4, not here; matching on `ec == EC_SVC64` alone
        // would service it as a syscall and hand a kernel-originated trap the userspace
        // return path. Slot-first makes an EL1 `svc` fall through to the fatal arm, which
        // is what it is.
        TAG_LOWER_A64_SYNC if ec == EC_SVC64 => {
            SVC_COUNT.fetch_add(1, Ordering::Relaxed);

            // **ELR is NOT advanced here**, and the `brk` arm twelve lines below does
            // advance it. That asymmetry is real, measured by 8.spike, and is the single
            // likeliest slip in this sub-phase:
            //
            //   `brk` — ELR points *at* the trapping instruction; returning without
            //           advancing re-executes it forever.
            //   `svc` — ELR points *after* it (measured delta 4). Advancing would skip
            //           one instruction of userspace after every syscall — corrupting
            //           user control flow in a way that looks like a miscompile.
            //
            // Copying `frame.elr += 4` into this arm is therefore a silent user-visible
            // bug, which is why the two arms sit next to each other with this note
            // between them.
            syscall::dispatch(frame);
            return;
        }

        // Synchronous at EL1.
        TAG_CUR_SPX_SYNC if ec == EC_BRK => {
            BRK_COUNT.fetch_add(1, Ordering::Relaxed);
            // Step over the `brk` instruction. Unlike x86's `int3`, ELR points *at*
            // the trapping instruction, not after it — returning without advancing
            // would re-execute it forever.
            frame.elr += 4;
            return;
        }

        _ => {}
    }

    // Anything else is fatal for this phase. Report everything useful before halting:
    // this is the diagnostic path the port did not have until now.
    println!();
    println!("!!! aarch64 EXCEPTION !!!");
    println!("  vector:  {} (slot {})", slot_name(tag), tag);
    println!("  frame at:{:#018x} (exception stack pointer)", frame_at);

    // Name the task through the per-CPU block, never through the scheduler.
    // `sched::current_task_id()` takes the scheduler lock, and `schedule()` holds that
    // lock while it runs — so a fault raised from inside it (or from anything else
    // holding it) would deadlock here, in the handler, with nothing printed. The
    // TPIDR_EL1 block is written by `schedule()` itself and read with no lock at all.
    if let Some(pc) = crate::arch::aarch64::percpu::snapshot() {
        println!(
            "  task:    {} (per-CPU block; {} context switches)",
            pc.current_task, pc.switches
        );
        // The handler is running on the *faulting* stack — there is no IST/TSS analog
        // to escape to — so if this fault is a stack overflow, saying so is the only
        // warning that will ever be printed. It is also the fault most likely to be
        // misread as a wild pointer.
        if let Some(hint) = crate::arch::aarch64::percpu::stack_overflow_hint(frame_at) {
            println!("  !! {}", hint);
            println!(
                "     stack is [{:#018x}, {:#018x})",
                pc.kernel_stack_limit, pc.kernel_stack_top
            );
        }
    }
    // ESR is only written by *synchronous* exceptions and SError. On an IRQ/FIQ slot it
    // holds whatever the last synchronous exception left there, which reads as a
    // confident diagnosis of the wrong thing — exactly how the unhandled slot-9 IRQ above
    // first presented as an SVC fault.
    let esr_meaningful = !matches!(tag, 1 | 2 | 5 | 6 | 9 | 10 | 13 | 14);
    if esr_meaningful {
        println!("  ESR_EL1: {:#018x}  EC={:#04x} ({})", frame.esr, ec, ec_name(ec));
    } else {
        println!(
            "  ESR_EL1: {:#018x}  (STALE — this is an IRQ/FIQ slot; ESR describes an \
             earlier synchronous exception, not this one)",
            frame.esr
        );
    }
    if matches!(
        ec,
        EC_DATA_ABORT_CURRENT | EC_DATA_ABORT_LOWER | EC_INSTR_ABORT_CURRENT | EC_INSTR_ABORT_LOWER
    ) {
        println!(
            "  FAR_EL1: {:#018x}  ({})",
            frame.far,
            fault_status(frame.esr)
        );
    }
    println!("  ELR_EL1: {:#018x}  (faulting instruction)", frame.elr);
    println!("  SPSR:    {:#018x}", frame.spsr);
    println!("  x0-x3:   {:#018x} {:#018x} {:#018x} {:#018x}",
        frame.x[0], frame.x[1], frame.x[2], frame.x[3]);
    println!("  x29/x30: {:#018x} {:#018x}", frame.x[29], frame.x[30]);

    panic!("unhandled aarch64 exception (EC={:#04x}: {})", ec, ec_name(ec));
}

/// Assert that interrupts are masked, for the exception-return tail.
///
/// The aarch64 analog of the Phase 4.5 "syscall-exit double-fault" fix, which made the
/// x86 exit tail atomic by masking interrupts across it. The shared state at risk here
/// is different but the shape is the same: `ELR_EL1` and `SPSR_EL1` are single
/// registers, and between the stub writing them back from the frame and the `eret` they
/// hold *this* task's return state. An exception in that window overwrites both, and
/// the `eret` returns to the wrong place with the wrong PSTATE.
///
/// The window is closed by construction — the CPU masks `DAIF.I` on exception entry and
/// nothing in the handler path unmasks it, `schedule()` included. That is a property
/// worth checking rather than assuming, because it is invisible in the source: no line
/// says "interrupts are off here", and a future handler that enables them to do
/// something slow would break the exit tail with no symptom but rare, impossible-looking
/// returns.
#[inline]
fn debug_assert_irqs_masked(where_: &str) {
    debug_assert!(
        !crate::arch::irq::are_enabled(),
        "{}: interrupts are unmasked inside the exception path — the ELR/SPSR \
         write-back before `eret` is no longer atomic",
        where_
    );
}

/// Install the vector table into `VBAR_EL1`.
///
/// Must run before interrupts are unmasked. Until it does, an exception branches to
/// whatever `VBAR_EL1` held at handoff.
///
/// Note it is *not* the first thing boot does: the PL011 must be mapped first, since a
/// vector table whose handler cannot print is of little use. That ordering means the
/// most fault-prone step in early boot — editing Limine's live TTBR1 tables to map the
/// UART — still runs unprotected. Moving the install earlier would trade a diagnosable
/// fault later for an undiagnosable one there.
pub fn init() {
    unsafe extern "C" {
        /// The table defined by the `global_asm!` block above.
        static aarch64_vector_table: u8;
    }

    // SAFETY: `aarch64_vector_table` is 2 KiB-aligned executable code in our own
    // image, which is exactly what VBAR_EL1 requires. The ISB ensures the write has
    // taken effect before any subsequent exception can be taken.
    unsafe {
        let vbar = &raw const aarch64_vector_table as u64;
        // VBAR_EL1 ignores the low 11 bits, so a table that is not 2 KiB aligned is
        // silently used at a *different* address than the symbol says — every
        // exception then lands in the wrong place. Cheap to check, and the failure it
        // prevents is otherwise diagnosable only by disassembling the image.
        assert!(
            vbar & 0x7ff == 0,
            "exception vector table at {:#018x} is not 2 KiB aligned",
            vbar
        );
        core::arch::asm!(
            "msr VBAR_EL1, {}",
            "isb",
            in(reg) vbar,
            options(nostack, preserves_flags),
        );
        println!("[exc] VBAR_EL1 installed at {:#018x}", vbar);
    }

    // Now that there is somewhere for one to land, unmask SErrors.
    //
    // Limine hands off with the whole of DAIF masked, and `arch::irq::enable` only
    // touches the I bit — correctly, since SError masking is not an interrupt-disable
    // and `irq::disable` has no business re-masking it. So without this the `A` bit
    // stays set forever and the SError vector slot, which exists precisely to report
    // asynchronous external aborts, is never reached.
    //
    // Deliberately *after* the VBAR write: unmasking first would mean a pending SError
    // (an abort left over from the bootloader's device probing, say) is taken against
    // whatever vector table Limine left behind.
    //
    // SAFETY: clears DAIF.A only. The vector table is installed and the ISB above has
    // retired, so an SError taken from here on reaches our handler.
    unsafe {
        core::arch::asm!("msr DAIFClr, #4", options(nomem, nostack, preserves_flags));
    }
}

/// Trap FP/SIMD access at EL1 by clearing `CPACR_EL1.FPEN`.
///
/// Must run **after** [`init`] has installed the vector table: the entire point is to
/// turn a stray vector instruction into a reported exception, and before `VBAR_EL1` is
/// valid that same instruction would branch into whatever the bootloader left behind.
///
/// Phase 7.0b did the opposite — it *enabled* `FPEN`, because the hardfloat
/// `aarch64-unknown-none` target lowers struct moves and `core::fmt` to SIMD, so the
/// first `println!` trapped without it. The move to `aarch64-unknown-none-softfloat`
/// inverted that: the kernel now emits no vector instructions at all, which is the
/// premise both the exception stub and `switch_context` rest on when they save no
/// vector registers.
///
/// Limine hands off with `FPEN = 0b11` — FP fully enabled — which was *measured*, not
/// assumed, by [`verify_fp_trapped`] on the first boot that checked. Three separate
/// comments in this port had already claimed the opposite ("with `FPEN` off, any SIMD
/// that ever sneaks in traps loudly"), describing a safety net that did not exist.
/// This makes the claim true instead of deleting it.
pub fn trap_fp_access() {
    // FPEN is CPACR_EL1 bits 21:20. 0b00 traps FP/SIMD at both EL0 and EL1.
    // SAFETY: read-modify-write of a control register that gates FP access only. The
    // ISB retires it before the next instruction, so no access can slip past on the
    // old setting. Sound because this kernel is softfloat and executes no FP.
    unsafe {
        let cpacr: u64;
        core::arch::asm!("mrs {}, CPACR_EL1", out(reg) cpacr, options(nomem, nostack));
        core::arch::asm!(
            "msr CPACR_EL1, {}",
            "isb",
            in(reg) cpacr & !(0b11 << 20),
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Verify FP/SIMD is trapped, which is what makes the missing FP save area sound.
///
/// [`crate::arch::aarch64::context`] saves `x19`-`x30` and no vector registers, even
/// though AAPCS64 makes the low 64 bits of `v8`-`v15` callee-saved. That is only sound
/// because the kernel is built for `aarch64-unknown-none-softfloat` and emits no vector
/// instructions at all.
///
/// "Emits none today" is a property of the current build, though, and the backstop for
/// the day it stops holding is `CPACR_EL1.FPEN`, cleared by [`trap_fp_access`]: with FP
/// access trapped, a stray SIMD instruction raises `EC = 0x07` — which [`ec_name`]
/// names — instead of quietly corrupting another task's floating-point state across a
/// context switch, with no fault to point at it.
///
/// Checking it is not ceremony. That backstop was asserted in three separate comments
/// and never once read, and the first boot that actually read the register found
/// `FPEN = 0b11`: Limine leaves FP fully enabled, so the net had never existed. Same
/// spirit as `paging::verify_tcr` — confirm the register the surrounding code depends
/// on rather than trust the comment about it.
///
/// Returns `true` if FP access traps at EL1.
pub fn verify_fp_trapped() -> bool {
    let cpacr: u64;
    // SAFETY: reading CPACR_EL1 has no side effects.
    unsafe { core::arch::asm!("mrs {}, CPACR_EL1", out(reg) cpacr, options(nomem, nostack)) };

    // FPEN is bits 21:20. 0b11 means "no trapping at either EL"; 0b01 traps EL0 only;
    // 0b00 and 0b10 both trap EL1. We need EL1 accesses to trap.
    let fpen = (cpacr >> 20) & 0b11;
    let trapped = fpen != 0b11 && fpen != 0b01;

    if trapped {
        println!(
            "[exc] CPACR_EL1.FPEN={:#04b} — FP/SIMD traps at EL1 (softfloat kernel; \
             stray SIMD is reported, not silently mis-saved)",
            fpen
        );
    } else {
        println!(
            "[exc] CPACR_EL1.FPEN={:#04b} — FP/SIMD does NOT trap at EL1. The context \
             switch saves no v8-v15, so floating-point state would be corrupted across \
             tasks with no fault to show for it.",
            fpen
        );
    }
    trapped
}

/// Number of `BRK` exceptions handled since boot.
pub fn brk_count() -> u64 {
    BRK_COUNT.load(Ordering::Relaxed)
}

/// Prove the synchronous-exception path end to end by executing a real `brk`.
///
/// This is the `int3` analog of the x86 breakpoint self-test: it takes an actual
/// exception, decodes a real syndrome, advances `ELR_EL1` past the trapping
/// instruction, and returns to the next one. Reaching the line after the `brk` *is*
/// the proof — if the handler failed to step over it, the CPU would loop on it
/// forever; if the vector table were wrong, we would not come back at all.
///
/// Returns `true` if the handler ran exactly once.
pub fn selftest() -> bool {
    let before = brk_count();

    // Where is the stack, and how much of it is left? The exception stub reserves 288
    // bytes and the Rust handler formats output on top of that, so a stack sitting
    // near a mapping boundary would fail here in a way that looks like a vector-table
    // bug. Print it so the two are distinguishable from the serial log alone.
    let sp: u64;
    // SAFETY: reading SP has no side effects.
    unsafe { core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack)) };
    println!("[exc] SP before brk: {:#018x}", sp);

    // SAFETY: `brk #0` raises a synchronous exception with EC=0x3C, which the handler
    // above recognises and resumes from.
    //
    // Deliberately *without* `nomem`/`nostack`. Both would be lies: the handler writes
    // BRK_COUNT, so telling the compiler this asm touches no memory would let it cache
    // the counter reads either side and fold the comparison to a constant; and taking
    // the exception pushes a 288-byte frame, so it very much touches the stack.
    unsafe {
        core::arch::asm!("brk #0", options(preserves_flags));
    }

    let after = brk_count();
    if after == before + 1 {
        println!("[selftest] exceptions: PASS (brk #0 trapped, decoded, resumed)");
        true
    } else {
        println!(
            "[selftest] exceptions: FAIL — brk count went {} -> {}, expected +1",
            before, after
        );
        false
    }
}
