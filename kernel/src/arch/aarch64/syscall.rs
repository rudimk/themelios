//! # aarch64 syscall entry and dispatch
//!
//! The EL0 → EL1 → EL0 round trip. Userspace executes `svc #0`; the CPU vectors to slot
//! 8 (lower EL, AArch64, synchronous) where [`crate::arch::aarch64::exceptions`] has
//! already built an [`ExceptionFrame`]; this module reads the call out of that frame,
//! dispatches it, and writes the result back into the frame's `x0`.
//!
//! ## The ABI, and why `x8`
//!
//! | register | role |
//! |----------|------|
//! | `x8`     | syscall number |
//! | `x0`-`x5`| arguments |
//! | `x0`     | return value |
//!
//! Not because AAPCS64 leaves `x8` spare — it does not; `x8` is the *indirect result
//! location register*, and `x9`-`x15` are equally free. The reason is that **`x8` is what
//! Linux and `asm-generic` use for the syscall number on aarch64**, so every toolchain,
//! debugger, `strace` and libc already agrees. Picking a "cleaner" register would cost
//! that agreement for nothing. The Linux personality is a separate table layered on top of
//! this one, not a different calling convention.
//!
//! The `svc` immediate is ignored. The number lives in `x8`, uniformly.
//!
//! ## No kernel state may live in a GPR across the boundary
//!
//! 8.spike established this and it is a rule, not a caution. The exception exit stub
//! restores **all** of `x0`-`x30` from the frame, and on a lower-EL exception that frame
//! holds *EL0's* register values. So any path returning to EL1 through `eret` arrives with
//! userspace's registers in every GPR — callee-saved included — and `clobber_abi("C")`
//! cannot express that, because it covers only caller-saved registers.
//!
//! Concretely: kernel-side syscall context belongs in the task structure or in the frame.
//! Never in a register the exit path will overwrite.
//!
//! ## What this is not
//!
//! It is not the x86 `syscall`/`sysretq` path transliterated. There is no `swapgs` analog
//! here — the per-CPU block is reached through `TPIDR_EL1`, which 7.3 already rewrites on
//! every context switch — and no `MSR`-programmed entry point: the vector table is the
//! entry point, installed once in `VBAR_EL1`.

use super::exceptions::ExceptionFrame;

/// Syscall numbers. Deliberately the kernel's own small set for now; the Linux
/// personality's `asm-generic` numbering is a separate table layered above this.
pub mod nr {
    /// Write a string to the kernel console. `x0` = pointer, `x1` = length.
    ///
    /// Returns the number of bytes printed, or one of the error sentinels below.
    pub const DEBUG_PRINT: u64 = 1;
    /// Record the calling task's exit code.
    ///
    /// **Does not terminate anything yet.** There is no task teardown for EL0 tasks in
    /// this sub-phase: the handler stores the code and returns, and the caller is expected
    /// to spin. Named `EXIT` for the ABI it will have, documented for the one it has.
    pub const EXIT: u64 = 2;
    /// Return `x0 + x1`, so a test can assert a value *derived from its arguments*
    /// rather than merely that a syscall returned.
    pub const ADD: u64 = 3;
}

/// Error returned in `x0` for an unrecognised syscall number.
///
/// A large sentinel rather than `-1`: `-1` is a plausible *success* value for a syscall
/// that returns a signed quantity, so a test asserting "did not fail" could not tell the
/// two apart.
pub const ENOSYS: u64 = u64::MAX - 37;

/// A user pointer could not be accessed — [`super::uaccess`] rejected the range.
///
/// Distinct from [`ENOSYS`] deliberately. An earlier version returned `ENOSYS` for a bad
/// pointer, an over-long length, invalid UTF-8 *and* an unknown syscall number, which left
/// a caller unable to distinguish "this kernel does not implement that call" from "your
/// argument was wrong" — and left a test unable to assert that the range check, rather
/// than the dispatch table, was what rejected a hostile pointer.
pub const EFAULT: u64 = u64::MAX - 13;

