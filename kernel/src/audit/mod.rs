//! # Kernel audit log
//!
//! A tamper-evident audit trail recording all security-relevant operations in
//! the kernel: capability creation, granting, revocation, IPC messages, and
//! syscall invocations. The log is a fixed-size ring buffer in kernel memory
//! that overwrites the oldest entries when full.
//!
//! ## Design
//!
//! The audit log is a `[AuditEntry; CAPACITY]` ring buffer behind an
//! `InterruptMutex`. Each entry has a monotonically increasing **sequence
//! number** — when the buffer wraps, sequence numbers jump (e.g., entry
//! at position 0 has seq=2048 while the previous occupant had seq=0),
//! enabling consumers to detect overwrites.
//!
//! ## Entry format
//!
//! Each `AuditEntry` records:
//! - **seq**: unique monotonic sequence number (never wraps in practice — u64)
//! - **timestamp**: PIT tick count at the time of the event (~10 ms granularity)
//! - **source_pid**: the process that triggered the operation
//! - **operation**: what happened (Create, Grant, Revoke, IpcSend, etc.)
//! - **cap_type**: the type of capability involved (or Null if N/A)
//! - **detail**: a 64-bit operation-specific value (e.g., endpoint ID, handle
//!   raw value, target PID)
//!
//! ## Overhead
//!
//! Each call to `log_event()` acquires the InterruptMutex, writes one entry
//! (a few cache lines), and releases the lock. At ~32 bytes per entry, this
//! is trivially fast and does not measurably affect timer tick consistency.
//!
//! ## Access
//!
//! The audit log is kernel-internal only — no userspace access in Phase 2.
//! The `audit [n]` shell command reads the last N entries for debugging.
//! External streaming comes in Phase 12.

extern crate alloc;

use crate::sync::InterruptMutex;
use crate::process::ProcessId;
use crate::cap::CapType;

/// Maximum number of entries the ring buffer can hold.
///
/// 2048 entries × ~32 bytes each ≈ 64 KiB, which fits comfortably in the
/// kernel heap. When the buffer is full, new entries overwrite the oldest.
const CAPACITY: usize = 2048;

/// The type of operation being audited.
///
/// Each variant corresponds to a security-relevant kernel event. The
/// `detail` field in `AuditEntry` carries operation-specific data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOp {
    /// A new capability was inserted into a CSpace.
    /// Detail: the raw CapHandle value of the newly created capability.
    CapCreate,

    /// A capability was derived (granted) with equal or reduced rights.
    /// Detail: the raw CapHandle value of the source capability.
    CapGrant,

    /// A capability (and all its descendants) was revoked.
    /// Detail: the raw CapHandle value that was revoked.
    CapRevoke,

    /// A capability was transferred via IPC message.
    /// Detail: the raw CapHandle value of the transferred capability.
    CapTransfer,

    /// A message was sent to an IPC endpoint.
    /// Detail: the endpoint ID.
    IpcSend,

    /// A message was received from an IPC endpoint.
    /// Detail: the endpoint ID.
    IpcReceive,

    /// An RPC call was made to an IPC endpoint.
    /// Detail: the endpoint ID.
    IpcCall,

    /// A reply was sent in response to an RPC call.
    /// Detail: the endpoint ID.
    IpcReply,

    /// A syscall was invoked from userspace.
    /// Detail: the syscall number.
    Syscall,

    /// A filesystem operation was dispatched through the VFS layer.
    /// Detail: the filesystem syscall number (SYS_OPEN .. SYS_READDIR).
    FsAccess,

    /// A socket operation was dispatched through the kernel socket router
    /// (Phase 4.5). Detail: the socket syscall number (SYS_SOCKET .. SYS_SOCKET_CLOSE).
    NetAccess,

    /// A management-ABI operation was invoked by a holder of the `Management`
    /// capability (Phase 6.3): container list/inspect/create/start/stop/logs or a
    /// listener open. Detail: the management op number.
    ApiAccess,
}

