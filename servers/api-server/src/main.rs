//! # api-server — ThemeliOS Docker Engine API (Phase 6.5b, read + write)
//!
//! The node's ring-3 control plane. It holds a spawn-granted `Management`
//! capability, opens an inbound-TCP listener through the management ABI
//! (`SYS_MGMT`/listen), and serves a subset of the Docker Engine API:
//! **accept → HTTP parse (6.0) → route → management ABI (6.3) → JSON → reply.**
//!
//! Phase 6.5 landed the **read (GET) pipeline**; 6.5b adds the **write verbs**
//! (`POST /containers/create`, `.../start`, `.../stop`, `GET .../logs`) and the
//! untrusted request-body JSON parsing that `create` needs.
//!
//! ## Deterministic self-test
//!
//! The inbound-TCP path is timing-sensitive under a shared single core, so the
//! *content* of the routing + JSON-parsing logic is proven by an in-process
//! self-test (no network): with the [`SELF_TEST_FLAG`] bit set in `arg1`, the
//! server runs a fixed set of requests through [`route`], records each HTTP status
//! to the result page, and exits — the kernel asserts those statuses directly. The
//! live inbound path keeps a separate, count-based smoke (a single `GET /_ping`).
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
use libthemelios::{boot_info, http, json, syscall};

/// Default listen port (Docker's unencrypted port) when the spawn didn't pin one
/// via `BootInfo.arg0`. The test pins port 7 to reuse the existing hostfwd rule.
const DEFAULT_PORT: u64 = 2375;

/// High bit marking a syscall return as an encoded error.
const ERR_FLAG: u64 = 1 << 63;
const MGMT_PERMISSION_DENIED: u64 = 1; // MgmtError::PermissionDenied
const MGMT_SERVER_UNAVAILABLE: u64 = 6; // MgmtError::ServerUnavailable (net not up yet)
const SOCK_WOULDBLOCK: u64 = 1; // SockError::WouldBlock

/// Result page contract shared with `test_api_server` (commit word written last).
/// Layout (u64 words): [0]=magic(commit) [1]=state [2]=served count
/// [3+i]=HTTP status of request i — so the test can assert response *content*, not
/// just that a request was served.
const RESULT_MAGIC: u64 = 0x_4150_4953_5256_0000; // "APISRV\0\0"
const STATUS_DENIED: u64 = 2; // listen denied (fail-closed control) → word[1]
const STATUS_SERVING: u64 = 1; // listener open → word[1]
/// Word index where the per-request HTTP status array begins.
const STATUS_ARRAY_OFF: usize = 3;
/// Bound on recorded per-request statuses (result page is one 4 KiB frame; the test
/// sends a handful of requests, production records nothing — `result` is null).
const MAX_RECORDED: u64 = 16;

/// Static `GET /version` response body.
const VERSION_JSON: &[u8] =
    br#"{"Version":"0.1.0-themelios","ApiVersion":"1.43","Os":"themelios","Arch":"amd64"}"#;

/// Fixed cap on the JSON a management read verb may return into our buffer. Bounds
/// heap use on a hostile/large `list`; an over-cap response surfaces as a 500.
const MGMT_OUT_CAP: usize = 16 * 1024;
/// Per-recv chunk.
const RECV_CHUNK: usize = 2048;
/// Generous spin cap (each iteration yields) so no loop wedges the single core.
const MAX_SPINS: u64 = 4_000_000;

