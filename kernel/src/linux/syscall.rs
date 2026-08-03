//! # Linux syscall dispatch (Phase 5.1)
//!
//! Services the Linux x86-64 syscall ABI for `Personality::Linux` processes. The
//! kernel's shared `syscall` entry (`arch::x86_64::syscall`) branches here when
//! the calling process is a Linux one — Linux syscall numbers collide with the
//! native ThemeliOS ones, so the *personality* selects the table.
//!
//! ## ABI
//!
//! - `rax` = syscall number; args in `rdi, rsi, rdx, r10, r8, r9`; return in `rax`.
//! - Errors are returned as `-errno` (two's complement) in `rax`, the Linux
//!   convention — the C runtime reads `rax` as a signed value and, if in
//!   `[-4095, -1]`, treats `-rax` as `errno`.
//!
//! ## Scope for 5.1
//!
//! The minimum a statically-linked program's `_start` needs to run and exit:
//! `write`/`writev` (stdout/stderr → serial), `read` (stdin → EOF), `brk`,
//! anonymous `mmap`/`munmap`/`mprotect`, `arch_prctl(SET_FS)` (TLS), `ioctl`
//! (isatty → `-ENOTTY`), `set_tid_address`, `clock_gettime`, `getrandom`, the
//! id-getters, and `exit`/`exit_group`. Filesystem syscalls over the VFS are 5.2;
//! threads + `futex` are 5.3. Everything unimplemented returns `-ENOSYS`.

use crate::arch::x86_64::syscall::{copy_from_user, copy_to_user, user_range_ok, SyscallFrame};
use crate::mm::addr::VirtAddr;
use crate::mm::page_table::PageFlags;
use crate::process::{self, LINUX_BRK_BASE, LINUX_MMAP_BASE};

extern crate alloc;

// --- Linux x86-64 syscall numbers (the subset we handle) ---
const SYS_CLOSE: u64 = 3;
const SYS_MMAP: u64 = 9;
const SYS_MPROTECT: u64 = 10;
const SYS_MUNMAP: u64 = 11;
const SYS_BRK: u64 = 12;
const SYS_RT_SIGACTION: u64 = 13;
const SYS_RT_SIGPROCMASK: u64 = 14;
const SYS_IOCTL: u64 = 16;
const SYS_WRITEV: u64 = 20;
/// `socket(2)` — deliberately denied for containers (see the dispatch arm).
const SYS_SOCKET: u64 = 41;
/// `wait4(2)` — no parent/child linkage exists, so it reports "no children".
const SYS_WAIT4: u64 = 61;
/// `kill(2)` — a process may only signal itself (no cross-process signal cap).
const SYS_KILL: u64 = 62;
const SYS_SCHED_YIELD: u64 = 24;
const SYS_CLONE: u64 = 56;
const SYS_FUTEX: u64 = 202;
const SYS_GETPID: u64 = 39;
const SYS_EXIT: u64 = 60;
const SYS_GETUID: u64 = 102;
const SYS_GETGID: u64 = 104;
const SYS_GETEUID: u64 = 107;
const SYS_GETEGID: u64 = 108;
const SYS_ARCH_PRCTL: u64 = 158;
const SYS_GETTID: u64 = 186;
const SYS_CLOCK_GETTIME: u64 = 228;
const SYS_EXIT_GROUP: u64 = 231;
const SYS_SET_TID_ADDRESS: u64 = 218;
const SYS_GETRANDOM: u64 = 318;

// --- errno values (negated on return) ---
const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EBADF: i64 = 9;
const ECHILD: i64 = 10;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const ENOTTY: i64 = 25;
const ENOSYS: i64 = 38;

/// Encode `-errno` as the `u64` the caller reads back in `rax`.
fn err(e: i64) -> u64 {
    (-e) as u64
}

/// `arch_prctl` subfunction: set the `%fs` base (thread pointer / TLS).
const ARCH_SET_FS: u64 = 0x1002;
/// `arch_prctl` subfunction: read the `%fs` base into a user pointer.
const ARCH_GET_FS: u64 = 0x1003;

