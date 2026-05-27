//! # IPC Endpoint kernel object
//!
//! An Endpoint is a rendezvous point for synchronous inter-process
//! communication. It holds two wait queues — one for senders blocked
//! waiting for a receiver, and one for receivers blocked waiting for
//! a sender. When a send and receive meet at the same endpoint, the
//! kernel transfers the message directly.
//!
//! ## Rendezvous semantics (seL4-style)
//!
//! - If a sender arrives and no receiver is waiting: sender blocks.
//! - If a receiver arrives and no sender is waiting: receiver blocks.
//! - If a sender arrives and a receiver IS waiting: immediate transfer,
//!   both continue.
//!
//! ## Call/reply
//!
//! The `call` operation combines send + wait-for-reply. The caller sends
//! a message and blocks waiting for a reply. The server receives the
//! message (which includes a kernel-generated reply token), processes it,
//! and calls `reply` with the token to unblock the caller.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::string::String;
use crate::sched::task::TaskId;
use super::IpcMessage;

/// A blocked sender waiting for a receiver on this endpoint.
#[derive(Debug)]
pub struct WaitingSender {
    /// The blocked sender's task ID (for waking it).
    pub task_id: TaskId,

    /// The message the sender wants to deliver.
    pub message: IpcMessage,
}

/// A blocked receiver waiting for a sender on this endpoint.
#[derive(Debug)]
pub struct WaitingReceiver {
    /// The blocked receiver's task ID (for waking it).
    pub task_id: TaskId,
}

/// A blocked caller waiting for a reply after a `call` operation.
///
/// When a task uses `ipc_call()`, the kernel sends the message to a
/// receiver and then blocks the caller until a matching `ipc_reply()`
/// arrives. The caller's state is stored here.
#[derive(Debug)]
pub struct WaitingCaller {
    /// The blocked caller's task ID (for waking it on reply).
    pub task_id: TaskId,

    /// Kernel-generated token identifying this call. The server includes
    /// this token in its `ipc_reply()` so the kernel knows which caller
    /// to unblock. Tokens are unique within an endpoint.
    pub reply_token: u64,
}

/// An IPC endpoint — a rendezvous point for synchronous message passing.
///
/// Each endpoint has a unique ID (assigned at creation), a name for debugging,
/// and three wait queues:
/// - `senders`: tasks blocked in `send()` waiting for a receiver
/// - `receivers`: tasks blocked in `receive()` waiting for a sender
/// - `callers`: tasks blocked in `call()` waiting for a reply
pub struct Endpoint {
    /// Unique endpoint identifier.
    pub id: u64,

    /// Human-readable name for debug output.
    pub name: String,

    /// Tasks blocked on `send()`, waiting for a receiver to arrive.
    pub senders: VecDeque<WaitingSender>,

    /// Tasks blocked on `receive()`, waiting for a sender to arrive.
    pub receivers: VecDeque<WaitingReceiver>,

    /// Tasks blocked on `call()`, waiting for a `reply()`.
    pub callers: VecDeque<WaitingCaller>,

    /// Monotonically increasing counter for generating reply tokens.
    /// Each `call()` operation gets a unique token so the kernel can
    /// match `reply()` calls to the correct blocked caller.
    pub next_reply_token: u64,
}

impl Endpoint {
    /// Create a new endpoint with the given ID and name.
    pub fn new(id: u64, name: &str) -> Self {
        Self {
            id,
            name: String::from(name),
            senders: VecDeque::new(),
            receivers: VecDeque::new(),
            callers: VecDeque::new(),
            next_reply_token: 1,
        }
    }

    /// Generate a unique reply token for a `call()` operation.
    pub fn alloc_reply_token(&mut self) -> u64 {
        let token = self.next_reply_token;
        self.next_reply_token += 1;
        token
    }
}