/// High bit of `arg1` requesting the deterministic in-process self-test (no
/// network). Kept well clear of the serve-then-exit count in the low bits — the
/// same arg-packing idiom the net-server uses for `NET_ARG_DHCP`.
const SELF_TEST_FLAG: u64 = 1 << 63;
/// Container id the self-test's `start` request targets. The kernel test injects a
/// **Running** container under this id, so `start` hits the state guard and returns
/// `InvalidState` → HTTP 409 (a status the catch-all 404 can never produce). Keep in
/// sync with `SELFTEST_ID` in `test_api_server`.
const SELF_TEST_CONTAINER: &str = "selftest-run";

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
    // `arg1` low bits = serve-then-exit count for the test harness (0 = unlimited,
    // the production default). Exiting cleanly after N requests — closing the
    // listener first — lets the test tear us down without leaving an orphaned listen
    // socket in the net server (which would starve the next test's NIC). The top bit
    // (`SELF_TEST_FLAG`) requests the in-process self-test instead.
    let self_test = info.arg1 & SELF_TEST_FLAG != 0;
    let max_requests = info.arg1 & !SELF_TEST_FLAG;
    // Optional result page (the test maps one as our `shared` region; production
    // spawns don't). `0` = absent → we simply don't report.
    let result = if info.shared_vaddr != 0 {
        info.shared_vaddr as *mut u64
    } else {
        core::ptr::null_mut()
    };

    // Deterministic self-test: run canned requests through `route` (no network),
    // record the statuses, and exit before ever opening a listener.
    if self_test {
        run_self_test(result);
        syscall::exit(0);
    }

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
            report_denied(result);
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
        let status = serve_connection(conn);
        let _ = syscall::socket_close(conn); // per-request close (CSpace hygiene)
        let index = served; // 0-based index of the request just served
        served = served.wrapping_add(1);
        // Test harness: after serving the requested number, close the listener
        // BEFORE the committing report and exit cleanly. Closing first means the
        // moment the test observes the marker, the listen socket is already gone —
        // no orphaned listener even if it tears us down immediately (production runs
        // with max_requests == 0 and never exits).
        let done = max_requests != 0 && served >= max_requests;
        if done {
            let _ = syscall::socket_close(listener);
        }
        report_served(result, index, status, served);
        if done {
            syscall::exit(0);
        }
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

