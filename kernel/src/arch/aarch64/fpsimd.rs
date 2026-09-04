//! # FPSIMD state, saved and restored per task
//!
//! The last piece of Phase 8.4, and the one that reverses an earlier decision on purpose.
//!
//! ## Why EL0 must have hardfloat
//!
//! Plan decision 8. There is no soft-float aarch64 A-profile ABI — AAPCS64 defines one
//! only for Armv8-**R** — and real userspace uses SIMD unconditionally, not as an
//! optimisation: glibc's *base* `sysdeps/aarch64/strlen.S` opens with `ld1 {v0.16b}, [src]`,
//! musl ships SIMD `memcpy.S`/`memset.S`, and `sysdeps/aarch64/dl-trampoline.S` saves
//! `q0`-`q7` unconditionally, so **the first lazy PLT binding in any dynamically-linked
//! aarch64 binary executes SIMD**. A softfloat userspace is not a restriction we could
//! impose; it is one nothing would run under.
//!
//! ## Why that forces FP on at EL1 too, and what replaces the old backstop
//!
//! `CPACR_EL1.FPEN` has four encodings and **none of them traps EL1 while permitting
//! EL0**:
//!
//! | `FPEN` | EL0 | EL1 |
//! |---|---|---|
//! | `0b00` | traps | traps |
//! | `0b01` | traps | allowed |
//! | `0b10` | traps | traps |
//! | `0b11` | allowed | allowed |
//!
//! So giving userspace FP means `0b11`, which also enables it at EL1 — and that removes
//! the trap 8.4b installed and *verified*, whose whole job was to catch a stray SIMD
//! instruction in a kernel that saves no vector registers. Deleting a checked safety net
//! is exactly the kind of change that should not happen quietly, so the net is replaced
//! rather than dropped, in two layers:
//!
//! 1. **A build-time assertion that the kernel is softfloat.** `target_feature = "neon"`
//!    is set for `aarch64-unknown-none` and unset for `aarch64-unknown-none-softfloat`
//!    (verified with `rustc --print cfg` for both), so the compiler can tell us. This is
//!    strictly stronger than the trap for the case that actually worried us — someone
//!    switching the target — because it fails the build instead of a boot.
//! 2. **A boot self-test that the kernel does not clobber FP.** [`selftest`] loads a known
//!    pattern into `v0`-`v31`, runs representative kernel work over it, and checks the
//!    pattern survives. The trap could only catch FP the kernel *executed*; this catches
//!    the consequence, which is what actually matters.
//!
//! Neither is airtight — (2) covers only the paths it exercises — but both are
//! measurements rather than claims, which the thing they replace had not been until 8.4b
//! went looking and found `FPEN = 0b11` behind three comments asserting the opposite.
//!
//! ## Save on context switch, not on exception entry
//!
//! The kernel emits no FP, so it cannot clobber a task's vector registers while handling
//! that task's own exception: EL0 → `svc` → kernel → `eret` leaves `v0`-`v31` untouched.
//! What *can* clobber them is another EL0 task running in between. So the save belongs
//! with the context switch and nowhere else — the same conclusion Linux reaches, and the
//! reason [`crate::arch::aarch64::exceptions::ExceptionFrame`] gains no vector fields.
//!
//! Saving is unconditional rather than lazy. Linux traps first-FP-use per thread and
//! restores only for threads that touched FP; that is an optimisation whose bookkeeping
//! is its own source of bugs, and at 100 Hz the 528 bytes and ~40 instructions here do not
//! register. Laziness is a deliberate non-goal, not an oversight.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::println;

/// The kernel must be built softfloat, or the reasoning above collapses: with FP enabled
/// at EL1 and no trap, compiler-emitted SIMD in kernel code would corrupt whichever task's
/// state sat in the registers, with nothing to fault on.
///
/// This replaces the runtime `CPACR_EL1.FPEN` trap that 8.4b installed and that 8.4e has
/// to remove — and catches the realistic version of the hazard (a target change) at build
/// time rather than at boot.
const _: () = assert!(
    !cfg!(target_feature = "neon"),
    "the aarch64 kernel must be built for a softfloat target: FP is enabled at EL1 for \
     userspace's sake and nothing traps kernel SIMD, so compiler-emitted vector code would \
     silently corrupt a task's FPSIMD state across a context switch"
);

/// A task's floating-point and SIMD register state.
///
/// `v0`-`v31` are 128 bits each, plus the two status/control registers — 528 bytes with
/// alignment. `align(16)` is load-bearing: the save and restore below use `stp q`/`ldp q`,
/// which require 16-byte alignment.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct FpState {
    /// `v0`-`v31`, in register order.
    pub v: [u128; 32],
    /// Floating-point Control Register (rounding mode, trap enables). Only the low 32
    /// bits are architected; stored as `u64` so the struct needs no packing.
    pub fpcr: u64,
    /// Floating-point Status Register (cumulative exception flags).
    pub fpsr: u64,
}

