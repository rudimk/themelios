//! Block server IPC protocol — shared with the kernel's `drivers::block_server`.
//!
//! A filesystem server reads/writes disk blocks by sending requests to the
//! kernel block server's endpoint and transferring data through its shared
//! memory region. Request words: `[op, start_lba, block_count, shared_offset]`.
//! Reply words: `[status, error_code, _, _]`.
//!
//! Keep these constants in step with `kernel/src/drivers/block_server.rs`.

/// Read `block_count` blocks from `start_lba` into the shared region at `offset`.
pub const OP_READ: u64 = 0;
/// Write `block_count` blocks from the shared region at `offset` to `start_lba`.
pub const OP_WRITE: u64 = 1;
/// Flush the device write cache (no data buffer).
pub const OP_FLUSH: u64 = 2;

/// Reply status: the request succeeded.
pub const STATUS_OK: u64 = 0;
/// Reply status: the request failed (see `error_code`).
pub const STATUS_ERROR: u64 = 1;

/// VirtIO block sector size, in bytes. All block addressing is in these units.
pub const BLOCK_SIZE: u64 = 512;
