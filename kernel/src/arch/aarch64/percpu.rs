//! # aarch64 per-CPU data (`TPIDR_EL1`)
//!
//! The aarch64 counterpart to the x86_64 `PerCpu` block that
//! [`crate::arch::x86_64::syscall`] reaches through the GS base: a small structure of
//! state that belongs to *the CPU*, not to a task, describing whichever task is
//! currently running. `TPIDR_EL1` — the EL1 software thread-ID register, which the
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
//! this module is currently the only writer of `TPIDR_EL1`, so at present the register
//! could not go stale even if the write were conditional. The unconditional write is
//! what keeps that true once EL0 lands and the register acquires a second writer. It is
//! nonetheless *tested* rather than assumed — see [`selftest`], which poisons the
//! register and requires a context switch to have repaired it.
//!
//! ## Reading it back through the register
//!
//! [`snapshot`] loads `TPIDR_EL1` and dereferences *that*, rather than reading
//! `PER_CPU` directly. Naming the static would be faster and obviously correct — and
//! would make the register decorative, since a `TPIDR_EL1` that was never written, or
//! written with a wrong value, would read back fine. Going through the register makes
//! every per-CPU access a live test of it.
//!
//! It returns a *copy* rather than a reference. The block describes whichever task is
//! running **now**, so a borrow held across a context switch silently describes a
//! different task; handing out `&'static PerCpu` from a `static mut` would let the type
//! system promise a lifetime the data does not have. A `Copy` snapshot cannot go stale
//! in the caller's hands — it can only be *old*, which is obvious at the use site.
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
//!   per-switch update happens rather than assuming it.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::println;

/// Per-CPU state, addressed through `TPIDR_EL1`.
///
/// `#[repr(C)]` and the offset assertions below pin the layout. Nothing in assembly
/// reads it *yet*, but the EL0 entry stub will — that is the whole reason the pointer
/// lives in a register rather than being a plain static — and an entry stub that reads
/// the wrong offset is the kind of bug that corrupts silently. Pinning the layout now
/// costs nothing and makes a future reordering a build error.
#[repr(C)]
#[derive(Clone, Copy)]
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

/// A second, never-installed block used only by [`selftest`] as a poison value.
///
/// The self-test parks a *wrong but valid* pointer in `TPIDR_EL1` to check that a
/// context switch repairs it. Pointing at real, mapped, correctly-typed memory means
/// that if anything does read the register during that brief window, it reads
/// recognisable nonsense instead of faulting on a garbage address — a test for a
/// diagnostic facility should not be able to take the machine down.
static mut DECOY_PER_CPU: PerCpu = PerCpu {
    current_task: u64::MAX,
    kernel_stack_top: 0,
    kernel_stack_limit: 0,
    switches: 0,
};

/// Set once [`init`] has pointed `TPIDR_EL1` at [`PER_CPU`].
///
/// `TPIDR_EL1`'s reset value is architecturally UNKNOWN, so before `init` the register
/// is not a pointer at all. This flag is what lets [`snapshot`] be a *safe* function:
/// it answers "is the register meaningful yet" without the caller having to know the
/// boot order. That matters most for the fault path, which is exactly where a caller
/// is least able to reason about how far boot got.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

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
/// `value` must be the address of a live [`PerCpu`], since [`snapshot`] dereferences
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
/// Must run before anything expects [`snapshot`] to return data — that is, before the
/// scheduler starts and before the exception reporter is relied on to name a task.
/// Until it does, `snapshot` returns `None` rather than dereferencing a register whose
/// reset value the architecture leaves UNKNOWN.
pub fn init() {
    let addr = &raw const PER_CPU as u64;
    // SAFETY: `addr` is the address of the static above, which lives for the whole
    // program.
    unsafe { write_tpidr(addr) };
    INITIALIZED.store(true, Ordering::Release);
    println!("[percpu] TPIDR_EL1 -> {:#018x} (per-CPU block)", addr);
}

