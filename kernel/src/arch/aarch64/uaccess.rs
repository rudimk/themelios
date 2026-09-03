//! # Copying across the EL0 boundary
//!
//! `copy_from_user` / `copy_to_user` and the range check they share. This is the surface
//! where a user-supplied pointer first reaches the kernel, and the only place in the
//! aarch64 port where "the caller is hostile" is the operating assumption.
//!
//! ## The bound is derived from `TCR_EL1`, not written down
//!
//! The user regime spans `[0, 2^(64 - T0SZ))`. That is a hardware-configured quantity, so
//! [`user_range_ok`] reads it through [`crate::arch::paging::user_va_bits`] rather than
//! hard-coding 48 bits.
//!
//! The two failure directions are not symmetric, which is why this matters. A bound that
//! is too *small* merely rejects addresses a process could legitimately use — annoying,
//! visible, harmless. A bound that is too *large* admits addresses beyond the top of the
//! regime, and the check then passes a pointer the walker will never translate as the
//! caller intended. `verify_tcr` asserts the T0SZ this is derived from at boot, so the
//! constant and the hardware cannot drift apart silently.
//!
//! ## What this deliberately does not do yet
//!
//! It does not survive a fault. A user pointer that is in-range but *unmapped* takes a
//! data abort at EL1, which the Phase 7.2 handler treats as fatal — so a malicious or
//! merely buggy process can currently halt the node by passing a plausible pointer into a
//! hole.
//!
//! Linux solves this with an exception table: the faulting instruction is registered, and
//! the abort handler redirects to a fixup that returns `-EFAULT`. That machinery is real
//! work and belongs with the rest of the fault-handling port; recording the gap here is
//! not a substitute for it, but it is better than the gap being discovered by someone
//! reading the abort. **This is a hostile-input hole and it is open.**
//!
//! It is also **not the widest one**, and an earlier version of this note implied it was.
//! `exceptions.rs` dispatches only `EC_SVC64` from slot 8; *every other* synchronous
//! exception from EL0 — a data abort on the payload's own bad pointer, an instruction
//! abort, an undefined instruction, an SP-alignment fault — falls through to the fatal arm
//! and panics, as do slots 10 and 11. So the node-halt surface is "anything EL0 can do
//! wrong", not "user pointers passed to syscalls". Closing it is one job (a fault handler
//! that kills the task instead of the node), not two, and this file is only one of its
//! callers.
//!
//! ## PAN
//!
//! Reading EL0 memory from EL1 works today, and — stated carefully, because the natural
//! phrasing gets this backwards in the dangerous direction — **it will keep working on
//! Armv8.1+ parts too, unless someone enables PAN.**
//!
//! Privileged Access Never is Armv8.1 and the emulated CPU (`cortex-a72`) is Armv8.0, so
//! `PSTATE.PAN` does not exist here. But merely *implementing* FEAT_PAN does not make
//! privileged accesses to EL0-accessible memory fault: `PSTATE.PAN` must be **1**, and
//! whether exception entry to EL1 sets it is governed by `SCTLR_EL1.SPAN`, which resets to
//! 1 — meaning "leave `PSTATE.PAN` unchanged" — precisely for backwards compatibility with
//! Armv8.0 software. This kernel never writes `SCTLR_EL1.SPAN` and never executes
//! `msr PAN, #1`, so on a modern server part these copies would behave exactly as they do
//! under QEMU.
//!
//! That is the problem, not the reassurance. The hardware phase must *turn PAN on* — and
//! at that point these functions must bracket each access with `msr PAN, #0` /
//! `msr PAN, #1`, or use `LDTR`/`STTR`, the unprivileged-access load/store forms, which is
//! what Linux does. Until then the port is silently running without the protection on
//! hardware that offers it. This file being a chokepoint is what makes that a small change
//! rather than an audit.

use crate::arch::paging;

