//! # Embedded server binaries
//!
//! The userspace servers are compiled to flat binaries (see the `servers/`
//! workspace and the server linker script) and embedded directly in the kernel
//! image with `include_bytes!`. The kernel never reads them from disk and never
//! parses an executable format — at spawn time it simply copies these bytes into
//! ring-3 pages and jumps in (see `process::server::spawn_server`).
//!
//! ## Build dependency
//!
//! These files are produced by `cargo xtask build`/`test`, which builds the
//! `servers/` workspace and copies each flat binary to `target/servers/`
//! **before** compiling the kernel. Building the kernel directly with `cargo`
//! (bypassing xtask) requires those files to already exist.

/// The echo server: a minimal IPC echo used to validate the server framework
/// (sub-phase 3.4). Replaced/joined by the real filesystem servers in 3.5+.
pub static ECHO_SERVER: &[u8] = include_bytes!("../../../target/servers/echo-server.bin");

/// The SquashFS server: reads the read-only SquashFS root image (sub-phase 3.5).
pub static SQUASHFS_SERVER: &[u8] = include_bytes!("../../../target/servers/squashfs-server.bin");

/// The overlay server: RAM upper layer merged over a read-only lower (3.6).
pub static OVERLAY_SERVER: &[u8] = include_bytes!("../../../target/servers/overlay-server.bin");

/// The ext2 server: read-write ext2 for persistent data volumes (3.7).
pub static EXT2_SERVER: &[u8] = include_bytes!("../../../target/servers/ext2-server.bin");

/// The filesystem syscall test client: exercises the VFS syscalls from ring 3 (3.8).
pub static FSTEST_CLIENT: &[u8] = include_bytes!("../../../target/servers/fstest-client.bin");
