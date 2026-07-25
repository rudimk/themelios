//! # Socket routing — the kernel's capability-checked socket API (Phase 4.5)
//!
//! Sockets follow the exact same hybrid-microkernel pattern as the Phase 3
//! filesystem: the kernel is a **router**, not an implementor. The UDP/TCP
//! protocol state lives in the ring-3 net server (smoltcp); the kernel checks a
//! capability, forwards the request to the net server over IPC, moves datagram
//! bytes through a shared region, and audit-logs. **The kernel never parses
//! packet data.**
//!
//! ## Capability model
//!
//! Access is gated by [`CapType::Socket`], with two roles:
//!
//! - The **network authority** (`socket_id == `[`SOCKET_FACTORY`]) is the right
//!   to *create* sockets. [`sys_socket`] requires the caller to present it, just
//!   as `SYS_OPEN` requires a [`CapType::Filesystem`] capability. A process
//!   without it cannot make sockets.
//! - A **per-socket capability** (`socket_id` = a net-server socket id) is minted
//!   by [`sys_socket`] and consumed by bind/sendto/recvfrom/close, mirroring how
//!   `FileDescriptor` capabilities work. WRITE grants send, READ grants receive.
//!
//! ## Data path
//!
//! Like the VFS, the payload region is shared between the **kernel and the net
//! server** (not the client process): the kernel copies validated user bytes
//! into it and reads results out via the HHDM, so a client never shares memory
//! with the server directly and the kernel mediates every transfer. Phase 4.5
//! uses a single such region and, like the FS servers, serves one request at a
//! time in practice; per-client regions are a later refinement.

use crate::cap::{CapHandle, CapRights, CapType, Capability, SOCKET_FACTORY};
use crate::ipc::{self, IpcMessage};
use crate::mm::shared::SharedRegion;
use crate::process::{self, ProcessId};
use crate::sync::InterruptMutex;

// --- Socket protocol opcodes (mirror libthemelios::net_proto) ---

const OP_SOCK_OPEN: u64 = 10;
const OP_SOCK_BIND: u64 = 11;
const OP_SOCK_SEND: u64 = 12;
const OP_SOCK_RECV: u64 = 13;
const OP_SOCK_CLOSE: u64 = 14;
const OP_SOCK_CONNECT: u64 = 15;
const OP_SOCK_LISTEN: u64 = 16;
const OP_SOCK_ACCEPT: u64 = 17;
const OP_SOCK_LIST: u64 = 18;
const OP_SOCK_PING: u64 = 19;

/// Bytes per entry returned by `OP_SOCK_LIST` (mirrors
/// `net_proto::SOCK_LIST_ENTRY_BYTES`).
const SOCK_LIST_ENTRY_BYTES: usize = 24;

/// Socket type: UDP (Phase 4.5).
pub const SOCK_TYPE_UDP: u64 = 0;
/// Socket type: TCP stream (Phase 4.6).
pub const SOCK_TYPE_TCP: u64 = 1;
/// Socket type: ICMP echo (Phase 4.7, backs `ping`).
pub const SOCK_TYPE_ICMP: u64 = 2;

/// Net-server reply status words (mirror `net_proto`).
const SOCK_OK: u64 = 0;
const SOCK_WOULDBLOCK: u64 = 1;
const SOCK_REFUSED: u64 = 3;

/// High bit marking a socket syscall return as an encoded [`SockError`].
const STATUS_ERROR_FLAG: u64 = 1 << 63;

/// Socket errors — returned by the socket syscalls. The discriminants are the
/// low bits of a high-bit-set syscall return, so userspace can distinguish them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SockError {
    /// Non-blocking op with nothing ready (no datagram to receive, TX full).
    WouldBlock = 1,
    /// The capability check failed (missing/wrong-type/insufficient-rights cap).
    PermissionDenied = 2,
    /// A malformed argument (bad length, unroutable address).
    InvalidArgument = 3,
    /// The requested local port is already in use.
    AddrInUse = 4,
    /// Out of socket handles or buffers in the net server.
    NoResources = 5,
    /// The net server is unreachable (not started, IPC failed).
    ServerUnavailable = 6,
    /// A TCP connection was refused or reset by the peer (Phase 4.6).
    ConnectionRefused = 7,
}

