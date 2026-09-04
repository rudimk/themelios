//! # The EL0 preemption soak
//!
//! Phase 8's plan calls the exception-return race class its **riskiest unknown**, and for
//! a measured reason: the x86 equivalents (Phase 4.5's syscall-exit double-fault, the
//! stale GS base, the spurious IPC reply) were 2-in-10 flakes that grew to
//! majority-of-runs as the suite added tasks and syscalls. They were not found by a test
//! that ran a syscall once.
//!
//! ## The predicate, and why the obvious one is worthless
//!
//! "≥1000 syscalls under preemption is clean" would be satisfied by 1000 syscalls with
//! interrupts accidentally masked throughout — which is the *opposite* of what the soak
//! exists to establish, and would pass most brightly in exactly the broken configuration.
//! So three things are asserted, and each fails differently:
//!
//! 1. **Every return is checked, and derived from its arguments.** Each task runs
//!    `ADD(i, i)` for `i` in `1..=ITERATIONS` and accumulates the returns. The expected
//!    total is `ITERATIONS * (ITERATIONS + 1)`, so a single wrong return — one syscall
//!    returning another task's value, or `x0` surviving from the wrong frame — changes the
//!    exit code. This is what catches a corrupted exception return.
//! 2. **The tick advanced *while the soak ran*.** If the timer were masked for the
//!    duration, the loop would still complete and (1) would still pass. The span is
//!    measured from the first soak syscall to the last — not from before interrupts are
//!    unmasked, which credits the whole masked backlog in one go and made an earlier
//!    version of this predicate satisfiable by an artefact.
//! 3. **The two tasks overlapped in time.** Both tasks completing proves neither hung; it
//!    does *not* prove they ran concurrently, since A running to completion before B ever
//!    starts satisfies (1) and (2). Each task records the tick of its first and last
//!    syscall, and the soak asserts the two intervals intersect — the cheapest available
//!    evidence that the scheduler actually interleaved two EL0 address spaces.
//!
//! ## Which assertion goes red for which fault
//!
//! | injected fault | fails |
//! |---|---|
//! | exception return restores the wrong `x0` | (1) — wrong accumulator |
//! | `TTBR0_EL1` not switched between tasks | the payload faults; the run dies with an exception |
//! | timer masked / never re-armed | (2) — tick did not advance (the spin bound in the wait loop is what lets this be *reported* rather than hang) |
//! | scheduler runs each task to completion in turn | (3) — intervals do not overlap |
//! | a task never scheduled at all | its slot never records an exit |
//!
//! ## Two address spaces, deliberately
//!
//! Each task gets its **own** `AddressSpace`, mapping its own code and stack frames at the
//! same virtual addresses. That is the point: identical VAs backed by different physical
//! frames means a missed `TTBR0_EL1` switch is not a subtle corruption but an immediate
//! divergence, and it is the first thing in the port to depend on per-task address spaces
//! at all.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::time::tick_count;
use crate::mm::addr::VirtAddr;
use crate::mm::frame;
use crate::mm::page_table::{AddressSpace, PageFlags};
use crate::println;

/// Syscalls each soak task performs.
///
/// Sized by measurement, not guess. The first run used 2000 and both tasks completed
/// *inside a single 10 ms tick* — so the interval-overlap predicate correctly reported
/// that they had not interleaved, and would have kept reporting it however many times it
/// ran. A soak that finishes before the first preemption is not a soak. This spans tens of
/// ticks, so the scheduler has many opportunities to switch mid-loop.
///
/// Must also be encodable by a single `mov` immediate on aarch64, which means a 16-bit
/// value optionally shifted left by a multiple of 16 (the MOVZ form). 100_000 is not, and
/// the assembler rejects it with "expected compatible register or logical immediate";
/// 65_536 is `MOVZ #1, LSL #16`. Changing this constant to an unencodable value is a build
/// error, not a silent truncation, which is the right failure.
const ITERATIONS: u64 = 65_536;

/// The syscall-return half of the accumulator: `sum(2i for i in 1..=ITERATIONS)`.
///
/// Each task's actual exit code is this plus its own TLS base, so the two tasks assert on
/// different numbers and neither can pass on the other's result.
const EXPECTED_SUM: u64 = ITERATIONS * (ITERATIONS + 1);

/// Per-task `TPIDR_EL0` bases. Distinct, non-zero, and recognisable in a fault report.
///
/// Non-zero matters: every task's `tpidr_el0` field defaults to 0, so a TLS base of 0
/// would be indistinguishable from the scheduler never having restored anything.
const TLS_BASES: [u64; TASKS] = [0x5A5A_0000_0000_0001, 0x5A5A_0000_0000_0002];

