//! # Inter-process communication (IPC)
//!
//! Since ThemeliOS is a microkernel, most system services (drivers, filesystems,
//! networking) run in userspace. IPC is how they communicate — it's the backbone
//! of the entire system.
//!
//! ## Synchronous message passing (seL4-style)
//!
//! The primary IPC mechanism is synchronous rendezvous: `send` blocks until a
//! receiver calls `receive` on the same endpoint (and vice versa). When both
//! sides rendezvous, the kernel copies the message directly from sender to
//! receiver with zero intermediate buffering.
//!
//! ## IpcMessage
//!
//! Each message carries:
//! - **4 data words** (32 bytes): inline register-sized values for small payloads
//! - **Badge** (u64): kernel-stamped sender identity from the endpoint capability.
//!   Unforgeable — the receiver can trust the badge to identify the sender.
//! - **Cap transfer** (optional `CapHandle`): the kernel can move a capability
//!   from the sender's CSpace to the receiver's CSpace atomically with the message.
//! - **Reply token** (u64): for `call`/`reply` patterns, the kernel generates
//!   a unique token so the server knows which caller to reply to.
//!
//! ## Operations
//!
//! - `ipc_send`: send a message, block until a receiver is ready
//! - `ipc_receive`: receive a message, block until a sender is ready
//! - `ipc_call`: send a message and block waiting for a reply (RPC pattern)
//! - `ipc_reply`: reply to a `call`, unblocking the caller
//!
//! ## Capability integration
//!
//! IPC endpoints are themselves capabilities. To send a message to a service,
//! you need a capability for that service's endpoint. This means you can't even
//! *address* a service you haven't been granted access to.

extern crate alloc;

pub mod endpoint;

use alloc::vec::Vec;
use crate::sync::InterruptMutex;
use crate::sched;
use crate::sched::task::TaskId;
use crate::cap::CapHandle;
use crate::cap::CapType;
use crate::audit;

use endpoint::{Endpoint, WaitingSender, WaitingReceiver, WaitingCaller};

// --- IPC message ---

/// A message passed between processes via an IPC endpoint.
///
/// Messages are small and fixed-size — designed to be passed in registers
/// for the fast path (4 words = 32 bytes fits in 4 general-purpose registers).
/// For bulk data, processes should use shared memory capabilities and just
/// pass a "data ready" notification via IPC.
#[derive(Debug, Clone)]
pub struct IpcMessage {
    /// Four 64-bit data words (32 bytes of inline payload).
    /// Interpretation is up to the sender/receiver protocol.
    pub words: [u64; 4],

    /// Sender badge, stamped by the kernel from the sender's endpoint
    /// capability badge value. The receiver can use this to identify the
    /// sender without trusting user-supplied data. Zero for unbadged sends.
    pub badge: u64,

    /// Optional capability to transfer. If `Some`, the kernel removes the
    /// capability from the sender's CSpace and inserts it into the receiver's
    /// CSpace during message delivery. The receiver gets a new handle.
    pub cap_transfer: Option<CapHandle>,

    /// Reply token for `call`/`reply` patterns. Set by the kernel when the
    /// message was sent via `ipc_call()`. The server includes this token in
    /// its `ipc_reply()` so the kernel knows which caller to unblock.
    /// Zero for normal `send()` messages.
    pub reply_token: u64,
}

impl IpcMessage {
    /// Create a new message with the given data words and no cap transfer.
    pub fn new(words: [u64; 4]) -> Self {
        Self {
            words,
            badge: 0,
            cap_transfer: None,
            reply_token: 0,
        }
    }
}

// --- Endpoint registry ---

/// Global registry of all IPC endpoints in the system.
///
/// Protected by InterruptMutex since IPC operations can be triggered from
/// syscall context. Endpoints are indexed by position in the Vec; the
/// endpoint's `id` field is its unique identifier (matching position on
/// creation, but we search by id for robustness).
static ENDPOINT_REGISTRY: InterruptMutex<EndpointRegistry> =
    InterruptMutex::new(EndpointRegistry::new_empty());

/// Internal state of the endpoint registry.
struct EndpointRegistry {
    /// All endpoints. `None` slots are for destroyed endpoints.
    endpoints: Vec<Option<Endpoint>>,