impl core::fmt::Display for AuditOp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AuditOp::CapCreate   => write!(f, "cap_create"),
            AuditOp::CapGrant    => write!(f, "cap_grant"),
            AuditOp::CapRevoke   => write!(f, "cap_revoke"),
            AuditOp::CapTransfer => write!(f, "cap_transfer"),
            AuditOp::IpcSend     => write!(f, "ipc_send"),
            AuditOp::IpcReceive  => write!(f, "ipc_receive"),
            AuditOp::IpcCall     => write!(f, "ipc_call"),
            AuditOp::IpcReply    => write!(f, "ipc_reply"),
            AuditOp::Syscall     => write!(f, "syscall"),
            AuditOp::FsAccess    => write!(f, "fs_access"),
            AuditOp::NetAccess   => write!(f, "net_access"),
            AuditOp::ApiAccess   => write!(f, "api_access"),
        }
    }
}

/// A single entry in the audit log.
///
/// Designed to be small (fits in a couple of cache lines) and self-contained.
/// The `seq` field enables detection of overwrites when the ring buffer wraps.
#[derive(Debug, Clone, Copy)]
pub struct AuditEntry {
    /// Monotonically increasing sequence number. Assigned by the ring buffer
    /// on append. When two entries have non-consecutive sequence numbers at
    /// adjacent buffer positions, the gap reveals how many entries were
    /// overwritten.
    pub seq: u64,

    /// PIT tick count at the time of the event (~10 ms per tick at 100 Hz).
    /// Useful for correlating audit events with other timed observations.
    pub timestamp: u64,

    /// The process that triggered this operation. PID 0 = kernel.
    pub source_pid: ProcessId,

    /// What happened — the type of operation being recorded.
    pub operation: AuditOp,

    /// The type of capability involved in the operation, or `Null` if the
    /// event is not capability-related (e.g., a generic syscall).
    pub cap_type: CapType,

    /// Operation-specific 64-bit value. Interpretation depends on `operation`:
    /// - CapCreate/CapGrant/CapRevoke/CapTransfer: raw CapHandle value
    /// - IpcSend/IpcReceive/IpcCall/IpcReply: endpoint ID
    /// - Syscall: syscall number
    pub detail: u64,
}

/// Fixed-size ring buffer for audit log entries.
///
/// Entries are written at `write_pos % CAPACITY`. When the buffer is full,
/// new entries overwrite the oldest ones. The monotonic `next_seq` counter
/// lets consumers detect how many entries were lost to wrapping.
struct AuditRingBuffer {
    /// The entry storage. Initialized to zeroed entries on creation.
    entries: [AuditEntry; CAPACITY],

    /// Index of the next write position (modulo CAPACITY).
    write_pos: usize,

    /// The sequence number to assign to the next entry. Starts at 1 so
    /// that seq=0 indicates an unwritten/default slot.
    next_seq: u64,

    /// Total number of entries that have been written (including overwrites).
    /// Used to determine how many valid entries exist when the buffer hasn't
    /// wrapped yet.
    total_written: u64,
}

/// Create a zeroed AuditEntry for array initialization.
///
/// The seq=0 and Null operation/type mark this as an unwritten slot.
const fn zeroed_entry() -> AuditEntry {
    AuditEntry {
        seq: 0,
        timestamp: 0,
        source_pid: ProcessId::KERNEL,
        operation: AuditOp::CapCreate, // placeholder — seq=0 marks it as unwritten
        cap_type: CapType::Null,
        detail: 0,
    }
}

impl AuditRingBuffer {
    /// Create a new empty ring buffer with all slots zeroed.
    const fn new() -> Self {
        Self {
            entries: [zeroed_entry(); CAPACITY],
            write_pos: 0,
            next_seq: 1, // seq 0 reserved as "unwritten" sentinel
            total_written: 0,
        }
    }