/// `mmap` flag: anonymous mapping (not file-backed).
const MAP_ANONYMOUS: u64 = 0x20;

/// Dispatch one Linux syscall. Reads the number/args from `frame` and writes the
/// result (or `-errno`) into `frame.rax`.
pub fn dispatch(frame: &mut SyscallFrame) {
    // Filesystem syscalls (read/write/open/openat/close/fstat/lseek/getdents64/
    // getcwd/chdir/newfstatat/…) are serviced by the VFS-backed `fs` module,
    // which owns those numbers (Phase 5.2). If it doesn't recognise the number,
    // fall through to the process/memory/misc table below.
    if let Some(r) = super::fs::dispatch(frame.rax, frame) {
        frame.rax = r;
        return;
    }
    let pid = crate::sched::current_process_id();
    match frame.rax {
        SYS_WRITEV => frame.rax = sys_writev(frame.rdi, frame.rsi, frame.rdx),
        SYS_BRK => frame.rax = sys_brk(pid, frame.rdi),
        SYS_MMAP => frame.rax = sys_mmap(pid, frame.rsi, frame.r10, frame.r8),
        // Accept mprotect/munmap as no-ops for now (W^X refinement + real unmap
        // are later work); returning success keeps allocators happy.
        SYS_MPROTECT | SYS_MUNMAP => frame.rax = 0,
        SYS_ARCH_PRCTL => frame.rax = sys_arch_prctl(frame.rdi, frame.rsi),
        // isatty(): report "not a terminal" so stdio picks full buffering.
        SYS_IOCTL => frame.rax = err(ENOTTY),
        // Threads + futex (Phase 5.3).
        SYS_CLONE => frame.rax = super::thread::sys_clone(frame),
        SYS_FUTEX => {
            // FUTEX_WAIT may block, so enable interrupts first (like SYS_YIELD).
            crate::arch::irq::enable();
            frame.rax = super::thread::sys_futex(frame);
        }
        SYS_SET_TID_ADDRESS => frame.rax = super::thread::sys_set_tid_address(frame.rdi),
        SYS_GETTID => frame.rax = crate::sched::current_task_id() as u64,
        SYS_GETPID => frame.rax = pid.as_usize() as u64,
        // Everything runs as root (uid/gid 0) for now.
        SYS_GETUID | SYS_GETEUID | SYS_GETGID | SYS_GETEGID => frame.rax = 0,
        // Signals are stubbed: accept sigaction/sigprocmask so runtimes proceed.
        // (Signal-handler *delivery* is deferred — see Phase 5.7 notes.)
        SYS_RT_SIGACTION | SYS_RT_SIGPROCMASK => frame.rax = 0,
        // Capability isolation (Phase 5.7): a container holds no `SOCKET_FACTORY`
        // capability, and the Linux `socket()` ABI carries no capability handle to
        // present one. So `socket()` is an unconditional, *enforced* `-EPERM` —
        // a checked denial with a real Linux errno, not the `-ENOSYS` default.
        // (Reusing the native `socket::sys_socket` would be wrong twice over: its
        // `SockError` uses the native high-bit ABI, which Linux userspace misreads
        // as a huge valid fd, and it needs a cap handle the ABI can't supply.)
        SYS_SOCKET => frame.rax = err(EPERM),
        // `kill(2)`: no cross-process signal capability exists, so a process may
        // only signal itself. `wait4(2)`: no parent/child linkage, so "no children".
        SYS_KILL => sys_kill(pid, frame.rdi, frame.rsi, &mut frame.rax),
        SYS_WAIT4 => frame.rax = err(ECHILD),
        SYS_CLOSE => frame.rax = 0,
        SYS_CLOCK_GETTIME => frame.rax = sys_clock_gettime(frame.rsi),
        SYS_GETRANDOM => frame.rax = sys_getrandom(frame.rdi, frame.rsi),
        SYS_SCHED_YIELD => {
            crate::arch::irq::enable();
            crate::sched::yield_now();
            frame.rax = 0;
        }
        // `exit` terminates just the calling thread (honouring CLONE_CHILD_
        // CLEARTID so a joiner wakes); `exit_group` terminates the whole process.
        SYS_EXIT => super::thread::thread_exit(frame.rdi),
        SYS_EXIT_GROUP => super::thread::exit_group(frame.rdi),
        n => {
            crate::println!("[linux] ENOSYS: unimplemented syscall {}", n);
            frame.rax = err(ENOSYS);
        }
    }
}