// The save/restore asm hardcodes the FPCR/FPSR offsets, so tie them to the struct — the
// `EXC_FRAME_RESERVE` lesson from 8.4b, where a constant sat beside hand-written assembly
// that held its own copy of the number and the two could drift silently.
const _: () = assert!(core::mem::offset_of!(FpState, v) == 0);
const _: () = assert!(core::mem::offset_of!(FpState, fpcr) == 512);
const _: () = assert!(core::mem::offset_of!(FpState, fpsr) == 520);
const _: () = assert!(core::mem::size_of::<FpState>() == 528);
// `stp q`/`ldp q` require 16-byte alignment.
const _: () = assert!(core::mem::align_of::<FpState>() % 16 == 0);

impl FpState {
    /// A zeroed state, which is what a task starts with.
    pub const fn new() -> Self {
        FpState {
            v: [0; 32],
            fpcr: 0,
            fpsr: 0,
        }
    }
}

impl Default for FpState {
    fn default() -> Self {
        Self::new()
    }
}

/// Save the live FPSIMD registers into `state`.
///
/// # Safety
///
/// `state` must be a valid, 16-byte-aligned `FpState`. Called with interrupts masked from
/// the context switch, where the registers belong to the outgoing task.
pub unsafe fn save(state: *mut FpState) {
    // SAFETY: writes 528 bytes through a caller-supplied aligned pointer. The `q`
    // registers are named literally rather than through a register class, because the
    // softfloat target exposes no `vreg` class to the register allocator — which is the
    // point: the compiler never touches these, so there is nothing to conflict with.
    unsafe {
        core::arch::asm!(
            // The softfloat target's assembler rejects FP mnemonics outright
            // ("instruction requires: fp-armv8"), so the extension is enabled for the
            // span of this block and switched back off after it. That is the whole
            // softfloat arrangement in miniature: the compiler must never emit vector
            // code, and this one hand-written window is where the kernel touches it.
            ".arch_extension fp",
            "stp q0,  q1,  [{p}, #(0 * 32)]",
            "stp q2,  q3,  [{p}, #(1 * 32)]",
            "stp q4,  q5,  [{p}, #(2 * 32)]",
            "stp q6,  q7,  [{p}, #(3 * 32)]",
            "stp q8,  q9,  [{p}, #(4 * 32)]",
            "stp q10, q11, [{p}, #(5 * 32)]",
            "stp q12, q13, [{p}, #(6 * 32)]",
            "stp q14, q15, [{p}, #(7 * 32)]",
            "stp q16, q17, [{p}, #(8 * 32)]",
            "stp q18, q19, [{p}, #(9 * 32)]",
            "stp q20, q21, [{p}, #(10 * 32)]",
            "stp q22, q23, [{p}, #(11 * 32)]",
            "stp q24, q25, [{p}, #(12 * 32)]",
            "stp q26, q27, [{p}, #(13 * 32)]",
            "stp q28, q29, [{p}, #(14 * 32)]",
            "stp q30, q31, [{p}, #(15 * 32)]",
            "mrs {t0}, FPCR",
            "mrs {t1}, FPSR",
            // Separate `str`s, not an `stp`: the GPR pair form encodes a signed 7-bit
            // scaled offset, i.e. [-512, 504], and 512 is one past its top. `str`'s
            // unsigned form reaches far further.
            "str {t0}, [{p}, #512]",
            "str {t1}, [{p}, #520]",
            ".arch_extension nofp",
            p = in(reg) state,
            t0 = out(reg) _,
            t1 = out(reg) _,
            options(nostack, preserves_flags),
        );
    }
}

