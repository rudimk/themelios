//! # aarch64 early boot
//!
//! Runs after Limine's higher-half handoff — the CPU is already at **EL1 with the MMU
//! enabled, caches on, stack set, and BSS zeroed** (confirmed by the Phase 7 boot
//! spike). So this is *not* a bare-metal reset: we inherit Limine's state and do the
//! minimum to get a console up, then bring the kernel-core subsystems online in
//! dependency order, each proved by a self-test before the next one starts:
//!
//! | Sub-phase | Brought up here                                     |
//! |-----------|-----------------------------------------------------|
//! | 7.0b      | PL011 UART mapped, serial console, banner           |
//! | 7.1       | Frame allocator, kernel page tables, heap           |
//! | 7.2       | `VBAR_EL1` vectors, GICv2, `CNTV` tick              |
//! | 7.3       | Scheduler: preemptive round-robin over kernel tasks |
//!
//! The self-tests are not decoration. This is a bring-up path with no debugger and no
//! test harness, where the failure mode of almost everything below is a silent hang —
//! so each stage prints a sentinel the CI smoke asserts on, and the smoke fails on the
//! specific `FAIL` line rather than on a timeout.
//!
//! ## FP/SIMD is deliberately left disabled
//!
//! Phase 7.0b built for the *hardfloat* `aarch64-unknown-none` target, where the
//! compiler lowers struct moves and `core::fmt` to SIMD, and so had to enable
//! `CPACR_EL1.FPEN` before the first `println!` or those lowerings trapped.
//!
//! The kernel now targets **`aarch64-unknown-none-softfloat`**, which emits no vector
//! instructions, so boot *disables* FP access rather than enabling it. That is not
//! merely simpler — it is what makes both the exception path and the context switch
//! sound. Neither saves any
//! vector register: the entry stub saves x0-x30, and `switch_context` saves x19-x30,
//! even though AAPCS64 makes the low half of `v8`-`v15` callee-saved. With softfloat
//! *and* `FPEN` trapping, any SIMD that ever sneaks in raises `ESR_EL1.EC = 0x07` —
//! which the exception decoder names — instead of corrupting another task's
//! floating-point state with no fault to point at.
//!
//! That backstop is **checked**, not assumed:
//! [`exceptions::verify_fp_trapped`](crate::arch::aarch64::exceptions::verify_fp_trapped)
//! reads `CPACR_EL1` at boot and the 7.3 sentinel depends on it. It was asserted in
//! three comments and read by nothing until Phase 7.3.
//!
//! One thing must still happen before the first `println!`:
//! 1. **Map the PL011 UART.** Limine's HHDM maps RAM but **not** device MMIO, so the
//!    UART at physical `0x0900_0000` is unmapped after handoff (spike-confirmed: both
//!    raw-phys and `HHDM + 0x0900_0000` data-abort). We add a single Device-`nGnRnE`
//!    page to the kernel tables (`TTBR1_EL1`) pointing at the UART, then install the
//!    serial writer at that virtual address.

use core::arch::asm;

/// PL011 MMIO base on QEMU `virt`.
const PL011_PHYS: u64 = 0x0900_0000;

// --- Page-table constants (4 KiB granule, 48-bit VA, 4-level) ---
/// Mask selecting the output-address bits [47:12] of a descriptor.
const ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;
/// Table/valid descriptor bits (valid + table/page).
const DESC_VALID: u64 = 1 << 0;
const DESC_TABLE: u64 = 1 << 1; // "table" at L0–L2, "page" at L3
const DESC_AF: u64 = 1 << 10; // access flag (else access faults)
const DESC_PXN: u64 = 1 << 53; // privileged execute-never
const DESC_UXN: u64 = 1 << 54; // unprivileged execute-never

/// A 4 KiB page table (512 × 8-byte descriptors), page-aligned.
#[repr(C, align(4096))]
struct Table([u64; 512]);

/// A tiny bump pool of page-table frames in `.bss`, used to populate any missing
/// intermediate levels for the one UART mapping (avoids depending on the frame
/// allocator this early). Four frames is plenty for a single 4 KiB mapping (≤3 new
/// tables). `.bss` is zeroed by Limine, so fresh tables start empty.
struct Pool {
    tables: [Table; 4],
    next: usize,
}
static mut POOL: Pool = Pool {
    tables: [const { Table([0; 512]) }; 4],
    next: 0,
};

/// Kernel virtual→physical translation, captured from Limine's executable-address
/// response so we can compute the physical address of a `.bss` page table.
#[derive(Clone, Copy)]
pub struct KernelAddr {
    pub phys_base: u64,
    pub virt_base: u64,
}

impl KernelAddr {
    /// Physical address of a kernel virtual address in the loaded image (linear map).
    fn phys_of(&self, virt: u64) -> u64 {
        virt - self.virt_base + self.phys_base
    }
}

#[inline(always)]
fn read_sysreg_ttbr1() -> u64 {
    let v: u64;
    // SAFETY: reading TTBR1_EL1 has no side effects.
    unsafe { asm!("mrs {}, TTBR1_EL1", out(reg) v, options(nomem, nostack)) };
    v
}


