//! # Filesystem layer
//!
//! ThemeliOS uses a minimal filesystem design aligned with its immutable,
//! container-focused architecture:
//!
//! ## Filesystem layout
//!
//! - **Read-only root**: The OS image is a read-only filesystem containing the
//!   kernel, init process, and system services. It's never modified at runtime.
//!   Updates replace the entire image (A/B partition swap).
//!
//! - **Ephemeral writable layer**: A RAM-backed (tmpfs-like) writable layer for
//!   container runtime state — pulled images, running container filesystems,
//!   temporary data. This is lost on reboot, which is fine because nodes are cattle.
//!
//! ## OCI image support (Phase 5)
//!
//! Container images are OCI-format tarballs containing filesystem layers.
//! The filesystem layer will handle unpacking these layers and presenting them
//! as a unified filesystem view to each container, using overlay-style layering.
//!
//! ## No persistent storage for logs
//!
//! Logs are streamed off-node in real-time via the network stack. The filesystem
//! layer does not provide persistent writable storage for logs.
//!
//! ## VFS dispatch (Phase 3)
//!
//! In the hybrid microkernel, the kernel is a **router**, not a filesystem
//! implementor. Filesystem parsing runs in ring-3 servers (SquashFS, overlay,
//! ext2). This module maintains a *mount table* mapping mount ids to those
//! servers' IPC endpoints, and provides capability-checked operations that
//! forward filesystem requests to the right server. The kernel never parses
//! filesystem data — it validates capabilities and shuffles IPC messages and
//! shared-memory buffers.
//!
//! Access is gated by capabilities: a process needs a [`CapType::Filesystem`]
//! capability to open paths under a mount, and each open file is represented by
//! a [`CapType::FileDescriptor`] capability bound to that process. There is no
//! ambient authority to touch storage.

use alloc::vec::Vec;

use crate::cap::{Capability, CapHandle, CapRights, CapType};
use crate::ipc::{self, IpcMessage};
use crate::mm::shared::SharedRegion;
use crate::process;
use crate::process::ProcessId;
use crate::sync::InterruptMutex;

// --- Filesystem protocol opcodes (mirror libthemelios::fs_proto) ---

const OP_OPEN: u64 = 1;
const OP_READ: u64 = 2;
const OP_WRITE: u64 = 3;
const OP_CLOSE: u64 = 4;
const OP_STAT: u64 = 5;
const OP_READDIR: u64 = 6;

/// High bit in a reply status word marks an encoded [`FsError`].
const STATUS_ERROR_FLAG: u64 = 1 << 63;

/// Filesystem errors — mirror `libthemelios::fs_proto::FsError` discriminants so
/// a server's encoded error round-trips through the kernel to userspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    NotFound = 1,
    NotADirectory = 2,
    IsADirectory = 3,
    ReadOnlyFs = 4,
    PermissionDenied = 5,
    NoSpace = 6,
    NoInodes = 7,
    AlreadyExists = 8,
    NotEmpty = 9,
    IoError = 10,
    Corrupt = 11,
    NameTooLong = 12,
    InvalidArgument = 13,
    ServerUnavailable = 14,
}

impl FsError {
    /// Encode as a negative-style syscall return (high bit set + discriminant),
    /// matching what userspace expects.
    pub fn as_syscall_ret(self) -> u64 {
        STATUS_ERROR_FLAG | self as u64
    }
}

/// Decode a filesystem server's reply status word into a `Result`.
fn decode_status(status: u64) -> Result<(), FsError> {
    if status & STATUS_ERROR_FLAG == 0 {
        return Ok(());
    }
    let code = status & !STATUS_ERROR_FLAG;
    Err(match code {
        1 => FsError::NotFound,
        2 => FsError::NotADirectory,
        3 => FsError::IsADirectory,
        4 => FsError::ReadOnlyFs,
        5 => FsError::PermissionDenied,
        6 => FsError::NoSpace,
        7 => FsError::NoInodes,
        8 => FsError::AlreadyExists,
        9 => FsError::NotEmpty,
        11 => FsError::Corrupt,
        12 => FsError::NameTooLong,
        14 => FsError::ServerUnavailable,
        _ => FsError::IoError,
    })
}

// --- Mount table ---

/// A mounted filesystem: the server that backs it and the shared region the
/// kernel uses to pass paths/data to and from that server.
#[derive(Clone, Copy)]
struct Mount {
    mount_id: u64,
    /// IPC endpoint of the (top-level) filesystem server for this mount.
    fs_endpoint: u64,
    /// Shared region between the kernel and that server (the server's "client"
    /// region). The kernel writes paths / reads results here while forwarding.
    client_region: SharedRegion,
}

