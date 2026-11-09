//! # The user-memory chokepoint — one implementation, both architectures
//!
//! `copy_from_user`, `copy_to_user` and the range check they share. This is where a
//! ring-3 pointer first reaches the kernel, and the only place in the tree where "the
//! caller is hostile" is the operating assumption.
//!
//! ## Why this module exists (Phase 8.6)
//!
//! Until 8.6 there were **two** of these, and [`crate::arch::syscall`]'s own module docs
//! spelled out why that was not a facade but a collision:
//!
//! | | x86_64 | aarch64 |
//! |---|---|---|
//! | `copy_from_user` | `(uptr, len) -> Option<Vec<u8>>`, safe | `(dst: &mut [u8], src) -> Result<…>`, **`unsafe`** |
//! | `user_range_ok` | bounds check **plus a page-table walk proving every page is mapped** | bounds check only |
//!
//! Different arity, argument order, return type, safety — and, far worse than any of
//! those, a different *security guarantee*. The two were unrelated functions that
//! happened to share a spelling. That file said unifying them "wants a live consumer to
//! design against"; the filesystem syscalls arriving on aarch64 in 8.6 are that consumer,
//! and they are also what makes the difference load-bearing rather than theoretical:
//!
//! **Routing `SYS_STAT` at aarch64's bounds-only check would have handed ring 3 a node
//! halt in one syscall.** An in-range but unmapped pointer passes a bounds check, and the
//! read then takes a data abort at EL1 which the Phase 7.2 handler treats as fatal. x86
//! never had that hole, because its check walks the page tables first. Porting the
//! *dispatcher* without porting the *guarantee* is the kind of parity that makes a skip
//! list shorter and the kernel weaker, so the guarantee moved here and both architectures
//! now get the stronger one.
//!
//! ## What is guaranteed, stated narrowly
//!
//! After [`user_range_ok`] returns true: every page of the range is mapped **in the
//! address space of the process the scheduler currently attributes this task to**, the
//! range is wholly inside that architecture's user regime, it does not wrap, and it does
//! not start at null. That is a strictly stronger claim than either predecessor made on
//! aarch64, and exactly the claim x86 already made.
//!
//! What it still does **not** guarantee, and no amount of checking here can:
//!
//! * **TOCTOU.** Another task sharing the address space can unmap a page between the walk
//!   and the copy. The window is small and the fix is not a bigger check — it is an
//!   exception table, so the faulting instruction is registered and the abort handler
//!   redirects to a fixup returning `-EFAULT`. x86 has carried this same window since
//!   Phase 3; unifying the check did not introduce it and does not close it.
//! * **A fault is still fatal on aarch64.** `exceptions.rs` dispatches only `EC_SVC64`
//!   from EL0; every other synchronous exception falls through to the fatal arm. So the
//!   node-halt surface is "anything EL0 can do wrong", of which user pointers passed to
//!   syscalls is now the *smaller* part rather than the whole. Closing it is one job — a
//!   fault handler that kills the task instead of the node — and it is not this one.
//!
//! ## PAN
//!
//! Reading EL0 memory from EL1 works today and — stated carefully, because the natural
//! phrasing gets this backwards in the dangerous direction — **will keep working on
//! Armv8.1+ parts too, unless someone enables PAN.** `PSTATE.PAN` must be 1 for
//! privileged accesses to EL0 memory to fault, and whether exception entry sets it is
//! governed by `SCTLR_EL1.SPAN`, which resets to 1 meaning "leave it unchanged",
//! precisely for compatibility with Armv8.0 software. This kernel never writes
//! `SCTLR_EL1.SPAN` and never executes `msr PAN, #1`.
//!
//! That is the problem, not the reassurance: the hardware phase must turn PAN *on*, and
//! at that point the copies below must bracket each access with `msr PAN, #0` /
//! `msr PAN, #1`, or use `LDTR`/`STTR`, the unprivileged load/store forms, as Linux does.
//! This module being a single chokepoint is what makes that a small change rather than an
//! audit.
//!
//! ## The accesses are volatile and byte-wise, on both architectures now
//!
//! Inherited from the aarch64 implementation, which had it right, and applied to x86 as
//! part of the merge. x86's version built a `&[u8]` over user memory with
//! `slice::from_raw_parts` and called `to_vec()`. A shared reference asserts the bytes
//! will not change for its lifetime, which is false by construction for memory another
//! task is free to write — so that form was UB on a technicality that a sufficiently
//! clever optimiser is entitled to act on. Byte-wise `read_volatile` also stops the
//! compiler widening a load into a form that straddles the end of a mapping.
//!
//! The cost is real (a byte at a time, no `rep movsb`) and is accepted: these transfers
//! are capped at [`FS_MAX_XFER`] and sit behind syscalls that then block on a ring-3
//! server, so the copy is not the expensive part of any of them.

