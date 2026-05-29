//! # Block server — kernel-side IPC interface to block devices
//!
//! In the Phase 3 hybrid-microkernel design, filesystem parsing runs in ring 3.
//! But ring-3 servers can't call the kernel's `BlockDevice` methods directly —
//! they have no access to MMIO, DMA, or the device registry. The **block
//! server** bridges that gap: it is a kernel task that listens on an IPC
//! endpoint, receives block read/write/flush requests from userspace filesystem
//! servers, performs the I/O via the trusted in-kernel driver, and replies with
//! a status.
//!
//! ## Why a server and not a syscall?
//!
//! Routing block I/O through an IPC endpoint (rather than a dedicated syscall)
//! keeps the kernel's syscall surface small and models the device as just
//! another capability-guarded service. A filesystem server holds an endpoint
//! capability to the block server and a shared-memory capability for the data
//! buffer; with neither, it cannot touch storage. This is the capability model
//! applied uniformly.
//!
//! ## Data transfer
//!
//! IPC messages are four words — too small for block data — so payloads move
//! through a [`SharedRegion`](crate::mm::shared::SharedRegion) mapped into both
//! the server and the client. The request names a byte offset into that region;
//! the server reads/writes block data there.
//!
//! ## Protocol
//!
//! Request (client → server), via the four IPC message words:
//! - `word0` = operation: 0 = READ, 1 = WRITE, 2 = FLUSH
//! - `word1` = start LBA (block index)
//! - `word2` = block count
//! - `word3` = byte offset into the shared region
//!
//! Reply (server → client):
//! - `word0` = status: 0 = OK, 1 = ERROR
//! - `word1` = error code (a `BlockError` discriminant when status = ERROR)

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::ipc::{self, IpcMessage};
use crate::mm::shared::SharedRegion;
use crate::sched;

use super::block;

/// Operation codes carried in `word0` of a block request.
const OP_READ: u64 = 0;
const OP_WRITE: u64 = 1;
const OP_FLUSH: u64 = 2;

/// Reply status codes.
const STATUS_OK: u64 = 0;
const STATUS_ERROR: u64 = 1;

/// Size of each block server shared-memory transfer window (128 KiB), matching
/// the Phase 3 memory budget. One region is created per server instance.
const SHARED_REGION_BYTES: u64 = 128 * 1024;

// --- Server state ---
//
// The scheduler spawns tasks as bare `fn()` entry points with no arguments, so
// the server's configuration is published through these globals before the task
// starts. A single block server instance is supported in Phase 3.

/// IPC endpoint the server listens on (0 until started).
static SERVER_ENDPOINT: AtomicU64 = AtomicU64::new(0);
/// Physical base of the shared transfer region.
static SHARED_PHYS: AtomicU64 = AtomicU64::new(0);
/// Size of the shared transfer region in bytes.
static SHARED_SIZE: AtomicU64 = AtomicU64::new(0);
/// Registry index of the block device the server drives.
static DEVICE_INDEX: AtomicUsize = AtomicUsize::new(0);

/// A handle to a started block server, returned to whoever spawned it.
///
/// Carries the IPC endpoint clients send requests to and the shared region
/// description so the spawner can map it into client address spaces (or, in a
/// kernel-side test, access it directly via the HHDM).
#[derive(Clone, Copy)]
pub struct BlockServerHandle {
    /// Endpoint clients send block requests to.
    pub endpoint: u64,
    /// The shared transfer region (data buffer).
    pub region: SharedRegion,
}

