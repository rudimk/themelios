//! # aarch64 per-CPU data (`TPIDR_EL1`)
//!
//! The aarch64 counterpart to the x86_64 `PerCpu` block that
//! [`crate::arch::x86_64::syscall`] reaches through the GS base: a small structure of
//! state that belongs to *the CPU*, not to a task, holding whatever the currently
//! running task is. `TPIDR_EL1` — the EL1 software thread-ID register, which the
//! architecture reserves for exactly this and gives no other meaning — points at it.
//!
//! ## Why this exists before there is an EL0 to need it
//!
//! On x86_64 the per-CPU block is load-bearing from the first ring-3 entry: the
//! `syscall` instruction changes neither the stack pointer nor any segment base, so the
//! entry stub has nowhere to *get* a kernel stack from except `gs:[0]`. aarch64 has no
//! such gap — an exception at EL1 lands on `SP_EL1`, which the CPU has already loaded —
//! so nothing in a ring-0-only kernel is *forced* through a per-CPU pointer.
//!
//! What makes it worth plumbing now is the failure mode it forecloses. Phase 4.5
//! root-caused a bug (CLAUDE.md, "Stale GS base") where the GS base — a single global
//! register that `switch_context` does not save or restore — could still hold the
//! *previous* context's value after a context switch, because it was only refreshed on
//! the paths that seemed to need it. `TPIDR_EL1` is a global register with exactly the
//! same shape, and would have exactly the same bug available to it.
//!
//! The fix that worked there was structural rather than local: write the register on
//! **every** context switch, unconditionally, so "stale" is not a state the system can
//! be in. [`on_context_switch`] does that, from the same place in `schedule()` that
//! calls x86_64's `refresh_kernel_gs_base`.
//!
//! To be precise about what that buys today, since the comment is easy to overstate:
//! nothing currently writes `TPIDR_EL1` except this module, so at present the register
//! could not go stale even if the write were conditional. The unconditional write is
//! what keeps that true once EL0 lands and the register acquires a second writer.
//!
//! ## Reading it back through the register
//!
//! [`current`] deliberately loads `TPIDR_EL1` and dereferences *that*, rather than
//! returning `&PER_CPU` directly. Taking the static's address would be faster and
//! obviously correct — and would make the register decorative, since a `TPIDR_EL1` that
//! was never written, or written with a wrong value, would read back fine. Going
//! through the register means every per-CPU access is a live test of it, and
//! [`selftest`] can check the two agree.
//!
//! ## Contents
//!
//! Only fields that something reads today:
//!
//! - `current_task` — read by the exception reporter, which must name the faulting task
//!   *without* taking the scheduler lock. A fault can happen while that lock is held
//!   (`schedule()` runs with it), and a handler that blocks on it would deadlock the
//!   machine at precisely the moment diagnostics matter most.
//! - `kernel_stack_top` / `kernel_stack_limit` — the running task's stack bounds, used
//!   to tell a stack overflow apart from an ordinary wild pointer. aarch64 has no
//!   IST/TSS analog, so the handler runs on the *overflowing* stack; naming the
//!   condition explicitly is the only warning that will ever be printed.
//! - `switches` — context switches performed, which is what lets [`selftest`] prove the
//!   per-switch write actually happens rather than assuming it.

use core::arch::asm;

use crate::println;

/// Per-CPU state, addressed through `TPIDR_EL1`.
///
/// `#[repr(C)]` and the offset assertions below pin the layout. Nothing in assembly
/// reads it *yet*, but the EL0 entry stub will — that is the whole reason the pointer
/// lives in a register rather than being a plain static — and an entry stub that reads
/// the wrong offset is the kind of bug that corrupts silently. Pinning the layout now
/// costs nothing and makes a future reordering a build error.
#[repr(C)]
pub struct PerCpu {
    /// Task ID currently scheduled on this CPU.
    pub current_task: u64,
    /// Top of that task's kernel stack (exclusive; the stack grows down from here).
    /// Zero for the bootstrap task, which runs on Limine's stack and has no bounds we
    /// know.
    pub kernel_stack_top: u64,
    /// Lowest address the stack may reach before it runs into its padding page.
    /// Zero when unknown, as above.
    pub kernel_stack_limit: u64,
    /// Context switches performed since boot.
    pub switches: u64,
}

// Field offsets, for the assembly that will eventually read them.
const PERCPU_CURRENT_TASK: usize = 0;
const PERCPU_KERNEL_STACK_TOP: usize = 8;
const PERCPU_KERNEL_STACK_LIMIT: usize = 16;