/// Minimum ticks that must elapse across the soak for it to count as preemptive.
///
/// Deliberately low. The claim being made is "the timer was live and delivering", not
/// "the soak took a specific time" — a tight bound here would turn TCG's variable speed
/// into a flaky test, which is the failure mode the 7.3 fairness check was rewritten to
/// avoid.
const MIN_TICKS: u64 = 5;

/// User VAs for the soak payload. Same in both address spaces, backed by different frames.
const SOAK_CODE_VA: u64 = 0x0000_0000_0060_0000;
const SOAK_STACK_VA: u64 = 0x0000_0000_00a0_0000;

/// How many soak tasks run. Two is the minimum that can prove interleaving.
const TASKS: usize = 2;

/// Per-task observations, indexed by soak slot rather than task id so the array stays
/// small. `slot_for` maps a scheduler task id onto an index.
struct Slot {
    /// Scheduler task id occupying this slot, or `u64::MAX` when unused.
    task: AtomicU64,
    /// Syscalls this task has made.
    calls: AtomicU64,
    /// Tick of the first and most recent syscall, for the overlap check.
    first_tick: AtomicU64,
    last_tick: AtomicU64,
    /// The value passed to `SYS_EXIT`, and whether it arrived.
    exit_code: AtomicU64,
    exited: AtomicU64,
    /// The `TPIDR_EL0` this task observed via `SYS_GETTLS`, and the value it should have
    /// seen. Distinct per task, so a scheduler that fails to restore TLS hands one task
    /// the other's base and both mismatch.
    tls_seen: AtomicU64,
    tls_expected: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Slot {
            task: AtomicU64::new(u64::MAX),
            calls: AtomicU64::new(0),
            first_tick: AtomicU64::new(u64::MAX),
            last_tick: AtomicU64::new(0),
            exit_code: AtomicU64::new(0),
            exited: AtomicU64::new(0),
            tls_seen: AtomicU64::new(0),
            tls_expected: AtomicU64::new(0),
        }
    }
}

static SLOTS: [Slot; TASKS] = [Slot::new(), Slot::new()];

/// Whether the soak is running, so `dispatch` does no work in the common case.
static ACTIVE: AtomicU64 = AtomicU64::new(0);

/// Find the slot for a task id. Returns `None` when the soak is not running or the task is
/// not one of ours. Slots are *claimed* by `arm_task`; this only looks them up.
fn slot_for(task: u64) -> Option<&'static Slot> {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return None;
    }
    for s in SLOTS.iter() {
        if s.task.load(Ordering::Relaxed) == task {
            return Some(s);
        }
    }
    None
}

/// Record one syscall from an EL0 soak task. Called from the syscall dispatch path.
///
/// Reads the current task through the per-CPU block rather than the scheduler, because
/// this sits on a 131k-call path and a lock acquisition per syscall would dominate it.
///
/// Not, as an earlier version of this comment claimed, to avoid a deadlock: `SCHEDULER` is
/// an `InterruptMutex` that masks interrupts for the whole hold, and an `svc` only ever
/// arrives from EL0, so there is no interleaving in which a syscall lands while the
/// scheduler lock is held. The 7.3 precedent it cited is real but applies to the *fault
/// reporter*, which can run from inside `schedule()`; this cannot.
pub fn note_syscall() {
    let Some(pc) = crate::arch::aarch64::percpu::snapshot() else {
        return;
    };
    let Some(slot) = slot_for(pc.current_task) else {
        return;
    };
    let t = tick_count();
    slot.calls.fetch_add(1, Ordering::Relaxed);
    // `first_tick` is only ever lowered, from its u64::MAX initial value.
    let _ = slot
        .first_tick
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            if t < cur { Some(t) } else { None }
        });
    slot.last_tick.store(t, Ordering::Relaxed);
}

/// Record the `TPIDR_EL0` a soak task observed. No-op for non-soak tasks.
pub fn note_tls(value: u64) {
    let Some(pc) = crate::arch::aarch64::percpu::snapshot() else {
        return;
    };
    if let Some(slot) = slot_for(pc.current_task) {
        slot.tls_seen.store(value, Ordering::Relaxed);
    }
}

