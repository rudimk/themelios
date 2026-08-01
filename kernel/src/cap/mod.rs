//! # Capability system
//!
//! The core security primitive of ThemeliOS. Every resource in the system — memory
//! regions, IPC endpoints, device access — is accessed through **capability tokens**.
//!
//! ## What is a capability?
//!
//! A capability is an unforgeable token that grants its holder permission to
//! perform specific operations on a specific resource. Think of it like a key:
//! you can only open doors you have keys for, and you can't forge new keys.
//!
//! ## Key properties
//!
//! - **No ambient authority**: A process starts with zero capabilities. It can
//!   only access resources that have been explicitly granted to it by its parent
//!   or the kernel.
//! - **Unforgeable**: Capabilities are kernel-managed integers. Userspace cannot
//!   create, modify, or guess valid capability handles.
//! - **Transferable**: A process can pass a capability to another process via IPC,
//!   enabling controlled sharing.
//! - **Revocable**: The kernel (or a parent process with MANAGE rights) can revoke
//!   a capability, instantly cutting off access — including all derived children.
//!
//! ## Architecture
//!
//! - **CapHandle**: a 32-bit opaque token combining a 12-bit slot index and a
//!   20-bit generation counter. Prevents stale-handle reuse.
//! - **CSpace** (capability space): a per-process flat vector of capability slots,
//!   indexed by the handle's slot field. Maximum 4096 entries.
//! - **CapObject**: kernel-side registry of the actual objects that capabilities
//!   reference (IPC endpoints, memory regions, etc.). Capabilities in CSpaces are
//!   lightweight references into this registry.
//! - **Revocation**: walks the CSpace and invalidates all capabilities derived
//!   from a given parent. O(n) in CSpace size — acceptable for Phase 2.
//!
//! ## Inspiration
//!
//! This design draws from seL4 (formally verified capability microkernel) and
//! Fuchsia/Zircon (Google's capability-based OS).

extern crate alloc;

pub mod cspace;
pub mod object;

use core::fmt;

// --- Capability rights ---

/// Rights bitfield that controls what operations a capability permits.
///
/// Rights can only be *reduced* when deriving (granting) a new capability —
/// never expanded. This ensures that a child capability can never have more
/// authority than its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapRights(u32);

impl CapRights {
    /// Permission to read from the resource (memory read, receive on endpoint).
    pub const READ: Self = Self(1 << 0);

    /// Permission to write to the resource (memory write, send on endpoint).
    pub const WRITE: Self = Self(1 << 1);

    /// Permission to execute from the resource (memory execute).
    pub const EXECUTE: Self = Self(1 << 2);

    /// Permission to derive sub-capabilities via `grant()`.
    /// Without GRANT, the holder cannot share the capability with others.
    pub const GRANT: Self = Self(1 << 3);

    /// Permission to destroy the underlying object or revoke derived capabilities.
    pub const MANAGE: Self = Self(1 << 4);

    /// No rights at all. A Null capability has this.
    pub const NONE: Self = Self(0);

    /// All rights combined. Used when the kernel creates the initial capability
    /// for an object (the "root" capability).
    pub const ALL: Self = Self(
        Self::READ.0 | Self::WRITE.0 | Self::EXECUTE.0 | Self::GRANT.0 | Self::MANAGE.0
    );

    /// Create a rights bitfield from a raw u32 value.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Get the raw u32 value of this rights bitfield.
    pub const fn as_raw(self) -> u32 {
        self.0
    }