/// Start the block server for the device at registry `device_index`.
///
/// Allocates a 128 KiB shared transfer region, creates the IPC endpoint, and
/// spawns the server task. Returns a handle with the endpoint and region.
///
/// Panics if the shared region can't be allocated (out of contiguous memory)
/// or the device index is invalid.
pub fn start(device_index: usize) -> BlockServerHandle {
    assert!(block::get(device_index).is_some(), "block_server: invalid device index");

    let region = SharedRegion::alloc(SHARED_REGION_BYTES)
        .expect("block_server: failed to allocate shared region");
    let endpoint = ipc::create_endpoint("block-server");

    SHARED_PHYS.store(region.phys_base.as_u64(), Ordering::SeqCst);
    SHARED_SIZE.store(region.size, Ordering::SeqCst);
    DEVICE_INDEX.store(device_index, Ordering::SeqCst);
    SERVER_ENDPOINT.store(endpoint, Ordering::SeqCst);

    sched::spawn("block-server", server_loop);

    BlockServerHandle { endpoint, region }
}

/// The endpoint the running block server listens on (0 if not started).
pub fn endpoint() -> u64 {
    SERVER_ENDPOINT.load(Ordering::SeqCst)
}

/// The server task entry point: receive requests, perform I/O, reply.
///
/// Runs forever. Each iteration blocks in `ipc_receive` until a client sends a
/// request, dispatches it to the block device, and replies with the status.
fn server_loop() {
    let endpoint = SERVER_ENDPOINT.load(Ordering::SeqCst);

    loop {
        let request = match ipc::ipc_receive(endpoint) {
            Ok(msg) => msg,
            // If the endpoint is torn down, the server exits.
            Err(_) => return,
        };

        let (status, error_code) = handle_request(&request);

        let reply = IpcMessage::new([status, error_code, 0, 0]);
        // A failed reply (caller gone) is non-fatal — keep serving.
        let _ = ipc::ipc_reply(endpoint, request.reply_token, reply);
    }
}

/// Dispatch a single block request, returning `(status, error_code)`.
fn handle_request(request: &IpcMessage) -> (u64, u64) {
    let op = request.words[0];
    let start_lba = request.words[1];
    let block_count = request.words[2];
    let offset = request.words[3];

    let device = match block::get(DEVICE_INDEX.load(Ordering::SeqCst)) {
        Some(d) => d,
        None => return (STATUS_ERROR, 0),
    };

    // FLUSH carries no data buffer.
    if op == OP_FLUSH {
        return match device.flush() {
            Ok(()) => (STATUS_OK, 0),
            Err(e) => (STATUS_ERROR, e as u64),
        };
    }

    // READ/WRITE: compute and bounds-check the shared-buffer window.
    let block_size = device.block_size() as u64;
    let len = block_count * block_size;
    let shared_size = SHARED_SIZE.load(Ordering::SeqCst);
    if offset.checked_add(len).map_or(true, |end| end > shared_size) {
        // Request would read/write outside the shared region.
        return (STATUS_ERROR, BlockErrorCode::BadOffset as u64);
    }

    // SAFETY: the shared region is valid for `shared_size` bytes via the HHDM,
    // and the request/reply protocol serialises access to it (a client waits
    // for the reply before reusing the window).
    let region = SharedRegion {
        phys_base: crate::mm::addr::PhysAddr::new(SHARED_PHYS.load(Ordering::SeqCst)),
        size: shared_size,
    };
    let buf = unsafe { region.as_slice_mut() };
    let window = &mut buf[offset as usize..(offset + len) as usize];

    let result = match op {
        OP_READ => device.read_blocks(start_lba, window),
        OP_WRITE => device.write_blocks(start_lba, window),
        _ => return (STATUS_ERROR, BlockErrorCode::BadOp as u64),
    };

    match result {
        Ok(()) => (STATUS_OK, 0),
        Err(e) => (STATUS_ERROR, e as u64),
    }
}

/// Block-server-specific error codes (distinct from `BlockError` discriminants),
/// returned in the reply's `word1` for protocol-level failures.
#[repr(u64)]
enum BlockErrorCode {
    /// The requested offset/length fell outside the shared region.
    BadOffset = 100,
    /// An unknown operation code was supplied.
    BadOp = 101,
}
