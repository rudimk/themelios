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

/// Reply status: success.
pub const STATUS_OK: u64 = 0;
/// Reply status: failure.
pub const STATUS_ERROR: u64 = 1;