    /// Check whether this rights set contains a specific right.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Compute the intersection of two rights sets.
    /// The result contains only rights present in BOTH sets.
    pub const fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Check whether `other` is a subset of (or equal to) this rights set.
    /// Used to verify that a grant operation doesn't expand rights.
    pub const fn is_superset_of(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Bitwise OR for combining rights: `CapRights::READ | CapRights::WRITE`
impl core::ops::BitOr for CapRights {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl fmt::Display for CapRights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        let flags = [
            (Self::READ, "R"),
            (Self::WRITE, "W"),
            (Self::EXECUTE, "X"),
            (Self::GRANT, "G"),
            (Self::MANAGE, "M"),
        ];
        for (flag, name) in flags {
            if self.contains(flag) {
                if !first { write!(f, "|")?; }
                write!(f, "{}", name)?;
                first = false;
            }
        }
        if first {
            write!(f, "NONE")?;
        }
        Ok(())
    }
}

// --- Capability types ---

/// The type of resource a capability references.
///
/// Each variant carries type-specific metadata that identifies the
/// kernel object this capability grants access to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapType {
    /// An invalid/empty capability. Slot index 0 in every CSpace is always Null.
    Null,

    /// A contiguous range of physical memory frames.
    /// Used for mapping physical memory into a process's address space.
    Memory {
        /// Physical base address of the memory region (page-aligned).
        base: u64,
        /// Number of pages in the region.
        page_count: usize,
    },

    /// An IPC endpoint. Capabilities to the same endpoint_id can communicate.
    /// The badge is stamped by the kernel on derived capabilities to identify
    /// the sender without requiring trust.
    Endpoint {
        /// Kernel-assigned endpoint identifier.
        endpoint_id: u64,
        /// Badge value (set when deriving; 0 for the root capability).
        badge: u64,
    },

    /// A process. Grants control over the referenced process (can kill, inspect).
    Process {
        /// Process identifier.
        pid: usize,
    },

    /// A hardware IRQ line. Grants permission to receive and acknowledge
    /// interrupts on the specified IRQ number.
    Irq {
        /// IRQ number (0-15 for the 8259 PIC, higher for APIC).
        irq_number: u8,
    },

    /// A shared memory region for bulk data transfer between processes (and the
    /// kernel). IPC messages carry only four machine words — far too small for
    /// block data or filesystem buffers — so the storage stack moves payloads
    /// through shared memory instead: the kernel allocates physical frames and
    /// maps them into the address spaces of both endpoints (e.g. the block
    /// server and a filesystem server). The capability names the region; a
    /// holder with the right to it can have it mapped into its address space.
    SharedMemory {
        /// Physical base address of the region (page-aligned).
        phys_base: u64,
        /// Size of the region in bytes (a whole number of pages).
        size: u64,
        /// PID that allocated/owns the region, for accounting and revocation.
        owner_pid: usize,
    },

    /// Access to a mounted filesystem. A process holding this capability can
    /// open paths under the named mount (subject to READ/WRITE rights); without
    /// it, `SYS_OPEN` is denied. This is how the capability system gates
    /// filesystem access — there is no ambient "open any file" authority.
    Filesystem {
        /// Mount identifier (index into the kernel mount table).
        mount_id: u64,
    },

    /// An open file handle. Returned by `SYS_OPEN` and consumed by the file
    /// read/write/close/readdir syscalls. Bound to the opening process and the
    /// mount it came from, so one process cannot use another's descriptors.
    FileDescriptor {
        /// The filesystem-server-side file descriptor.
        fd: u64,
        /// The mount this descriptor belongs to.
        mount_id: u64,
    },

    /// A network socket (Phase 4.5). This is the networking analogue of the
    /// filesystem capabilities above, with two roles distinguished by
    /// `socket_id`:
    ///
    /// - `socket_id == `[`SOCKET_FACTORY`] — the **network authority**: the
    ///   right to *create* sockets. `SYS_SOCKET` requires the caller to hold
    ///   this; without it, socket creation is denied. It is the "mount"-like
    ///   root capability (compare [`CapType::Filesystem`]).
    /// - any other `socket_id` — a specific open socket in the ring-3 net
    ///   server. Minted by `SYS_SOCKET` and consumed by
    ///   `bind`/`sendto`/`recvfrom`/`close` (compare [`CapType::FileDescriptor`]).
    ///   READ grants receive, WRITE grants send.
    Socket {
        /// The net-server-side socket id, or [`SOCKET_FACTORY`] for the
        /// create-sockets authority.
        socket_id: u64,
    },