/// Switch to `SP_EL1` for ordinary kernel execution.
///
/// Limine hands off with **`SPSel = 0`**, meaning EL1 code runs on `SP_EL0` while
/// `SP_EL1` holds whatever the bootloader left there. That is not a stylistic detail —
/// it decides both *where exceptions are delivered* and *what stack they land on*:
///
/// - With `SPSel = 0`, a synchronous exception at EL1 goes to the **`0x000`** vector
///   group ("current EL with SP_EL0"), not the `0x200` group.
/// - On entry the CPU switches to `SP_EL1` regardless. If nothing has initialised it,
///   the entry stub's register save writes through a garbage pointer and takes a data
///   abort *inside the handler* — which then nests, lands in the `0x200` group, and
///   reports the nested syndrome. The original exception is never seen.
///
/// That failure is genuinely confusing from the outside: a `brk` presents as a data
/// abort at a fixed address, in the wrong vector slot, with an `SPSR` describing the
/// nested state rather than the interrupted one.
///
/// Copying the live stack pointer across and setting `SPSel = 1` makes the kernel run
/// on `SP_EL1` from here on, so exceptions are delivered to the `0x200` group with a
/// valid stack already loaded. SP is numerically unchanged, so this is invisible to
/// the surrounding Rust.
///
/// Note that handler and interrupted code then share one stack — there is no IST/TSS
/// analog on aarch64, so a kernel-stack overflow re-faults on the same stack. Accepted
/// for bring-up; a dedicated exception stack is future work.
#[inline(always)]
fn use_sp_el1() {
    // SAFETY: copies the current stack pointer into SP_EL1 and selects it, so the
    // numeric value of `sp` is unchanged across the sequence. The ISB ensures the
    // SPSel write has taken effect before the next instruction.
    unsafe {
        asm!(
            "mov x9, sp",
            "msr SPSel, #1",
            "mov sp, x9",
            "isb",
            out("x9") _,
            options(preserves_flags),
        );
    }
}


/// Allocate a zeroed page table from the pool; returns its kernel virtual address.
///
/// # Safety
/// Single-threaded early boot (UP, interrupts masked), so the non-atomic bump is
/// sound; panics via `None`-less indexing if the pool is exhausted (≤3 needed).
unsafe fn alloc_table() -> u64 {
    let pool = &mut *core::ptr::addr_of_mut!(POOL);
    let idx = pool.next;
    pool.next += 1;
    let t = &mut pool.tables[idx];
    // `.bss` is already zeroed, but be explicit.
    for e in t.0.iter_mut() {
        *e = 0;
    }
    t as *mut Table as u64
}

/// Map a single 4 KiB Device page at `virt` → `phys` into the current `TTBR1_EL1`
/// tables, allocating any missing intermediate tables from the pool.
///
/// # Safety
/// Edits live page tables; caller guarantees `virt` is a TTBR1 (upper-half) address
/// not already mapped, and that `hhdm` correctly direct-maps physical RAM.
unsafe fn map_device_page(k: KernelAddr, hhdm: u64, virt: u64, phys: u64) {
    // Use the shared lookup rather than a local copy: it panics instead of guessing.
    // A "fall back to index 0" here would map the UART *cacheable* on the MAIR Limine
    // actually provides (index 0 is Normal write-back), which is a silent failure —
    // speculative reads of device registers and coalesced writes, with no fault.
    let attr_idx = crate::arch::aarch64::paging::device_attr_index();
    // L3 page descriptor for Device memory: valid+page, AF, AttrIndx, XN, AP=00 (RW EL1),
    // SH=00 (device is outer-shareable implicitly), NS=0.
    let page_desc = (phys & ADDR_MASK)
        | DESC_VALID
        | DESC_TABLE
        | DESC_AF
        | (attr_idx << 2)
        | DESC_PXN
        | DESC_UXN;

    // Start at the L0 table (TTBR1). Walk L0→L1→L2, creating tables as needed.
    let mut table_phys = read_sysreg_ttbr1() & ADDR_MASK;
    for level in 0..3 {
        let shift = 39 - level * 9; // L0=39, L1=30, L2=21
        let idx = ((virt >> shift) & 0x1ff) as usize;
        let table = (hhdm + table_phys) as *mut u64;
        let entry = core::ptr::read_volatile(table.add(idx));
        if entry & DESC_VALID == 0 {
            let new_virt = alloc_table();
            let new_phys = k.phys_of(new_virt);
            core::ptr::write_volatile(table.add(idx), (new_phys & ADDR_MASK) | DESC_VALID | DESC_TABLE);
            table_phys = new_phys;
        } else {
            // A valid descriptor with bit 1 clear at L0-L2 is a *block*, not a table
            // pointer. Descending into one would treat the mapped data frame as a page
            // table and write a descriptor into it — the same hazard `ensure_table`
            // guards against in the shared walker. Unreachable while the bootloader
            // leaves the device-MMIO hole unmapped, but silent if that ever changes.
            assert!(
                entry & DESC_TABLE != 0,
                "map_device_page: descriptor maps a block — cannot install a 4 KiB \
                 mapping beneath it"
            );
            table_phys = entry & ADDR_MASK;
        }
    }
    // L3: set the page entry.
    let l3_idx = ((virt >> 12) & 0x1ff) as usize;
    let l3 = (hhdm + table_phys) as *mut u64;
    core::ptr::write_volatile(l3.add(l3_idx), page_desc);

    // Publish the new entry: ensure the stores land, invalidate the stale TLB entry
    // for this VA, and synchronize.
    //
    // The TLBI operand carries VA[55:12] in bits 43:0, a TTL hint in 47:44 and an ASID
    // in 63:48, so the page number must be masked or the upper VA bits corrupt both
    // fields. See `arch::aarch64::paging::TLBI_VA_MASK` — benign on QEMU's ARMv8.0
    // `cortex-a72`, not on FEAT_TTL hardware.
    asm!(
        "dsb ishst",
        "tlbi vae1is, {va}",
        "dsb ish",
        "isb",
        va = in(reg) ((virt >> 12) & crate::arch::aarch64::paging::TLBI_VA_MASK),
        options(nostack, preserves_flags),
    );
}