/// An argument was malformed: a length over the syscall's bound, or bytes that are not
/// valid UTF-8 where a string was required.
pub const EINVAL: u64 = u64::MAX - 21;

/// Dispatch one syscall from an EL0 exception frame.
///
/// Reads the number from `x8` and, for the calls that exist today, the arguments from `x0`
/// and `x1` — the ABI reserves `x0`-`x5` but nothing yet takes more than two — and writes
/// the result back
/// into `frame.x[0]` — the exception exit stub then restores that into the real `x0` on
/// the way back to EL0, which is how a return value reaches userspace.
///
/// **Does not touch `frame.elr`.** `ELR_EL1` already points past the `svc`; see the note
/// on the dispatch arm in `exceptions.rs`.
pub fn dispatch(frame: &mut ExceptionFrame) {
    let nr = frame.x[8];
    let a0 = frame.x[0];
    let a1 = frame.x[1];

    let ret = match nr {
        nr::DEBUG_PRINT => sys_debug_print(a0, a1),
        nr::ADD => a0.wrapping_add(a1),
        nr::EXIT => {
            // Nothing returns from here in a real process model; today the EL0 payload is
            // driven by a self-test that treats EXIT as "stop and tell me you got here",
            // so record it and let the test observe the count.
            // Code first, flag second, with the flag released — so an observer that sees
            // `EXITED` is guaranteed to see the code that goes with it. See `exit_status`.
            EXIT_CODE.store(a0, Ordering::Relaxed);
            EXITED.store(true, Ordering::Release);
            0
        }
        _ => ENOSYS,
    };

    frame.x[0] = ret;
}

/// `SYS_DEBUG_PRINT` — copy a string out of userspace and print it.
///
/// Bounds-checks the range against the user regime before reading a byte of it. The check
/// is the point of the syscall existing this early: it is the first code in the port that
/// takes a pointer *from* userspace, and getting it wrong is how a kernel reads or writes
/// its own memory at a user process's request.
fn sys_debug_print(ptr: u64, len: u64) -> u64 {
    /// Longest string this will print. Bounds the work a single syscall can cause, and
    /// keeps the stack buffer below fixed.
    const MAX: usize = 256;

    if len as usize > MAX {
        return EINVAL;
    }
    let mut buf = [0u8; MAX];
    // SAFETY: `copy_from_user` is unsafe because a validated range is not a *mapped*
    // range — see its docs. The obligation discharged here is the caller-context one: this
    // runs on the syscall path, so the address space the pointer belongs to is the one
    // currently installed in `TTBR0_EL1`. The fault risk is the documented open hole, not
    // something this call site can close.
    let copied = unsafe { super::uaccess::copy_from_user(&mut buf[..len as usize], ptr) };
    match copied {
        Ok(()) => {
            // Only valid UTF-8 is printed; anything else is reported rather than passed
            // through, so a malformed pointer cannot spray bytes at the console.
            match core::str::from_utf8(&buf[..len as usize]) {
                Ok(s) => {
                    crate::print!("{s}");
                    // Record that a copy from userspace actually completed and produced
                    // printable bytes. Without this the self-test cannot tell a working
                    // uaccess path from a completely broken one: the EL0 payload ignores
                    // this syscall's return value, so breaking `copy_from_user` outright
                    // removes only a console line that nothing asserts on. A review
                    // measured exactly that — mutating this function to fail
                    // unconditionally still produced `el0: PASS`.
                    PRINTED_BYTES.fetch_add(len, Ordering::Relaxed);
                    len
                }
                Err(_) => EINVAL,
            }
        }
        Err(_) => EFAULT,
    }
}

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Set when an EL0 payload calls `SYS_EXIT`, with the code it passed.
static EXITED: AtomicBool = AtomicBool::new(false);
static EXIT_CODE: AtomicU64 = AtomicU64::new(0);