/// A copy of the per-CPU block for the running CPU, read through `TPIDR_EL1`.
///
/// `None` before [`init`], when the register holds no meaningful address.
///
/// The value describes the task running *at the moment of the call*. It is a snapshot,
/// not a view: after a context switch it is history, and treating it as current is the
/// caller's bug rather than a memory-safety one.
#[inline]
pub fn snapshot() -> Option<PerCpu> {
    if !INITIALIZED.load(Ordering::Acquire) {
        return None;
    }
    let base = read_tpidr();
    // SAFETY: `INITIALIZED` is set only by `init`, and only after it has written the
    // address of a `'static` `PerCpu` into the register. `PerCpu` is `Copy` and
    // contains no padding of consequence, so this reads a complete, aligned value.
    Some(unsafe { *(base as *const PerCpu) })
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
/// Call it as late as possible — immediately before `switch_context`. Between this
/// call and the actual stack switch the block claims the incoming task is running while
/// the CPU is still on the outgoing task's stack, so an exception taken in that window
/// is reported against the wrong task and compared against the wrong stack bounds.
/// The window cannot be eliminated (something must write the block first) but it can be
/// made a couple of instructions long, which is what `schedule()` does.
///
/// Passing zero for the stack bounds means "unknown" (the bootstrap task, which runs on
/// the bootloader's stack); the overflow check then simply does not fire.
///
/// # Safety
///
/// Must be called with interrupts masked — that is, from inside `schedule()`. The four
/// field stores below are not atomic with respect to an observer, so an exception taken
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

/// Classify a stack pointer against the running task's stack bounds.
///
/// Returns `None` when the bounds are unknown (the bootstrap task), when the per-CPU
/// block is not up yet, or when `sp` is inside them. Returns a description when it is
/// not — which is the case worth naming, because the handler that calls this is
/// *itself* running on the stack in question.
pub fn stack_overflow_hint(sp: u64) -> Option<&'static str> {
    let pc = snapshot()?;
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

/// Entry function for the throwaway task [`selftest`] spawns. Returns immediately;
/// its only job is to exist so that a `schedule()` call has somewhere to switch to.
fn probe_entry() {}

/// Prove the per-CPU pointer is real, and that it is refreshed on **every** switch.
///
/// Three separate properties, because they fail independently:
///
/// 1. `TPIDR_EL1` addresses the block. Checked directly against the static's address.
/// 2. The block agrees with the scheduler. `current_task` is written by `schedule()`
///    and read back *through the register*, while
///    [`crate::sched::current_task_id`] reads the scheduler's own state through an
///    entirely different path; a register pointing somewhere wrong disagrees.
/// 3. **The per-switch `msr` actually happens.** This is the one worth the effort. An
///    earlier version of this test asserted it in prose and could not detect it:
///    `init()` already leaves the correct value in the register and nothing else writes
///    it, so deleting the `write_tpidr` from [`on_context_switch`] entirely would have
///    left every check passing. So the test now *poisons* the register with a decoy
///    block, forces a context switch, and requires the switch to have repaired it.
///    Nothing but the per-switch write can do that.
///
/// The poison window is held with interrupts masked and points at real, mapped memory
/// ([`DECOY_PER_CPU`]), and the register is restored unconditionally afterwards — so
/// even a `schedule()` that declines to switch cannot leave the machine poisoned.
pub fn selftest() -> bool {
    let expected = &raw const PER_CPU as u64;

    let tpidr = read_tpidr();
    if tpidr != expected {
        println!(
            "[selftest] percpu: FAIL — TPIDR_EL1 is {:#018x}, expected {:#018x}",
            tpidr, expected
        );
        return false;
    }

    let Some(pc) = snapshot() else {
        println!("[selftest] percpu: FAIL — snapshot() is None after init()");
        return false;
    };
    let real_task = crate::sched::current_task_id() as u64;

    // The scheduler must have switched by now — this runs after the round-robin test,
    // which measured dozens of slices. Zero means `on_context_switch` is never called
    // at all, which no other check here would notice.
    if pc.switches == 0 {
        println!(
            "[selftest] percpu: FAIL — no context switches recorded; \
             `on_context_switch` is not wired into schedule()"
        );
        return false;
    }

    if pc.current_task != real_task {
        println!(
            "[selftest] percpu: FAIL — TPIDR_EL1 block says task {}, scheduler says {}",
            pc.current_task, real_task
        );
        return false;
    }

    // --- Property 3: the per-switch write ---
    //
    // Give `schedule()` somewhere to go. By this point the round-robin workers are all
    // dead and the ready queue is empty, so a bare `schedule()` would take the
    // "next_id == current_id" early return and never reach `on_context_switch`.
    crate::sched::spawn("tpidr-probe", probe_entry);

    let decoy = &raw const DECOY_PER_CPU as u64;
    let switches_before = pc.switches;

    crate::arch::irq::disable();
    // SAFETY: `DECOY_PER_CPU` is a live, correctly-typed `PerCpu`, so the register
    // still points at readable memory for the duration; interrupts are masked, so the
    // only reader that could observe it (the exception reporter) cannot run.
    unsafe { write_tpidr(decoy) };
    // `schedule()` requires interrupts masked, which they are.
    crate::sched::schedule();
    let after = read_tpidr();
    // Restore unconditionally, before deciding anything. If the switch did not happen,
    // the register is still poisoned and must not be left that way.
    // SAFETY: `expected` is the address of the real block.
    unsafe { write_tpidr(expected) };
    crate::arch::irq::enable();

    let switched = snapshot().is_some_and(|p| p.switches > switches_before);
    if !switched {
        println!(
            "[selftest] percpu: FAIL — could not force a context switch, so the \
             per-switch TPIDR_EL1 write is unproven"
        );
        return false;
    }
    if after != expected {
        println!(
            "[selftest] percpu: FAIL — after a context switch TPIDR_EL1 is {:#018x}, \
             expected {:#018x}: `on_context_switch` is updating the block's fields but \
             not re-writing the register, so it can go stale once EL0 has a second \
             writer",
            after, expected
        );
        return false;
    }

    println!(
        "[selftest] percpu: PASS (TPIDR_EL1 -> {:#018x}, task {} agrees with the \
         scheduler, {} switches recorded, register repaired by a switch after being \
         poisoned)",
        expected, pc.current_task, pc.switches
    );
    true
}
