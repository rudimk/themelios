//! # The filesystem syscall surface — one dispatcher, both architectures
//!
//! `SYS_OPEN`, `SYS_READ_FILE`, `SYS_WRITE_FILE`, `SYS_CLOSE`, `SYS_STAT` and
//! `SYS_READDIR`, from a ring-3 caller to the capability-checked VFS operations in
//! [`crate::fs`].
//!
//! These are thin by design: validate and copy the user pointers, then delegate. The
//! interesting work — capability checks, the IPC round trip to the filesystem server — is
//! in `vfs_*`, and the interesting *risk* is in [`crate::arch::uaccess`]. This module is
//! the glue, and its only job is to get the arguments out of the frame in the right order
//! and the errors back in the right encoding.
//!
//! ## Why it is arch-neutral (Phase 8.6)
//!
//! Until 8.6 this lived in `arch::x86_64::syscall` as `dispatch_fs_syscall`, reading
//! `frame.rdi`, `frame.rsi`, `frame.rdx` and `frame.r10` directly, and the aarch64
//! dispatcher answered all six numbers with `ENOSYS` from an explicitly written list,
//! whose comment said: *"deleting a number from this list is how it becomes implemented,
//! and a list is something you can read to see what is missing."* This module is that
//! deletion.
//!
//! It is one implementation rather than two because there is nothing architectural in it.
//! The arguments are read through the positional accessors on
//! [`crate::arch::syscall::SyscallFrame`] (`arg0()`-`arg3()`), the user copies through
//! the now-shared [`crate::arch::uaccess`], and everything else is `crate::fs`. Writing a
//! second copy for aarch64 would have meant two dispatchers to keep in step across every
//! future change to the FS ABI — and the failure mode of a drifted dispatcher is a
//! syscall that works on one architecture and silently reads the wrong argument slot on
//! the other.
//!
//! ## The one trap in here: do not reach for `arg4()` or `arg5()`
//!
//! **Positions 0-3 are the same registers under both the native and the Linux ABI;
//! positions 4 and 5 are not, on x86_64.** The frame's `arg4()`/`arg5()` return `r8`/`r9`,
//! which is the *Linux* x86-64 ABI and is what [`crate::linux`] needs. The native
//! ThemeliOS ABI that `libthemelios` emits puts argument 4 in `r9` and argument 5 in
//! `r8` — crossed — and its own comment flags this as deliberate and preserved. On
//! aarch64 both ABIs use `x0`-`x5` and the question does not arise, so a mistake here
//! would be invisible on arm64 and wrong on amd64.
//!
//! The filesystem calls use positions 0-3 only, which is why this module can use the
//! positional accessors at all. **Any native syscall that needs positions 4 or 5 must get
//! its own accessors first** (`arg_native4`/`arg_native5`, or an ABI parameter) rather
//! than borrowing these. `SYS_SENDTO`, `SYS_MGMT`, `SYS_SEND` and `SYS_CALL` all use
//! those slots today and all still read their registers by name in the x86 dispatcher,
//! which is correct for them and is why nothing is broken right now.
//!
//! ## Read every argument before writing the return
//!
//! Enforced by construction below: each arm pulls what it needs into locals first. The
//! reason is that the return slot aliases a *different* register on each architecture —
//! the number's (`rax`) on x86, argument 0's (`x0`) on aarch64 — so on aarch64 writing
//! the return before reading `arg0()` silently loses the capability handle. See
//! [`crate::arch::syscall`]'s module docs, which state the rule and note that an earlier
//! version of them got it half right.

// Everything architecture-facing comes through the one facade, including the user-copy
// helpers it re-exports from `crate::arch::uaccess`. Importing those from `uaccess`
// directly also works and was the first draft, but it left the facade's re-export unused
// on aarch64 — `mod linux` is its only other consumer and is still x86-gated — and an
// unused re-export is the shape of thing a review of this kernel has twice found to be
// decorative. Going through the facade makes it load-bearing on both architectures.
use crate::arch::syscall::{
    abi, copy_from_user, copy_to_user, user_range_ok, SyscallFrame, FS_MAX_XFER,
};
use crate::cap::CapHandle;
use crate::fs::{self, FsError};