/// Record a soak task's `SYS_EXIT`. Returns true if this was a soak task.
pub fn note_exit(code: u64) -> bool {
    let Some(pc) = crate::arch::aarch64::percpu::snapshot() else {
        return false;
    };
    let Some(slot) = slot_for(pc.current_task) else {
        return false;
    };
    slot.exit_code.store(code, Ordering::Relaxed);
    slot.exited.store(1, Ordering::Release);
    true
}

// --- The soak payload ---
//
// Position-independent, like the 8.4b self-test payload, and for the same reason: it runs
// at a user VA unrelated to where it was linked. It uses only immediate `mov`, `svc`,
// register arithmetic and a self-relative branch.
//
// x19 accumulates the returns and x20 counts down. Both are callee-saved in AAPCS64, which
// is irrelevant here — nothing in this payload makes a call — but they are also *not*
// touched by the kernel's syscall path, which matters: the exception exit restores all of
// x0-x30 from the frame, so a value surviving in x19 across an `svc` is itself evidence
// that the frame round-tripped intact.
core::arch::global_asm!(
    r#"
.section .rodata
.balign 4
.globl soak_payload_start
.globl soak_payload_end
soak_payload_start:
    mov  x19, #0            // accumulator
    mov  x20, #{iters}      // countdown

    // Read this task's TLS base and park it on the *user stack* for the duration.
    //
    // Two mechanisms get their only coverage from these four instructions. `GETTLS`
    // returns the live TPIDR_EL0, which is the sole reader of the per-task TLS the
    // scheduler restores on every switch — poisoning that machinery previously left the
    // whole suite green. And holding the value on the stack across 65536 syscalls makes
    // SP_EL0 load-bearing *here*, which an earlier version of this payload did not: it
    // never touched its stack, so the page mapped for it was decoration.
    mov  x8, #6             // SYS_GETTLS
    svc  #0
    str  x0, [sp, #-16]!
1:
    mov  x8, #3             // SYS_ADD
    mov  x0, x20
    mov  x1, x20
    svc  #0
    add  x19, x19, x0       // accumulate the return
    subs x20, x20, #1
    b.ne 1b

    // Reload the TLS value and fold it into the accumulator, so a lost SP_EL0 or a
    // mis-restored TPIDR_EL0 both change the single number the soak asserts on.
    ldr  x1, [sp], #16
    add  x19, x19, x1

    mov  x0, x19            // EXIT(accumulator)
    mov  x8, #2
    svc  #0
2:  b    2b                 // unreachable: SYS_EXIT blocks the task and never returns
soak_payload_end:
"#,
    iters = const ITERATIONS,
);

unsafe extern "C" {
    static soak_payload_start: u8;
    static soak_payload_end: u8;
}

fn payload() -> &'static [u8] {
    // SAFETY: both symbols are defined by the `global_asm!` above and bracket a contiguous
    // run of bytes in `.rodata`.
    unsafe {
        let start = &raw const soak_payload_start;
        let end = &raw const soak_payload_end;
        core::slice::from_raw_parts(start, end as usize - start as usize)
    }
}

/// Build one soak task's address space and hand it to the scheduler.
///
/// Returns `true` on success. The address space is deliberately leaked: there is no EL0
/// task teardown, and the task is still blocked inside its own tree when the soak
/// finishes.
fn arm_task(slot_index: usize, task_id: usize) -> bool {
    let bytes = payload();
    // `el0_selftest` asserts this for its payload and `arm_task` did not, which is the
    // kind of asymmetry that survives until the payload grows past a page.
    assert!(
        bytes.len() <= crate::mm::PAGE_SIZE as usize,
        "soak payload is {} bytes, larger than the single page it is copied into",
        bytes.len()
    );

    let (Some(code_frame), Some(stack_frame)) = (frame::allocate_frame(), frame::allocate_frame())
    else {
        println!("[soak] FAIL — out of frames arming task {}", task_id);
        return false;
    };
    // Allocated only after the frames are secured, so the OOM path leaks nothing.
    let space = AddressSpace::new_user();
    // SAFETY: freshly allocated frame, reachable through the HHDM, exclusively ours.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), code_frame.as_mut_ptr::<u8>(), bytes.len());
    }

    space.map_page(
        VirtAddr::new(SOAK_CODE_VA),
        code_frame,
        PageFlags::PRESENT.union(PageFlags::USER),
    );
    space.map_page(
        VirtAddr::new(SOAK_STACK_VA),
        stack_frame,
        PageFlags::PRESENT
            .union(PageFlags::WRITABLE)
            .union(PageFlags::USER)
            .union(PageFlags::NO_EXECUTE),
    );

    SLOTS[slot_index].task.store(task_id as u64, Ordering::Relaxed);
    SLOTS[slot_index]
        .tls_expected
        .store(TLS_BASES[slot_index], Ordering::Relaxed);
    crate::sched::set_task_user_space(task_id, space.root_phys().as_u64(), space.asid());
    crate::sched::set_task_tpidr_el0(task_id, TLS_BASES[slot_index]);
    // The space must outlive the task, which never exits. See the doc above.
    core::mem::forget(space);
    true
}

