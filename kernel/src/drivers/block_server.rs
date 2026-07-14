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

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::ipc::{self, IpcMessage};
use crate::mm::shared::SharedRegion;
use crate::sync::InterruptMutex;
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
// a server task can't be handed its configuration directly. Instead each
// `start()` fills a self-contained [`Instance`] slot *before* spawning its task,
// and every server task claims a distinct slot at entry via [`CLAIM`]. Because
// each slot fully describes its endpoint, device, and shared region, multiple
// block servers can run concurrently without sharing mutable global state —
// which is exactly what boot needs (one block server for the SquashFS root disk,
// another for the ext2 data disk). An earlier single-global design silently made
// every server drive whichever device `start()` was called for *last*.

/// Maximum number of concurrent block server instances. Phase 3 needs two (root
/// SquashFS + data ext2); the headroom covers future data volumes cheaply.
const MAX_INSTANCES: usize = 8;

/// Per-instance configuration for one block server task. Fully self-contained so
/// a task only ever touches its own slot after claiming it.
#[derive(Clone, Copy)]
struct Instance {
    /// IPC endpoint this instance listens on.
    endpoint: u64,
    /// Shared transfer region (data buffer) for this instance.
    region: SharedRegion,
    /// Registry index of the block device this instance drives.
    device_index: usize,
}

/// Configuration slots, one per started instance. Written by `start()` before
/// the corresponding task is spawned; read once by that task when it claims its
/// slot. `None` until populated.
static INSTANCES: InterruptMutex<[Option<Instance>; MAX_INSTANCES]> =
    InterruptMutex::new([None; MAX_INSTANCES]);
/// Number of slots handed out by `start()` (the write cursor into `INSTANCES`).
static INSTANCE_COUNT: AtomicUsize = AtomicUsize::new(0);
/// Next slot for a starting server task to claim (the read cursor). Each task
/// does a single `fetch_add`, guaranteeing every task gets a distinct slot.
static CLAIM: AtomicUsize = AtomicUsize::new(0);

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

    // Reserve a configuration slot and publish this instance into it *before*
    // spawning the task, so the task sees fully-initialised config when it
    // claims the slot.
    let slot = INSTANCE_COUNT.fetch_add(1, Ordering::SeqCst);
    assert!(slot < MAX_INSTANCES, "block_server: too many instances");
    INSTANCES.lock()[slot] = Some(Instance {
        endpoint,
        region,
        device_index,
    });

    sched::spawn("block-server", server_loop);

    BlockServerHandle { endpoint, region }
}

/// The server task entry point: receive requests, perform I/O, reply.
///
/// Runs forever. Each iteration blocks in `ipc_receive` until a client sends a
/// request, dispatches it to the block device, and replies with the status.
fn server_loop() {
    // Claim a distinct configuration slot. Every server task does exactly one
    // `fetch_add`, so no two tasks ever share a slot, and each slot was fully
    // populated by its `start()` before the task was spawned.
    let slot = CLAIM.fetch_add(1, Ordering::SeqCst);
    let instance = match INSTANCES.lock().get(slot).copied().flatten() {
        Some(inst) => inst,
        // No config for this slot — misconfigured spawn; nothing to serve.
        None => return,
    };

    loop {
        let request = match ipc::ipc_receive(instance.endpoint) {
            Ok(msg) => msg,
            // If the endpoint is torn down, the server exits.
            Err(_) => return,
        };

        let (status, error_code) = handle_request(&instance, &request);

        let reply = IpcMessage::new([status, error_code, 0, 0]);
        // A failed reply (caller gone) is non-fatal — keep serving.
        let _ = ipc::ipc_reply(instance.endpoint, request.reply_token, reply);
    }
}

/// Dispatch a single block request for `instance`, returning `(status, error_code)`.
fn handle_request(instance: &Instance, request: &IpcMessage) -> (u64, u64) {
    let op = request.words[0];
    let start_lba = request.words[1];
    let block_count = request.words[2];
    let offset = request.words[3];

    let device = match block::get(instance.device_index) {
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
    let shared_size = instance.region.size;
    if offset.checked_add(len).map_or(true, |end| end > shared_size) {
        // Request would read/write outside the shared region.
        return (STATUS_ERROR, BlockErrorCode::BadOffset as u64);
    }

    // SAFETY: the shared region is valid for `shared_size` bytes via the HHDM,
    // and the request/reply protocol serialises access to it (a client waits
    // for the reply before reusing the window).
    let buf = unsafe { instance.region.as_slice_mut() };
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