impl SockError {
    /// Encode as a high-bit-set syscall return (matching the FS convention).
    pub fn as_syscall_ret(self) -> u64 {
        STATUS_ERROR_FLAG | self as u64
    }
}

/// Map a net-server reply status word into a `Result`.
fn map_status(status: u64) -> Result<(), SockError> {
    match status {
        SOCK_OK => Ok(()),
        SOCK_WOULDBLOCK => Err(SockError::WouldBlock),
        SOCK_REFUSED => Err(SockError::ConnectionRefused),
        _ => Err(SockError::InvalidArgument),
    }
}

// --- IPv4 packing (mirror net_proto; the kernel does not depend on libthemelios) ---

fn pack_ipv4(o: [u8; 4]) -> u64 {
    ((o[0] as u64) << 24) | ((o[1] as u64) << 16) | ((o[2] as u64) << 8) | (o[3] as u64)
}

fn unpack_ipv4(w: u64) -> [u8; 4] {
    [(w >> 24) as u8, (w >> 16) as u8, (w >> 8) as u8, w as u8]
}

/// Pack a peer address into one word: `(ip << 16) | port`.
fn pack_ip_port(ip: [u8; 4], port: u16) -> u64 {
    (pack_ipv4(ip) << 16) | (port as u64)
}

/// Unpack `(ip << 16) | port`.
fn unpack_ip_port(w: u64) -> ([u8; 4], u16) {
    (unpack_ipv4(w >> 16), w as u16)
}

// --- Router state ---

/// The socket router: where the net server listens and the shared payload region.
#[derive(Clone, Copy)]
struct Router {
    /// The net server's request endpoint (it serves `OP_SOCK_*` here).
    endpoint: u64,
    /// Kernel↔net-server shared region for datagram payloads.
    region: SharedRegion,
}

/// Single router (Phase 4 has one net server / one NIC). Set by [`init`] at boot.
static ROUTER: InterruptMutex<Option<Router>> = InterruptMutex::new(None);

/// Register the net server's socket endpoint and payload region. Called by
/// `boot_net` once the net server is spawned and its socket region mapped.
pub fn init(endpoint: u64, region: SharedRegion) {
    *ROUTER.lock() = Some(Router { endpoint, region });
}

/// Copy of the current router, or `ServerUnavailable` if sockets aren't up.
fn router() -> Result<Router, SockError> {
    ROUTER.lock().ok_or(SockError::ServerUnavailable)
}

// --- Capability resolution ---

/// Verify `pid` holds the network-authority capability at `handle`.
fn resolve_factory(pid: ProcessId, handle: CapHandle) -> Result<(), SockError> {
    process::with_cspace_mut(pid, |cspace| match cspace.lookup(handle) {
        Ok(cap) => match cap.cap_type {
            CapType::Socket { socket_id } if socket_id == SOCKET_FACTORY => Ok(()),
            _ => Err(SockError::PermissionDenied),
        },
        Err(_) => Err(SockError::PermissionDenied),
    })
    .unwrap_or(Err(SockError::PermissionDenied))
}

/// Resolve `handle` to a per-socket id, requiring `need` rights. Rejects the
/// factory capability (it is not a usable socket).
fn resolve_socket(pid: ProcessId, handle: CapHandle, need: CapRights) -> Result<u64, SockError> {
    process::with_cspace_mut(pid, |cspace| match cspace.lookup(handle) {
        Ok(cap) => match cap.cap_type {
            CapType::Socket { socket_id }
                if socket_id != SOCKET_FACTORY && cap.rights.contains(need) =>
            {
                Ok(socket_id)
            }
            _ => Err(SockError::PermissionDenied),
        },
        Err(_) => Err(SockError::PermissionDenied),
    })
    .unwrap_or(Err(SockError::PermissionDenied))
}