    /// The **management authority** (Phase 6.3): the single, global right to drive
    /// the container management ABI — list/inspect/create/start/stop/logs and open
    /// a listener via the connection-accept shim. It is a coarse sentinel, exactly
    /// like [`SOCKET_FACTORY`] is for sockets: holding it grants *all* management
    /// operations (per-verb / per-container sub-capabilities are a future
    /// refinement). **Invariant:** this cap is minted only to the ring-3
    /// `api-server` (the node's control plane) and **never** enters a container's
    /// capability space — a container is created with an empty CSpace, so it can
    /// never manage anything. `resolve_management` checks only the presence of this
    /// variant (rights are not consulted, mirroring `resolve_factory`).
    Management,
}

/// Sentinel `socket_id` marking a [`CapType::Socket`] as the **network
/// authority** (the right to create sockets via `SYS_SOCKET`) rather than a
/// specific open socket. A real net-server socket id never reaches `u64::MAX`.
pub const SOCKET_FACTORY: u64 = u64::MAX;

// --- Capability ---

/// A single capability — a reference to a kernel object with specific rights.
///
/// Capabilities live inside CSpace slots. They are never exposed to userspace
/// directly — userspace only sees the `CapHandle` integer. The kernel resolves
/// handles to capabilities on every syscall.
#[derive(Debug, Clone)]
pub struct Capability {
    /// What type of resource this capability references.
    pub cap_type: CapType,

    /// What operations are permitted through this capability.
    pub rights: CapRights,

    /// The handle of the capability this was derived from, or None if this
    /// is a root capability (created directly by the kernel).
    /// Used for revocation: revoking a parent invalidates all descendants.
    pub parent: Option<CapHandle>,
}

// --- Capability handle ---

/// An opaque 32-bit handle that userspace uses to reference capabilities.
///
/// Encodes two fields:
/// - **Index** (bits 0-11): slot position in the CSpace (max 4096 slots)
/// - **Generation** (bits 12-31): monotonically increasing counter per slot,
///   incremented each time a slot is freed. Prevents stale-handle reuse:
///   if a handle's generation doesn't match the slot's current generation,
///   the handle is invalid (the capability it referenced was removed).
///
/// With 20-bit generations, a slot must be reused over 1 million times before
/// a stale handle could accidentally match — effectively impossible in practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapHandle(u32);

/// Maximum number of slots per CSpace (12-bit index → 4096 slots).
pub const MAX_CSPACE_SIZE: usize = 4096;

/// Number of bits used for the slot index in a CapHandle.
const INDEX_BITS: u32 = 12;

/// Bitmask for extracting the index from a CapHandle.
const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;

impl CapHandle {
    /// Create a new handle from a slot index and generation counter.
    /// Panics if index >= MAX_CSPACE_SIZE.
    pub fn new(index: usize, generation: u32) -> Self {
        assert!(index < MAX_CSPACE_SIZE, "CapHandle index {} exceeds max {}", index, MAX_CSPACE_SIZE);
        Self((generation << INDEX_BITS) | (index as u32 & INDEX_MASK))
    }

    /// Extract the slot index (0..4095).
    pub fn index(self) -> usize {
        (self.0 & INDEX_MASK) as usize
    }

    /// Extract the generation counter.
    pub fn generation(self) -> u32 {
        self.0 >> INDEX_BITS
    }

    /// The null handle (index 0, generation 0). Always invalid.
    pub const NULL: Self = Self(0);

    /// Create a handle from a raw u32 value (for syscall arguments).
    pub fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Get the raw u32 value (for returning to userspace).
    pub fn as_raw(self) -> u32 {
        self.0
    }
}

impl fmt::Display for CapHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cap(idx={}, gen={})", self.index(), self.generation())
    }
}