/// Dispatch one filesystem syscall from `frame`.
///
/// The caller is responsible for having enabled interrupts: every arm here can block on
/// the ring-3 filesystem server through `vfs_*`, and a blocking wait with interrupts
/// masked never wakes.
pub fn dispatch(frame: &mut SyscallFrame) {
    let pid = crate::sched::current_process_id();

    // Read the number and every argument up front — see the module docs on return-slot
    // aliasing. `nr` is also what the audit record needs, and on x86 reading it after a
    // write to the return slot would yield the return value instead.
    let nr = frame.nr();
    let (a0, a1, a2, a3) = (frame.arg0(), frame.arg1(), frame.arg2(), frame.arg3());

    crate::audit::log_event(pid, crate::audit::AuditOp::FsAccess, crate::cap::CapType::Null, nr);
    let bad_arg = FsError::InvalidArgument.as_syscall_ret();

    match nr {
        // a0 = Filesystem cap, a1 = path ptr, a2 = path len, a3 = flags.
        abi::SYS_OPEN => {
            let fs_handle = CapHandle::from_raw(a0 as u32);
            match copy_from_user(a1, a2 as usize) {
                Some(path) => match fs::vfs_open(pid, fs_handle, &path) {
                    Ok(fd_raw) => frame.set_ret(fd_raw as u64),
                    Err(e) => frame.set_ret(e.as_syscall_ret()),
                },
                None => frame.set_ret(bad_arg),
            }
        }

        // a0 = fd cap, a1 = buf ptr, a2 = buf len, a3 = file offset.
        abi::SYS_READ_FILE => {
            let fd = CapHandle::from_raw(a0 as u32);
            let len = (a2 as usize).min(FS_MAX_XFER);
            if !user_range_ok(a1, len) {
                frame.set_ret(bad_arg);
                return;
            }
            let mut kbuf = alloc::vec![0u8; len];
            match fs::vfs_read(pid, fd, a3, &mut kbuf) {
                Ok(n) if copy_to_user(a1, &kbuf[..n]) => frame.set_ret(n as u64),
                Ok(_) => frame.set_ret(bad_arg),
                Err(e) => frame.set_ret(e.as_syscall_ret()),
            }
        }

        // a0 = fd cap, a1 = buf ptr, a2 = buf len, a3 = file offset.
        abi::SYS_WRITE_FILE => {
            let fd = CapHandle::from_raw(a0 as u32);
            match copy_from_user(a1, a2 as usize) {
                Some(data) => match fs::vfs_write(pid, fd, a3, &data) {
                    Ok(n) => frame.set_ret(n as u64),
                    Err(e) => frame.set_ret(e.as_syscall_ret()),
                },
                None => frame.set_ret(bad_arg),
            }
        }

        // a0 = fd cap.
        abi::SYS_CLOSE => {
            let fd = CapHandle::from_raw(a0 as u32);
            match fs::vfs_close(pid, fd) {
                Ok(()) => frame.set_ret(0),
                Err(e) => frame.set_ret(e.as_syscall_ret()),
            }
        }

        // a0 = Filesystem cap, a1 = path ptr, a2 = path len, a3 = stat-out ptr.
        abi::SYS_STAT => {
            let fs_handle = CapHandle::from_raw(a0 as u32);
            match copy_from_user(a1, a2 as usize) {
                Some(path) => match fs::vfs_stat(pid, fs_handle, &path) {
                    Ok((size, is_dir)) => {
                        // The user buffer is `[size: u64, is_dir: u64]`, little-endian.
                        let mut out = [0u8; 16];
                        out[0..8].copy_from_slice(&size.to_le_bytes());
                        out[8..16].copy_from_slice(&(is_dir as u64).to_le_bytes());
                        if copy_to_user(a3, &out) {
                            frame.set_ret(0);
                        } else {
                            frame.set_ret(bad_arg);
                        }
                    }
                    Err(e) => frame.set_ret(e.as_syscall_ret()),
                },
                None => frame.set_ret(bad_arg),
            }
        }

        // a0 = fd cap, a1 = entries-out ptr, a2 = max entries, a3 = out buffer len.
        abi::SYS_READDIR => {
            let fd = CapHandle::from_raw(a0 as u32);
            let out_len = (a3 as usize).min(FS_MAX_XFER);
            if !user_range_ok(a1, out_len) {
                frame.set_ret(bad_arg);
                return;
            }
            let mut kbuf = alloc::vec![0u8; out_len];
            match fs::vfs_readdir(pid, fd, a2, &mut kbuf) {
                Ok(count) if copy_to_user(a1, &kbuf) => frame.set_ret(count),
                Ok(_) => frame.set_ret(bad_arg),
                Err(e) => frame.set_ret(e.as_syscall_ret()),
            }
        }

        // Unreachable via either architecture's dispatcher, which match the six numbers
        // above explicitly before calling in. Answered rather than `unreachable!()`
        // because the alternative is a kernel panic reachable from ring 3 if a future
        // dispatcher ever widens its match.
        _ => frame.set_ret(bad_arg),
    }
}