/// Total bytes successfully copied out of userspace and printed by `SYS_DEBUG_PRINT`.
///
/// The self-test's only evidence that [`super::uaccess`] works. See the note at the
/// increment site for why an indirect observation is needed at all.
static PRINTED_BYTES: AtomicU64 = AtomicU64::new(0);

/// Bytes `SYS_DEBUG_PRINT` has copied from userspace and printed since the last clear.
pub fn printed_bytes() -> u64 {
    PRINTED_BYTES.load(Ordering::Acquire)
}

/// Whether an EL0 payload has called `SYS_EXIT`, and with what code.
///
/// `Acquire`/`Release` against the store pair rather than `Relaxed` on both sides. On the
/// current UP kernel `Relaxed` is sufficient — a single PE is self-consistent — but this is
/// a `pub` API whose whole purpose is one context observing another's progress, and it
/// would be read as correct long after SMP makes it wrong. The ordering costs nothing on
/// aarch64 here and makes the flag actually mean "the code is visible too".
pub fn exit_status() -> Option<u64> {
    if EXITED.load(Ordering::Acquire) {
        Some(EXIT_CODE.load(Ordering::Relaxed))
    } else {
        None
    }
}

/// Clear the recorded exit status and the print accounting, so a second EL0 run starts
/// from a known state.
pub fn clear_exit_status() {
    EXIT_CODE.store(0, Ordering::Relaxed);
    PRINTED_BYTES.store(0, Ordering::Relaxed);
    EXITED.store(false, Ordering::Release);
}

// --- Dropping to EL0 ---

/// `SPSR_EL1` for a fresh entry to EL0.
///
/// `M[3:0] = 0b0000` is **EL0t** — EL0 using `SP_EL0`, the only mode a user thread runs
/// in. `DAIF` is left clear so userspace runs with interrupts enabled and remains
/// preemptible; a task that could mask interrupts by being entered with them masked would
/// be able to monopolise the CPU without executing a single privileged instruction.
///
/// Built from a constant rather than by editing a saved `SPSR`, because the field this
/// controls is the one that decides *which exception level `eret` returns to*.
const SPSR_EL0T: u64 = 0;

/// `SPSR_EL1.M[3:0]` mask, and the EL0t value it must hold for a return to userspace.
const SPSR_M_MASK: u64 = 0b1111;
const SPSR_M_EL0T: u64 = 0b0000;

/// A build error if [`SPSR_EL0T`] is ever edited to something that is not EL0t.
///
/// `M = 0b0100` would `eret` to **EL1** at whatever `ELR_EL1` holds — a privilege
/// escalation costing one field — and `M = 0b0001` is Reserved, which sets `PSTATE.IL` and
/// halts the node on the next instruction fetch. Both were reproduced deliberately by
/// 8.spike. This catches the edit; see `enter_el0`'s docs for why it is not, and cannot be,
/// a runtime guard while the value is a constant.
const _: () = assert!(
    SPSR_EL0T & SPSR_M_MASK == SPSR_M_EL0T,
    "SPSR_EL0T is not EL0t — enter_el0's eret would not reach userspace"
);