/// Why a user copy was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserCopyError {
    /// The range is not entirely inside the user address regime.
    OutOfRange,
}

/// Is `[base, base + len)` entirely inside the EL0 address regime?
///
/// Rejects, in order: an overflowing end (so a huge `len` cannot wrap past the check), and
/// an end above the top of the regime. A zero-length range at a valid base is accepted —
/// callers pass empty slices, and rejecting them would push the special case outward.
pub fn user_range_ok(base: u64, len: usize) -> bool {
    let Some(end) = base.checked_add(len as u64) else {
        return false;
    };
    // `1 << bits` would overflow at bits == 64, so the shift is guarded rather than left
    // to rely on `T0SZ` never being 0. The guard **rejects**, and that direction is the
    // whole point: an earlier version returned `true` here, which is the "bound too large"
    // failure this module's own docs argue is the dangerous one — it would have accepted
    // every address in the machine, kernel VAs included, in the one function whose job is
    // to reject them. Unreachable today (`verify_tcr` asserts `T0SZ == 16`, so this is
    // always 48), but a fail-open default in a fail-closed file is worth removing on sight
    // rather than on exploitation.
    let bits = paging::user_va_bits();
    if bits >= 64 {
        return false;
    }
    end <= (1u64 << bits)
}

/// Copy `dst.len()` bytes from user address `src` into `dst`.
///
/// # Safety
///
/// **`unsafe` because a validated range is not a mapped range.** An earlier version was a
/// safe `fn` whose docs said it was "safe to *call* — it validates the range first … not
/// safe in the stronger sense that the read cannot fault". That is a category error: a
/// safe function that can read an unmapped address and take a fatal abort is unsound, not
/// safe-in-a-weaker-sense. [`user_range_ok`] establishes that the address is *numerically*
/// inside the EL0 regime and nothing more — not that it is mapped, not that the tree
/// currently in `TTBR0_EL1` belongs to the calling task, not that any user space is
/// installed at all.
///
/// The caller must ensure the address space this pointer belongs to is the one installed
/// in `TTBR0_EL1` — which on the syscall path is true by construction, and off it is not.
/// The fault risk itself cannot be discharged by any caller until there is an exception
/// table; see the module docs.
pub unsafe fn copy_from_user(dst: &mut [u8], src: u64) -> Result<(), UserCopyError> {
    if !user_range_ok(src, dst.len()) {
        return Err(UserCopyError::OutOfRange);
    }
    // SAFETY: the range lies inside the user regime. Note what this does *not* claim — an
    // earlier version of this comment asserted the range was one "which the active TTBR0
    // tree translates", which the module docs four screens up explicitly say may not hold;
    // that is the caller's obligation, discharged above, not a fact established here.
    // Byte-wise and volatile so the compiler cannot widen the access into a form that
    // would straddle the end of a mapping, and cannot assume the source is unchanging (it
    // is userspace memory; another task may be writing it).
    for (i, b) in dst.iter_mut().enumerate() {
        *b = unsafe { core::ptr::read_volatile((src + i as u64) as *const u8) };
    }
    Ok(())
}

/// Copy `src` into user address `dst`.
///
/// # Safety
///
/// As [`copy_from_user`], and the write direction makes the obligation sharper: this
/// writes to whatever the installed `TTBR0_EL1` maps at `dst`. The caller must ensure that
/// tree is the intended task's.
#[allow(dead_code)] // first consumer arrives with the ring-3 servers in 8.5
pub unsafe fn copy_to_user(dst: u64, src: &[u8]) -> Result<(), UserCopyError> {
    if !user_range_ok(dst, src.len()) {
        return Err(UserCopyError::OutOfRange);
    }
    // SAFETY: as above, in the other direction.
    for (i, b) in src.iter().enumerate() {
        unsafe { core::ptr::write_volatile((dst + i as u64) as *mut u8, *b) };
    }
    Ok(())
}