/// aarch64 kernel entry (called from the arch-neutral `kmain` prologue). Diverges.
///
/// `hhdm` is Limine's higher-half direct-map offset; `k` captures the kernel image's
/// physical/virtual bases (both from Limine). Maps and installs the UART, prints a
/// banner and a few sysregs, then brings up memory, exceptions, the timer and the
/// scheduler in the order the module docs lay out — idling once all of it is proved.
pub fn kmain_aarch64(
    hhdm: u64,
    k: KernelAddr,
    entries: &[&limine::memory_map::Entry],
) -> ! {
    // Must precede anything that can take an exception — see the function docs.
    use_sp_el1();

    // Map the PL011 at its HHDM address (upper-half, currently unmapped) and install
    // the serial writer there.
    let uart_va = hhdm + PL011_PHYS;
    // SAFETY: `uart_va` is a TTBR1 address Limine left unmapped (device MMIO hole);
    // hhdm direct-maps RAM incl. the page tables we walk.
    unsafe { map_device_page(k, hhdm, uart_va, PL011_PHYS) };
    crate::arch::aarch64::serial::init(uart_va as usize);

    let current_el = {
        let v: u64;
        // SAFETY: no side effects.
        unsafe { asm!("mrs {}, CurrentEL", out(reg) v, options(nomem, nostack)) };
        (v >> 2) & 0x3
    };

    crate::println!();
    crate::println!("========================================");
    crate::println!("  ThemeliOS — booting on aarch64 (EL{})", current_el);
    crate::println!("========================================");
    crate::println!("[boot] Limine higher-half handoff OK (MMU on)");
    crate::println!("[boot] HHDM offset: {:#018x}", hhdm);
    crate::println!("[boot] kernel phys/virt base: {:#x} / {:#018x}", k.phys_base, k.virt_base);
    {
        let spsel: u64;
        // SAFETY: reading SPSel has no side effects.
        unsafe { asm!("mrs {}, SPSel", out(reg) spsel, options(nomem, nostack)) };
        crate::println!(
            "[boot] SPSel={} (running on SP_EL{}) — exceptions use the SP_ELx vectors",
            spsel & 1,
            spsel & 1
        );
    }
    crate::println!("[boot] PL011 mapped + serial online");
    crate::println!("[boot] Phase 7.0b boot-to-banner reached; idling.");

    // Install exception vectors before anything that can fault. Until this runs, a
    // data abort branches to whatever VBAR_EL1 held at handoff and the kernel dies
    // silently; afterwards it prints a decoded syndrome, faulting address and PC.
    // Deliberately ahead of the memory bring-up so a paging mistake is diagnosable.
    crate::arch::aarch64::exceptions::init();

    // Point TPIDR_EL1 at the per-CPU block before anything can read it. The exception
    // reporter installed just above dereferences it to name the faulting task, so this
    // must not wait for the scheduler.
    crate::arch::aarch64::percpu::init();

    // Establish — then confirm — the assumption the context switch and the exception
    // stub both rest on: FP/SIMD traps, so the absent v8-v15 save area cannot corrupt
    // state silently. Deliberately after the vector table is live, so that a stray
    // vector instruction is reported rather than branching into bootloader leftovers.
    crate::arch::aarch64::exceptions::trap_fp_access();
    let fp_trapped = crate::arch::aarch64::exceptions::verify_fp_trapped();

    // --- Phase 7.1: memory management on our own page tables ---
    bring_up_memory(hhdm, k, entries);

    // Prove the synchronous-exception path with a real trap. Reported together with
    // the interrupt path below, in the single sentinel the CI smokes assert.
    let exc_ok = crate::arch::aarch64::exceptions::selftest();

    // --- Interrupt controller + periodic tick ---
    crate::arch::aarch64::gic::init();
    crate::arch::aarch64::timer::init();

    let timer_ok = timer_selftest();
    if exc_ok && timer_ok {
        crate::println!("[boot] Phase 7.2 exceptions+GIC+timer reached; self-tests passed.");
    } else {
        crate::println!("[boot] Phase 7.2 FAILED self-test.");
    }

    // --- Phase 7.3: preemptive scheduling ---
    crate::sched::init();
    let sched_ok = sched_selftest();
    // Run after the scheduler test, which is what gives the per-CPU block dozens of
    // switches to have been updated by; before it there would be nothing to check.
    let percpu_ok = crate::arch::aarch64::percpu::selftest();
    if sched_ok && percpu_ok && fp_trapped {
        crate::println!("[boot] Phase 7.3 scheduler reached; self-test passed.");
    } else {
        crate::println!("[boot] Phase 7.3 scheduler FAILED self-test.");
    }

    crate::println!("[boot] (shell=7.4)");

    // Leave the machine *running*, not merely stopped. The self-tests above mask
    // interrupts when they finish measuring, and halting in that state would wedge the
    // scheduler: `wfi` would keep returning, but no timer interrupt would ever be taken
    // again, so nothing would preempt this loop. Unmasking hands the CPU back to the
    // scheduler, which alternates this task with idle until 7.4 gives it real work.
    crate::arch::irq::enable();

    loop {
        crate::arch::irq::halt();
    }
}