/// Entry point for a soak task: drop to EL0 and never come back.
fn soak_entry() {
    // The scheduler has already installed this task's TTBR0 on the switch that got us
    // here, so the payload's VAs resolve.
    let sp = SOAK_STACK_VA + crate::mm::PAGE_SIZE;
    // SAFETY: code and stack are mapped in this task's installed tree with the permissions
    // the payload needs.
    unsafe { crate::arch::aarch64::syscall::enter_el0(SOAK_CODE_VA, sp) }
}

/// Run the soak. Returns true if all three predicates hold.
pub fn run() -> bool {
    // The precondition `run()` depends on, asserted rather than assumed. Tasks must not be
    // schedulable between `spawn` and `ACTIVE`/`set_task_user_space`, and the only thing
    // guaranteeing that is the caller having interrupts masked. If it ever stops holding, a
    // soak task enters EL0 with `ttbr0_root == 0` and the node takes a fatal abort — an
    // invisible cross-function coupling of exactly the kind `percpu::selftest` guards with
    // the same one-line check.
    debug_assert!(
        !crate::arch::irq::are_enabled(),
        "el0_soak::run() requires interrupts masked on entry"
    );
    let start_tick = tick_count();

    // Spawn first, then arm: `set_task_user_space` needs the task to exist, and the task
    // must not be scheduled before its address space is attached — which holds because
    // interrupts are masked here and `spawn` does not yield.
    let mut ids = [0usize; TASKS];
    for (i, id) in ids.iter_mut().enumerate() {
        *id = crate::sched::spawn("el0-soak", soak_entry);
        if !arm_task(i, *id) {
            return false;
        }
    }
    ACTIVE.store(1, Ordering::Release);

    // Let them run. Interrupts must be on for the tick to advance and for preemption to
    // happen at all — the same trap 8.4b's verifier fell into twice, where a bound was
    // waited on while the timer that advances it was masked.
    crate::arch::irq::enable();
    // **Two bounds, because one of them is the thing under test.** The tick bound is the
    // meaningful one; the spin bound exists because a review deleted the `irq::enable()`
    // above — the exact fault the doc table's "timer masked / never re-armed" row names —
    // and found the run did not fail, it *hung*: `tick_count()` is advanced only by the
    // timer, so a dead timer makes a tick-bounded wait unterminating. The harness then
    // reports a 120 s timeout and "the kernel hung", which is the least informative
    // diagnostic it has, for one of the most informative faults.
    //
    // The spin bound is deliberately enormous relative to the work: it is a liveness
    // backstop, not a schedule. Reaching it means the tick is not advancing, which the
    // predicate below then reports as itself.
    let deadline = tick_count() + 600;
    // The second bound is on **`CNTVCT_EL0`**, not on a spin count. The physical counter
    // advances whether or not interrupts are masked, so it bounds wall time directly;
    // a spin count does not, and the first attempt at this used one — two billion
    // iterations, which under TCG is far past the harness's own 120 s timeout, so the
    // mutation still hung and the fix did nothing. 7.2's timer self-test reaches for
    // `read_cntvct` as its escape hatch for exactly this reason.
    let counter_deadline =
        super::timer::read_cntvct().wrapping_add(10 * super::timer::frequency_hz());
    while tick_count() < deadline && super::timer::read_cntvct() < counter_deadline {
        if SLOTS.iter().all(|s| s.exited.load(Ordering::Acquire) == 1) {
            break;
        }
        core::hint::spin_loop();
    }
    crate::arch::irq::disable();
    ACTIVE.store(0, Ordering::Release);

    // **Measured from the soak's own first syscall to its last**, not from `start_tick`.
    //
    // `start_tick` is sampled while interrupts are still masked, and `timer::handle_tick`
    // credits one tick *per elapsed period* — so the first IRQ after the unmask dumps the
    // entire masked backlog into the counter at once. A review instrumented it: 87 of the
    // ~197 ticks this used to report were credited before the soak executed a single
    // instruction. `MIN_TICKS = 5` was therefore satisfied seventeen times over by an
    // artefact of the window that *precedes* the soak, and would still have been satisfied
    // if the timer had died the instant it was unmasked.
    let first = SLOTS
        .iter()
        .map(|s| s.first_tick.load(Ordering::Relaxed))
        .min()
        .unwrap_or(u64::MAX);
    let last = SLOTS
        .iter()
        .map(|s| s.last_tick.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    let elapsed = last.saturating_sub(first);
    let _ = start_tick; // retained for the boot log's benefit only
    let mut ok = true;

    // (1) every return checked, derived from its arguments
    for (i, s) in SLOTS.iter().enumerate() {
        if s.exited.load(Ordering::Acquire) != 1 {
            println!(
                "[soak] FAIL — task {} (slot {}) never reached SYS_EXIT after {} calls",
                s.task.load(Ordering::Relaxed),
                i,
                s.calls.load(Ordering::Relaxed)
            );
            ok = false;
            continue;
        }
        // Per-task expectation: the syscall returns plus *this* task's TLS base, which
        // the payload reloaded from its user stack. One number covering three mechanisms —
        // a wrong return value, a lost SP_EL0, or a mis-restored TPIDR_EL0 each change it.
        let tls_expected = s.tls_expected.load(Ordering::Relaxed);
        let expected = EXPECTED_SUM.wrapping_add(tls_expected);
        let got = s.exit_code.load(Ordering::Relaxed);
        if got != expected {
            println!(
                "[soak] FAIL — task {} accumulated {}, expected {} ({} syscalls; a return \
                 value, SP_EL0, or TPIDR_EL0 did not survive)",
                s.task.load(Ordering::Relaxed),
                got,
                expected,
                s.calls.load(Ordering::Relaxed)
            );
            ok = false;
        }

        // The TLS check, stated separately so a mismatch names the cause directly rather
        // than only shifting the accumulator. A scheduler that does not restore TPIDR_EL0
        // hands one task the other's base, so both slots report it.
        let tls_seen = s.tls_seen.load(Ordering::Relaxed);
        if tls_seen != tls_expected {
            println!(
                "[soak] FAIL — task {} read TPIDR_EL0 {:#x}, expected {:#x} (the scheduler \
                 did not restore this task's TLS base)",
                s.task.load(Ordering::Relaxed),
                tls_seen,
                tls_expected
            );
            ok = false;
        }
    }

    // (2) the tick advanced, so "preemption" is not a claim about a frozen timer
    if elapsed < MIN_TICKS {
        println!(
            "[soak] FAIL — only {} ticks elapsed (need {}); the timer was not delivering, \
             so nothing here was preemptive",
            elapsed, MIN_TICKS
        );
        ok = false;
    }

    // (3) the two tasks overlapped, so the scheduler interleaved them
    let (a_lo, a_hi) = (
        SLOTS[0].first_tick.load(Ordering::Relaxed),
        SLOTS[0].last_tick.load(Ordering::Relaxed),
    );
    let (b_lo, b_hi) = (
        SLOTS[1].first_tick.load(Ordering::Relaxed),
        SLOTS[1].last_tick.load(Ordering::Relaxed),
    );
    // Strict, and by more than a single tick. `a_lo > b_hi || b_lo > a_hi` treats a
    // *shared boundary tick* as overlap, so two tasks running strictly in sequence — A
    // finishing and B starting inside the same 10 ms tick — would pass the very check
    // written to catch them. Requiring the intersection to span at least two ticks closes
    // that, and is still far below the ~75 ticks a genuinely interleaved run produces.
    let overlap_lo = a_lo.max(b_lo);
    let overlap_hi = a_hi.min(b_hi);
    if overlap_lo > b_hi || overlap_lo > a_hi || overlap_hi.saturating_sub(overlap_lo) < 2 {
        println!(
            "[soak] FAIL — task intervals [{}, {}] and [{}, {}] do not overlap; the tasks \
             ran one after the other, not interleaved",
            a_lo, a_hi, b_lo, b_hi
        );
        ok = false;
    }

    if ok {
        println!(
            "[soak] PASS ({} syscalls across {} EL0 tasks in separate address spaces, \
             every return checked, each task's TPIDR_EL0 and SP_EL0 verified; {} ticks \
             elapsed during the soak; intervals [{}, {}] and [{}, {}] overlap)",
            SLOTS.iter().map(|s| s.calls.load(Ordering::Relaxed)).sum::<u64>(),
            TASKS,
            elapsed,
            a_lo,
            a_hi,
            b_lo,
            b_hi
        );
    }
    ok
}
