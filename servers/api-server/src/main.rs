//! # api-server — ThemeliOS Docker Engine API (Phase 6.5, read pipeline)
//!
//! The node's ring-3 control plane. It holds a spawn-granted `Management`
//! capability, opens an inbound-TCP listener through the management ABI
//! (`SYS_MGMT`/listen), and serves a subset of the Docker Engine API:
//! **accept → HTTP parse (6.0) → route → management ABI (6.3) → JSON → reply.**
//!
//! Phase 6.5 lands the **read (GET) pipeline**; the write verbs (POST create/
//! start/stop, logs) and request-body JSON parsing follow in 6.5b.
//!
//! ## Fault-freedom is mandatory
//!
//! A ring-3 fault halts the whole kernel, so this server — which parses
//! **untrusted** HTTP off a socket — is defensive: the request buffer is bounded
//! to `http::MAX_REQUEST` *before* it grows; every syscall return is checked; every
//! loop is bounded and yields on `WouldBlock`; the accepted connection socket is
//! **closed after every request** (or the CSpace fills); and libthemelios installs
//! an allocation-error handler so an OOM exits cleanly instead of aborting.
//!
//! ## Framing
//!
//! `http::parse_request` conflates "incomplete" and "malformed" (both `None`), so
//! we frame *before* parsing: accumulate until the `\r\n\r\n` header terminator,
//! read `Content-Length` more bytes, and only then parse. Responses always carry
//! `Content-Length` + `Connection: close` (one request per accepted socket).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use libthemelios::{boot_info, http, syscall};

/// Default listen port (Docker's unencrypted port) when the spawn didn't pin one
/// via `BootInfo.arg0`. The test pins port 7 to reuse the existing hostfwd rule.
const DEFAULT_PORT: u64 = 2375;

/// High bit marking a syscall return as an encoded error.
const ERR_FLAG: u64 = 1 << 63;
const MGMT_PERMISSION_DENIED: u64 = 1; // MgmtError::PermissionDenied
const MGMT_SERVER_UNAVAILABLE: u64 = 6; // MgmtError::ServerUnavailable (net not up yet)
const SOCK_WOULDBLOCK: u64 = 1; // SockError::WouldBlock

/// Result page contract shared with `test_api_server` (commit word written last).
const RESULT_MAGIC: u64 = 0x_4150_4953_5256_0000; // "APISRV\0\0"
const STATUS_DENIED: u64 = 2; // listen denied (fail-closed control)
const STATUS_SERVING: u64 = 1; // listener open; word[2] = requests served

/// Fixed cap on the JSON a management read verb may return into our buffer. Bounds
/// heap use on a hostile/large `list`; an over-cap response surfaces as a 500.
const MGMT_OUT_CAP: usize = 16 * 1024;
/// Per-recv chunk.
const RECV_CHUNK: usize = 2048;
/// Generous spin cap (each iteration yields) so no loop wedges the single core.
const MAX_SPINS: u64 = 4_000_000;

fn is_err(r: u64) -> bool {
    r & ERR_FLAG != 0
}
fn err_code(r: u64) -> u64 {
    r & !ERR_FLAG
}

#[no_mangle]
pub extern "C" fn server_main() -> ! {
    let info = boot_info();
    let port = if info.arg0 == 0 { DEFAULT_PORT } else { info.arg0 };
    // Optional result page (the test maps one as our `shared` region; production
    // spawns don't). `0` = absent → we simply don't report.
    let result = if info.shared_vaddr != 0 {
        info.shared_vaddr as *mut u64
    } else {
        core::ptr::null_mut()
    };

    // Open the listener via the management ABI. No Management cap → PermissionDenied
    // before any NIC access (the fail-closed control path). At boot the net-server
    // may still be coming up, so retry (bounded, yielding) on ServerUnavailable.
    let mut listener = syscall::mgmt_listen(info.mgmt_cap_handle, port);
    let mut listen_spins = 0;
    while is_err(listener)
        && err_code(listener) == MGMT_SERVER_UNAVAILABLE
        && listen_spins < MAX_SPINS
    {
        syscall::yield_now();
        listener = syscall::mgmt_listen(info.mgmt_cap_handle, port);
        listen_spins += 1;
    }
    if is_err(listener) {
        if err_code(listener) == MGMT_PERMISSION_DENIED {
            report(result, STATUS_DENIED, 0);
        }
        syscall::exit(0);
    }

    // Sequential accept/serve loop. Persistent: serves forever (a test tears us
    // down after asserting; in production we are the node's control plane).
    let mut served: u64 = 0;
    loop {
        let conn = match accept_one(listener) {
            Some(c) => c,
            None => continue, // transient accept error; keep listening
        };
        serve_connection(conn);
        let _ = syscall::socket_close(conn); // per-request close (CSpace hygiene)
        served = served.wrapping_add(1);
        report(result, STATUS_SERVING, served);
    }
}