// ------------------------------------------------------------------------
// Phase 7.3 scheduler self-test
// ------------------------------------------------------------------------

/// Per-worker loop-iteration counters, indexed by worker number.
///
/// Informational only — see [`WORKER_TICKS`] for why these are *not* what fairness is
/// asserted on; the only thing checked here is that each is non-zero.
///
/// Plain atomics rather than a lock: the workers are preempted at arbitrary instruction
/// boundaries by the timer, and a worker holding a lock when its slice expires would
/// deadlock against the next worker.
///
/// `Relaxed` is enough, and not for the reason two earlier versions of this comment
/// gave. It is *not* the `Acquire`/`Release` pair on [`WORKERS_EXITED`], which orders
/// only the all-workers-exited path while the test deliberately also handles a drain
/// that times out with workers still running. Nor does the reader mask interrupts
/// first — the counters are read with interrupts still enabled, before the drain.
///
/// What makes it sound is simply that these are `AtomicU64` loads: they cannot tear, so
/// every value read is a value some worker actually wrote. The result is a slightly
/// skewed snapshot — one worker's count may be a few iterations newer than another's —
/// and nothing here cares, because the checks are a floor and a ceiling on quantities
/// in the tens, sampled from counts in the hundreds of thousands.
static WORKER_ITERS: [core::sync::atomic::AtomicU64; 3] = [
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
];

/// Per-worker count of **distinct timer ticks observed while running**.
///
/// This, not [`WORKER_ITERS`], is what the fairness check asserts on.
///
/// Iteration counts measure how much work a worker got through, which under QEMU's
/// TCG is wildly variable: the first task to reach a given loop pays for translating
/// it, the host runner is shared, and a measured run showed shares of 4.5×, 1.2× and
/// 1.45× across three otherwise identical boots. None of that variation is the
/// scheduler's doing, so asserting on it would be asserting on the emulator's mood.
///
/// A worker's *residency* is immune to all of that. Each worker notes the tick counter
/// on every iteration; when it sees a value it has not seen before, it was resident
/// for a tick it previously was not. Round-robin over N runnable tasks gives each one
/// roughly `elapsed_ticks / N` distinct ticks no matter how fast the underlying CPU
/// happens to be, because the tick *is* the scheduling quantum.
static WORKER_TICKS: [core::sync::atomic::AtomicU64; 3] = [
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
];

/// Set false to ask the workers to return from their entry functions.
static RUN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// Set by a worker that gave up waiting to be preempted and returned on its own.
///
/// This is what turns the sub-phase's single most likely regression from a hang into a
/// diagnosis. If `task_bootstrap` failed to unmask `DAIF.I`, the first worker switched
/// to would run with interrupts masked forever: no timer IRQ, so the tick never
/// advances, so the bootstrap task is never resumed — and a deadline evaluated *in the
/// bootstrap task* can never fire, however carefully it is bounded. The only code still
/// executing is the worker, so the escape has to live there.
///
/// A worker that escapes returns normally, which routes through `task_exit` and hands
/// the CPU to the next task cooperatively. Every worker in turn does the same, control
/// reaches the bootstrap task, and the test reports which workers were never preempted
/// instead of the CI job timing out with an empty log.
static WORKER_ESCAPED: [core::sync::atomic::AtomicBool; 3] = [
    core::sync::atomic::AtomicBool::new(false),
    core::sync::atomic::AtomicBool::new(false),
    core::sync::atomic::AtomicBool::new(false),
];

/// Wall-clock ticks a worker will spin before concluding it is never going to be
/// preempted. Comfortably beyond the measured window plus the drain (~110 ticks in a
/// healthy run), so it cannot fire on a slow runner: it is bounded on `CNTVCT_EL0`,
/// which is real time and does not care how fast the emulated CPU is.
const WORKER_ESCAPE_TICKS: u64 = 200;

