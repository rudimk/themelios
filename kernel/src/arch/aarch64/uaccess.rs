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
//! ## PAN
//!
//! Reading EL0 memory from EL1 works today because Privileged Access Never is Armv8.1 and
//! the emulated CPU (`cortex-a72`) is Armv8.0. Real server parts implement PAN, where
//! these functions must bracket the access with `msr PAN, #0` / `msr PAN, #1` (or use
//! `LDTR`/`STTR`, the unprivileged-access load/store forms, which is what Linux does).
//! Without that, every one of these copies faults on hardware. Flagged for the hardware
//! phase, and the reason this file is a chokepoint rather than open-coded copies.

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
    // `1 << bits` would overflow at bits == 64; the regime is never that large, but the
    // shift is written to be total rather than relying on it.
    let bits = paging::user_va_bits();
    if bits >= 64 {
        return true;
    }
    end <= (1u64 << bits)
}

/// Copy `dst.len()` bytes from user address `src` into `dst`.
///
/// # Safety
///
/// This function is safe to *call* — it validates the range first. It is not safe in the
/// stronger sense that the read cannot fault: see the module docs on the missing
/// exception table.
pub fn copy_from_user(dst: &mut [u8], src: u64) -> Result<(), UserCopyError> {
    if !user_range_ok(src, dst.len()) {
        return Err(UserCopyError::OutOfRange);
    }
    // SAFETY: the range lies inside the user regime, which the active TTBR0 tree
    // translates. Byte-wise and volatile so the compiler cannot widen the access into a
    // form that would straddle the end of a mapping, and cannot assume the source is
    // unchanging (it is userspace memory; another task may be writing it).
    for (i, b) in dst.iter_mut().enumerate() {
        *b = unsafe { core::ptr::read_volatile((src + i as u64) as *const u8) };
    }
    Ok(())
}

/// Copy `src` into user address `dst`.
///
/// # Safety
///
/// As [`copy_from_user`]: the range is validated, the fault is not caught.
#[allow(dead_code)] // first consumer arrives with the ring-3 servers in 8.5
pub fn copy_to_user(dst: u64, src: &[u8]) -> Result<(), UserCopyError> {
    if !user_range_ok(dst, src.len()) {
        return Err(UserCopyError::OutOfRange);
    }
    // SAFETY: as above, in the other direction.
    for (i, b) in src.iter().enumerate() {
        unsafe { core::ptr::write_volatile((dst + i as u64) as *mut u8, *b) };
    }
    Ok(())
}