    /// Append an entry to the ring buffer.
    ///
    /// Assigns the next sequence number and current timestamp, then writes
    /// the entry at the current write position. If the buffer is full, the
    /// oldest entry is overwritten (the sequence number gap reveals the loss).
    fn append(&mut self, mut entry: AuditEntry) {
        // Stamp the entry with sequence number and timestamp
        entry.seq = self.next_seq;
        self.next_seq += 1;

        #[cfg(target_arch = "x86_64")]
        {
            entry.timestamp = crate::arch::x86_64::idt::tick_count();
        }

        // Write at the current position, wrapping around if necessary
        self.entries[self.write_pos] = entry;
        self.write_pos = (self.write_pos + 1) % CAPACITY;
        self.total_written += 1;
    }

    /// Read the last `n` entries in chronological order (oldest first).
    ///
    /// Returns a Vec of entries, skipping any unwritten (seq=0) slots.
    /// If `n` exceeds the number of valid entries, returns all valid entries.
    fn last_n(&self, n: usize) -> alloc::vec::Vec<AuditEntry> {
        let valid_count = core::cmp::min(self.total_written as usize, CAPACITY);
        let count = core::cmp::min(n, valid_count);

        if count == 0 {
            return alloc::vec::Vec::new();
        }

        let mut result = alloc::vec::Vec::with_capacity(count);

        // The most recent entry is at write_pos - 1 (wrapping).
        // We want the last `count` entries in chronological order,
        // so we start from (write_pos - count) and read forward.
        let start = if self.write_pos >= count {
            self.write_pos - count
        } else {
            CAPACITY - (count - self.write_pos)
        };

        for i in 0..count {
            let idx = (start + i) % CAPACITY;
            let entry = self.entries[idx];
            // Skip unwritten slots (shouldn't happen if count <= valid_count,
            // but defensive)
            if entry.seq > 0 {
                result.push(entry);
            }
        }

        result
    }

    /// Return the total number of entries ever written (including overwrites).
    fn total_written(&self) -> u64 {
        self.total_written
    }

    /// Return the current sequence number (one past the last written entry).
    fn current_seq(&self) -> u64 {
        self.next_seq
    }
}

/// Global audit log, protected by InterruptMutex since audit events can be
/// logged from syscall context or interrupt handlers.
static AUDIT_LOG: InterruptMutex<AuditRingBuffer> =
    InterruptMutex::new(AuditRingBuffer::new());

/// Log an audit event.
///
/// This is the primary API for the audit subsystem. Call it from any kernel
/// code path that performs a security-relevant operation. The sequence number
/// and timestamp are assigned automatically.
///
/// ## Example
///
/// ```ignore
/// audit::log_event(
///     ProcessId::KERNEL,
///     AuditOp::CapCreate,
///     CapType::Endpoint { endpoint_id: 42, badge: 0 },
///     handle.as_raw() as u64,
/// );
/// ```
pub fn log_event(source_pid: ProcessId, operation: AuditOp, cap_type: CapType, detail: u64) {
    let entry = AuditEntry {
        seq: 0,       // will be overwritten by append()
        timestamp: 0, // will be overwritten by append()
        source_pid,
        operation,
        cap_type,
        detail,
    };
    AUDIT_LOG.lock().append(entry);
}

/// Read the last `n` entries from the audit log, oldest first.
///
/// Used by the `audit` shell command and by tests.
pub fn last_entries(n: usize) -> alloc::vec::Vec<AuditEntry> {
    AUDIT_LOG.lock().last_n(n)
}

/// Get the total number of events ever logged (including overwritten ones).
pub fn total_event_count() -> u64 {
    AUDIT_LOG.lock().total_written()
}

/// Get the current sequence number (next seq to be assigned).
///
/// Useful for tests: take a "before" snapshot, perform operations, then
/// read entries with seq >= the snapshot to see only the new events.
pub fn current_seq() -> u64 {
    AUDIT_LOG.lock().current_seq()
}
