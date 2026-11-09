//! # The socket syscall surface — one dispatcher, both architectures
//!
//! `SYS_SOCKET`, `SYS_BIND`, `SYS_SENDTO`, `SYS_RECVFROM`, `SYS_SOCKET_CLOSE`,
//! `SYS_CONNECT`, `SYS_LISTEN`, `SYS_ACCEPT`, `SYS_TCP_SEND` and `SYS_TCP_RECV`, from a
//! ring-3 caller to the capability-checked router in [`crate::net::socket`].
//!
//! Thin by design, exactly as [`crate::fs::syscall`] is: validate and copy the user
//! pointers, then delegate. The capability checks and the IPC round trip to the ring-3
//! net server live in `socket::sys_*`; this module's only job is to get the arguments out
//! of the frame in the right order and the errors back in the right encoding. **It never
//! parses a packet** — that is the net server's work, in ring 3.
//!
//! ## Why it is arch-neutral (Phase 8.7)
//!
//! Until 8.7 this was `dispatch_socket_syscall` in `arch::x86_64::syscall`, naming
//! `frame.rdi`/`rsi`/`rdx`/`r10`/`r9` directly, and the aarch64 dispatcher answered all
//! ten numbers with `ENOSYS` from an explicitly written list. Deleting them from that list
//! is how they became implemented — the same mechanism 8.6 used for the filesystem calls,
//! and the same reason there is one implementation here rather than two that can drift.
//!
//! ## The slot-4 trap, which is live in this module and was not in 8.6's
//!
//! [`crate::fs::syscall`] could use the frame's plain positional accessors because the
//! filesystem calls use slots 0-3 only, where the native and Linux ABIs agree on both
//! architectures. Its docs then said: *"any native syscall that needs positions 4 or 5
//! must get its own accessors first."*
//!
//! **`SYS_SENDTO` needs slot 4** — the destination port. On x86_64 the native ABI carries
//! slot 4 in `r9` and slot 5 in `r8`, *crossed* relative to the Linux ABI that
//! `arg4()`/`arg5()` implement. So this module reads it through
//! [`SyscallFrame::arg_native4`], added in this sub-phase.
//!
//! Reading it through `arg4()` instead would compile, pass on aarch64 (where every ABI
//! uses `x0`-`x5`), and send every UDP datagram to **port 0** on amd64 — because
//! `libthemelios::sendto` passes `[sock, buf, len, ip, port, 0]`, so `r8` holds slot 5's
//! zero. An amd64-only, silent, wrong-destination bug, in the one direction a
//! single-architecture test run would not reveal. `test_udp_echo` covers it on both.
//!
//! ## Read every argument before writing the return
//!
//! As in `fs::syscall`, and for the same reason: the return slot aliases the number's
//! register on x86 (`rax`) and argument 0's on aarch64 (`x0`), so there is no ordering
//! that is safe on both except "read all inputs first". Each arm below pulls what it needs
//! into locals before touching `set_ret`.

use crate::arch::syscall::{
    abi, copy_from_user, copy_to_user, user_range_ok, SyscallFrame, FS_MAX_XFER,
};
use crate::cap::CapHandle;
use crate::net::socket::{self, SockError};

/// Unpack a syscall IPv4 argument (`a<<24 | b<<16 | c<<8 | d`) into octets.
///
/// The packed form is big-endian-by-value: the first octet of the dotted quad sits in the
/// most significant byte, so `10.0.2.2` is `0x0A000202`. That keeps a literal readable in
/// a hex dump and matches what `libthemelios` builds.
fn unpack_ipv4_arg(w: u64) -> [u8; 4] {
    [(w >> 24) as u8, (w >> 16) as u8, (w >> 8) as u8, w as u8]
}