/// Enter EL0 at `entry` with stack `sp`, and do not come back.
///
/// ## Why this validates `SPSR_EL1` immediately before `eret`
///
/// On x86, `sysretq` returns to ring 3 *by construction* — the instruction encodes the
/// privilege change. On aarch64 the target exception level is a **data field in a
/// clobberable system register**, and `eret` obeys it. That makes this the more dangerous
/// of the two, not the less, in two distinct ways:
///
/// **Privilege escalation.** `M = 0b0000` is EL0t; `M = 0b0100` is EL1t.
/// `IllegalExceptionReturn` rejects only returns to a *higher* EL, so EL1→EL1 is
/// perfectly legal. One flipped bit and `eret` returns to **EL1** at whatever `ELR_EL1`
/// holds. 8.spike confirmed this by doing it deliberately: redirecting an `eret` from EL0
/// to EL1 took exactly one field.
///
/// **A user-reachable node halt.** `M = 0b0001` is Reserved. `SetPSTATEFromPSR` then takes
/// the `illegal_psr_state` branch: it sets `PSTATE.IL` and *skips* the assignment of
/// `PSTATE.EL`/`PSTATE.SP`, so the PE stays at EL1 on `SP_EL1` and the next instruction
/// fetch raises Illegal Execution State — EL1 to EL1, vector slot 4, which this kernel
/// treats as fatal. 8.spike reproduced it: `ESR 0x3a000000` (EC 0x0E), `SPSR 0x1003c5`.
///
/// So the value is checked against `SPSR_M_EL0T` — but **at compile time, not before the
/// `eret`**, and it is worth being exact about what that does and does not buy.
///
/// The check is `const`: `SPSR_EL0T` is a constant, so the comparison is constant-folded
/// and costs nothing at runtime. It catches the failure mode that can actually happen here
/// — someone editing `SPSR_EL0T` to a value that is not EL0t — at the point of the edit.
///
/// It is **not** the aarch64 counterpart of the canonical-`RCX` check on the x86 syscall
/// exit path, and an earlier version of this comment claimed it was. That check tests a
/// runtime value an attacker controls. This one tests a literal against a literal. There
/// is no attacker-controlled input to `enter_el0` at all today: both `entry` and `sp` come
/// from the kernel, and the SPSR is a constant. A runtime read-back of `SPSR_EL1` between
/// the `msr` and the `eret` would be the honest analog, and is the right thing to add when
/// this function starts taking a caller-supplied PSTATE — at which point the value stops
/// being a constant and the compile-time check stops applying.
///
/// ## SError
///
/// `DAIFSet, #0xf` masks SError as well as IRQ/FIQ/Debug across the register writes.
/// Phase 7.2 unmasks SError at boot and 7.3 made every task inherit it unmasked, which is
/// safe today only because an SError at EL1 is fatal — so a clobbered `ELR`/`SPSR` never
/// gets a chance to matter. This sub-phase makes the lower-EL synchronous path
/// *resumable*, so that reasoning expires: an SError landing between these writes and the
/// `eret` would corrupt all of them and then return somewhere arbitrary. Masking the full
/// set across the tail is the cheap half of the fix; the other half — deciding whether
/// SError-at-EL1 stays fatal once EL0 exists — belongs with the fault-handling work.
///
/// # Safety
///
/// Transfers control to `entry` at EL0 with `sp` as its stack. Both must be mapped in the
/// currently installed `TTBR0_EL1` tree with appropriate permissions, and this never
/// returns to the caller.
pub unsafe fn enter_el0(entry: u64, sp: u64) -> ! {
    // SAFETY: masks all of DAIF, installs the EL0 context, and `eret`s. Nothing after the
    // mask can preempt, so ELR/SPSR/SP_EL0 cannot be rewritten by another task between
    // being set and being consumed.
    unsafe {
        core::arch::asm!(
            "msr DAIFSet, #0xf",
            "msr ELR_EL1, {entry}",
            "msr SP_EL0, {sp}",
            "msr SPSR_EL1, {spsr}",
            "eret",
            entry = in(reg) entry,
            sp = in(reg) sp,
            spsr = in(reg) SPSR_EL0T,
            options(noreturn, nostack),
        )
    }
}