/// The global mount table.
static MOUNTS: InterruptMutex<Vec<Mount>> = InterruptMutex::new(Vec::new());

/// Register a mounted filesystem, returning its mount id.
///
/// `fs_endpoint` is the server's request endpoint; `client_region` is the
/// kernel↔server shared buffer for paths and bulk data.
pub fn register_mount(fs_endpoint: u64, client_region: SharedRegion) -> u64 {
    let mut mounts = MOUNTS.lock();
    let mount_id = mounts.len() as u64;
    mounts.push(Mount { mount_id, fs_endpoint, client_region });
    mount_id
}

/// Look up a mount by id (returns a copy of its routing info).
fn lookup_mount(mount_id: u64) -> Option<Mount> {
    MOUNTS.lock().iter().find(|m| m.mount_id == mount_id).copied()
}

// --- Capability resolution helpers ---

/// Resolve `handle` in `pid`'s capability space to a `Filesystem` mount id,
/// requiring `need` rights. Returns `PermissionDenied` if the handle is missing,
/// the wrong type, or lacks the rights.
fn resolve_filesystem(pid: ProcessId, handle: CapHandle, need: CapRights) -> Result<u64, FsError> {
    process::with_cspace_mut(pid, |cspace| match cspace.lookup(handle) {
        Ok(cap) => match cap.cap_type {
            CapType::Filesystem { mount_id } if cap.rights.contains(need) => Ok(mount_id),
            _ => Err(FsError::PermissionDenied),
        },
        Err(_) => Err(FsError::PermissionDenied),
    })
    .unwrap_or(Err(FsError::PermissionDenied))
}

/// Resolve `handle` to a `FileDescriptor` (fd, mount_id), requiring `need` rights.
fn resolve_fd(pid: ProcessId, handle: CapHandle, need: CapRights) -> Result<(u64, u64), FsError> {
    process::with_cspace_mut(pid, |cspace| match cspace.lookup(handle) {
        Ok(cap) => match cap.cap_type {
            CapType::FileDescriptor { fd, mount_id } if cap.rights.contains(need) => {
                Ok((fd, mount_id))
            }
            _ => Err(FsError::PermissionDenied),
        },
        Err(_) => Err(FsError::PermissionDenied),
    })
    .unwrap_or(Err(FsError::PermissionDenied))
}

/// Call a filesystem server and split its reply into (status-result, word1).
fn fs_call(endpoint: u64, words: [u64; 4]) -> (Result<(), FsError>, u64) {
    let reply = ipc::ipc_call(endpoint, IpcMessage::new(words), 0);
    match reply {
        Ok(msg) => (decode_status(msg.words[0]), msg.words[1]),
        Err(_) => (Err(FsError::ServerUnavailable), 0),
    }
}

// --- VFS operations (called by the syscall layer with kernel-side buffers) ---

/// Open `path` under the filesystem named by `fs_handle`, returning a new
/// `FileDescriptor` capability handle in `pid`'s capability space.
pub fn vfs_open(pid: ProcessId, fs_handle: CapHandle, path: &[u8]) -> Result<u32, FsError> {
    let mount_id = resolve_filesystem(pid, fs_handle, CapRights::READ)?;
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;

    // Hand the path to the server through the shared region.
    // SAFETY: the kernel owns this region's frames; the call is synchronous.
    let buf = unsafe { m.client_region.as_slice_mut() };
    let n = path.len().min(buf.len());
    buf[..n].copy_from_slice(&path[..n]);

    let (status, fd) = fs_call(m.fs_endpoint, [OP_OPEN, n as u64, 0, 0]);
    status?;

    // Mint a FileDescriptor capability for the opened file.
    process::with_cspace_mut(pid, |cspace| {
        cspace.insert(Capability {
            cap_type: CapType::FileDescriptor { fd, mount_id },
            rights: CapRights::READ | CapRights::WRITE,
            parent: Some(fs_handle),
        })
    })
    .ok_or(FsError::ServerUnavailable)?
    .map(|h| h.as_raw())
    .map_err(|_| FsError::ServerUnavailable)
}

/// Read up to `buf.len()` bytes at `offset` from the file `fd_handle`.
pub fn vfs_read(pid: ProcessId, fd_handle: CapHandle, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
    let (fd, mount_id) = resolve_fd(pid, fd_handle, CapRights::READ)?;
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;
    let want = buf.len().min(m.client_region.size as usize);
    let (status, n) = fs_call(m.fs_endpoint, [OP_READ, fd, offset, want as u64]);
    status?;
    let n = (n as usize).min(want);
    // SAFETY: kernel-owned region; synchronous call completed.
    let src = unsafe { m.client_region.as_slice_mut() };
    buf[..n].copy_from_slice(&src[..n]);
    Ok(n)
}