/// Read one HTTP request off `conn`, route it, send the framed response, and return
/// the HTTP status sent.
fn serve_connection(conn: u64) -> u16 {
    let request = match read_request(conn) {
        Some(bytes) => bytes,
        // Malformed / oversized / peer hung up: best-effort 400, then close.
        None => {
            send_all(conn, &http::build_response(400, "Bad Request", "text/plain", b"bad request"));
            return 400;
        }
    };
    let (status, response) = route(&request);
    send_all(conn, &response);
    status
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

/// Route a fully-buffered request to `(status, framed response)`. GET read pipeline
/// (6.5) + POST write verbs (6.5b).
fn route(request: &[u8]) -> (u16, Vec<u8>) {
    let req = match http::parse_request(request) {
        Some(r) => r,
        None => return resp(400, "Bad Request", "text/plain", b"bad request"),
    };
    let cap = boot_info().mgmt_cap_handle;
    let method = req.method.as_str();
    let path = req.path.as_str();

    match (method, path) {
        ("GET", "/_ping") => resp(200, "OK", "text/plain", b"OK"),
        ("GET", "/version") => resp(200, "OK", "application/json", VERSION_JSON),
        ("GET", "/info") => read_verb(|out| syscall::mgmt_node_info(cap, out.as_mut_ptr(), out.len() as u64)),
        ("GET", "/containers/json") => {
            read_verb(|out| syscall::mgmt_list(cap, out.as_mut_ptr(), out.len() as u64))
        }
        ("POST", "/containers/create") => create_container(cap, &req),
        // Path-parameter routes: /containers/<id>/{json,logs,start,stop}.
        _ => match container_action(path) {
            Some((id, "json")) if method == "GET" => read_verb(|out| {
                syscall::mgmt_inspect(cap, id.as_ptr(), id.len() as u64, out.as_mut_ptr(), out.len() as u64)
            }),
            Some((id, "logs")) if method == "GET" => logs_verb(cap, id),
            Some((id, "start")) if method == "POST" => {
                write_verb(204, "No Content", syscall::mgmt_start(cap, id.as_ptr(), id.len() as u64))
            }
            Some((id, "stop")) if method == "POST" => {
                write_verb(204, "No Content", syscall::mgmt_stop(cap, id.as_ptr(), id.len() as u64))
            }
            _ => resp(404, "Not Found", "text/plain", b"not found"),
        },
    }
}

/// `(status, build_response(...))` — the common route return shape.
fn resp(status: u16, reason: &str, ctype: &str, body: &[u8]) -> (u16, Vec<u8>) {
    (status, http::build_response(status, reason, ctype, body))
}

/// Map a management-verb `MgmtError` return to a Docker-style error response.
fn mgmt_err_response(r: u64) -> (u16, Vec<u8>) {
    match err_code(r) {
        2 => resp(404, "Not Found", "application/json", br#"{"message":"no such container"}"#),
        3 => resp(409, "Conflict", "application/json", br#"{"message":"conflict: invalid container state"}"#),
        4 => resp(400, "Bad Request", "application/json", br#"{"message":"bad parameter"}"#),
        _ => resp(500, "Internal Server Error", "application/json", br#"{"message":"server error"}"#),
    }
}

/// Run a management **read** verb into a fixed buffer → `200` JSON, or an error
/// response (`404`/`409`/`400`/`500`).
fn read_verb<F: FnOnce(&mut [u8]) -> u64>(call: F) -> (u16, Vec<u8>) {
    let mut out = [0u8; MGMT_OUT_CAP];
    let r = call(&mut out);
    if is_err(r) {
        return mgmt_err_response(r);
    }
    let n = (r as usize).min(out.len());
    resp(200, "OK", "application/json", &out[..n])
}

/// A management **write** verb with no success body (`start`/`stop`) → `ok_status`
/// (204) on success, else an error response.
fn write_verb(ok_status: u16, ok_reason: &str, r: u64) -> (u16, Vec<u8>) {
    if is_err(r) {
        return mgmt_err_response(r);
    }
    resp(ok_status, ok_reason, "text/plain", b"")
}

/// `GET /containers/{id}/logs` → `200 text/plain` raw log bytes, or an error.
fn logs_verb(cap: u64, id: &str) -> (u16, Vec<u8>) {
    let mut out = [0u8; MGMT_OUT_CAP];
    let r = syscall::mgmt_logs(cap, id.as_ptr(), id.len() as u64, out.as_mut_ptr(), out.len() as u64);
    if is_err(r) {
        return mgmt_err_response(r);
    }
    let n = (r as usize).min(out.len());
    resp(200, "OK", "text/plain", &out[..n])
}

/// `POST /containers/create` — parse the JSON body, extract a non-empty, NUL-free
/// `Image`, and create. `201` with `{"Id":…}` on success; `400` on a bad body.
fn create_container(cap: u64, req: &http::Request) -> (u16, Vec<u8>) {
    // Untrusted JSON body (bounded to MAX_BODY upstream; parse has a depth guard).
    let parsed = json::parse(&req.body);
    let image = parsed
        .as_ref()
        .and_then(|v| v.get("Image"))
        .and_then(|v| v.as_str());
    let image = match image {
        // Reject an empty Image or one containing NUL (which would corrupt the
        // "image\0name" split).
        Some(s) if !s.is_empty() && !s.as_bytes().contains(&0) => s,
        _ => {
            return resp(
                400,
                "Bad Request",
                "application/json",
                br#"{"message":"create requires a non-empty Image"}"#,
            )
        }
    };
    let name = req.query_param("name").unwrap_or("");
    // Build the "image\0name" spec.
    let mut spec = Vec::with_capacity(image.len() + 1 + name.len());
    spec.extend_from_slice(image.as_bytes());
    spec.push(0);
    spec.extend_from_slice(name.as_bytes());

    let mut out = [0u8; MGMT_OUT_CAP];
    let r = syscall::mgmt_create(cap, spec.as_ptr(), spec.len() as u64, out.as_mut_ptr(), out.len() as u64);
    if is_err(r) {
        return mgmt_err_response(r);
    }
    let n = (r as usize).min(out.len());
    resp(201, "Created", "application/json", &out[..n])
}

/// Parse `/containers/<id>/<action>` → `(id, action)` with a non-empty id and
/// action. Returns `None` for anything else (so `/containers/json` and unknown
/// paths fall through to 404). The last `/` splits id from action, so an id
/// containing `/` just resolves to NotFound downstream.
fn container_action(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/containers/")?;
    let slash = rest.rfind('/')?;
    let id = &rest[..slash];
    let action = &rest[slash + 1..];
    if id.is_empty() || action.is_empty() {
        return None;
    }
    Some((id, action))
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

/// Deterministic in-process self-test (no network): drive `route` over a fixed set
/// of requests and record each HTTP status to the result page, then commit.
///
/// Each request is chosen so its status is **impossible for the catch-all 404** — so
/// observing it proves the specific arm ran, not that the request merely fell
/// through. This exercises the real routing, the untrusted request-body JSON parse,
/// `Image` extraction, and the write verbs without depending on the timing-sensitive
/// inbound-TCP path:
///   - `GET /_ping` → **200** (GET routing).
///   - `POST /containers/create {}` → **400** (create arm; empty `Image` rejected).
///   - `POST /containers/create {"Image":"demo"}` → **500** (`Image` extracted →
///     `SYS_MGMT` create → `CreateFailed` with no `/data` mount in the harness).
///   - `POST /containers/<id>/start` → **409** (`container_action` + start verb; the
///     injected container is `Running`, so the state guard returns `InvalidState`).
fn run_self_test(result: *mut u64) {
    // Three static requests; the fourth (start) is built from SELF_TEST_CONTAINER so
    // the id stays in sync with the container the kernel test injects.
    let mut start_req: Vec<u8> = Vec::new();
    start_req.extend_from_slice(b"POST /containers/");
    start_req.extend_from_slice(SELF_TEST_CONTAINER.as_bytes());
    start_req.extend_from_slice(b"/start HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\n\r\n");
    let requests: [&[u8]; 4] = [
        b"GET /_ping HTTP/1.1\r\nHost: t\r\n\r\n",
        b"POST /containers/create HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\n\r\n{}",
        b"POST /containers/create HTTP/1.1\r\nHost: t\r\nContent-Length: 16\r\n\r\n{\"Image\":\"demo\"}",
        &start_req,
    ];

    let mut count: u64 = 0;
    for (i, req) in requests.iter().enumerate() {
        let (status, _response) = route(req);
        if !result.is_null() && (i as u64) < MAX_RECORDED {
            // SAFETY: kernel-mapped shared page (checked non-null); i < 4 ≤
            // MAX_RECORDED, so the slot is in-bounds within the 4 KiB frame.
            unsafe {
                core::ptr::write_volatile(result.add(STATUS_ARRAY_OFF + i), status as u64);
            }
        }
        count = count.wrapping_add(1);
    }
    if result.is_null() {
        return;
    }
    // SAFETY: kernel-mapped shared page (checked non-null). MAGIC written LAST so the
    // kernel poll never sees a set marker over a stale count/state.
    unsafe {
        core::ptr::write_volatile(result.add(2), count);
        core::ptr::write_volatile(result.add(1), STATUS_SERVING);
        core::ptr::write_volatile(result, RESULT_MAGIC); // commit
    }
}

/// Fail-closed control: record that `listen` was denied (word[1] = DENIED), commit
/// word last.
fn report_denied(result: *mut u64) {
    if result.is_null() {
        return;
    }
    // SAFETY: kernel-mapped shared page (checked non-null); in-bounds slots.
    unsafe {
        core::ptr::write_volatile(result.add(1), STATUS_DENIED);
        core::ptr::write_volatile(result, RESULT_MAGIC); // commit
    }
}

/// Record the HTTP `status` of request `index` and the running `served` count, then
/// commit (`MAGIC` written LAST with volatile stores, so the kernel poll never
/// observes a set magic over a stale status/count).
fn report_served(result: *mut u64, index: u64, status: u16, served: u64) {
    if result.is_null() {
        return;
    }
    // SAFETY: kernel-mapped shared page (checked non-null). The status array is
    // bounded to MAX_RECORDED words, well within the 4 KiB result frame.
    unsafe {
        if index < MAX_RECORDED {
            core::ptr::write_volatile(result.add(STATUS_ARRAY_OFF + index as usize), status as u64);
        }
        core::ptr::write_volatile(result.add(2), served);
        core::ptr::write_volatile(result.add(1), STATUS_SERVING);
        core::ptr::write_volatile(result, RESULT_MAGIC); // commit
    }
}