    /// Monotonically increasing counter for assigning unique endpoint IDs.
    next_id: u64,
}

impl EndpointRegistry {
    const fn new_empty() -> Self {
        Self {
            endpoints: Vec::new(),
            next_id: 1, // 0 reserved as "no endpoint"
        }
    }
}

/// Message delivery table: per-task inbox for delivering messages to blocked tasks.
///
/// When a message is delivered to a blocked receiver (or a reply to a blocked
/// caller), the message is stored in slot [task_id]. When the task wakes up,
/// it reads its message from here.
///
/// This avoids the need to pass messages through the scheduler or stack — the
/// kernel simply writes to the inbox before waking the task, and the task reads
/// from the inbox after being scheduled.
static DELIVERY_TABLE: InterruptMutex<Vec<Option<IpcMessage>>> =
    InterruptMutex::new(Vec::new());

/// Ensure the delivery table has at least `task_id + 1` slots.
fn ensure_delivery_slot(task_id: TaskId) {
    let mut table = DELIVERY_TABLE.lock();
    while table.len() <= task_id {
        table.push(None);
    }
}

/// Store a message in a task's delivery slot.
fn deliver_message(task_id: TaskId, msg: IpcMessage) {
    ensure_delivery_slot(task_id);
    let mut table = DELIVERY_TABLE.lock();
    table[task_id] = Some(msg);
}

/// Take a message from the current task's delivery slot.
fn take_delivery(task_id: TaskId) -> Option<IpcMessage> {
    let mut table = DELIVERY_TABLE.lock();
    if task_id < table.len() {
        table[task_id].take()
    } else {
        None
    }
}

// --- Public API ---

/// Error type for IPC operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    /// The endpoint ID does not match any registered endpoint.
    InvalidEndpoint,

    /// The reply token does not match any blocked caller on the endpoint.
    InvalidReplyToken,
}

/// Create a new IPC endpoint and return its unique ID.
///
/// The endpoint starts with empty wait queues. Callers should create
/// endpoint capabilities and grant them to communicating processes.
pub fn create_endpoint(name: &str) -> u64 {
    let mut registry = ENDPOINT_REGISTRY.lock();
    let id = registry.next_id;
    registry.next_id += 1;

    let ep = Endpoint::new(id, name);

    // Try to reuse a free slot
    for slot in registry.endpoints.iter_mut() {
        if slot.is_none() {
            *slot = Some(ep);
            return id;
        }
    }

    registry.endpoints.push(Some(ep));
    id
}

/// Destroy an endpoint, removing it from the registry.
///
/// Any tasks blocked on this endpoint will remain blocked (they should be
/// woken and given an error in a full implementation, but for Phase 2 we
/// assume endpoints are only destroyed when no tasks are using them).
pub fn destroy_endpoint(endpoint_id: u64) -> bool {
    let mut registry = ENDPOINT_REGISTRY.lock();
    for slot in registry.endpoints.iter_mut() {
        if let Some(ep) = slot {
            if ep.id == endpoint_id {
                *slot = None;
                return true;
            }
        }
    }
    false
}