/// Accept one connection, yielding on `WouldBlock`. `None` on a real error.
fn accept_one(listener: u64) -> Option<u64> {
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

/// Read one HTTP request off `conn`, route it, and send the framed response.
fn serve_connection(conn: u64) {
    let request = match read_request(conn) {
        Some(bytes) => bytes,
        // Malformed / oversized / peer hung up: best-effort 400, then close.
        None => {
            send_all(conn, &http::build_response(400, "Bad Request", "text/plain", b"bad request"));
            return;
        }
    };
    let response = route(&request);
    send_all(conn, &response);
}

/// Accumulate bytes until the full request (headers + any Content-Length body) is
/// buffered, bounded by `http::MAX_REQUEST`. `None` on oversize, peer-close before
/// a complete request, or a socket error.
fn read_request(conn: u64) -> Option<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; RECV_CHUNK];
    let mut spins = 0;
    loop {
        // Complete once the header terminator is seen and the declared body is in.
        if let Some(hdr_end) = http::find_sub(&buf, b"\r\n\r\n") {
            let body_start = hdr_end + 4;
            let need = http::content_length(&buf[..hdr_end]).unwrap_or(0);
            if need > http::MAX_BODY {
                return None;
            }
            if buf.len() >= body_start.checked_add(need)? {
                return Some(buf);
            }
        }
        // Refuse to grow past the ceiling BEFORE allocating more (OOM guard).
        if buf.len() >= http::MAX_REQUEST {
            return None;
        }
        if spins >= MAX_SPINS {
            return None;
        }
        let r = syscall::tcp_recv(conn, chunk.as_mut_ptr(), chunk.len());
        if is_err(r) {
            if err_code(r) != SOCK_WOULDBLOCK {
                return None;
            }
            syscall::yield_now();
            spins += 1;
            continue;
        }
        let n = r as usize;
        if n == 0 {
            return None; // peer closed before a complete request
        }
        // Clamp so the buffer never exceeds the ceiling.
        let take = n.min(http::MAX_REQUEST - buf.len());
        buf.extend_from_slice(&chunk[..take]);
    }
}

/// Route a fully-buffered request to a framed HTTP response. GET-only in 6.5.
fn route(request: &[u8]) -> Vec<u8> {
    let req = match http::parse_request(request) {
        Some(r) => r,
        None => return http::build_response(400, "Bad Request", "text/plain", b"bad request"),
    };
    let cap = boot_info().mgmt_cap_handle;

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/_ping") => http::build_response(200, "OK", "text/plain", b"OK"),
        ("GET", "/version") => http::build_response(
            200,
            "OK",
            "application/json",
            br#"{"Version":"0.1.0-themelios","ApiVersion":"1.43","Os":"themelios","Arch":"amd64"}"#,
        ),
        ("GET", "/info") => mgmt_json(|out| syscall::mgmt_node_info(cap, out.as_mut_ptr(), out.len() as u64)),
        ("GET", "/containers/json") => {
            mgmt_json(|out| syscall::mgmt_list(cap, out.as_mut_ptr(), out.len() as u64))
        }
        ("GET", p) if is_container_inspect(p) => {
            let id = inspect_id(p);
            mgmt_json(|out| {
                syscall::mgmt_inspect(cap, id.as_ptr(), id.len() as u64, out.as_mut_ptr(), out.len() as u64)
            })
        }
        _ => http::build_response(404, "Not Found", "text/plain", b"not found"),
    }
}

/// Call a management read verb into a fixed buffer and wrap the JSON in a 200
/// response; map any `MgmtError` to a 404 (NotFound) or 500.
fn mgmt_json<F: FnOnce(&mut [u8]) -> u64>(call: F) -> Vec<u8> {
    let mut out = [0u8; MGMT_OUT_CAP];
    let r = call(&mut out);
    if is_err(r) {
        // MgmtError::NotFound = 2.
        return if err_code(r) == 2 {
            http::build_response(404, "Not Found", "application/json", br#"{"message":"no such container"}"#)
        } else {
            http::build_response(500, "Internal Server Error", "application/json", br#"{"message":"error"}"#)
        };
    }
    let n = r as usize;
    let n = n.min(out.len());
    http::build_response(200, "OK", "application/json", &out[..n])
}

/// `true` if `path` is `/containers/<id>/json` (with a non-empty id).
fn is_container_inspect(path: &str) -> bool {
    path.starts_with("/containers/") && path.ends_with("/json") && inspect_id(path).len() > 0
}

/// Extract `<id>` from `/containers/<id>/json`.
fn inspect_id(path: &str) -> &str {
    let start = "/containers/".len();
    let end = path.len().saturating_sub("/json".len());
    if end >= start {
        &path[start..end]
    } else {
        ""
    }
}

/// Send all of `data`, yielding on `WouldBlock`/partial writes. Best-effort.
fn send_all(conn: u64, data: &[u8]) {
    let mut sent = 0usize;
    let mut spins = 0;
    while sent < data.len() && spins < MAX_SPINS {
        let r = syscall::tcp_send(conn, data[sent..].as_ptr(), data.len() - sent);
        if is_err(r) {
            if err_code(r) != SOCK_WOULDBLOCK {
                return;
            }
            syscall::yield_now();
            spins += 1;
            continue;
        }
        sent += r as usize;
    }
}

/// Write the verdict to the result page, commit word (`MAGIC`) LAST with volatile
/// stores so the kernel poll never observes a set magic over a stale status/count.
fn report(result: *mut u64, status: u64, served: u64) {
    if result.is_null() {
        return;
    }
    // SAFETY: kernel-mapped shared page (checked non-null); three in-bounds slots.
    unsafe {
        core::ptr::write_volatile(result.add(1), status);
        core::ptr::write_volatile(result.add(2), served);
        core::ptr::write_volatile(result, RESULT_MAGIC); // commit
    }
}
