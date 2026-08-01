//! # tcp-echo-smoke — Phase 6.4 ring-3 inbound-TCP proof
//!
//! The **first** ThemeliOS ring-3 server to serve an inbound TCP connection. It
//! exists to de-risk, in isolation, the two new mechanisms the 6.5 api-server
//! depends on:
//!
//! 1. **The sentinel-cap spawn grant** — the kernel grants this server a
//!    `Management` capability at spawn (`ServerConfig.grant_management`) and hands
//!    its handle in via `BootInfo.mgmt_cap_handle`.
//! 2. **Ring-3 inbound TCP** — the server opens a listener through the management
//!    ABI (`SYS_MGMT`/`mgmt_listen`, which runs the trusted kernel `ksocket_*`
//!    path — no `sys_socket` TCP gate to relax), then `accept`/`tcp_recv`/
//!    `tcp_send` on the capability-checked socket syscalls.
//!
//! ## Two roles, selected by whether the grant was given
//!
//! - **Granted** (`mgmt_cap_handle != 0`): listen on [`PORT`], accept one
//!   connection, echo the bytes it receives, then report `OK` + the byte count.
//! - **Fail-closed control** (`mgmt_cap_handle == 0`): `mgmt_listen` returns
//!   `PermissionDenied` **before** touching the NIC; the server reports `DENIED`.
//!
//! ## Fault-freedom is mandatory (not optional)
//!
//! A ring-3 page fault halts the whole kernel (the IDT halts on any user fault),
//! which would hang the test suite to its 90 s wall-clock timeout rather than fail
//! a marker. So this server is deliberately tiny and defensive: fixed stack buffers
//! only (no unbounded recursion, no growth), every syscall return checked, every
//! loop bounded, and it writes to the result page only after confirming the kernel
//! mapped it. It never dereferences an unchecked pointer.
//!
//! ## Reporting (commit-word-last)
//!
//! The kernel maps a shared page as the server's `shared` region (at
//! `BootInfo.shared_vaddr`). The server writes `[MAGIC, status, count]` to it, but
//! writes **`MAGIC` last** with a volatile store: on a single core the kernel poll
//! can preempt between stores, so the commit word must land after the payload it
//! commits. The kernel polls `MAGIC` and only then trusts `status`/`count`.

#![no_std]
#![no_main]

use libthemelios::{boot_info, syscall};

/// The guest TCP port the granted server listens on. Reuses the port the host
/// test harness already forwards (`hostfwd 127.0.0.1:15007 -> guest:7`).
const PORT: u64 = 7;

/// High bit marking a syscall return as an encoded error (kernel convention).
const ERR_FLAG: u64 = 1 << 63;
/// `MgmtError::PermissionDenied` discriminant (the low bits of the `mgmt_listen`
/// error return when the caller lacks the Management cap). Note this is the
/// `MgmtError` numbering (PermissionDenied = 1), distinct from `SockError`'s.
const MGMT_PERMISSION_DENIED: u64 = 1;
/// `SockError::WouldBlock` discriminant (non-blocking accept/recv/send retry).
const SOCK_WOULDBLOCK: u64 = 1;

/// Commit word written (last) to the result page once the server has a verdict.
const RESULT_MAGIC: u64 = 0x_5243_4845_5F4F_4B00; // "RCHE_OK\0"-ish, arbitrary.
/// Result status codes (result page word[1]).
const STATUS_OK: u64 = 1;
const STATUS_DENIED: u64 = 2;
const STATUS_ERR: u64 = 3;

/// Generous per-loop iteration cap. Each iteration yields, so this bounds how long
/// the server waits on the net-server/peer before giving up and reporting `ERR`
/// (rather than spinning forever). The kernel-side poll is *also* bounded, so
/// neither side can wedge the suite.
const MAX_SPINS: u64 = 4_000_000;

/// `true` if `ret` is an encoded syscall error (high bit set).
fn is_err(ret: u64) -> bool {
    ret & ERR_FLAG != 0
}

/// The low-bits error discriminant of an encoded error return.
fn err_code(ret: u64) -> u64 {
    ret & !ERR_FLAG
}