/// Write `buf` at `offset` to the file `fd_handle`. Requires WRITE rights.
pub fn vfs_write(pid: ProcessId, fd_handle: CapHandle, offset: u64, buf: &[u8]) -> Result<usize, FsError> {
    let (fd, mount_id) = resolve_fd(pid, fd_handle, CapRights::WRITE)?;
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;
    // SAFETY: kernel-owned region; synchronous call.
    let dst = unsafe { m.client_region.as_slice_mut() };
    let n = buf.len().min(dst.len());
    dst[..n].copy_from_slice(&buf[..n]);
    let (status, written) = fs_call(m.fs_endpoint, [OP_WRITE, fd, offset, n as u64]);
    status?;
    Ok((written as usize).min(n))
}

/// Close the file `fd_handle` and revoke its capability.
pub fn vfs_close(pid: ProcessId, fd_handle: CapHandle) -> Result<(), FsError> {
    let (fd, mount_id) = resolve_fd(pid, fd_handle, CapRights::READ)?;
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;
    let (status, _) = fs_call(m.fs_endpoint, [OP_CLOSE, fd, 0, 0]);
    // Remove the descriptor capability regardless of the server's reply.
    let _ = process::with_cspace_mut(pid, |cspace| cspace.remove(fd_handle));
    status
}

/// Stat `path` under `fs_handle`, returning (size, is_dir).
pub fn vfs_stat(pid: ProcessId, fs_handle: CapHandle, path: &[u8]) -> Result<(u64, bool), FsError> {
    let mount_id = resolve_filesystem(pid, fs_handle, CapRights::READ)?;
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;
    let buf = unsafe { m.client_region.as_slice_mut() };
    let n = path.len().min(buf.len());
    buf[..n].copy_from_slice(&path[..n]);
    let reply = ipc::ipc_call(m.fs_endpoint, IpcMessage::new([OP_STAT, n as u64, 0, 0]), 0)
        .map_err(|_| FsError::ServerUnavailable)?;
    decode_status(reply.words[0])?;
    Ok((reply.words[1], reply.words[2] != 0))
}

/// List the directory `fd_handle`, copying packed entries into `out` and
/// returning the entry count.
pub fn vfs_readdir(pid: ProcessId, fd_handle: CapHandle, max: u64, out: &mut [u8]) -> Result<u64, FsError> {
    let (fd, mount_id) = resolve_fd(pid, fd_handle, CapRights::READ)?;
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;
    let (status, count) = fs_call(m.fs_endpoint, [OP_READDIR, fd, max, 0]);
    status?;
    // SAFETY: kernel-owned region; synchronous call completed.
    let src = unsafe { m.client_region.as_slice_mut() };
    let n = out.len().min(src.len());
    out[..n].copy_from_slice(&src[..n]);
    Ok(count)
}

// --- Kernel-internal operations ---
//
// These mirror the VFS operations but address a mount directly and skip the
// capability check: the kernel is trusted, and the debug shell / boot sequence
// use them to inspect and mount filesystems. Userspace must always go through
// the capability-checked `vfs_*` syscalls.

/// Open `path` on `mount_id` directly, returning the server-side fd.
pub fn kopen(mount_id: u64, path: &[u8]) -> Result<u64, FsError> {
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;
    let buf = unsafe { m.client_region.as_slice_mut() };
    let n = path.len().min(buf.len());
    buf[..n].copy_from_slice(&path[..n]);
    let (status, fd) = fs_call(m.fs_endpoint, [OP_OPEN, n as u64, 0, 0]);
    status?;
    Ok(fd)
}

/// Read [offset, offset+buf.len()) of file `fd` on `mount_id`.
pub fn kread(mount_id: u64, fd: u64, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;
    let want = buf.len().min(m.client_region.size as usize);
    let (status, n) = fs_call(m.fs_endpoint, [OP_READ, fd, offset, want as u64]);
    status?;
    let n = (n as usize).min(want);
    let src = unsafe { m.client_region.as_slice_mut() };
    buf[..n].copy_from_slice(&src[..n]);
    Ok(n)
}

/// Close file `fd` on `mount_id`.
pub fn kclose(mount_id: u64, fd: u64) -> Result<(), FsError> {
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;
    fs_call(m.fs_endpoint, [OP_CLOSE, fd, 0, 0]).0
}