/// Number of workers that reached the end of their entry function.
static WORKERS_EXITED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Entry function shared by the self-test workers.
///
/// A worker cannot be told which index it is — [`crate::sched::spawn`] takes a bare
/// `fn()` with no argument — so each index gets a one-line wrapper below.
///
/// The body deliberately does **not** yield. The whole point of the test is
/// *involuntary* preemption: if the loop only advanced when it called `yield_now`,
/// a passing result would prove cooperative switching and say nothing about whether
/// the timer interrupt actually reaches `schedule()`.
fn worker_body(index: usize) {
    use crate::arch::aarch64::timer::{read_cntvct, reload};
    use core::sync::atomic::Ordering;

    // Tick value last observed by *this* worker. A plain local: it lives in the
    // worker's own registers/stack, which the context switch preserves, so each
    // worker tracks its own residency without any shared state.
    let mut last_tick = u64::MAX;

    // See `WORKER_ESCAPED`: the deadline has to be evaluated here, by the worker, on a
    // clock that advances whether or not interrupts are being delivered.
    let escape_at = read_cntvct() + reload() * WORKER_ESCAPE_TICKS;

    while RUN.load(Ordering::Acquire) {
        WORKER_ITERS[index].fetch_add(1, Ordering::Relaxed);

        let now = crate::arch::time::tick_count();
        if now != last_tick {
            last_tick = now;
            WORKER_TICKS[index].fetch_add(1, Ordering::Relaxed);
        }

        if read_cntvct() >= escape_at {
            WORKER_ESCAPED[index].store(true, Ordering::Relaxed);
            break;
        }

        core::hint::spin_loop();
    }
    WORKERS_EXITED.fetch_add(1, Ordering::Release);
    // Returning here falls into `task_bootstrap`'s `bl task_exit`, which marks the
    // task Dead and schedules away — the path this self-test also exercises.
}

fn worker_0() {
    worker_body(0);
}
fn worker_1() {
    worker_body(1);
}
fn worker_2() {
    worker_body(2);
}