/// Send a message to an endpoint. Blocks until a receiver is ready.
///
/// ## Rendezvous semantics
///
/// - If a receiver is already waiting on this endpoint, the message is
///   delivered immediately and the receiver is woken. The sender does
///   NOT block.
/// - If no receiver is waiting, the sender is placed in the endpoint's
///   sender queue and blocks until a receiver arrives.
///
/// ## Badge
///
/// The `badge` parameter is stamped into the message by the kernel.
/// Typically this comes from the sender's endpoint capability badge
/// field, set at grant time. The receiver can identify the sender
/// by checking the badge without trusting user-supplied data.
pub fn ipc_send(endpoint_id: u64, mut message: IpcMessage, badge: u64) -> Result<(), IpcError> {
    message.badge = badge;
    let current_task = sched::current_task_id();

    // Audit log the send attempt. Logged before blocking so the event
    // appears even if the sender waits a long time for a receiver.
    audit::log_event(
        sched::current_process_id(),
        audit::AuditOp::IpcSend,
        CapType::Endpoint { endpoint_id, badge },
        endpoint_id,
    );

    // Disable interrupts BEFORE acquiring the endpoint lock to prevent
    // a lost-wakeup race: if a timer interrupt fires between dropping
    // the lock (which would re-enable interrupts) and calling
    // block_current_task(), the other side's wake_task() could see us
    // in Ready state and do nothing, causing us to block forever.
    // With interrupts already disabled when InterruptMutex is acquired,
    // its drop sees were_enabled=false and keeps them disabled through
    // to block_current_task().
    #[cfg(target_arch = "x86_64")]
    crate::arch::x86_64::cpu::cli();

    let should_block = {
        let mut registry = ENDPOINT_REGISTRY.lock();
        let ep = find_endpoint_mut(&mut registry, endpoint_id)
            .ok_or_else(|| {
                // Re-enable interrupts on error return — we won't block.
                #[cfg(target_arch = "x86_64")]
                crate::arch::x86_64::cpu::sti();
                IpcError::InvalidEndpoint
            })?;

        if let Some(receiver) = ep.receivers.pop_front() {
            // Immediate rendezvous: deliver message and wake receiver
            deliver_message(receiver.task_id, message.clone());
            sched::wake_task(receiver.task_id);
            false // sender does NOT block
        } else {
            // No receiver waiting: queue the sender
            ep.senders.push_back(WaitingSender {
                task_id: current_task,
                message,
            });
            true // sender must block
        }
    };

    if should_block {
        // block_current_task() calls cli() (no-op, already disabled),
        // marks us Blocked, calls schedule(), and calls sti() on resume.
        sched::block_current_task();
    } else {
        // Non-blocking path: re-enable interrupts ourselves.
        #[cfg(target_arch = "x86_64")]
        crate::arch::x86_64::cpu::sti();
    }

    Ok(())
}

/// Receive a message from an endpoint. Blocks until a sender is ready.
///
/// ## Rendezvous semantics
///
/// - If a sender is already waiting on this endpoint, the sender's message
///   is taken immediately and the sender is woken. The receiver does NOT block.
/// - If no sender is waiting, the receiver is placed in the endpoint's
///   receiver queue and blocks until a sender arrives.
///
/// Returns the received message with the sender's badge and any transferred
/// capability handle.
pub fn ipc_receive(endpoint_id: u64) -> Result<IpcMessage, IpcError> {
    let current_task = sched::current_task_id();

    // Audit log the receive attempt.
    audit::log_event(
        sched::current_process_id(),
        audit::AuditOp::IpcReceive,
        CapType::Endpoint { endpoint_id, badge: 0 },
        endpoint_id,
    );

    // Disable interrupts to prevent lost-wakeup race (see ipc_send comment).
    #[cfg(target_arch = "x86_64")]
    crate::arch::x86_64::cpu::cli();

    let immediate_msg = {
        let mut registry = ENDPOINT_REGISTRY.lock();
        let ep = find_endpoint_mut(&mut registry, endpoint_id)
            .ok_or_else(|| {
                #[cfg(target_arch = "x86_64")]
                crate::arch::x86_64::cpu::sti();
                IpcError::InvalidEndpoint
            })?;

        if let Some(sender) = ep.senders.pop_front() {
            // Immediate rendezvous: take the message and wake the sender
            sched::wake_task(sender.task_id);
            Some(sender.message)
        } else {
            // No sender waiting: queue the receiver
            ep.receivers.push_back(WaitingReceiver {
                task_id: current_task,
            });
            None
        }
    };

    if let Some(msg) = immediate_msg {
        // Non-blocking path: re-enable interrupts.
        #[cfg(target_arch = "x86_64")]
        crate::arch::x86_64::cpu::sti();
        return Ok(msg);
    }

    // block_current_task() handles cli/sti — interrupts are already disabled.
    sched::block_current_task();

    // After waking, the message should be in our delivery slot
    take_delivery(current_task).ok_or(IpcError::InvalidEndpoint)
}