// --- IPC helper ---

/// Call the net server and return its reply, or `ServerUnavailable` on IPC error.
fn sock_call(endpoint: u64, words: [u64; 4]) -> Result<IpcMessage, SockError> {
    ipc::ipc_call(endpoint, IpcMessage::new(words), 0).map_err(|_| SockError::ServerUnavailable)
}

/// Audit-log a socket operation (detail = the socket syscall number).
fn audit(pid: ProcessId, syscall_num: u64, socket_id: u64) {
    crate::audit::log_event(
        pid,
        crate::audit::AuditOp::NetAccess,
        CapType::Socket { socket_id },
        syscall_num,
    );
}

// --- Capability-checked operations (called by the syscall layer) ---

/// Create a socket. `factory_handle` must resolve to the network authority.
/// Returns a fresh `Socket` capability handle for the new socket.
pub fn sys_socket(
    pid: ProcessId,
    factory_handle: CapHandle,
    sock_type: u64,
    syscall_num: u64,
) -> Result<u32, SockError> {
    resolve_factory(pid, factory_handle)?;
    if sock_type != SOCK_TYPE_UDP {
        return Err(SockError::InvalidArgument);
    }
    let r = router()?;
    let reply = sock_call(r.endpoint, [OP_SOCK_OPEN, sock_type, 0, 0])?;
    map_status(reply.words[0])?;
    let socket_id = reply.words[1];
    audit(pid, syscall_num, socket_id);

    // Mint the per-socket capability under the factory authority.
    process::with_cspace_mut(pid, |cspace| {
        cspace.insert(Capability {
            cap_type: CapType::Socket { socket_id },
            rights: CapRights::READ | CapRights::WRITE,
            parent: Some(factory_handle),
        })
    })
    .ok_or(SockError::ServerUnavailable)?
    .map(|h| h.as_raw())
    .map_err(|_| SockError::NoResources)
}

/// Bind a socket to a local port (and optional local address; `ip == [0;4]`
/// means any). Requires WRITE rights.
pub fn sys_bind(
    pid: ProcessId,
    handle: CapHandle,
    ip: [u8; 4],
    port: u16,
    syscall_num: u64,
) -> Result<(), SockError> {
    let socket_id = resolve_socket(pid, handle, CapRights::WRITE)?;
    let r = router()?;
    audit(pid, syscall_num, socket_id);
    let reply = sock_call(r.endpoint, [OP_SOCK_BIND, socket_id, pack_ipv4(ip), port as u64])?;
    map_status(reply.words[0])
}

/// Send a datagram to `(ip, port)`. Requires WRITE (send) rights. Returns the
/// number of bytes accepted.
pub fn sys_sendto(
    pid: ProcessId,
    handle: CapHandle,
    buf: &[u8],
    ip: [u8; 4],
    port: u16,
    syscall_num: u64,
) -> Result<usize, SockError> {
    let socket_id = resolve_socket(pid, handle, CapRights::WRITE)?;
    let r = router()?;
    audit(pid, syscall_num, socket_id);
    // Stage the payload in the shared region.
    // SAFETY: the kernel owns this region's frames; the call is synchronous.
    let dst = unsafe { r.region.as_slice_mut() };
    let n = buf.len().min(dst.len());
    dst[..n].copy_from_slice(&buf[..n]);
    let reply = sock_call(
        r.endpoint,
        [OP_SOCK_SEND, socket_id, n as u64, pack_ip_port(ip, port)],
    )?;
    map_status(reply.words[0])?;
    Ok((reply.words[1] as usize).min(n))
}

