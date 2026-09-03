//! # Linux filesystem syscalls over the VFS (Phase 5.2)
//!
//! Services the Linux path/fd filesystem syscalls for a `Personality::Linux`
//! process by translating them onto the Phase 3 VFS. The key difference from the
//! native FS syscalls: **Linux syscalls carry paths and integer fds, not
//! capabilities.** A container's isolation is therefore structural — the kernel
//! only ever resolves its paths against the single [`rootfs_mount`] the process
//! holds, and [`resolve_path`] clamps `..` at the root so a path can never escape
//! above the container's rootfs.
//!
//! [`rootfs_mount`]: crate::process::rootfs_mount

use crate::arch::syscall::{copy_from_user, copy_to_user, user_range_ok, SyscallFrame};
use crate::process::{self, LinuxFd};
use alloc::string::String;
use alloc::vec::Vec;

extern crate alloc;

// --- errno (negated on return), mirrored locally to keep this module decoupled ---
const EBADF: i64 = 9;
const ENOENT: i64 = 2;
const EFAULT: i64 = 14;
const ENOTDIR: i64 = 20;
const EINVAL: i64 = 22;
const EMFILE: i64 = 24;
const ENOSYS: i64 = 38;
const ERANGE: i64 = 34;

fn err(e: i64) -> u64 {
    (-e) as u64
}

/// `AT_FDCWD` — the `dirfd` value meaning "resolve relative to the cwd".
const AT_FDCWD: i64 = -100;
/// `O_CREAT` open flag.
const O_CREAT: u64 = 0o100;

/// Longest path we accept from userspace (bounds the copy).
const PATH_MAX: usize = 4096;
/// Largest single read/write we service in one syscall.
const IO_MAX: usize = 64 * 1024;

// Linux `struct stat` field offsets (x86-64), see the note in `fill_stat`.
const STAT_SIZE: usize = 144;

// Linux d_type values (distinct from the VFS `DT_*` wire values).
const LINUX_DT_DIR: u8 = 4;
const LINUX_DT_REG: u8 = 8;

/// Re-exported from [`crate::path`], which is where the implementation now lives.
///
/// Moved out in 8.4c so `test_path_resolve` can run on aarch64: the logic is pure string
/// manipulation, but this file's imports kept all of `mod linux` x86-gated. Re-exported
/// under the old name so the call sites below are unchanged.
pub use crate::path::resolve_path;

/// Map a container-relative resolved path (`rel`, the `..`-clamped output of
/// [`resolve_path`], always absolute) to the **host** path actually used against
/// the mount, applying the process's rootfs base subdirectory (Phase 6.1b).
///
/// If the process is confined (`rootfs_base = Some("/c/<id>")`), the container's
/// `/` maps to `/c/<id>`: `rel == "/"` → the base itself (avoiding a trailing-slash
/// ambiguity), any other `rel` (which always starts with `/`) → `base + rel`.
/// Because `rel` is already clamped so it can never rise above `/`, `base + rel`
/// can never escape the base subtree. Unconfined processes (`None`) get `rel`
/// unchanged (rooted at the mount root). This is the **single choke point** every
/// mount-by-path access must pass through.
fn host_path(pid: process::ProcessId, rel: &str) -> String {
    match process::rootfs_base(pid) {
        Some(base) => {
            if rel == "/" {
                base
            } else {
                let mut s = base;
                s.push_str(rel);
                s
            }
        }
        None => String::from(rel),
    }
}