/// `kill(pid, sig)` (Phase 5.7). ThemeliOS tracks no cross-process signal
/// capability, so a process may only signal **itself**:
/// - target ≠ self → `-EPERM` (we hold no right to signal another process);
/// - `sig == 0` (existence probe) on self → `0` (we're alive);
/// - fatal self-signal → terminate the whole thread group via `exit_group`,
///   recording the conventional `128 + signo` exit status (never returns).
///
/// Writes the result into `*rax` for the non-terminating paths. `ESRCH` is in the
/// errno set for completeness (a signal to a vanished target) but unreachable on
/// the self-only path.
fn sys_kill(current: crate::process::ProcessId, target: u64, sig: u64, rax: &mut u64) {
    if target as usize != current.as_usize() {
        *rax = err(EPERM);
        return;
    }
    if sig == 0 {
        *rax = 0;
        return;
    }
    // Fatal self-signal: never returns.
    super::thread::exit_group(128 + sig);
}

/// Write a container's stdout/stderr (fd 1/2). Routes to the calling container's
/// per-container capture buffer (Phase 6.2, keyed by container id so the log
/// survives the process) and mirrors to the serial console. A non-container Linux
/// process just reaches serial.
fn console_write(bytes: &[u8]) {
    let pid = crate::sched::current_process_id();
    crate::container::registry::write_stdout(pid, bytes);
}

/// `writev(fd, iov, iovcnt)` — gathered write of the `iovec[]` to fd 1/2.
fn sys_writev(fd: u64, iov: u64, iovcnt: u64) -> u64 {
    if fd != 1 && fd != 2 {
        return err(EBADF);
    }
    let count = iovcnt as usize;
    // Each iovec is { iov_base: u64, iov_len: u64 } (16 bytes).
    let table = match copy_from_user(iov, count * 16) {
        Some(t) => t,
        None => return err(EFAULT),
    };
    let mut total: u64 = 0;
    for i in 0..count {
        let base = u64::from_le_bytes(table[i * 16..i * 16 + 8].try_into().unwrap());
        let ilen = u64::from_le_bytes(table[i * 16 + 8..i * 16 + 16].try_into().unwrap()) as usize;
        if ilen == 0 {
            continue;
        }
        match copy_from_user(base, ilen) {
            Some(data) => {
                console_write(&data);
                total += ilen as u64;
            }
            None => return err(EFAULT),
        }
    }
    total
}

/// Map `pages` fresh, zeroed, user pages at `vaddr` in process `pid` with `flags`.
/// Returns false if a frame allocation or the address-space lookup fails.
fn map_anon_pages(pid: process::ProcessId, vaddr: u64, pages: usize, flags: PageFlags) -> bool {
    let page = crate::mm::PAGE_SIZE;
    for i in 0..pages {
        let phys = match crate::mm::frame::allocate_frame() {
            Some(p) => p,
            None => return false,
        };
        // SAFETY: fresh HHDM-mapped frame, exclusively owned.
        unsafe {
            core::ptr::write_bytes(phys.to_virt().as_mut_ptr::<u8>(), 0, page as usize);
        }
        let ok = process::with_address_space(pid, |a| {
            a.map_page(VirtAddr::new(vaddr + i as u64 * page), phys, flags);
        })
        .is_some();
        if !ok {
            return false;
        }
    }
    true
}