/// Prove the aarch64 scheduler preempts and round-robins.
///
/// Acceptance for 7.3 is "≥2 in-kernel tasks preempt and round-robin under the
/// timer". Three properties have to hold, and each is checked separately because
/// they fail in different ways:
///
/// 1. **Every worker ran.** A zero counter means that task was never switched *to* —
///    a broken initial stack frame, or a `switch_context` that does not restore `x30`.
/// 2. **They ran without yielding.** Nothing in `worker_body` calls into the
///    scheduler, so the only way control leaves a worker is the timer IRQ reaching
///    `schedule()`. This is what separates 7.3 from cooperative switching.
/// 3. **Each got a real share of slices.** Round-robin, not "one task monopolised the
///    CPU and the others got a slice apiece" — which is why the check is a *band*
///    (half to double an equal share) and not a floor. A floor alone would pass the
///    monopoly case it claims to reject. Measured in ticks of residency rather than
///    work done, for the reason in [`WORKER_TICKS`].
/// 4. **Each returned from its entry function**, and none of them escaped. That
///    exercises `task_exit` and the Dead-task cleanup in `schedule()`, which nothing
///    else on aarch64 has run yet — and the escape flag is what catches a worker that
///    was never preempted at all (see [`WORKER_ESCAPED`]).
/// 5. **The time is accounted for.** Per-worker bounds alone permit three workers to
///    clear the floor while most of the window went to the idle task, so their combined
///    residency is checked against the elapsed window too.
///
/// Termination is bounded from two directions, because one is not enough. The bootstrap
/// task's own loops are deadlined on `CNTVCT_EL0`, which covers a timer that stops
/// delivering while the bootstrap task is running. It does *not* cover a worker that
/// runs with interrupts masked — the bootstrap task is then never resumed to evaluate
/// any deadline at all — so the workers carry their own escape.
fn sched_selftest() -> bool {
    use crate::arch::aarch64::timer::{read_cntvct, reload};
    use crate::arch::time::tick_count;
    use core::sync::atomic::Ordering;

    // ~50 ticks ≈ 500 ms at 100 Hz: enough slices for round-robin to be visible.
    const RUN_TICKS: u64 = 50;

    let ids = [
        crate::sched::spawn("w0", worker_0),
        crate::sched::spawn("w1", worker_1),
        crate::sched::spawn("w2", worker_2),
    ];
    crate::println!("[sched] spawned workers {:?}; running {} ticks...", ids, RUN_TICKS);

    crate::arch::irq::enable();

    // Absorb the backlog before starting the clock.
    //
    // Interrupts have been masked since the end of the 7.2 timer self-test, across
    // `sched::init`, three task-stack allocations and half a dozen UART writes — on
    // QEMU that is hundreds of milliseconds. The generic timer kept counting through
    // all of it, so `handle_tick`'s skip-crediting (correctly) advances the tick by the
    // whole elapsed backlog on the first interrupt after unmasking. A measured run saw
    // one interrupt take the tick from 5 to 77.
    //
    // A window opened before that point is therefore satisfied the instant the backlog
    // lands, and measures nothing at all: an earlier version of this test "ran for 50
    // ticks" that were entirely credited by a single interrupt, and the workers got
    // three slices between them. So wait for the catch-up interrupt, and only then
    // start counting.
    //
    // Bounded on `CNTVCT_EL0`, not on tick changes: if delivery is broken the tick
    // never moves and a loop waiting on it would hang the boot rather than fail it.
    // The counter advances regardless of interrupt state, which is what makes it the
    // only sound timeout for a test of the interrupt path.
    {
        let t0 = tick_count();
        let c0 = read_cntvct();
        let deadline = reload() * 10;
        while tick_count() == t0 && read_cntvct() - c0 < deadline {
            core::hint::spin_loop();
        }
    }

    // The catch-up interrupt also ran `schedule()`, so the workers may already have a
    // slice or two. Zero the counters so the numbers reported below describe exactly
    // the measured window. A worker racing this store loses at most one count, which a
    // floor-based check does not care about.
    for i in 0..3 {
        WORKER_ITERS[i].store(0, Ordering::Relaxed);
        WORKER_TICKS[i].store(0, Ordering::Relaxed);
    }

    // From here until the workers are asked to stop, the timer IRQ preempts this task
    // too — the bootstrap task is an ordinary scheduler citizen and goes back on the
    // ready queue like any other. `tick_count` is updated by the timer handler
    // regardless of which task is running, so this deadline is sound *when interrupts
    // are being delivered*.
    //
    // Which is why it is not the only bound. `CNTVCT_EL0` advances regardless of
    // interrupt state, so this loop still terminates if delivery stops entirely while
    // the bootstrap task is the one running — a dead GIC or an unarmed timer.
    //
    // It is *not* sufficient on its own, and it is worth being exact about why, because
    // the obvious reading is wrong. If `task_bootstrap` failed to unmask DAIF — the
    // single most likely regression here — the first worker switched to would run
    // masked, no timer IRQ would ever be taken again, and this task would never be
    // resumed at all. A deadline evaluated *in this loop* cannot fire if this loop is
    // never re-entered. That case is caught by `WORKER_ESCAPED`, in the only code still
    // running: the worker itself.
    let start = tick_count();
    let c_start = read_cntvct();
    let hang_bound = reload() * RUN_TICKS * 4;
    while tick_count() < start + RUN_TICKS && read_cntvct() - c_start < hang_bound {
        core::hint::spin_loop();
    }

    // Snapshot before the drain. The workers keep counting until they actually observe
    // `RUN == false`, which can take another slice apiece, so reading the counters
    // after the drain would score them over a longer interval than `RUN_TICKS` — and
    // the floor below is derived from `RUN_TICKS`. Mixing the two denominators would
    // make the check quietly more lenient than it reads.
    let iters = [
        WORKER_ITERS[0].load(Ordering::Relaxed),
        WORKER_ITERS[1].load(Ordering::Relaxed),
        WORKER_ITERS[2].load(Ordering::Relaxed),
    ];
    let slices = [
        WORKER_TICKS[0].load(Ordering::Relaxed),
        WORKER_TICKS[1].load(Ordering::Relaxed),
        WORKER_TICKS[2].load(Ordering::Relaxed),
    ];
    let ticks = tick_count() - start;

    // Ask the workers to return. They are still runnable, so give them slices to
    // notice: `yield_now` puts this task at the back of the queue each time round.
    RUN.store(false, Ordering::Release);
    let drain_deadline = tick_count() + RUN_TICKS;
    while WORKERS_EXITED.load(Ordering::Acquire) < ids.len() as u64
        && tick_count() < drain_deadline
    {
        crate::sched::yield_now();
    }
    crate::arch::irq::disable();

    let exited = WORKERS_EXITED.load(Ordering::Acquire);

    // Runnable tasks during the measured window: three workers plus the bootstrap
    // task, which goes back on the ready queue like any other. The idle task never
    // enters the queue, so it does not dilute the share.
    const RUNNABLE: u64 = 4;
    let share = RUN_TICKS / RUNNABLE; // 12 ticks apiece at RUN_TICKS = 50

    // Round-robin means a *band*, not a floor. A floor alone would pass `[60, 6, 6]` —
    // precisely the "one task monopolised the CPU and the others got a token slice"
    // outcome this is supposed to reject — so bound it from above as well.
    //
    // Half to double an equal share. Measured runs sit at 12-14 against a share of 12,
    // so both bounds have better than 40% headroom; loose enough for a busy CI runner,
    // tight enough that starvation or monopoly fails.
    let floor = share / 2; // 6
    let ceiling = share * 2; // 24

    let escaped = [
        WORKER_ESCAPED[0].load(Ordering::Relaxed),
        WORKER_ESCAPED[1].load(Ordering::Relaxed),
        WORKER_ESCAPED[2].load(Ordering::Relaxed),
    ];

    let all_ran = iters.iter().all(|&c| c > 0);
    let none_escaped = !escaped.iter().any(|&e| e);
    let fair = slices.iter().all(|&s| s >= floor && s <= ceiling);
    // Per-worker bounds say nothing about where the *rest* of the time went: slices of
    // [6, 6, 6] over 52 ticks clear the floor while leaving 34 ticks unaccounted for,
    // which is what it would look like if the idle task were being scheduled in
    // preference to runnable work. Three of the four runnable tasks are workers, so
    // their combined residency should be around three quarters of the window; require
    // at least half of it.
    let total: u64 = slices.iter().sum();
    let accounted = total >= ticks / 2;
    let all_exited = exited == ids.len() as u64;

    if all_ran && none_escaped && fair && accounted && all_exited {
        crate::println!(
            "[selftest] sched: PASS (slices {:?} within [{}, {}], {} of {} measured \
             ticks accounted for; iterations {:?}; {}/{} workers exited cleanly, none \
             escaped)",
            slices, floor, ceiling, total, ticks, iters, exited, ids.len()
        );
        return true;
    }

    crate::println!(
        "[selftest] sched: FAIL — slices {:?} (want [{}, {}], sum {} of {} ticks), \
         iterations {:?}, {}/{} workers exited, escaped {:?}",
        slices, floor, ceiling, total, ticks, iters, exited, ids.len(), escaped
    );
    if !none_escaped {
        crate::println!(
            "  a worker gave up waiting to be preempted and returned on its own after \
             {} ticks of wall clock: while it was running, no timer interrupt was ever \
             taken. Check that `task_bootstrap` unmasks DAIF.I — a new task is entered \
             by `ret` out of the IRQ handler, where hardware masked it, not by `eret`.",
            WORKER_ESCAPE_TICKS
        );
    } else if !all_ran {
        crate::println!(
            "  a worker never ran at all: check `setup_initial_stack`'s frame layout \
             against `switch_context`'s restore offsets (x19 = entry, x30 = \
             task_bootstrap)"
        );
    } else if !fair {
        let starved = slices.iter().any(|&s| s < floor);
        if starved {
            crate::println!(
                "  a worker ran but was starved of slices: the timer IRQ is reaching \
                 `schedule()` from some tasks and not others — check that \
                 `task_bootstrap` unmasks DAIF.I, and that the IRQ path re-enables \
                 delivery on every return"
            );
        } else {
            crate::println!(
                "  a worker got far more than an equal share: the ready queue is not \
                 round-robin — check that `schedule()` pushes the preempted task to \
                 the *back* of the queue and pops from the front"
            );
        }
    } else if !accounted {
        crate::println!(
            "  the workers together account for only {} of {} ticks: runnable tasks are \
             losing the CPU to something else — check that `schedule()` prefers the \
             ready queue over the idle task",
            total, ticks
        );
    } else if !all_exited {
        crate::println!(
            "  a worker never returned from its entry function: check `task_exit` and \
             the Dead-task cleanup path in `schedule()`"
        );
    }
    false
}

