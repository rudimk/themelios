//! Net service IPC protocol — shared between the kernel net service
//! (`kernel/src/net/net_service.rs`) and the ring-3 net server.
//!
//! The frame bridge is **pull-based**: the net server always initiates and the
//! kernel service always replies (the only shape the synchronous-rendezvous IPC
//! supports without deadlock). These opcodes MUST match the kernel-side constants
//! in `net_service.rs` — like `fs_proto`/`block_proto`, they are duplicated on
//! both sides and kept in sync by hand.

/// Request: hand back the next received frame (if any) plus the current time.
/// Reply: `[status, frame_len, uptime_ms, 0]` — the frame is in the RX shared
/// region; `frame_len == 0` means no frame is waiting.
pub const MSG_POLL: u64 = 0;

/// Request: transmit the frame staged in the TX shared region. `word1 = length`.
/// Reply: `[status, 0, 0, 0]`.
pub const MSG_TX_FRAME: u64 = 1;

/// Request: report the interface's IP configuration to the kernel (sub-phase
/// 4.4). The ring-3 net server calls this whenever its DHCPv4 client acquires,
/// renews, or loses a lease, so the kernel can surface the live configuration
/// through the `ifconfig` shell command (the kernel never parses packets — it
/// only records what the server tells it). Reply: `[status, 0, 0, 0]`.
///
/// Word layout (all IPv4 addresses packed by [`pack_ipv4`]):
/// ```text
/// word1 = pack_ipv4(address) in bits 0..32 | (prefix_len << 32)
/// word2 = pack_ipv4(gateway)   (0 = no default gateway)
/// word3 = pack_ipv4(dns[0])    (0 = no DNS server; display only in Phase 4)
/// ```
/// A fully-zero `word1` means "deconfigured" (lease lost / not yet acquired).
pub const MSG_CONFIG: u64 = 2;

/// Reply status: success.
pub const STATUS_OK: u64 = 0;
/// Reply status: failure.
pub const STATUS_ERROR: u64 = 1;

// --- Socket protocol (kernel socket router ↔ net server, Phase 4.5) ---
//
// The kernel routes capability-checked socket syscalls to the net server on the
// server's request endpoint; datagram payloads travel through the shared socket
// region (`SOCKET_REGION_VADDR`). Opcodes start at 10 to stay clear of the frame
// opcodes above. Mirrored by the kernel in `net/socket.rs`.

/// Create a socket. `[OP_SOCK_OPEN, sock_type, 0, 0]` → `[status, socket_id]`.
pub const OP_SOCK_OPEN: u64 = 10;
/// Bind to a local port. `[OP_SOCK_BIND, socket_id, local_ip, port]` → `[status]`.
pub const OP_SOCK_BIND: u64 = 11;
/// Send a datagram staged in the socket region.
/// `[OP_SOCK_SEND, socket_id, len, (dest_ip << 16) | dest_port]` → `[status, bytes]`.
pub const OP_SOCK_SEND: u64 = 12;
/// Receive a datagram into the socket region.
/// `[OP_SOCK_RECV, socket_id, max_len, 0]` → `[status, bytes, (src_ip << 16) | src_port]`.
pub const OP_SOCK_RECV: u64 = 13;
/// Close a socket. `[OP_SOCK_CLOSE, socket_id, 0, 0]` → `[status]`.
pub const OP_SOCK_CLOSE: u64 = 14;

// TCP-specific opcodes (Phase 4.6). Stream send/recv reuse OP_SOCK_SEND /
// OP_SOCK_RECV — the net server dispatches by the socket's kind, and for a TCP
// socket ignores the peer-address word on send and reports none on recv.