/// Receive a datagram into `buf`. Requires READ (receive) rights. Returns
/// `(bytes, src_ip, src_port)`, or `WouldBlock` if nothing is pending.
pub fn sys_recvfrom(
    pid: ProcessId,
    handle: CapHandle,
    buf: &mut [u8],
    syscall_num: u64,
) -> Result<(usize, [u8; 4], u16), SockError> {
    let socket_id = resolve_socket(pid, handle, CapRights::READ)?;
    let r = router()?;
    audit(pid, syscall_num, socket_id);
    let max = buf.len().min(r.region.size as usize);
    let reply = sock_call(r.endpoint, [OP_SOCK_RECV, socket_id, max as u64, 0])?;
    map_status(reply.words[0])?;
    let n = (reply.words[1] as usize).min(max);
    let (ip, port) = unpack_ip_port(reply.words[2]);
    // SAFETY: kernel-owned region; the synchronous call has completed.
    let src = unsafe { r.region.as_slice_mut() };
    buf[..n].copy_from_slice(&src[..n]);
    Ok((n, ip, port))
}

/// Close a socket and revoke its capability. Requires READ rights (any holder).
pub fn sys_close(
    pid: ProcessId,
    handle: CapHandle,
    syscall_num: u64,
) -> Result<(), SockError> {
    let socket_id = resolve_socket(pid, handle, CapRights::READ)?;
    let r = router()?;
    audit(pid, syscall_num, socket_id);
    let reply = sock_call(r.endpoint, [OP_SOCK_CLOSE, socket_id, 0, 0]);
    // Drop the capability regardless of the server's reply.
    let _ = process::with_cspace_mut(pid, |cspace| cspace.remove(handle));
    reply.map(|_| ())
}

// --- TCP stream operations (Phase 4.6) ---

/// Begin an outbound TCP connection to `(ip, port)`. Requires WRITE rights.
/// Non-blocking: it returns once the handshake is initiated; the connection
/// completes asynchronously, so [`sys_send`]/[`sys_recv`] return `WouldBlock`
/// until it is established (or `ConnectionRefused` if the peer rejects it).
pub fn sys_connect(
    pid: ProcessId,
    handle: CapHandle,
    ip: [u8; 4],
    port: u16,
    syscall_num: u64,
) -> Result<(), SockError> {
    let socket_id = resolve_socket(pid, handle, CapRights::WRITE)?;
    let r = router()?;
    audit(pid, syscall_num, socket_id);
    let reply = sock_call(r.endpoint, [OP_SOCK_CONNECT, socket_id, pack_ipv4(ip), port as u64])?;
    map_status(reply.words[0])
}

/// Send bytes on a connected (TCP) socket. Requires WRITE rights. Returns the
/// number of bytes accepted (may be fewer than requested); `WouldBlock` if the
/// connection is still establishing or the send window is full.
pub fn sys_send(
    pid: ProcessId,
    handle: CapHandle,
    buf: &[u8],
    syscall_num: u64,
) -> Result<usize, SockError> {
    let socket_id = resolve_socket(pid, handle, CapRights::WRITE)?;
    let r = router()?;
    audit(pid, syscall_num, socket_id);
    // SAFETY: the kernel owns this region's frames; the call is synchronous.
    let dst = unsafe { r.region.as_slice_mut() };
    let n = buf.len().min(dst.len());
    dst[..n].copy_from_slice(&buf[..n]);
    // word3 = 0: TCP is connection-oriented, no per-datagram destination.
    let reply = sock_call(r.endpoint, [OP_SOCK_SEND, socket_id, n as u64, 0])?;
    map_status(reply.words[0])?;
    Ok((reply.words[1] as usize).min(n))
}

/// Receive bytes from a connected (TCP) socket. Requires READ rights. Returns
/// the number of bytes read (`0` = the peer closed the connection);
/// `WouldBlock` if connected but no data is pending yet.
pub fn sys_recv(
    pid: ProcessId,
    handle: CapHandle,
    buf: &mut [u8],
    syscall_num: u64,
) -> Result<usize, SockError> {
    let socket_id = resolve_socket(pid, handle, CapRights::READ)?;
    let r = router()?;
    audit(pid, syscall_num, socket_id);
    let max = buf.len().min(r.region.size as usize);
    let reply = sock_call(r.endpoint, [OP_SOCK_RECV, socket_id, max as u64, 0])?;
    map_status(reply.words[0])?;
    let n = (reply.words[1] as usize).min(max);
    // SAFETY: kernel-owned region; the synchronous call has completed.
    let src = unsafe { r.region.as_slice_mut() };
    buf[..n].copy_from_slice(&src[..n]);
    Ok(n)
}