/// Prove the timer ticks, **at the right rate**.
///
/// Acceptance for 7.2 is "the timer IRQ fires at a fixed rate and increments the
/// tick". Counting interrupts alone does not show that, and an earlier version of this
/// test — which waited for five ticks and checked nothing else — could not tell a
/// healthy timer from a runaway one.
///
/// The reason is that `CNTV` drives a **level** line into the GIC. If the handler
/// failed to re-arm, the compare condition would stay met and the GIC would re-pend
/// immediately after every `EOIR`: five ticks would arrive in microseconds rather than
/// 50 ms, with five IRQs and no spurious ones, printing a PASS line
/// character-for-character identical to the healthy case. So the test brackets the
/// wait with `CNTVCT_EL0` and checks the elapsed counter time is in the right
/// neighbourhood. That is the difference between "five interrupts happened" and "five
/// interrupts happened, 10 ms apart".
///
/// The wait is deadlined on the counter too, not on `wfi` returns. `wfi` only wakes on
/// an interrupt, and in this phase the timer is the *only* interrupt source — so if
/// delivery is broken, `wfi` blocks forever and a loop counting its returns can never
/// time out. The counter advances regardless, which makes it the only sound clock for
/// timing out a test of the interrupt path.
fn timer_selftest() -> bool {
    use crate::arch::aarch64::timer::read_cntvct;
    use crate::arch::time::tick_count;

    const WANT_TICKS: u64 = 5;

    let reload = crate::arch::aarch64::timer::reload();
    let expect = reload * WANT_TICKS;
    // Give delivery four periods of slack before declaring failure.
    let deadline_ticks = expect * 4;

    let start = tick_count();
    let c0 = read_cntvct();
    crate::println!(
        "[timer] waiting for {} ticks (tick={}, expecting ~{} counter units)...",
        WANT_TICKS, start, expect
    );

    crate::arch::irq::enable();
    while tick_count() < start + WANT_TICKS && read_cntvct() - c0 < deadline_ticks {
        crate::arch::irq::halt(); // wfi — sleeps until the next interrupt
    }
    let elapsed = read_cntvct() - c0;
    crate::arch::irq::disable();

    let end = tick_count();
    let irqs = crate::arch::aarch64::gic::irq_count();
    let spurious = crate::arch::aarch64::gic::spurious_count();
    let got_ticks = end >= start + WANT_TICKS;
    // Half to double the expected interval. Wide enough not to be flaky under a loaded
    // CI runner, tight enough that a storm (elapsed ≈ 0) or a stalled timer fails.
    let rate_ok = elapsed >= expect / 2 && elapsed <= expect * 2;

    if got_ticks && rate_ok {
        crate::println!(
            "[selftest] timer: PASS (tick {} -> {}, {} IRQs, {} spurious, {} counter \
             units elapsed vs {} expected)",
            start, end, irqs, spurious, elapsed, expect
        );
        return true;
    }

    crate::println!(
        "[selftest] timer: FAIL — tick {} -> {}, {} IRQs, {} spurious, {} counter units \
         elapsed vs {} expected",
        start, end, irqs, spurious, elapsed, expect
    );
    // Name the suspect. These branches are now reachable: the loop exits on the
    // counter deadline even when no interrupt is ever delivered.
    if irqs == 0 {
        crate::println!(
            "  no IRQs reached the CPU: check GICC_PMR/GICC_CTLR/GICD_CTLR, the PPI \
             enable, and that DAIF.I is actually clear"
        );
    } else if got_ticks && !rate_ok && elapsed < expect / 2 {
        crate::println!(
            "  ticks arrived far too fast: the timer is not being re-armed, so the \
             level line stays asserted and the GIC re-pends after every EOI"
        );
    } else if irqs == 1 {
        crate::println!(
            "  exactly one IRQ: it was never completed (GICC_EOIR), so nothing further \
             was delivered"
        );
    }
    false
}