/// Begin an outbound TCP connection. `[OP_SOCK_CONNECT, socket_id, ip, port]` →
/// `[status]`. Non-blocking: the handshake completes asynchronously; subsequent
/// send/recv return `SOCK_WOULDBLOCK` until the connection is established, or
/// `SOCK_REFUSED` if it was refused/reset.
pub const OP_SOCK_CONNECT: u64 = 15;
/// Listen for inbound TCP connections on the socket's bound port.
/// `[OP_SOCK_LISTEN, socket_id, backlog, 0]` → `[status]`.
pub const OP_SOCK_LISTEN: u64 = 16;
/// Accept one established inbound connection. `[OP_SOCK_ACCEPT, socket_id, 0, 0]`
/// → `[status, new_socket_id, (peer_ip << 16) | peer_port]`. Returns
/// `SOCK_WOULDBLOCK` if no connection is pending.
pub const OP_SOCK_ACCEPT: u64 = 17;

// --- Inspection / ICMP opcodes (Phase 4.7) ---

/// List the net server's open sockets. `[OP_SOCK_LIST, 0, 0, 0]` →
/// `[status, count, 0, 0]`. The server writes `count` fixed-size
/// [`SOCK_LIST_ENTRY_BYTES`]-byte entries into the shared socket region (see the
/// entry layout on [`SOCK_LIST_ENTRY_BYTES`]); the caller reads them back out.
/// Backs the `sockets` shell command.
pub const OP_SOCK_LIST: u64 = 18;

/// Send one ICMPv4 echo request (ping). `[OP_SOCK_PING, socket_id, dest_ip, seq]`
/// → `[status]`. The socket must be an ICMP socket (`SOCK_TYPE_ICMP`); the server
/// builds the echo request with the socket's identifier and the given sequence
/// number and transmits it. `dest_ip` is a bare [`pack_ipv4`] word. Echo replies
/// are collected with a normal `OP_SOCK_RECV` on the same socket, which returns
/// `[status, seq_no, (src_ip << 16)]` for an ICMP socket. Backs `ping`.
pub const OP_SOCK_PING: u64 = 19;

/// Socket type: UDP datagram socket.
pub const SOCK_TYPE_UDP: u64 = 0;
/// Socket type: TCP stream socket (Phase 4.6).
pub const SOCK_TYPE_TCP: u64 = 1;
/// Socket type: ICMP echo socket (Phase 4.7, backs `ping`).
pub const SOCK_TYPE_ICMP: u64 = 2;

/// Size of one entry written by [`OP_SOCK_LIST`] into the shared socket region.
///
/// Layout (little-endian, one entry per socket, tightly packed):
/// ```text
/// [0..8]   socket id (u64)
/// [8]      kind      (SLK_UDP / SLK_TCP / SLK_ICMP)
/// [9]      state     (SLS_* — smoltcp TCP state, or a fixed code for UDP/ICMP)
/// [10..12] local port (u16; the bound port, 0 if unbound)
/// [12..16] remote IP  (4 octets, most-significant first; 0.0.0.0 if none)
/// [16..18] remote port (u16; 0 if none)
/// [18..24] reserved (zero)
/// ```
pub const SOCK_LIST_ENTRY_BYTES: usize = 24;

// Socket "kind" codes in a `SOCK_LIST` entry (byte 8). Distinct from
// `SOCK_TYPE_*` only to keep the on-wire listing format self-contained.
/// `SOCK_LIST` kind: UDP socket.
pub const SLK_UDP: u8 = 0;
/// `SOCK_LIST` kind: TCP socket.
pub const SLK_TCP: u8 = 1;
/// `SOCK_LIST` kind: ICMP socket.
pub const SLK_ICMP: u8 = 2;