/// Dispatch one socket syscall from `frame`.
///
/// The caller must have enabled interrupts: every arm can block on the ring-3 net server
/// through `socket::sys_*`, and a task that blocks with interrupts masked is never woken.
pub fn dispatch(frame: &mut SyscallFrame) {
    let pid = crate::sched::current_process_id();

    // Number and arguments first — see the module docs on return-slot aliasing. `num` is
    // also passed down to `socket::sys_*`, which audit-log it.
    let num = frame.nr();
    let (a0, a1, a2, a3) = (frame.arg0(), frame.arg1(), frame.arg2(), frame.arg3());
    // Slot 4 under the NATIVE ABI (`r9` on x86, `x4` on aarch64). Not `arg4()`. See the
    // module docs — this single line is the whole reason `arg_native4` exists.
    let a4_native = frame.arg_native4();

    crate::audit::log_event(pid, crate::audit::AuditOp::NetAccess, crate::cap::CapType::Null, num);
    let bad_arg = SockError::InvalidArgument.as_syscall_ret();

    match num {
        // a0 = socket type, a1 = network-authority (factory) cap handle.
        abi::SYS_SOCKET => {
            let factory = CapHandle::from_raw(a1 as u32);
            match socket::sys_socket(pid, factory, a0, num) {
                Ok(handle) => frame.set_ret(handle as u64),
                Err(e) => frame.set_ret(e.as_syscall_ret()),
            }
        }

        // a0 = socket cap, a1 = local IPv4 (packed), a2 = port.
        abi::SYS_BIND => {
            let handle = CapHandle::from_raw(a0 as u32);
            match socket::sys_bind(pid, handle, unpack_ipv4_arg(a1), a2 as u16, num) {
                Ok(()) => frame.set_ret(0),
                Err(e) => frame.set_ret(e.as_syscall_ret()),
            }
        }

        // a0 = socket cap, a1 = buf ptr, a2 = len, a3 = dest IPv4, NATIVE slot 4 = port.
        abi::SYS_SENDTO => {
            let handle = CapHandle::from_raw(a0 as u32);
            let ip = unpack_ipv4_arg(a3);
            let port = a4_native as u16;
            match copy_from_user(a1, a2 as usize) {
                Some(data) => match socket::sys_sendto(pid, handle, &data, ip, port, num) {
                    Ok(n) => frame.set_ret(n as u64),
                    Err(e) => frame.set_ret(e.as_syscall_ret()),
                },
                None => frame.set_ret(bad_arg),
            }
        }

        // a0 = socket cap, a1 = buf ptr, a2 = len, a3 = source-out ptr (0 to skip).
        abi::SYS_RECVFROM => {
            let handle = CapHandle::from_raw(a0 as u32);
            let len = (a2 as usize).min(FS_MAX_XFER);
            if !user_range_ok(a1, len) {
                frame.set_ret(bad_arg);
                return;
            }
            let mut kbuf = alloc::vec![0u8; len];
            match socket::sys_recvfrom(pid, handle, &mut kbuf, num) {
                Ok((n, ip, port)) => {
                    if !copy_to_user(a1, &kbuf[..n]) {
                        frame.set_ret(bad_arg);
                        return;
                    }
                    // `[ip: u32, port: u16_le]` into the source-out buffer, if supplied.
                    if a3 != 0 {
                        let mut out = [0u8; 8];
                        out[0..4].copy_from_slice(&ip);
                        out[4..6].copy_from_slice(&port.to_le_bytes());
                        if !copy_to_user(a3, &out) {
                            frame.set_ret(bad_arg);
                            return;
                        }
                    }
                    frame.set_ret(n as u64);
                }
                Err(e) => frame.set_ret(e.as_syscall_ret()),
            }
        }

        // a0 = socket cap.
        abi::SYS_SOCKET_CLOSE => {
            let handle = CapHandle::from_raw(a0 as u32);
            match socket::sys_close(pid, handle, num) {
                Ok(()) => frame.set_ret(0),
                Err(e) => frame.set_ret(e.as_syscall_ret()),
            }
        }

        // a0 = socket cap, a1 = dest IPv4 (packed), a2 = dest port.
        abi::SYS_CONNECT => {
            let handle = CapHandle::from_raw(a0 as u32);
            match socket::sys_connect(pid, handle, unpack_ipv4_arg(a1), a2 as u16, num) {
                Ok(()) => frame.set_ret(0),
                Err(e) => frame.set_ret(e.as_syscall_ret()),
            }
        }

        // a0 = socket cap, a1 = buf ptr, a2 = len (stream send).
        abi::SYS_TCP_SEND => {
            let handle = CapHandle::from_raw(a0 as u32);
            match copy_from_user(a1, a2 as usize) {
                Some(data) => match socket::sys_send(pid, handle, &data, num) {
                    Ok(n) => frame.set_ret(n as u64),
                    Err(e) => frame.set_ret(e.as_syscall_ret()),
                },
                None => frame.set_ret(bad_arg),
            }
        }

        // a0 = socket cap, a1 = buf ptr, a2 = len (stream recv).
        abi::SYS_TCP_RECV => {
            let handle = CapHandle::from_raw(a0 as u32);
            let len = (a2 as usize).min(FS_MAX_XFER);
            if !user_range_ok(a1, len) {
                frame.set_ret(bad_arg);
                return;
            }
            let mut kbuf = alloc::vec![0u8; len];
            match socket::sys_recv(pid, handle, &mut kbuf, num) {
                Ok(n) if copy_to_user(a1, &kbuf[..n]) => frame.set_ret(n as u64),
                Ok(_) => frame.set_ret(bad_arg),
                Err(e) => frame.set_ret(e.as_syscall_ret()),
            }
        }

        // a0 = socket cap, a1 = backlog.
        abi::SYS_LISTEN => {
            let handle = CapHandle::from_raw(a0 as u32);
            match socket::sys_listen(pid, handle, a1, num) {
                Ok(()) => frame.set_ret(0),
                Err(e) => frame.set_ret(e.as_syscall_ret()),
            }
        }

        // a0 = listening socket cap, a1 = peer-out ptr (8 bytes, 0 to skip).
        abi::SYS_ACCEPT => {
            let handle = CapHandle::from_raw(a0 as u32);
            let peer_ptr = a1;
            // Validate the peer buffer **before** accepting. `sys_accept` promotes the
            // connection and mints its capability, so a copy failure afterwards would
            // orphan a live connection whose handle the caller never learns — a leak the
            // caller cannot even close. Order is load-bearing, not stylistic.
            if peer_ptr != 0 && !user_range_ok(peer_ptr, 8) {
                frame.set_ret(bad_arg);
                return;
            }
            match socket::sys_accept(pid, handle, num) {
                Ok((conn_handle, ip, port)) => {
                    if peer_ptr != 0 {
                        let mut out = [0u8; 8];
                        out[0..4].copy_from_slice(&ip);
                        out[4..6].copy_from_slice(&port.to_le_bytes());
                        // Range validated above, so this succeeds.
                        copy_to_user(peer_ptr, &out);
                    }
                    frame.set_ret(conn_handle as u64);
                }
                Err(e) => frame.set_ret(e.as_syscall_ret()),
            }
        }

        // Unreachable: both architectures' dispatchers match the ten numbers above
        // explicitly before calling in. Answered rather than `unreachable!()`, because the
        // alternative is a kernel panic reachable from ring 3 if a dispatcher ever widens
        // its match.
        _ => frame.set_ret(bad_arg),
    }
}