/// Read a NUL-terminated (or `PATH_MAX`-bounded) path string from userspace.
fn read_user_path(ptr: u64, cwd: &str) -> Option<String> {
    // Copy up to PATH_MAX bytes, then stop at the first NUL.
    let mut buf = copy_from_user(ptr, 1)?; // validate at least 1 byte
    buf.clear();
    for i in 0..PATH_MAX {
        let byte = copy_from_user(ptr + i as u64, 1)?;
        if byte[0] == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    let s = core::str::from_utf8(&buf).ok()?;
    Some(resolve_path(cwd, s))
}

/// Dispatch a Linux filesystem syscall. Returns the value to place in `rax`.
/// Numbers not handled here return `None` so the caller can try other tables.
pub fn dispatch(num: u64, frame: &SyscallFrame) -> Option<u64> {
    let pid = crate::sched::current_process_id();
    let r = match num {
        0 => sys_read(pid, frame.arg0() as i32, frame.arg1(), frame.arg2()),   // read
        1 => sys_write(pid, frame.arg0() as i32, frame.arg1(), frame.arg2()),  // write
        2 => sys_openat(pid, AT_FDCWD, frame.arg0(), frame.arg1()),          // open
        3 => sys_close(pid, frame.arg0() as i32),                         // close
        5 => sys_fstat(pid, frame.arg0() as i32, frame.arg1()),              // fstat
        8 => sys_lseek(pid, frame.arg0() as i32, frame.arg1(), frame.arg2()),   // lseek
        79 => sys_getcwd(pid, frame.arg0(), frame.arg1()),                   // getcwd
        80 => sys_chdir(pid, frame.arg0()),                               // chdir
        217 => sys_getdents64(pid, frame.arg0() as i32, frame.arg1(), frame.arg2()), // getdents64
        257 => sys_openat(pid, frame.arg0() as i64, frame.arg1(), frame.arg2()),     // openat
        262 => sys_newfstatat(pid, frame.arg0() as i64, frame.arg1(), frame.arg2()), // newfstatat
        267 => err(EINVAL), // readlinkat: no symlinks surfaced in 5.2
        _ => return None,
    };
    Some(r)
}

/// `openat(dirfd, path, flags, …)` — only `AT_FDCWD`/absolute paths in 5.2.
fn sys_openat(pid: process::ProcessId, dirfd: i64, path_ptr: u64, flags: u64) -> u64 {
    let mount = match process::rootfs_mount(pid) {
        Some(m) => m,
        None => return err(ENOENT), // no rootfs → nothing to open
    };
    // A real directory fd for dirfd is out of scope; only cwd-relative/absolute.
    if dirfd != AT_FDCWD {
        return err(ENOSYS);
    }
    let cwd = process::get_cwd(pid);
    let rel = match read_user_path(path_ptr, &cwd) {
        Some(p) => p,
        None => return err(EFAULT),
    };
    // Apply the container's rootfs base (confinement choke point). All three
    // mount accesses below use the host path, never the raw container-relative one.
    let path = host_path(pid, &rel);

    // Determine existence/type; create on O_CREAT if absent.
    let (size, is_dir) = match crate::fs::kstat(mount, path.as_bytes()) {
        Ok(v) => v,
        Err(_) if flags & O_CREAT != 0 => {
            match crate::fs::kcreate(mount, path.as_bytes()) {
                Ok(server_fd) => {
                    // Freshly created, empty regular file.
                    return finish_open(pid, mount, server_fd, 0, false);
                }
                Err(_) => return err(ENOENT),
            }
        }
        Err(_) => return err(ENOENT),
    };
    match crate::fs::kopen(mount, path.as_bytes()) {
        Ok(server_fd) => finish_open(pid, mount, server_fd, size, is_dir),
        Err(_) => err(ENOENT),
    }
}

/// Record a freshly-opened server fd in the process fd table, returning the int
/// fd (or closing it + `EMFILE` if the table can't grow).
fn finish_open(
    pid: process::ProcessId,
    mount: u64,
    server_fd: u64,
    size: u64,
    is_dir: bool,
) -> u64 {
    let fd = process::linux_fd_alloc(
        pid,
        LinuxFd { mount, server_fd, offset: 0, size, is_dir },
    );
    if fd < 0 {
        let _ = crate::fs::kclose(mount, server_fd);
        return err(EMFILE);
    }
    fd as u64
}

/// `read(fd, buf, len)` — fd 0 → EOF, fd 1/2 → EBADF, file fds → VFS read.
fn sys_read(pid: process::ProcessId, fd: i32, buf: u64, len: u64) -> u64 {
    if fd == 0 {
        return 0; // stdin: EOF
    }
    if fd == 1 || fd == 2 {
        return err(EBADF);
    }
    let entry = match process::linux_fd_get(pid, fd) {
        Some(e) => e,
        None => return err(EBADF),
    };
    let n = (len as usize).min(IO_MAX);
    if n == 0 {
        return 0;
    }
    if !user_range_ok(buf, n) {
        return err(EFAULT);
    }
    let mut kbuf = alloc::vec![0u8; n];
    match crate::fs::kread(entry.mount, entry.server_fd, entry.offset, &mut kbuf) {
        Ok(read) => {
            if !copy_to_user(buf, &kbuf[..read]) {
                return err(EFAULT);
            }
            process::linux_fd_set_offset(pid, fd, entry.offset + read as u64);
            read as u64
        }
        Err(_) => err(EINVAL),
    }
}

/// `write(fd, buf, len)` — fd 1/2 → console, fd 0 → EBADF, file fds → VFS write.
fn sys_write(pid: process::ProcessId, fd: i32, buf: u64, len: u64) -> u64 {
    if fd == 1 || fd == 2 {
        let n = len as usize;
        if n == 0 {
            return 0;
        }
        return match copy_from_user(buf, n) {
            Some(data) => {
                // Route to the container's capture buffer + serial (Phase 6.2).
                crate::container::registry::write_stdout(pid, &data);
                n as u64
            }
            None => err(EFAULT),
        };
    }
    if fd == 0 {
        return err(EBADF);
    }
    let entry = match process::linux_fd_get(pid, fd) {
        Some(e) => e,
        None => return err(EBADF),
    };
    let n = (len as usize).min(IO_MAX);
    if n == 0 {
        return 0;
    }
    let data = match copy_from_user(buf, n) {
        Some(d) => d,
        None => return err(EFAULT),
    };
    match crate::fs::kwrite(entry.mount, entry.server_fd, entry.offset, &data) {
        Ok(w) => {
            process::linux_fd_set_offset(pid, fd, entry.offset + w as u64);
            w as u64
        }
        Err(_) => err(EINVAL),
    }
}

/// `close(fd)` — free the fd and close the underlying server fd.
fn sys_close(pid: process::ProcessId, fd: i32) -> u64 {
    if fd == 0 || fd == 1 || fd == 2 {
        return 0; // closing stdio is a no-op for now
    }
    match process::linux_fd_close(pid, fd) {
        Some(e) => {
            let _ = crate::fs::kclose(e.mount, e.server_fd);
            0
        }
        None => err(EBADF),
    }
}

/// `lseek(fd, offset, whence)` — SEEK_SET/CUR/END.
fn sys_lseek(pid: process::ProcessId, fd: i32, offset: u64, whence: u64) -> u64 {
    let entry = match process::linux_fd_get(pid, fd) {
        Some(e) => e,
        None => return err(EBADF),
    };
    let off = offset as i64;
    let new = match whence {
        0 => off,                          // SEEK_SET
        1 => entry.offset as i64 + off,    // SEEK_CUR
        2 => entry.size as i64 + off,      // SEEK_END
        _ => return err(EINVAL),
    };
    if new < 0 {
        return err(EINVAL);
    }
    process::linux_fd_set_offset(pid, fd, new as u64);
    new as u64
}

/// Fill a Linux `struct stat` (144 bytes) with size/type; other fields are
/// zeroed except a plausible nlink/blksize/blocks.
///
/// x86-64 layout: `st_mode` at offset 24, `st_size` at 48, `st_nlink` at 16,
/// `st_blksize` at 56, `st_blocks` at 64. `st_mode` combines the type
/// (`S_IFDIR`=0o040000 / `S_IFREG`=0o100000) with permissions.
fn fill_stat(size: u64, is_dir: bool) -> [u8; STAT_SIZE] {
    let mut b = [0u8; STAT_SIZE];
    let mode: u32 = if is_dir { 0o040000 | 0o755 } else { 0o100000 | 0o644 };
    b[16..24].copy_from_slice(&1u64.to_le_bytes()); // st_nlink
    b[24..28].copy_from_slice(&mode.to_le_bytes()); // st_mode
    b[48..56].copy_from_slice(&size.to_le_bytes()); // st_size
    b[56..64].copy_from_slice(&4096u64.to_le_bytes()); // st_blksize
    b[64..72].copy_from_slice(&((size + 511) / 512).to_le_bytes()); // st_blocks
    b
}

/// `fstat(fd, statbuf)`.
fn sys_fstat(pid: process::ProcessId, fd: i32, statbuf: u64) -> u64 {
    // stdio fds report as character devices — good enough: zeroed stat, size 0.
    let (size, is_dir) = if fd <= 2 {
        (0, false)
    } else {
        match process::linux_fd_get(pid, fd) {
            Some(e) => (e.size, e.is_dir),
            None => return err(EBADF),
        }
    };
    let st = fill_stat(size, is_dir);
    if !user_range_ok(statbuf, STAT_SIZE) || !copy_to_user(statbuf, &st) {
        return err(EFAULT);
    }
    0
}

/// `newfstatat(dirfd, path, statbuf, flags)` — stat a path.
fn sys_newfstatat(pid: process::ProcessId, dirfd: i64, path_ptr: u64, statbuf: u64) -> u64 {
    if dirfd != AT_FDCWD {
        return err(ENOSYS);
    }
    let mount = match process::rootfs_mount(pid) {
        Some(m) => m,
        None => return err(ENOENT),
    };
    let cwd = process::get_cwd(pid);
    let rel = match read_user_path(path_ptr, &cwd) {
        Some(p) => p,
        None => return err(EFAULT),
    };
    let path = host_path(pid, &rel); // confinement choke point
    match crate::fs::kstat(mount, path.as_bytes()) {
        Ok((size, is_dir)) => {
            let st = fill_stat(size, is_dir);
            if !user_range_ok(statbuf, STAT_SIZE) || !copy_to_user(statbuf, &st) {
                return err(EFAULT);
            }
            0
        }
        Err(_) => err(ENOENT),
    }
}

/// `getdents64(fd, buf, count)` — translate VFS dir entries to `linux_dirent64`.
///
/// Reads a batch from the server via `kreaddir` and writes `linux_dirent64`
/// records into the user buffer. 5.2 limitation: the whole batch must fit in the
/// caller's buffer (the probe passes an ample one); a streaming cursor is later
/// work. Returns 0 at end of directory.
fn sys_getdents64(pid: process::ProcessId, fd: i32, buf: u64, count: u64) -> u64 {
    let entry = match process::linux_fd_get(pid, fd) {
        Some(e) => e,
        None => return err(EBADF),
    };
    if !entry.is_dir {
        return err(ENOTDIR);
    }
    // Read up to 64 VFS entries into a scratch buffer.
    let mut wire = alloc::vec![0u8; 16 * 1024];
    let n_entries = match crate::fs::kreaddir(entry.mount, entry.server_fd, 64, &mut wire) {
        Ok(n) => n as usize,
        Err(_) => return err(EINVAL),
    };
    if n_entries == 0 {
        return 0; // end of directory
    }
    // Build the linux_dirent64 records.
    let cap = (count as usize).min(IO_MAX);
    let mut out: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    let mut ino_seq: u64 = 1;
    for _ in 0..n_entries {
        if pos + 4 > wire.len() {
            break;
        }
        let nlen = u16::from_le_bytes([wire[pos], wire[pos + 1]]) as usize;
        let vfs_type = u16::from_le_bytes([wire[pos + 2], wire[pos + 3]]);
        pos += 4;
        if pos + nlen > wire.len() {
            break;
        }
        let name = &wire[pos..pos + nlen];
        pos += nlen;

        let d_type = match vfs_type {
            2 => LINUX_DT_DIR, // VFS DT_DIR
            1 => LINUX_DT_REG, // VFS DT_FILE
            _ => 0,
        };
        // d_reclen = align_up(8+8+2+1 + nlen+1, 8)
        let reclen = (19 + nlen + 1 + 7) & !7;
        out.extend_from_slice(&ino_seq.to_le_bytes()); // d_ino
        ino_seq += 1;
        out.extend_from_slice(&0i64.to_le_bytes()); // d_off (unused by our callers)
        out.extend_from_slice(&(reclen as u16).to_le_bytes()); // d_reclen
        out.push(d_type); // d_type
        out.extend_from_slice(name); // d_name
        out.push(0); // NUL
        // pad to reclen
        while out.len() % 8 != 0 {
            out.push(0);
        }
    }
    if out.len() > cap {
        // Whole batch doesn't fit (5.2 limitation) — the server already advanced.
        return err(EINVAL);
    }
    if !user_range_ok(buf, out.len()) || !copy_to_user(buf, &out) {
        return err(EFAULT);
    }
    // Advance the fd offset by the entry count (informational; the server tracks
    // the real position).
    process::linux_fd_set_offset(pid, fd, entry.offset + n_entries as u64);
    out.len() as u64
}

/// `getcwd(buf, size)` — copy the cwd (with NUL) to userspace; returns the byte
/// count written (including NUL), the raw-syscall convention.
fn sys_getcwd(pid: process::ProcessId, buf: u64, size: u64) -> u64 {
    let cwd = process::get_cwd(pid);
    let bytes = cwd.as_bytes();
    let needed = bytes.len() + 1;
    if (size as usize) < needed {
        return err(ERANGE);
    }
    if !user_range_ok(buf, needed) {
        return err(EFAULT);
    }
    let mut out = Vec::with_capacity(needed);
    out.extend_from_slice(bytes);
    out.push(0);
    if !copy_to_user(buf, &out) {
        return err(EFAULT);
    }
    needed as u64
}

/// `chdir(path)` — resolve, verify it is a directory, and update the cwd.
fn sys_chdir(pid: process::ProcessId, path_ptr: u64) -> u64 {
    let mount = match process::rootfs_mount(pid) {
        Some(m) => m,
        None => return err(ENOENT),
    };
    let cwd = process::get_cwd(pid);
    let rel = match read_user_path(path_ptr, &cwd) {
        Some(p) => p,
        None => return err(EFAULT),
    };
    // Verify the target exists on the *host* path, but store the *container-
    // relative* path as the cwd — storing the host path would double-prefix the
    // base on the next relative open, and would leak the base to getcwd.
    match crate::fs::kstat(mount, host_path(pid, &rel).as_bytes()) {
        Ok((_, true)) => {
            process::set_cwd(pid, rel);
            0
        }
        Ok((_, false)) => err(ENOTDIR),
        Err(_) => err(ENOENT),
    }
}