/// Send a message and wait for a reply (RPC pattern).
///
/// This is the "call" half of call/reply. The kernel:
/// 1. Generates a unique reply token
/// 2. Stamps the token into the message
/// 3. Delivers the message to a receiver (like `ipc_send`)
/// 4. Blocks the caller until `ipc_reply()` is called with the matching token
///
/// The receiver sees the reply token in the message and must call
/// `ipc_reply(endpoint_id, reply_token, reply_msg)` to unblock the caller.
///
/// Returns the reply message.
pub fn ipc_call(endpoint_id: u64, mut message: IpcMessage, badge: u64) -> Result<IpcMessage, IpcError> {
    message.badge = badge;
    let current_task = sched::current_task_id();

    // Audit log the call attempt.
    audit::log_event(
        sched::current_process_id(),
        audit::AuditOp::IpcCall,
        CapType::Endpoint { endpoint_id, badge },
        endpoint_id,
    );

    // Disable interrupts to prevent lost-wakeup race (see ipc_send comment).
    // For call(), the caller ALWAYS blocks (waiting for reply), so we never
    // take a non-blocking early return — sti() happens inside block_current_task().
    #[cfg(target_arch = "x86_64")]
    crate::arch::x86_64::cpu::cli();

    {
        let mut registry = ENDPOINT_REGISTRY.lock();
        let ep = find_endpoint_mut(&mut registry, endpoint_id)
            .ok_or_else(|| {
                #[cfg(target_arch = "x86_64")]
                crate::arch::x86_64::cpu::sti();
                IpcError::InvalidEndpoint
            })?;

        let reply_token = ep.alloc_reply_token();
        message.reply_token = reply_token;

        // Register the caller as waiting for a reply.
        ep.callers.push_back(WaitingCaller {
            task_id: current_task,
            reply_token,
        });

        // Try to deliver to a waiting receiver
        if let Some(receiver) = ep.receivers.pop_front() {
            deliver_message(receiver.task_id, message);
            sched::wake_task(receiver.task_id);
        } else {
            // No receiver: queue as a sender (but also in callers for the reply)
            ep.senders.push_back(WaitingSender {
                task_id: current_task,
                message,
            });
        }
    }

    // Always block — caller waits for ipc_reply() to deliver the response.
    // block_current_task() calls sti() when we're rescheduled.
    sched::block_current_task();

    // After waking, the reply message should be in our delivery slot
    take_delivery(current_task).ok_or(IpcError::InvalidEndpoint)
}

/// Reply to a `call`, unblocking the caller.
///
/// This is the "reply" half of call/reply. The server provides the reply
/// token (from the received message's `reply_token` field) and the reply
/// message. The kernel matches the token to the blocked caller, delivers
/// the reply, and wakes the caller.
///
/// The reply token is single-use: after the reply is delivered, the
/// caller's wait entry is removed from the endpoint.
pub fn ipc_reply(endpoint_id: u64, reply_token: u64, reply_msg: IpcMessage) -> Result<(), IpcError> {
    // Audit log the reply.
    audit::log_event(
        sched::current_process_id(),
        audit::AuditOp::IpcReply,
        CapType::Endpoint { endpoint_id, badge: 0 },
        endpoint_id,
    );

    let mut registry = ENDPOINT_REGISTRY.lock();
    let ep = find_endpoint_mut(&mut registry, endpoint_id)
        .ok_or(IpcError::InvalidEndpoint)?;

    // Find the caller waiting for this reply token
    let caller_idx = ep.callers.iter().position(|c| c.reply_token == reply_token);
    let caller = match caller_idx {
        Some(idx) => ep.callers.remove(idx).unwrap(),
        None => return Err(IpcError::InvalidReplyToken),
    };

    // Deliver the reply and wake the caller
    deliver_message(caller.task_id, reply_msg);
    sched::wake_task(caller.task_id);

    Ok(())
}

/// Get the number of registered endpoints (for diagnostics).
pub fn endpoint_count() -> usize {
    let registry = ENDPOINT_REGISTRY.lock();
    registry.endpoints.iter().filter(|s| s.is_some()).count()
}

// --- Internal helpers ---

/// Find an endpoint by ID in the registry, returning a mutable reference.
///
/// The registry lock must be held by the caller.
fn find_endpoint_mut(registry: &mut EndpointRegistry, id: u64) -> Option<&mut Endpoint> {
    registry.endpoints.iter_mut()
        .filter_map(|slot| slot.as_mut())
        .find(|ep| ep.id == id)
}