/// `brk(addr)` — grow/query the program break. Returns the (possibly new) break;
/// on failure, returns the *current* break unchanged (the Linux contract).
fn sys_brk(pid: process::ProcessId, addr: u64) -> u64 {
    let page = crate::mm::PAGE_SIZE;
    // Initialise the break on first use.
    let mut cur = process::get_brk(pid);
    if cur == 0 {
        cur = LINUX_BRK_BASE;
        process::set_brk(pid, cur);
    }
    // Query, or an out-of-range request → no change.
    if addr == 0 || addr < LINUX_BRK_BASE {
        return cur;
    }
    if addr > cur {
        // Map any pages between the old and new page-aligned tops.
        let old_top = (cur + page - 1) & !(page - 1);
        let new_top = (addr + page - 1) & !(page - 1);
        if new_top > old_top {
            let pages = ((new_top - old_top) / page) as usize;
            let flags =
                PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE | PageFlags::NO_EXECUTE;
            if !map_anon_pages(pid, old_top, pages, flags) {
                return cur; // out of memory — leave the break where it was
            }
        }
    }
    // Shrink is accepted without unmapping (simplicity; pages are reused).
    process::set_brk(pid, addr);
    addr
}

/// `mmap(_, len, _, flags, _, _)` — anonymous private mappings only (5.1). Uses a
/// per-process upward bump allocator; ignores the hint address.
fn sys_mmap(pid: process::ProcessId, len: u64, flags: u64, _fd: u64) -> u64 {
    if flags & MAP_ANONYMOUS == 0 {
        // File-backed mmap is 5.3.
        return err(ENOSYS);
    }
    if len == 0 {
        return err(EINVAL);
    }
    let page = crate::mm::PAGE_SIZE;
    let pages = ((len + page - 1) / page) as usize;

    let mut next = process::get_mmap_next(pid);
    if next == 0 {
        next = LINUX_MMAP_BASE;
    }
    let base = next;
    let pflags = PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE | PageFlags::NO_EXECUTE;
    if !map_anon_pages(pid, base, pages, pflags) {
        return err(ENOMEM);
    }
    process::set_mmap_next(pid, base + pages as u64 * page);
    base
}

/// `arch_prctl(code, addr)` — set/get the `%fs` base for Linux TLS.
fn sys_arch_prctl(code: u64, addr: u64) -> u64 {
    match code {
        ARCH_SET_FS => {
            crate::sched::set_current_fs_base(addr);
            0
        }
        ARCH_GET_FS => {
            // Read back the current task's fs base into *addr.
            let base = crate::sched::current_fs_base();
            if !user_range_ok(addr, 8) || !copy_to_user(addr, &base.to_le_bytes()) {
                return err(EFAULT);
            }
            0
        }
        _ => err(EINVAL),
    }
}

/// `clock_gettime(_, timespec*)` — fills a `struct timespec { i64 sec; i64 nsec }`
/// from the kernel's millisecond uptime.
fn sys_clock_gettime(ts_ptr: u64) -> u64 {
    let ms = crate::arch::time::tick_count().wrapping_mul(10);
    let sec = (ms / 1000) as i64;
    let nsec = ((ms % 1000) * 1_000_000) as i64;
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&sec.to_le_bytes());
    buf[8..16].copy_from_slice(&nsec.to_le_bytes());
    if !user_range_ok(ts_ptr, 16) || !copy_to_user(ts_ptr, &buf) {
        return err(EFAULT);
    }
    0
}

/// `getrandom(buf, len, _)` — fills `buf` with pseudo-random bytes. A small
/// splitmix64 seeded from the uptime tick; good enough for a stack canary /
/// ASLR seed, not for cryptography (a real CSPRNG is later work).
fn sys_getrandom(buf: u64, len: u64) -> u64 {
    let n = len as usize;
    if n == 0 {
        return 0;
    }
    if !user_range_ok(buf, n) {
        return err(EFAULT);
    }
    let mut state = crate::arch::time::tick_count().wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ buf;
    let mut out = alloc::vec![0u8; n];
    for chunk in out.chunks_mut(8) {
        // splitmix64
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let bytes = z.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    if !copy_to_user(buf, &out) {
        return err(EFAULT);
    }
    n as u64
}
