//! # Filesystem syscall test client
//!
//! A minimal ring-3 program that exercises the Phase 3.8 filesystem syscalls
//! (`open`, `read_file`, `stat`) against a mount the kernel granted it a
//! `Filesystem` capability for. It validates the full userspace path —
//! syscall entry, user-pointer copy in/out, capability checks, and VFS
//! forwarding to the filesystem server — then reports a result code to the
//! kernel over IPC (0 = success; non-zero identifies the failing step).
//!
//! It also confirms capability *enforcement*: a filesystem syscall with a null
//! capability handle must be rejected.

#![no_std]
#![no_main]

extern crate alloc;

use libthemelios::{boot_info, ipc, syscall, BootInfo};

/// High bit set in a filesystem syscall return value indicates an error.
const ERR_BIT: u64 = 1 << 63;

#[no_mangle]
pub extern "C" fn server_main() -> ! {
    let info = boot_info();
    let result = run(&info);
    // Report the result code to the kernel (word0). ipc::send blocks until the
    // kernel receives it.
    ipc::send(info.fs_endpoint, [result, 0, 0, 0], 0);
    loop {
        syscall::yield_now();
    }
}

/// Run the checks, returning 0 on success or a step number on failure.
fn run(info: &BootInfo) -> u64 {
    let fs_cap = info.fs_cap_handle;
    if fs_cap == 0 {
        return 100; // no capability granted
    }
    let path = b"/version";

    // 1. stat → size 15, not a directory.
    let mut stat = [0u8; 16];
    if syscall::stat(fs_cap, path.as_ptr(), path.len(), stat.as_mut_ptr()) != 0 {
        return 1;
    }
    let size = u64::from_le_bytes([
        stat[0], stat[1], stat[2], stat[3], stat[4], stat[5], stat[6], stat[7],
    ]);
    if size != 15 {
        return 2;
    }

    // 2. open → a file descriptor capability.
    let fd = syscall::open(fs_cap, path.as_ptr(), path.len(), 0);
    if fd & ERR_BIT != 0 {
        return 3;
    }

    // 3. read → exact contents.
    let mut buf = [0u8; 15];
    let n = syscall::read_file(fd, buf.as_mut_ptr(), 15, 0);
    if n != 15 {
        return 4;
    }
    if &buf != b"THEMELIOS_ROOT\n" {
        return 5;
    }
    syscall::close(fd);

    // 4. Capability enforcement: a null capability handle must be rejected.
    let denied = syscall::open(0, path.as_ptr(), path.len(), 0);
    if denied & ERR_BIT == 0 {
        return 6;
    }

    0
}