/// Restore the FPSIMD registers from `state`.
///
/// # Safety
///
/// As [`save`]: valid, aligned, interrupts masked, registers belong to the incoming task.
pub unsafe fn restore(state: *const FpState) {
    // SAFETY: reads 528 bytes through a caller-supplied aligned pointer.
    unsafe {
        core::arch::asm!(
            ".arch_extension fp",
            "ldp q0,  q1,  [{p}, #(0 * 32)]",
            "ldp q2,  q3,  [{p}, #(1 * 32)]",
            "ldp q4,  q5,  [{p}, #(2 * 32)]",
            "ldp q6,  q7,  [{p}, #(3 * 32)]",
            "ldp q8,  q9,  [{p}, #(4 * 32)]",
            "ldp q10, q11, [{p}, #(5 * 32)]",
            "ldp q12, q13, [{p}, #(6 * 32)]",
            "ldp q14, q15, [{p}, #(7 * 32)]",
            "ldp q16, q17, [{p}, #(8 * 32)]",
            "ldp q18, q19, [{p}, #(9 * 32)]",
            "ldp q20, q21, [{p}, #(10 * 32)]",
            "ldp q22, q23, [{p}, #(11 * 32)]",
            "ldp q24, q25, [{p}, #(12 * 32)]",
            "ldp q26, q27, [{p}, #(13 * 32)]",
            "ldp q28, q29, [{p}, #(14 * 32)]",
            "ldp q30, q31, [{p}, #(15 * 32)]",
            "ldr {t0}, [{p}, #512]",
            "ldr {t1}, [{p}, #520]",
            "msr FPCR, {t0}",
            "msr FPSR, {t1}",
            ".arch_extension nofp",
            p = in(reg) state,
            t0 = out(reg) _,
            t1 = out(reg) _,
            options(nostack, preserves_flags),
        );
    }
}

/// Enable FP/SIMD access at EL0 **and** EL1 by setting `CPACR_EL1.FPEN = 0b11`.
///
/// Replaces 8.4b's `trap_fp_access`, which set `0b00`. See the module docs for why there
/// is no third option, and what replaced the backstop that removes.
///
/// `ZEN` is deliberately left alone. Neoverse V1/V2 (Graviton 3/4) implement SVE, and a
/// glibc ifunc resolver that selected SVE string routines would use `z`/`p` register state
/// this save area does not cover. Leaving SVE trapped means such a resolver faults visibly
/// instead of corrupting state invisibly — and `HWCAP_SVE` must not be advertised for the
/// same reason.
pub fn enable_fp_access() {
    // SAFETY: read-modify-write of a control register that gates FP/SVE access only. The
    // ISB retires it before the next instruction.
    unsafe {
        let cpacr: u64;
        core::arch::asm!("mrs {}, CPACR_EL1", out(reg) cpacr, options(nomem, nostack));
        core::arch::asm!(
            "msr CPACR_EL1, {}",
            "isb",
            in(reg) cpacr | (0b11 << 20),
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Count of `selftest` runs that observed a clobber, for diagnostics.
static CLOBBERS: AtomicU64 = AtomicU64::new(0);

/// Verify that FP is enabled and that **kernel code does not disturb FPSIMD state**.
///
/// The replacement for `verify_fp_trapped`, and it checks the consequence rather than the
/// mechanism. The old trap could only catch FP the kernel actually executed, and only by
/// faulting; with FP necessarily enabled at EL1 there is no trap to be had, so instead:
/// load a known pattern into every vector register, do representative kernel work over it
/// — formatting, allocation, and a scheduler round trip, which is where a clobber would
/// actually bite — and check the pattern survived.
///
/// Not airtight: it covers the paths it exercises, not all of them. The build-time
/// softfloat assertion at the top of this module is the stronger of the two guards; this
/// one catches hand-written `asm!` and anything linked in that the target flag does not
/// govern.
pub fn selftest() -> bool {
    // A pattern where every register differs, so a clobber of any single one is visible
    // and a swap between two is not mistaken for preservation.
    let mut pattern = FpState::new();
    for (i, slot) in pattern.v.iter_mut().enumerate() {
        *slot = 0xA5A5_0000_0000_0000_u128 << 64 | (0x1000 + i as u128);
    }

    // SAFETY: `pattern` is a live, aligned FpState.
    unsafe { restore(&pattern) };

    // Representative kernel work, chosen for what it touches rather than for volume:
    // `format!` exercises `core::fmt` (the lowering that forced FP *on* back in 7.0b, when
    // the kernel was hardfloat), the allocator runs, and `yield_now` forces a real context
    // switch — including this task's own FP save and restore, so the round trip is part of
    // what is being checked.
    let s = alloc::format!("[fp] selftest scratch {}", crate::arch::time::tick_count());
    core::hint::black_box(&s);
    crate::sched::yield_now();

    let mut observed = FpState::new();
    // SAFETY: `observed` is a live, aligned FpState.
    unsafe { save(&mut observed) };

    let mut ok = true;
    for i in 0..32 {
        if observed.v[i] != pattern.v[i] {
            println!(
                "[fp] selftest: FAIL — v{} changed across kernel work: wrote {:#034x}, \
                 read back {:#034x}",
                i, pattern.v[i], observed.v[i]
            );
            CLOBBERS.fetch_add(1, Ordering::Relaxed);
            ok = false;
            break; // one is enough; listing all 32 buries the signal
        }
    }

    if ok {
        println!(
            "[fp] selftest: PASS (FPEN=0b11, v0-v31 survived core::fmt, the allocator and \
             a context switch; kernel is softfloat by build assertion)"
        );
    }
    ok
}
