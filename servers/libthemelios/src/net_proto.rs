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