/// Stat `path` on `mount_id` → (size, is_dir).
pub fn kstat(mount_id: u64, path: &[u8]) -> Result<(u64, bool), FsError> {
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;
    let buf = unsafe { m.client_region.as_slice_mut() };
    let n = path.len().min(buf.len());
    buf[..n].copy_from_slice(&path[..n]);
    let reply = ipc::ipc_call(m.fs_endpoint, IpcMessage::new([OP_STAT, n as u64, 0, 0]), 0)
        .map_err(|_| FsError::ServerUnavailable)?;
    decode_status(reply.words[0])?;
    Ok((reply.words[1], reply.words[2] != 0))
}

/// List directory `fd` on `mount_id`, packed entries into `out`, returning count.
pub fn kreaddir(mount_id: u64, fd: u64, max: u64, out: &mut [u8]) -> Result<u64, FsError> {
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;
    let (status, count) = fs_call(m.fs_endpoint, [OP_READDIR, fd, max, 0]);
    status?;
    let src = unsafe { m.client_region.as_slice_mut() };
    let n = out.len().min(src.len());
    out[..n].copy_from_slice(&src[..n]);
    Ok(count)
}

/// Create a regular file at `path` on `mount_id`, returning its fd.
pub fn kcreate(mount_id: u64, path: &[u8]) -> Result<u64, FsError> {
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;
    let buf = unsafe { m.client_region.as_slice_mut() };
    let n = path.len().min(buf.len());
    buf[..n].copy_from_slice(&path[..n]);
    let (status, fd) = fs_call(m.fs_endpoint, [/* OP_CREATE */ 7, n as u64, 0, 0]);
    status?;
    Ok(fd)
}

/// Write `data` at `offset` to file `fd` on `mount_id`. Returns bytes written.
pub fn kwrite(mount_id: u64, fd: u64, offset: u64, data: &[u8]) -> Result<usize, FsError> {
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;
    let dst = unsafe { m.client_region.as_slice_mut() };
    let n = data.len().min(dst.len());
    dst[..n].copy_from_slice(&data[..n]);
    let (status, written) = fs_call(m.fs_endpoint, [OP_WRITE, fd, offset, n as u64]);
    status?;
    Ok((written as usize).min(n))
}

/// Create a directory at `path` on `mount_id`.
pub fn kmkdir(mount_id: u64, path: &[u8]) -> Result<(), FsError> {
    let m = lookup_mount(mount_id).ok_or(FsError::ServerUnavailable)?;
    let buf = unsafe { m.client_region.as_slice_mut() };
    let n = path.len().min(buf.len());
    buf[..n].copy_from_slice(&path[..n]);
    fs_call(m.fs_endpoint, [/* OP_MKDIR */ 8, n as u64, 0, 0]).0
}

// --- Boot-time storage bring-up ---

use core::sync::atomic::{AtomicU64, Ordering};

/// Mount id of the root filesystem ("/", overlay over SquashFS), or MAX if not
/// mounted.
static ROOT_MOUNT: AtomicU64 = AtomicU64::new(u64::MAX);
/// Mount id of the data filesystem ("/data", ext2), or MAX if not mounted.
static DATA_MOUNT: AtomicU64 = AtomicU64::new(u64::MAX);

/// The root mount id, if storage has been brought up.
pub fn root_mount() -> Option<u64> {
    match ROOT_MOUNT.load(Ordering::SeqCst) {
        u64::MAX => None,
        v => Some(v),
    }
}

/// The data mount id, if a data volume is mounted.
pub fn data_mount() -> Option<u64> {
    match DATA_MOUNT.load(Ordering::SeqCst) {
        u64::MAX => None,
        v => Some(v),
    }
}