#[no_mangle]
pub extern "C" fn server_main() -> ! {
    let info = boot_info();

    // The result page must be mapped, or we cannot report anything — bail cleanly
    // (a clean exit, never a fault) if the kernel didn't give us one.
    if info.shared_vaddr == 0 {
        syscall::exit(1);
    }
    let result = info.shared_vaddr as *mut u64;

    // --- Open the listener via the management ABI ---
    let listener = syscall::mgmt_listen(info.mgmt_cap_handle, PORT);
    if is_err(listener) {
        // The fail-closed control lands here: no Management cap → PermissionDenied,
        // reported before the NIC is ever touched. Any other error is a real fault.
        let status = if err_code(listener) == MGMT_PERMISSION_DENIED {
            STATUS_DENIED
        } else {
            STATUS_ERR
        };
        report(result, status, 0);
        syscall::exit(0);
    }

    // --- Accept one inbound connection (non-blocking; yield between tries) ---
    let conn = match spin_accept(listener) {
        Some(c) => c,
        None => {
            report(result, STATUS_ERR, 0);
            syscall::exit(0);
        }
    };

    // --- Receive a line, then echo exactly those bytes back ---
    // A single fixed stack buffer; the test payload ("THEMELIOS_TCP_PING") is well
    // under this size, and we never grow it.
    let mut buf = [0u8; 128];
    let got = match spin_recv(conn, &mut buf) {
        Some(n) => n,
        None => {
            report(result, STATUS_ERR, 0);
            syscall::exit(0);
        }
    };
    if !spin_send_all(conn, &buf[..got]) {
        report(result, STATUS_ERR, 0);
        syscall::exit(0);
    }

    // --- Tear down and report success + the echoed byte count ---
    let _ = syscall::socket_close(conn);
    let _ = syscall::socket_close(listener);
    report(result, STATUS_OK, got as u64);
    syscall::exit(0);
}

/// Write the verdict to the result page, commit word (`MAGIC`) LAST so the kernel
/// never observes a set magic over a stale status/count. All stores are volatile.
fn report(result: *mut u64, status: u64, count: u64) {
    // SAFETY: `result` is the kernel-mapped shared page (checked non-null above);
    // three in-bounds u64 slots. Volatile + magic-last is the ordering contract.
    unsafe {
        core::ptr::write_volatile(result.add(1), status);
        core::ptr::write_volatile(result.add(2), count);
        core::ptr::write_volatile(result, RESULT_MAGIC); // commit
    }
}

/// Accept one connection, yielding on `WouldBlock`. Returns the connection socket
/// handle, or `None` on a real error / spin-cap exhaustion.
fn spin_accept(listener: u64) -> Option<u64> {
    let mut spins = 0;
    while spins < MAX_SPINS {
        let r = syscall::accept(listener, core::ptr::null_mut());
        if !is_err(r) {
            return Some(r);
        }
        if err_code(r) != SOCK_WOULDBLOCK {
            return None;
        }
        syscall::yield_now();
        spins += 1;
    }
    None
}

/// Receive one chunk into `buf`, yielding on `WouldBlock`. Returns bytes read
/// (>0), or `None` on a real error, peer-close-before-data, or spin exhaustion.
fn spin_recv(conn: u64, buf: &mut [u8]) -> Option<usize> {
    let mut spins = 0;
    while spins < MAX_SPINS {
        let r = syscall::tcp_recv(conn, buf.as_mut_ptr(), buf.len());
        if !is_err(r) {
            let n = r as usize;
            if n == 0 {
                return None; // peer closed before sending anything
            }
            return Some(n);
        }
        if err_code(r) != SOCK_WOULDBLOCK {
            return None;
        }
        syscall::yield_now();
        spins += 1;
    }
    None
}

/// Send all of `data`, yielding on `WouldBlock`/partial writes. Returns whether the
/// full buffer was sent.
fn spin_send_all(conn: u64, data: &[u8]) -> bool {
    let mut sent = 0usize;
    let mut spins = 0;
    while sent < data.len() && spins < MAX_SPINS {
        let r = syscall::tcp_send(conn, data[sent..].as_ptr(), data.len() - sent);
        if is_err(r) {
            if err_code(r) != SOCK_WOULDBLOCK {
                return false;
            }
            syscall::yield_now();
            spins += 1;
            continue;
        }
        sent += r as usize;
    }
    sent == data.len()
}
