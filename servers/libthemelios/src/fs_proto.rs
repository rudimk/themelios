//! Filesystem server IPC protocol — shared between the kernel's VFS dispatch
//! (sub-phase 3.8) and the filesystem servers (3.5–3.7).
//!
//! A client (the kernel's VFS layer, or the overlay server talking to the
//! SquashFS server) sends a request whose first word is an opcode; the
//! remaining words and the shared memory region carry the arguments. Path
//! strings and bulk data travel through the shared region; the message words
//! carry offsets/lengths and small scalars.
//!
//! Request word 0 is always the opcode. Reply word 0 is a status: 0 = OK, or a
//! negative-encoded [`FsError`] (stored as its `u64` discriminant with the high
//! bit set) on failure. Reply word 1 carries the primary result (bytes read,
//! file handle, entry count, …).

/// Open a path. Request: `[OP_OPEN, path_offset, path_len, flags]`.
/// Reply: `[status, fd]`.
pub const OP_OPEN: u64 = 1;
/// Read from an open file. Request: `[OP_READ, fd, buf_offset, buf_len]`.
/// Reply: `[status, bytes_read]`.
pub const OP_READ: u64 = 2;
/// Write to an open file. Request: `[OP_WRITE, fd, buf_offset, buf_len]`.
/// Reply: `[status, bytes_written]`.
pub const OP_WRITE: u64 = 3;
/// Close an open file. Request: `[OP_CLOSE, fd]`. Reply: `[status]`.
pub const OP_CLOSE: u64 = 4;
/// Stat a path. Request: `[OP_STAT, path_offset, path_len, stat_offset]`.
/// Reply: `[status]` (stat struct written to the shared region).
pub const OP_STAT: u64 = 5;
/// List a directory. Request: `[OP_READDIR, fd, entries_offset, max_entries]`.
/// Reply: `[status, entry_count]`.
pub const OP_READDIR: u64 = 6;
/// Create a new regular file. Request: `[OP_CREATE, path_len]`.
/// Reply: `[status, fd]`. (Writable filesystems only.)
pub const OP_CREATE: u64 = 7;
/// Create a new directory. Request: `[OP_MKDIR, path_len]`. Reply: `[status]`.
pub const OP_MKDIR: u64 = 8;
/// Remove a file or directory. Request: `[OP_UNLINK, path_len]`. Reply: `[status]`.
pub const OP_UNLINK: u64 = 9;

/// Reply status: success.
pub const STATUS_OK: u64 = 0;

/// Filesystem error codes — mirror the kernel's `fs::FsError` discriminants.
///
/// Returned in a reply's status word (with the high bit set to distinguish an
/// error from a small success value). Kept as an explicit enum so the kernel
/// and servers share one numbering.
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    /// File or directory not found.
    NotFound = 1,
    /// A path component used as a directory is not one.
    NotADirectory = 2,
    /// Target is a directory where a file was expected.
    IsADirectory = 3,
    /// The filesystem is read-only.
    ReadOnlyFs = 4,
    /// Capability check failed.
    PermissionDenied = 5,
    /// No space left on the device.
    NoSpace = 6,
    /// No free inodes remain.
    NoInodes = 7,
    /// The target already exists.
    AlreadyExists = 8,
    /// Directory not empty (for directory removal).
    NotEmpty = 9,
    /// Underlying block I/O error.
    IoError = 10,
    /// Corrupt on-disk structure.
    Corrupt = 11,
    /// Name exceeds the maximum length.
    NameTooLong = 12,
    /// Invalid argument (bad offset, length, handle, …).
    InvalidArgument = 13,
    /// The filesystem server is unreachable.
    ServerUnavailable = 14,
}

/// High bit marker: a reply status word with this bit set encodes an `FsError`.
pub const STATUS_ERROR_FLAG: u64 = 1 << 63;

/// Encode an [`FsError`] into a reply status word.
pub fn encode_error(err: FsError) -> u64 {
    STATUS_ERROR_FLAG | err as u64
}