// Socket "state" codes in a `SOCK_LIST` entry (byte 9). For TCP these mirror
// smoltcp's `tcp::State`; UDP and ICMP use single fixed codes.
/// `SOCK_LIST` state: a UDP socket (connectionless — no TCP state).
pub const SLS_UDP: u8 = 0;
/// `SOCK_LIST` state: TCP `Closed`.
pub const SLS_TCP_CLOSED: u8 = 1;
/// `SOCK_LIST` state: TCP `Listen`.
pub const SLS_TCP_LISTEN: u8 = 2;
/// `SOCK_LIST` state: TCP `SynSent`.
pub const SLS_TCP_SYN_SENT: u8 = 3;
/// `SOCK_LIST` state: TCP `SynReceived`.
pub const SLS_TCP_SYN_RECEIVED: u8 = 4;
/// `SOCK_LIST` state: TCP `Established`.
pub const SLS_TCP_ESTABLISHED: u8 = 5;
/// `SOCK_LIST` state: TCP `FinWait1`.
pub const SLS_TCP_FIN_WAIT1: u8 = 6;
/// `SOCK_LIST` state: TCP `FinWait2`.
pub const SLS_TCP_FIN_WAIT2: u8 = 7;
/// `SOCK_LIST` state: TCP `CloseWait`.
pub const SLS_TCP_CLOSE_WAIT: u8 = 8;
/// `SOCK_LIST` state: TCP `Closing`.
pub const SLS_TCP_CLOSING: u8 = 9;
/// `SOCK_LIST` state: TCP `LastAck`.
pub const SLS_TCP_LAST_ACK: u8 = 10;
/// `SOCK_LIST` state: TCP `TimeWait`.
pub const SLS_TCP_TIME_WAIT: u8 = 11;
/// `SOCK_LIST` state: an ICMP socket (bound to an identifier).
pub const SLS_ICMP: u8 = 12;

/// Socket reply status (`word0` of an `OP_SOCK_*` reply): success.
pub const SOCK_OK: u64 = 0;
/// Socket reply status: the operation would block — no datagram pending on
/// `recv`, or the TX buffer is full / address unresolved on `send`. Distinct
/// from a hard error so the caller can retry/poll.
pub const SOCK_WOULDBLOCK: u64 = 1;
/// Socket reply status: a hard error (bad socket id, bind failure, no room).
pub const SOCK_ERR: u64 = 2;
/// Socket reply status: a TCP connection was refused or reset by the peer
/// (Phase 4.6). Distinct from `SOCK_ERR` so the client sees a connection error
/// rather than a generic failure or a hang.
pub const SOCK_REFUSED: u64 = 3;

/// Fixed virtual address where the kernel maps the shared **socket payload
/// region** into the net server (mirrors `SERVER_SOCKET_VIRT` in the kernel's
/// server loader). Datagram bytes for `send`/`recv` live here; the four IPC
/// words carry only the header (socket id, length, peer address). Both sides
/// hardcode this address, so no boot-info field is needed.
pub const SOCKET_REGION_VADDR: u64 = 0x5200_0000;

/// Size of the shared socket payload region in bytes (one datagram in flight).
pub const SOCKET_REGION_BYTES: usize = 64 * 1024;

/// Pack an IPv4 address and port into a single word: `(pack_ipv4(ip) << 16) | port`.
/// Used by `OP_SOCK_SEND`/`OP_SOCK_RECV` to carry the peer endpoint in one word.
#[inline]
pub const fn pack_ip_port(octets: [u8; 4], port: u16) -> u64 {
    (pack_ipv4(octets) << 16) | (port as u64)
}

/// Unpack `(ip << 16) | port` into `(octets, port)`. Inverse of [`pack_ip_port`].
#[inline]
pub const fn unpack_ip_port(word: u64) -> ([u8; 4], u16) {
    (unpack_ipv4(word >> 16), word as u16)
}

/// Pack an IPv4 address (`[a, b, c, d]`) into the low 32 bits of a `u64`, with
/// `a` most significant. The inverse of [`unpack_ipv4`]. Used by [`MSG_CONFIG`];
/// the kernel side mirrors this layout by hand (like the opcodes above).
#[inline]
pub const fn pack_ipv4(octets: [u8; 4]) -> u64 {
    ((octets[0] as u64) << 24)
        | ((octets[1] as u64) << 16)
        | ((octets[2] as u64) << 8)
        | (octets[3] as u64)
}

/// Unpack the low 32 bits of a `u64` into an IPv4 address. Inverse of
/// [`pack_ipv4`].
#[inline]
pub const fn unpack_ipv4(word: u64) -> [u8; 4] {
    [
        (word >> 24) as u8,
        (word >> 16) as u8,
        (word >> 8) as u8,
        word as u8,
    ]
}