/// Listen for inbound TCP connections on a bound socket. Requires WRITE rights.
/// The socket must already be bound (via [`sys_bind`]).
pub fn sys_listen(
    pid: ProcessId,
    handle: CapHandle,
    backlog: u64,
    syscall_num: u64,
) -> Result<(), SockError> {
    let socket_id = resolve_socket(pid, handle, CapRights::WRITE)?;
    let r = router()?;
    audit(pid, syscall_num, socket_id);
    let reply = sock_call(r.endpoint, [OP_SOCK_LISTEN, socket_id, backlog, 0])?;
    map_status(reply.words[0])
}

/// Accept one established inbound connection on a listening socket. Requires READ
/// rights. On success mints a fresh `Socket` capability for the accepted
/// connection (mirroring how `SYS_SOCKET`/`vfs_open` mint caps — the net server
/// never fabricates them) and returns `(cap_handle, peer_ip, peer_port)`.
/// Returns `WouldBlock` if no connection is pending.
pub fn sys_accept(
    pid: ProcessId,
    handle: CapHandle,
    syscall_num: u64,
) -> Result<(u32, [u8; 4], u16), SockError> {
    let listen_id = resolve_socket(pid, handle, CapRights::READ)?;
    let r = router()?;
    audit(pid, syscall_num, listen_id);
    let reply = sock_call(r.endpoint, [OP_SOCK_ACCEPT, listen_id, 0, 0])?;
    map_status(reply.words[0])?;
    let conn_socket_id = reply.words[1];
    let (ip, port) = unpack_ip_port(reply.words[2]);
    // Mint a per-connection Socket capability under the listener.
    let cap = process::with_cspace_mut(pid, |cspace| {
        cspace.insert(Capability {
            cap_type: CapType::Socket { socket_id: conn_socket_id },
            rights: CapRights::READ | CapRights::WRITE,
            parent: Some(handle),
        })
    })
    .ok_or(SockError::ServerUnavailable)?
    .map(|h| h.as_raw())
    .map_err(|_| SockError::NoResources)?;
    Ok((cap, ip, port))
}

// --- Kernel-internal operations (no capability check) ---
//
// These address the net server directly, for the debug shell's `udpsend`. The
// kernel is trusted; userspace must go through the capability-checked `sys_*`.

/// Create a UDP socket directly, returning its net-server socket id.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_open() -> Result<u64, SockError> {
    let r = router()?;
    let reply = sock_call(r.endpoint, [OP_SOCK_OPEN, SOCK_TYPE_UDP, 0, 0])?;
    map_status(reply.words[0])?;
    Ok(reply.words[1])
}

/// Bind socket `socket_id` to a local port directly.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_bind(socket_id: u64, ip: [u8; 4], port: u16) -> Result<(), SockError> {
    let r = router()?;
    let reply = sock_call(r.endpoint, [OP_SOCK_BIND, socket_id, pack_ipv4(ip), port as u64])?;
    map_status(reply.words[0])
}

/// Send `data` from socket `socket_id` to `(ip, port)` directly.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_sendto(socket_id: u64, data: &[u8], ip: [u8; 4], port: u16) -> Result<usize, SockError> {
    let r = router()?;
    let dst = unsafe { r.region.as_slice_mut() };
    let n = data.len().min(dst.len());
    dst[..n].copy_from_slice(&data[..n]);
    let reply = sock_call(r.endpoint, [OP_SOCK_SEND, socket_id, n as u64, pack_ip_port(ip, port)])?;
    map_status(reply.words[0])?;
    Ok((reply.words[1] as usize).min(n))
}