// --- The EL0 payload ---
//
// Written as assembly the toolchain assembles, and copied byte-for-byte into a user page
// at test time. Two alternatives were rejected:
//
//   * Hard-coded instruction words. No assembler checks them, and a wrong encoding
//     produces an undefined-instruction abort from EL0 that reads like a broken vector
//     table rather than a typo in a constant.
//   * Mapping the kernel's own .text into the user tree. Kernel text lives in the high
//     half (TTBR1); EL0 cannot reach it at all, and making it reachable would be a far
//     larger hole than the test is worth.
//
// The payload must be position-independent, since it executes at a user VA unrelated to
// where it was linked. It uses only immediate `mov`, `svc`, SP-relative `str`/`ldr`, a
// PC-relative `adr`, and a self-relative `b` — every one of which is either absolute-free
// or relative to a register the kernel supplies, so it is.
//
// It also requires `sp` to arrive 16-byte aligned: `str x0, [sp, #-16]!` at EL0 raises an
// SP-alignment fault otherwise (SCTLR_EL1.SA0), which would present as a mysterious data
// abort from userspace. `EL0_STACK_VA` plus a page satisfies this.

core::arch::global_asm!(
    r#"
.section .rodata
.balign 4
.globl el0_payload_start
.globl el0_payload_end
.globl el0_payload_msg
.globl el0_payload_msg_end
el0_payload_start:
    // ADD(40, 2) -> x0 should come back 42.
    mov  x8, #3
    mov  x0, #40
    mov  x1, #2
    svc  #0

    // Park the result on the **user stack** rather than in a register, and read it back
    // after the next two syscalls. This is the only thing in the port that makes SP_EL0
    // load-bearing, and it is here on purpose: with the result stashed in x19 instead, a
    // review zeroed SP_EL0 on every single exception return and the self-test still
    // passed, because no EL0 code ever touched its stack. Now that mutation faults on the
    // reload — the payload is the test's only instrument for the frame's SP_EL0 slot, so
    // it has to actually use the register the slot exists to preserve.
    str  x0, [sp, #-16]!

    // DEBUG_PRINT(msg, len) — the message sits immediately after the code, and is
    // reached PC-relative so the payload works at whatever user VA it is copied to.
    mov  x8, #1
    adr  x0, el0_payload_msg
    mov  x1, #(el0_payload_msg_end - el0_payload_msg)
    svc  #0

    // EXIT(result of ADD, recovered from the stack). The self-test asserts this is 42,
    // which now proves two things at once: the *return value* travelled back to EL0 in x0,
    // and SP_EL0 survived the intervening syscall and any preemption inside it.
    ldr  x0, [sp], #16
    mov  x8, #2
    svc  #0

    // SYS_EXIT does not yet unwind the task, so spin rather than falling into whatever
    // follows. The self-test observes the exit status, not this loop.
1:  b    1b

el0_payload_msg:
    .ascii "[el0] hello from userspace\n"
el0_payload_msg_end:
el0_payload_end:
"#
);

unsafe extern "C" {
    static el0_payload_start: u8;
    static el0_payload_end: u8;
    static el0_payload_msg: u8;
    static el0_payload_msg_end: u8;
}

/// Length of the message the payload passes to `SYS_DEBUG_PRINT`.
///
/// Taken from the assembler's own symbols rather than written as a literal beside the
/// self-test that checks it. The test asserts that exactly this many bytes made it through
/// `copy_from_user`, so a literal here would turn any edit to the message into a spurious
/// failure — or, worse, would be "fixed" by adjusting the constant until it matched,
/// which is how an assertion quietly stops asserting.
pub fn msg_len() -> u64 {
    // No `unsafe` needed: `&raw const` only takes the addresses, and never reads through
    // them. Both symbols are defined by the `global_asm!` block above and bracket the
    // message bytes.
    (&raw const el0_payload_msg_end) as u64 - (&raw const el0_payload_msg) as u64
}

/// The assembled EL0 payload, as bytes to copy into a user page.
pub fn payload() -> &'static [u8] {
    // SAFETY: both symbols are defined by the `global_asm!` block above and bracket a
    // contiguous run of bytes in `.rodata`.
    unsafe {
        let start = &raw const el0_payload_start;
        let end = &raw const el0_payload_end;
        core::slice::from_raw_parts(start, end as usize - start as usize)
    }
}