use crate::arch::paging;
use alloc::vec::Vec;

/// Cap on a single syscall transfer, bounding kernel allocation against a hostile or
/// merely buggy user length.
///
/// Shared by the filesystem, socket and management dispatchers. Named `FS_` for its
/// first caller in Phase 3 and kept rather than renamed, because the number is what
/// matters and the spelling appears at a dozen call sites.
pub(crate) const FS_MAX_XFER: usize = 256 * 1024;

/// Is `[uptr, uptr + len)` wholly mapped in the current process's address space, inside
/// the user regime, non-wrapping and not based at null?
///
/// Returns `false` on any gap. See the module docs for what this does and does not
/// establish — in particular that it is a check, not a fault guard.
///
/// A zero-length range is accepted without dereferencing anything: callers legitimately
/// pass empty slices, and rejecting them pushes the special case outward to every one of
/// them.
pub(crate) fn user_range_ok(uptr: u64, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    // Reject a wrapping end first, so a huge `len` cannot carry the range past the
    // comparisons below by overflowing into a small number.
    let Some(end) = uptr.checked_add(len as u64) else {
        return false;
    };
    if uptr == 0 || end > paging::user_addr_limit() {
        return false;
    }
    // Then prove every page is actually translated. This is the half aarch64 did not
    // have before 8.6, and the reason a bad pointer is an error rather than a dead node.
    //
    // Resolved through the *process* table rather than the active hardware root, because
    // the question is "may this process name this address", and on aarch64 the scheduler
    // reaches a task's translation root by a different path than on x86 (see
    // `sched::spawn_in_process`). `with_address_space` returns `None` for a task with no
    // process — a kernel task, or the bare-EL0 self-tests — and that collapses to
    // `false`, which is correct: a caller with no user address space has no user
    // pointers. Those self-tests therefore use the bounds-only check in
    // `arch::aarch64::uaccess`, deliberately and under its own name.
    let pid = crate::sched::current_process_id();
    crate::process::with_address_space(pid, |a| {
        let mut page = uptr & !0xFFF;
        while page < end {
            if a.translate(crate::mm::addr::VirtAddr::new(page)).is_none() {
                return false;
            }
            page += 0x1000;
        }
        true
    })
    .unwrap_or(false)
}

/// Copy `len` bytes from a user pointer into a fresh kernel `Vec`.
///
/// Returns `None` if the length exceeds [`FS_MAX_XFER`] or the range does not validate.
pub(crate) fn copy_from_user(uptr: u64, len: usize) -> Option<Vec<u8>> {
    // A zero-length transfer touches no memory, and must not dereference the pointer —
    // which may be null — on the way to discovering that.
    if len == 0 {
        return Some(Vec::new());
    }
    if len > FS_MAX_XFER || !user_range_ok(uptr, len) {
        return None;
    }
    let mut out = alloc::vec![0u8; len];
    // SAFETY: `user_range_ok` has established that every page of `[uptr, uptr+len)` is
    // mapped in the calling process's address space, which is the one installed in the
    // translation base during a syscall from that process. Volatile and byte-wise: see
    // the module docs on why a `&[u8]` over user memory is the wrong tool here.
    unsafe {
        for (i, b) in out.iter_mut().enumerate() {
            *b = core::ptr::read_volatile((uptr + i as u64) as *const u8);
        }
    }
    Some(out)
}

/// Copy `data` to a user pointer. Returns `false` if the range does not validate.
pub(crate) fn copy_to_user(uptr: u64, data: &[u8]) -> bool {
    // Nothing to copy → success, without dereferencing a possibly-null pointer.
    if data.is_empty() {
        return true;
    }
    if !user_range_ok(uptr, data.len()) {
        return false;
    }
    // SAFETY: as `copy_from_user`, in the other direction. The write direction makes the
    // obligation sharper — this writes through whatever translation base is installed —
    // and `user_range_ok` discharging it via the process table is what makes that the
    // intended process's space rather than whichever one happened to be live.
    unsafe {
        for (i, b) in data.iter().enumerate() {
            core::ptr::write_volatile((uptr + i as u64) as *mut u8, *b);
        }
    }
    true
}