/// Receive a datagram on socket `socket_id` directly into `buf`.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_recvfrom(socket_id: u64, buf: &mut [u8]) -> Result<(usize, [u8; 4], u16), SockError> {
    let r = router()?;
    let max = buf.len().min(r.region.size as usize);
    let reply = sock_call(r.endpoint, [OP_SOCK_RECV, socket_id, max as u64, 0])?;
    map_status(reply.words[0])?;
    let n = (reply.words[1] as usize).min(max);
    let (ip, port) = unpack_ip_port(reply.words[2]);
    let src = unsafe { r.region.as_slice_mut() };
    buf[..n].copy_from_slice(&src[..n]);
    Ok((n, ip, port))
}

/// Close socket `socket_id` directly.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_close(socket_id: u64) -> Result<(), SockError> {
    let r = router()?;
    sock_call(r.endpoint, [OP_SOCK_CLOSE, socket_id, 0, 0]).map(|_| ())
}

/// Create a TCP socket directly, returning its net-server socket id.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_open_tcp() -> Result<u64, SockError> {
    let r = router()?;
    let reply = sock_call(r.endpoint, [OP_SOCK_OPEN, SOCK_TYPE_TCP, 0, 0])?;
    map_status(reply.words[0])?;
    Ok(reply.words[1])
}

/// Begin a TCP connect on `socket_id` to `(ip, port)` directly (non-blocking).
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_connect(socket_id: u64, ip: [u8; 4], port: u16) -> Result<(), SockError> {
    let r = router()?;
    let reply = sock_call(r.endpoint, [OP_SOCK_CONNECT, socket_id, pack_ipv4(ip), port as u64])?;
    map_status(reply.words[0])
}

/// Stream-send `data` on TCP `socket_id` directly. Returns bytes accepted.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_send(socket_id: u64, data: &[u8]) -> Result<usize, SockError> {
    let r = router()?;
    let dst = unsafe { r.region.as_slice_mut() };
    let n = data.len().min(dst.len());
    dst[..n].copy_from_slice(&data[..n]);
    let reply = sock_call(r.endpoint, [OP_SOCK_SEND, socket_id, n as u64, 0])?;
    map_status(reply.words[0])?;
    Ok((reply.words[1] as usize).min(n))
}

/// Stream-receive into `buf` on TCP `socket_id` directly. Returns bytes read.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_recv(socket_id: u64, buf: &mut [u8]) -> Result<usize, SockError> {
    let r = router()?;
    let max = buf.len().min(r.region.size as usize);
    let reply = sock_call(r.endpoint, [OP_SOCK_RECV, socket_id, max as u64, 0])?;
    map_status(reply.words[0])?;
    let n = (reply.words[1] as usize).min(max);
    let src = unsafe { r.region.as_slice_mut() };
    buf[..n].copy_from_slice(&src[..n]);
    Ok(n)
}

/// Bind TCP `socket_id` to a local port directly (records the port for listen).
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_bind_port(socket_id: u64, port: u16) -> Result<(), SockError> {
    let r = router()?;
    let reply = sock_call(r.endpoint, [OP_SOCK_BIND, socket_id, 0, port as u64])?;
    map_status(reply.words[0])
}

/// Put TCP `socket_id` into LISTEN directly.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_listen(socket_id: u64) -> Result<(), SockError> {
    let r = router()?;
    let reply = sock_call(r.endpoint, [OP_SOCK_LISTEN, socket_id, 1, 0])?;
    map_status(reply.words[0])
}

/// Accept one connection on listening TCP `socket_id` directly. Returns the new
/// connection's net-server socket id and peer `(ip, port)`; `WouldBlock` if none.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_accept(socket_id: u64) -> Result<(u64, [u8; 4], u16), SockError> {
    let r = router()?;
    let reply = sock_call(r.endpoint, [OP_SOCK_ACCEPT, socket_id, 0, 0])?;
    map_status(reply.words[0])?;
    let (ip, port) = unpack_ip_port(reply.words[2]);
    Ok((reply.words[1], ip, port))
}