/// Bring up the memory subsystem: frame allocator, kernel heap, and the kernel's own
/// page tables — then prove the MMU work with an end-to-end self-test.
///
/// Mirrors the x86_64 bring-up order in `kmain_x86_64`, with one deliberate omission
/// noted below.
///
/// ## Why bootloader memory is not reclaimed here
///
/// x86_64 reclaims `BOOTLOADER_RECLAIMABLE` regions after switching page tables. We do
/// not, on purpose. Our kernel root clones Limine's L0 descriptors, which point at
/// Limine's *lower-level* tables — and those live in bootloader-reclaimable memory. On
/// aarch64 this sub-phase has no exception handlers yet (7.2), so if a reclaimed table
/// frame were handed out and overwritten, the resulting translation fault would be an
/// unrecoverable silent hang rather than a diagnosable abort. Reclaiming is an
/// optimization; correctness first. Revisit once 7.2 gives us fault reporting.
fn bring_up_memory(hhdm: u64, k: KernelAddr, entries: &[&limine::memory_map::Entry]) {
    use crate::arch::aarch64::paging;

    // Physical↔virtual conversion must work before anything touches a PhysAddr.
    crate::mm::init_hhdm(hhdm);

    // Confirm the translation geometry we are about to assume. Verify rather than
    // program: we are already executing on tables built for the current TCR, so
    // rewriting it would fault instantly and undiagnosably.
    let (t1sz, tg1) = paging::verify_tcr();
    let mair = paging::read_mair();
    crate::println!(
        "[mm] TCR_EL1: T1SZ={} TG1={:#b} (48-bit kernel VA, 4 KiB granule) — verified",
        t1sz,
        tg1
    );
    crate::println!(
        "[mm] MAIR_EL1: {:#018x} (normal idx {}, device idx {}) — adopted from Limine",
        mair,
        paging::normal_attr_index(),
        paging::device_attr_index()
    );

    // Physical frame allocator, from Limine's memory map.
    crate::mm::frame::init(entries, hhdm, k.phys_base);
    let free = crate::mm::frame::free_frame_count();
    let total = crate::mm::frame::total_frame_count();
    crate::println!(
        "[mm] Frame allocator: {} free / {} total ({} MiB usable)",
        free,
        total,
        (free as u64 * crate::mm::PAGE_SIZE) / (1024 * 1024)
    );

    // The critical moment: build our own L0 tree from Limine's kernel-half mappings
    // and load it into TTBR1_EL1. If anything is missing — the running code, the
    // stack, the HHDM, or the UART page mapped in 7.0b — the CPU faults before the
    // next line prints.
    crate::mm::page_table::init();
    crate::println!("[mm] Running on kernel-owned page tables (TTBR1_EL1, TTBR0_EL1=0)");

    // Kernel heap. Not strictly required by the paging self-test, but bringing it up
    // here proves `alloc` works on aarch64 and is what the 7.4 test suite will need.
    crate::mm::heap::init();
    {
        use alloc::vec::Vec;
        let mut v: Vec<u64> = Vec::new();
        for i in 0..64 {
            v.push(i * i);
        }
        assert!(v.len() == 64 && v[63] == 63 * 63, "aarch64 heap smoke failed");
        crate::println!("[mm] Kernel heap online (Vec smoke OK)");
    }


    // Prove the descriptor encoding, MAIR attributes, and TLB discipline actually
    // work, rather than merely compiling.
    let ok = crate::mm::page_table::selftest();
    if ok {
        crate::println!("[boot] Phase 7.1 MMU/paging reached; self-test passed.");
    } else {
        crate::println!("[boot] Phase 7.1 MMU/paging FAILED self-test.");
    }
}
