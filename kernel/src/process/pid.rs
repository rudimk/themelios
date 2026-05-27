//! # Process identifier type
//!
//! A simple newtype wrapper around `usize` that makes process IDs type-safe
//! and self-documenting. Process IDs are sequential integers assigned at
//! creation time, starting from 0 (the kernel process).

use core::fmt;

/// A unique process identifier.
///
/// PID 0 is always the kernel process — the special ring-0 process that
/// owns all boot-time tasks (main, idle, shell). User processes start
/// at PID 1.
///
/// ProcessIds are indices into the global process table
/// (`Vec<Option<Process>>`). When a process is destroyed, its slot becomes
/// `None` and the PID is never reused — the table only grows. This avoids
/// stale-PID confusion at the cost of (negligible) table growth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessId(pub usize);

impl ProcessId {
    /// The kernel process PID. Always 0.
    pub const KERNEL: Self = Self(0);

    /// Create a ProcessId from a raw usize.
    pub const fn new(id: usize) -> Self {
        Self(id)
    }

    /// Get the raw usize value.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PID({})", self.0)
    }
}