// --- ICMP / inspection (Phase 4.7) ---

/// Create an ICMP echo socket directly, returning its net-server socket id. The
/// net server binds it to an identifier at open time so echo replies route back.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_open_icmp() -> Result<u64, SockError> {
    let r = router()?;
    let reply = sock_call(r.endpoint, [OP_SOCK_OPEN, SOCK_TYPE_ICMP, 0, 0])?;
    map_status(reply.words[0])?;
    Ok(reply.words[1])
}

/// Send one ICMPv4 echo request (ping) with sequence `seq` to `ip` on ICMP
/// `socket_id`. `WouldBlock` if the socket's TX buffer is momentarily full.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_ping(socket_id: u64, ip: [u8; 4], seq: u16) -> Result<(), SockError> {
    let r = router()?;
    let reply = sock_call(r.endpoint, [OP_SOCK_PING, socket_id, pack_ipv4(ip), seq as u64])?;
    map_status(reply.words[0])
}

/// Collect the next buffered ICMP echo reply on ICMP `socket_id`. Returns the
/// reply's `(sequence_number, source_ip)`; `WouldBlock` if none has arrived.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_ping_recv(socket_id: u64) -> Result<(u16, [u8; 4]), SockError> {
    let r = router()?;
    let reply = sock_call(r.endpoint, [OP_SOCK_RECV, socket_id, 0, 0])?;
    map_status(reply.words[0])?;
    let seq = reply.words[1] as u16;
    // The ICMP recv reply packs the source address as `ip << 16` (port unused).
    let (ip, _) = unpack_ip_port(reply.words[2]);
    Ok((seq, ip))
}

/// A single row of the `sockets` listing, decoded from an `OP_SOCK_LIST` entry.
#[cfg_attr(feature = "test", allow(dead_code))]
pub struct SockInfo {
    /// Net-server socket id.
    pub id: u64,
    /// Socket kind code (`net_proto::SLK_*`: 0=UDP, 1=TCP, 2=ICMP).
    pub kind: u8,
    /// State code (`net_proto::SLS_*`: TCP state, or a fixed code for UDP/ICMP).
    pub state: u8,
    /// Bound local port (0 if unbound); for ICMP, the bound identifier.
    pub local_port: u16,
    /// Connected peer address (0.0.0.0 if none).
    pub remote_ip: [u8; 4],
    /// Connected peer port (0 if none).
    pub remote_port: u16,
}

/// List the net server's open sockets. Asks the server to serialise its socket
/// table into the shared region (`OP_SOCK_LIST`), then decodes the fixed-size
/// entries. Backs the `sockets` shell command; the kernel is trusted here (no
/// capability check), like the other `ksocket_*` helpers.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn ksocket_list() -> Result<alloc::vec::Vec<SockInfo>, SockError> {
    let r = router()?;
    let reply = sock_call(r.endpoint, [OP_SOCK_LIST, 0, 0, 0])?;
    map_status(reply.words[0])?;
    let count = reply.words[1] as usize;
    // SAFETY: kernel-owned region; the synchronous call has completed, so the
    // server has finished writing the entries.
    let src = unsafe { r.region.as_slice_mut() };
    let mut out = alloc::vec::Vec::new();
    for i in 0..count {
        let off = i * SOCK_LIST_ENTRY_BYTES;
        if off + SOCK_LIST_ENTRY_BYTES > src.len() {
            break;
        }
        let e = &src[off..off + SOCK_LIST_ENTRY_BYTES];
        let mut id_bytes = [0u8; 8];
        id_bytes.copy_from_slice(&e[0..8]);
        out.push(SockInfo {
            id: u64::from_le_bytes(id_bytes),
            kind: e[8],
            state: e[9],
            local_port: u16::from_le_bytes([e[10], e[11]]),
            remote_ip: [e[12], e[13], e[14], e[15]],
            remote_port: u16::from_le_bytes([e[16], e[17]]),
        });
    }
    Ok(out)
}