/// Bring up the full storage stack and register the root and data mounts.
///
/// Discovers the attached VirtIO disks (classifying each by its on-disk magic),
/// then for the SquashFS disk: block server → SquashFS server → overlay server,
/// registered as the root mount "/"; and for the ext2 disk: block server → ext2
/// server, registered as "/data". The mounts are recorded for the shell.
///
/// Idempotent guard: does nothing if the root mount is already set.
pub fn boot_storage() {
    use crate::drivers::block::BlockDevice;
    use crate::drivers::virtio::blk::VirtioBlk;
    use crate::drivers::{block, block_server, pci};
    use crate::mm::shared::SharedRegion;
    use crate::process::embedded;
    use crate::process::server::{spawn_server, ServerConfig};
    use crate::sched;
    use alloc::boxed::Box;

    if root_mount().is_some() {
        return;
    }

    // Classify each VirtIO block device by probing its on-disk magic.
    let mut squashfs_idx: Option<usize> = None;
    let mut ext2_idx: Option<usize> = None;
    for dev in pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID)
        .iter()
        .filter(|d| d.class == 0x01)
    {
        let blk = match VirtioBlk::init_from_pci(dev) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut sec0 = [0u8; 512];
        let mut sec2 = [0u8; 512];
        let r0 = blk.read_blocks(0, &mut sec0);
        let r2 = blk.read_blocks(2, &mut sec2);
        if r0.is_ok() && sec0[0..4] == [0x68, 0x73, 0x71, 0x73] {
            squashfs_idx = Some(block::register("virtio-blk-root", Box::new(blk)));
        } else if r2.is_ok() && sec2[56] == 0x53 && sec2[57] == 0xEF {
            ext2_idx = Some(block::register("virtio-blk-data", Box::new(blk)));
        }
        // else: a scratch/unknown disk — leave it unregistered.
    }

    // --- Root: SquashFS (lower) + overlay (upper) ---
    if let Some(idx) = squashfs_idx {
        let block_handle = block_server::start(idx);
        if let Some(sqfs_client) = SharedRegion::alloc(128 * 1024) {
            let sqfs_ep = crate::ipc::create_endpoint("root-squashfs");
            spawn_server(ServerConfig {
                name: "squashfs-server",
                binary: embedded::SQUASHFS_SERVER,
                fs_endpoint: sqfs_ep,
                block_endpoint: block_handle.endpoint,
                shared: Some(block_handle.region),
                client_shared: Some(sqfs_client),
                heap_bytes: 2 * 1024 * 1024,
                arg0: 0,
                arg1: 0,
                filesystem_mount: None,
            });
            // Overlay over the SquashFS lower layer.
            if let Some(ovl_client) = SharedRegion::alloc(128 * 1024) {
                let ovl_ep = crate::ipc::create_endpoint("root-overlay");
                spawn_server(ServerConfig {
                    name: "overlay-server",
                    binary: embedded::OVERLAY_SERVER,
                    fs_endpoint: ovl_ep,
                    block_endpoint: 0,
                    shared: Some(sqfs_client),
                    client_shared: Some(ovl_client),
                    heap_bytes: 4 * 1024 * 1024,
                    arg0: sqfs_ep,
                    arg1: 0,
                    filesystem_mount: None,
                });
                let root_id = register_mount(ovl_ep, ovl_client);
                ROOT_MOUNT.store(root_id, Ordering::SeqCst);
            }
        }
    }

    // --- Data: ext2 ---
    if let Some(idx) = ext2_idx {
        let block_handle = block_server::start(idx);
        if let Some(ext2_client) = SharedRegion::alloc(128 * 1024) {
            let ext2_ep = crate::ipc::create_endpoint("data-ext2");
            spawn_server(ServerConfig {
                name: "ext2-server",
                binary: embedded::EXT2_SERVER,
                fs_endpoint: ext2_ep,
                block_endpoint: block_handle.endpoint,
                shared: Some(block_handle.region),
                client_shared: Some(ext2_client),
                heap_bytes: 2 * 1024 * 1024,
                arg0: 0,
                arg1: 0,
                filesystem_mount: None,
            });
            let data_id = register_mount(ext2_ep, ext2_client);
            DATA_MOUNT.store(data_id, Ordering::SeqCst);
        }
    }

    // Let the freshly-spawned servers reach their request loops (parse
    // superblocks, etc.) before anyone issues filesystem requests.
    for _ in 0..2000 {
        sched::yield_now();
    }
}

/// Print the mount table and a sample of the root filesystem to serial.
pub fn print_mount_status() {
    use crate::println;
    println!();
    println!("Storage mounts:");
    match root_mount() {
        Some(id) => {
            println!("  / (mount {})  overlay over SquashFS (read-only root + RAM upper)", id);
            // Confirm the root is readable by stat-ing a known file. The
            // overlay→SquashFS→block chain may still be initialising, so retry
            // a few times, yielding to let those servers reach their loops.
            let mut readable = None;
            for _ in 0..32 {
                if let Ok((size, _)) = kstat(id, b"/version") {
                    readable = Some(size);
                    break;
                }
                for _ in 0..256 {
                    crate::sched::yield_now();
                }
            }
            match readable {
                Some(size) => println!("    /version: {} bytes (readable)", size),
                None => println!("    (root not yet responding)"),
            }
        }
        None => println!("  / : not mounted"),
    }
    match data_mount() {
        Some(id) => println!("  /data (mount {})  ext2 (read-write)", id),
        None => println!("  /data : not mounted"),
    }
}