const _: () = assert!(core::mem::offset_of!(PerCpu, current_task) == PERCPU_CURRENT_TASK);
const _: () = assert!(core::mem::offset_of!(PerCpu, kernel_stack_top) == PERCPU_KERNEL_STACK_TOP);
const _: () =
    assert!(core::mem::offset_of!(PerCpu, kernel_stack_limit) == PERCPU_KERNEL_STACK_LIMIT);

/// The single per-CPU block (uniprocessor for now).
///
/// On SMP each core gets its own, and `TPIDR_EL1` is precisely the mechanism that lets
/// identical code on every core reach the right one — which is the other reason to
/// route access through the register rather than through this symbol.
static mut PER_CPU: PerCpu = PerCpu {
    current_task: 0,
    kernel_stack_top: 0,
    kernel_stack_limit: 0,
    switches: 0,
};

/// Read `TPIDR_EL1`.
#[inline]
pub fn read_tpidr() -> u64 {
    let v: u64;
    // SAFETY: reading TPIDR_EL1 has no side effects.
    unsafe { asm!("mrs {}, TPIDR_EL1", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Write `TPIDR_EL1`.
///
/// # Safety
///
/// `value` must be the address of a live [`PerCpu`], since [`current`] dereferences
/// whatever is in the register.
#[inline]
unsafe fn write_tpidr(value: u64) {
    // SAFETY: the caller guarantees `value` addresses a live PerCpu. No barrier is
    // needed: `TPIDR_EL1` is not a translation or memory-attribute control, so it has
    // no effect on any in-flight access — unlike `TTBR`/`MAIR`/`TCR`, which do and
    // therefore need an ISB.
    unsafe { asm!("msr TPIDR_EL1, {}", in(reg) value, options(nomem, nostack, preserves_flags)) };
}

/// Point `TPIDR_EL1` at the per-CPU block.
///
/// Must run before anything calls [`current`] — that is, before the scheduler starts
/// and before any exception can be taken by a handler that reports per-CPU state. A
/// `TPIDR_EL1` left at its reset value would be dereferenced as a pointer.
pub fn init() {
    let addr = &raw const PER_CPU as u64;
    // SAFETY: `addr` is the address of the static above, which lives for the whole
    // program.
    unsafe { write_tpidr(addr) };
    println!("[percpu] TPIDR_EL1 -> {:#018x} (per-CPU block)", addr);
}

/// The per-CPU block for the running CPU, reached through `TPIDR_EL1`.
///
/// # Safety
///
/// [`init`] must have run. Callers must not hold a reference across a context switch:
/// the contents describe *whichever* task is running now, so a reference taken before a
/// switch describes the wrong one afterwards.
#[inline]
pub unsafe fn current() -> &'static PerCpu {
    let base = read_tpidr();
    debug_assert!(base != 0, "TPIDR_EL1 read before percpu::init()");
    // SAFETY: `init` put the address of a `'static` PerCpu here, and this is the only
    // writer of the register.
    unsafe { &*(base as *const PerCpu) }
}

/// Record a context switch: point `TPIDR_EL1` at the per-CPU block and describe the
/// task that is about to run.
///
/// Called from `schedule()` on **every** switch, including switches to kernel-only
/// tasks such as idle and bootstrap. That "including" is the entire point, and is the
/// lesson of the Phase 4.5 stale-GS-base bug: the x86 version of this was originally
/// called only from the paths that appeared to need it, which left the register holding
/// a previous context's value on every other path. Refreshing unconditionally is one
/// `msr` and removes the failure mode instead of narrowing it.
///
/// Passing zero for the stack bounds means "unknown" (the bootstrap task, which runs on
/// the bootloader's stack); the overflow check then simply does not fire.
///
/// # Safety
///
/// Must be called with interrupts masked — that is, from inside `schedule()`. The three
/// stores below are not atomic with respect to an observer, so an exception taken
/// between them would read a half-updated block and mis-report the faulting task.
/// `schedule()` guarantees the masking; see its contract.
pub unsafe fn on_context_switch(task_id: u64, stack_top: u64, stack_limit: u64) {
    let per_cpu = &raw mut PER_CPU;

    // Re-establish the pointer itself, not just the contents. `switch_context` saves
    // and restores x19-x30 and nothing else, so `TPIDR_EL1` survives a switch untouched
    // — which is exactly why it must be treated as something that can be stale rather
    // than as something known-good.
    // SAFETY: address of a 'static.
    unsafe { write_tpidr(per_cpu as u64) };

    // SAFETY: uniprocessor, interrupts masked by the caller, and the only other reader
    // of this block is an exception handler — which cannot run here.
    unsafe {
        (*per_cpu).current_task = task_id;
        (*per_cpu).kernel_stack_top = stack_top;
        (*per_cpu).kernel_stack_limit = stack_limit;
        (*per_cpu).switches += 1;
    }
}

/// Context switches recorded so far.
pub fn switch_count() -> u64 {
    // SAFETY: a single aligned u64 read on a uniprocessor; a torn value is not possible
    // and a stale one is acceptable for a diagnostic counter.
    unsafe { (*(&raw const PER_CPU)).switches }
}

/// Classify a stack pointer against the running task's stack bounds.
///
/// Returns `None` when the bounds are unknown (the bootstrap task) or when `sp` is
/// inside them. Returns a description when it is not — which is the case worth naming,
/// because the handler that calls this is *itself* running on the stack in question.
pub fn stack_overflow_hint(sp: u64) -> Option<&'static str> {
    // SAFETY: `init` has run by the time any exception can be taken; see `current`.
    let pc = unsafe { current() };
    let (top, limit) = (pc.kernel_stack_top, pc.kernel_stack_limit);
    if top == 0 || limit == 0 {
        return None;
    }
    if sp < limit {
        // Below the usable stack means it has run into the padding page reserved
        // beneath it — the allocator owns that page, so this is a genuine overflow and
        // not a wild pointer that happens to be low.
        Some("stack pointer is BELOW this task's stack — kernel stack overflow")
    } else if sp > top {
        Some("stack pointer is ABOVE this task's stack top — corrupted SP")
    } else {
        None
    }
}

/// Prove the per-CPU pointer is real: that `TPIDR_EL1` addresses the block, that the
/// scheduler updates it on switches, and that what it says agrees with the scheduler.
///
/// The agreement check is the one that matters. `current_task` is written by
/// `schedule()` and read back through the register, so a `TPIDR_EL1` that was never
/// refreshed, or refreshed with the wrong address, disagrees with
/// [`crate::sched::current_task_id`] — which reads the scheduler's own state through a
/// different path entirely.
pub fn selftest() -> bool {
    let tpidr = read_tpidr();
    let expected = &raw const PER_CPU as u64;

    if tpidr != expected {
        println!(
            "[selftest] percpu: FAIL — TPIDR_EL1 is {:#018x}, expected {:#018x}",
            tpidr, expected
        );
        return false;
    }

    // SAFETY: checked non-null and equal to the static's address just above.
    let pc = unsafe { current() };
    let seen_task = pc.current_task;
    let real_task = crate::sched::current_task_id() as u64;
    let switches = pc.switches;

    // The scheduler must have switched by now — the self-test runs after the round-robin
    // test, which measured dozens of slices. Zero here means `on_context_switch` is
    // never being called, which no other check in this file would notice.
    if switches == 0 {
        println!(
            "[selftest] percpu: FAIL — no context switches recorded; \
             `on_context_switch` is not wired into schedule()"
        );
        return false;
    }

    if seen_task != real_task {
        println!(
            "[selftest] percpu: FAIL — TPIDR_EL1 block says task {}, scheduler says {}",
            seen_task, real_task
        );
        return false;
    }

    println!(
        "[selftest] percpu: PASS (TPIDR_EL1 -> {:#018x}, task {} agrees with the \
         scheduler, {} switches recorded)",
        tpidr, seen_task, switches
    );
    true
}

/// Assert that interrupts are masked, for the exception-return tail.
///
/// The aarch64 analog of the Phase 4.5 "syscall-exit double-fault" fix, which made the
/// x86 exit tail atomic by masking interrupts across it.
///
/// The shared state at risk here is different but the shape is the same. `ELR_EL1` and
/// `SPSR_EL1` are *single registers*, not per-task storage: the entry stub copies them
/// into the frame on the way in and writes them back on the way out, and between that
/// write-back and the `eret` they hold this task's return state. An exception taken in
/// that window would overwrite both with its own, and the `eret` would return to the
/// wrong place with the wrong PSTATE — the same class of bug as a shared RSP scratch
/// slot read with interrupts enabled.
///
/// Today the window is closed by construction: the CPU masks `DAIF.I` on exception
/// entry and nothing in the handler path unmasks it, including the `schedule()` call,
/// which is entered and left masked. That is a property worth *checking* rather than
/// assuming, because it is invisible in the source — no line says "interrupts are off
/// here", and a future handler that enables them to do something slow would break the
/// exit tail with no other symptom than rare, impossible-looking returns.
#[inline]
pub fn debug_assert_irqs_masked(where_: &str) {
    if cfg!(debug_assertions) {
        let daif: u64;
        // SAFETY: reading DAIF has no side effects.
        unsafe { asm!("mrs {}, DAIF", out(reg) daif, options(nomem, nostack)) };
        // DAIF bit 7 is I (IRQ mask). Set means masked.
        assert!(
            daif & (1 << 7) != 0,
            "{}: interrupts are unmasked inside the exception path — the ELR/SPSR \
             write-back before `eret` is no longer atomic",
            where_
        );
    }
}
