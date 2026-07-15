# Storage Architecture

This document describes ThemeliOS's storage stack — the block driver, the
filesystem servers, and the capability-guarded syscalls that connect a userspace
process to a file on disk. It reflects the system as built in Phase 3.

> **Status**: Implemented in Phase 3.

## The core idea: a hybrid microkernel

Filesystem parsers are one of the most exploited pieces of code in a monolithic
kernel. SquashFS decompression, ext2 metadata walking, directory parsing — all of
it consumes untrusted bytes from a disk that an attacker may control, and all of
it historically runs with full kernel privilege. A single bug becomes a kernel
compromise.

ThemeliOS splits the storage stack across the privilege boundary:

- **The block driver stays in the kernel (ring 0).** It is thin (~500 lines),
  talks only to trusted, emulated VirtIO hardware, and exposes a single
  abstraction: read and write fixed-size blocks.
- **Every filesystem runs in userspace (ring 3)**, as a separate server process
  with its own address space and its own capabilities. SquashFS, ext2, and the
  overlay each parse untrusted on-disk data entirely outside the kernel.

A corrupt or malicious disk image can, at worst, crash the filesystem server that
parses it. It cannot touch kernel memory, cannot read another process's data, and
cannot bypass a capability check — because the code doing the parsing never had
those privileges to begin with.

```
┌─────────────────────────────────────────────────────────────┐
│                       Ring 0 (Kernel)                        │
│                                                              │
│   PCI scan ─▶ VirtIO-blk driver ─▶ BlockDevice trait         │
│                                        │                     │
│                                   Block server               │
│                                   (kernel task on an         │
│                                    IPC endpoint)             │
│                                        ▲                     │
│              VFS dispatch              │ IPC + shared memory  │
│         (routes SYS_OPEN/READ/… )      │                     │
│                   │                    │                     │
├───────────────────┼────────────────────┼─────────────────────┤
│                   │  Ring 3 (Userspace)│                     │
│                   ▼                    ▼                     │
│   ┌────────────┐   ┌────────────────┐   ┌────────────────┐   │
│   │  overlay   │──▶│    squashfs    │   │      ext2      │   │
│   │  server    │   │    server      │   │     server     │   │
│   │ (RAM upper │   │  (read-only    │   │  (read-write   │   │
│   │  + lower)  │   │   root)        │   │   data volume) │   │
│   └────────────┘   └────────────────┘   └────────────────┘   │
│      mount "/"                              mount "/data"    │
└─────────────────────────────────────────────────────────────┘
```

Everything above the block driver is a message. The kernel is a *router*, not a
filesystem implementor — it validates capabilities and forwards IPC, but never
parses a superblock or an inode.

## The ring-0 block path

### PCI enumeration

VirtIO devices on QEMU's Q35 machine present as PCI devices. At boot the kernel
scans PCI configuration space (via the `0xCF8`/`0xCFC` I/O ports on x86_64),
identifies devices by vendor and class, and reads their BAR (Base Address
Register) regions. VirtIO devices carry vendor ID `0x1AF4`; a block device has
PCI class `0x01`. The scan records every device so a driver can bind to it later.

### VirtIO transport and the virtqueue

VirtIO defines a standard transport (how to find config registers, negotiate
features, and set up queues) shared by all device types. ThemeliOS implements the
modern (VirtIO 1.0+) PCI transport: it walks the device's vendor capabilities to
locate the common/notify/ISR/device-config regions, maps them as uncached MMIO,
and runs the initialization handshake (`reset → ACKNOWLEDGE → DRIVER →
FEATURES_OK → DRIVER_OK`).

Data moves through a **split virtqueue**: a descriptor table plus an *available*
ring (driver → device: "process these buffers") and a *used* ring (device →
driver: "I finished these"). ThemeliOS polls the used ring for completion rather
than taking an interrupt — the spec permits it, and it is simpler and
deterministic. Building the transport as a shared layer means the Phase 4
VirtIO-net driver inherits it for free.

### The `BlockDevice` trait

The transport is device-type-agnostic; block semantics live behind a trait:

```rust
pub trait BlockDevice: Send + Sync {
    fn read_blocks(&self, start_lba: u64, buf: &mut [u8]) -> Result<(), BlockError>;
    fn write_blocks(&self, start_lba: u64, buf: &[u8]) -> Result<(), BlockError>;
    fn block_size(&self) -> u32;   // typically 512
    fn block_count(&self) -> u64;
    fn flush(&self) -> Result<(), BlockError>;
}
```

`VirtioBlk` is the first implementation. A block request is a three-descriptor
chain — a header (request type + sector), the data buffer, and a one-byte status.
Because heap buffers are not guaranteed to be physically contiguous (which DMA
requires), the driver copies through a physically-contiguous **bounce buffer**,
chunking large transfers at 64 KiB. The trait is the seam that lets Phase 8 add
NVMe or VirtIO-SCSI drivers with zero changes to any filesystem code.

### The block server

Ring-3 filesystem servers cannot call `BlockDevice` methods directly — they have
no access to MMIO or DMA. The **block server** bridges the gap. It is a kernel
task that listens on an IPC endpoint, receives block requests, performs the I/O
through the trait, and replies with a status:

```
Request (client → server), in the four IPC message words:
  word0 = operation   (0 = READ, 1 = WRITE, 2 = FLUSH)
  word1 = start LBA
  word2 = block count
  word3 = byte offset into the shared region

Reply (server → client):
  word0 = status      (0 = OK, 1 = ERROR)
  word1 = error code
```

Modeling the device as an IPC service (rather than a dedicated syscall) keeps the
kernel's syscall surface small: a filesystem server can touch storage only if it
holds both an endpoint capability to the block server and a shared-memory
capability for the data buffer. With neither, it cannot reach the disk at all.

One block server runs per disk. At boot, ThemeliOS starts two — one for the
SquashFS root disk and one for the ext2 data disk — each with its own endpoint,
device, and shared region, so a request on one never touches the other's state.

## Moving data: shared memory

IPC messages are four words — far too small for a 512-byte block, let alone a
128 KiB SquashFS block. Bulk data travels through a **shared memory region**
instead. The kernel allocates contiguous physical frames and maps them into both
participants' address spaces; the IPC message only names a byte offset into that
region.

A new capability type, `CapType::SharedMemory { phys_base, size, owner_pid }`,
governs these regions. The kernel reaches them through the HHDM (the
higher-half direct map of physical memory) for its own DMA; each server sees them
as ordinary user-writable, non-executable pages.

Two shared regions participate in a typical read:

- A **block region** between the block server and a filesystem server, carrying
  raw disk blocks.
- A **client region** between a filesystem server and its client (the kernel's
  VFS layer, or another server), carrying paths and file data.

The request/reply protocol serializes access to each window — a client waits for
the reply before reusing the buffer — so no locking is needed on the region
itself.

## The ring-3 server framework

Each filesystem server is a separate `no_std` Rust crate compiled to a **flat
binary** (no ELF headers — the kernel has no ELF parser, since ELF parsing is
itself attack surface). The binaries are embedded into the kernel image with
`include_bytes!()` and loaded into fresh user pages at spawn time. A shared
linker script fixes their load address.

`spawn_server()` creates a process with an isolated address space, copies the
binary into user code pages, maps stack and heap pages, maps the shared regions,
grants the configured capabilities, and starts the server in ring 3 at its entry
point. Servers link against **`libthemelios`**, a small userspace library
providing:

- Syscall wrappers (`ipc_send`, `ipc_receive`, `ipc_call`, `yield_now`, `exit`, …)
- A global heap allocator over the server's fixed heap region
- A panic handler that reports the error and exits — a server panic is contained,
  never fatal to the kernel
- The shared filesystem and block protocol types (`fs_proto`)

## The filesystem servers

All three servers speak the same request/reply protocol on their IPC endpoint.
Paths and bulk data pass through the client shared region; the message words
carry the opcode, handles, offsets, and lengths.

```
FS_OPEN     [OP_OPEN,    path_off, path_len, flags]   → [status, fd]
FS_READ     [OP_READ,    fd, buf_off, buf_len]        → [status, bytes_read]
FS_WRITE    [OP_WRITE,   fd, buf_off, buf_len]        → [status, bytes_written]
FS_CLOSE    [OP_CLOSE,   fd]                          → [status]
FS_STAT     [OP_STAT,    path_off, path_len, …]       → [status, size, is_dir]
FS_READDIR  [OP_READDIR, fd, max_entries]             → [status, entry_count]
FS_CREATE / FS_MKDIR / FS_UNLINK                      → [status, …]
```

### SquashFS server — the read-only root

SquashFS is a compressed, read-only format — the natural choice for an immutable
root image. The server reads the superblock (magic `0x73717368`), then walks
compressed metadata blocks (each an 8 KiB-max block prefixed with a length header,
inflated with `miniz_oxide`'s pure-Rust zlib), parses inodes (basic and extended
directory/file forms), lists directories, reads file data blocks, and unpacks
**fragments** — the packed tails of small files. Writes are rejected with
`ReadOnlyFs`.

### Overlay server — the ephemeral writable layer

The immutable-root model needs a place for runtime writes to go without touching
the read-only image. The overlay server provides an **overlayfs-style** merged
view: a RAM-backed *upper* layer stacked over the SquashFS *lower* layer.

- **Reads** check the upper layer first; on a miss (and no whiteout) they forward
  to the SquashFS server via IPC.
- **Writes** to a lower-layer file trigger **copy-up**: the file is read from
  SquashFS, copied into RAM, and modified there (up to 1 MiB per file, 8 MiB
  total budget).
- **Deletes** of lower-layer files write a **whiteout** marker that hides the
  name.
- **Directory listings** merge upper and lower entries, with the upper winning
  and whiteouts removed.

The upper layer is pure RAM, so it evaporates on reboot — exactly the ephemeral
semantics a cattle-not-pets node wants. This is the same layering model container
runtimes use to stack image layers, which is why Phase 5's container storage comes
largely for free.

### ext2 server — the persistent data volume

Containers need real persistent volumes. ext2 is the simplest Linux-compatible
on-disk filesystem — ext4 without the journal or extents — so it is easy to
implement correctly and readable by standard host tools. The server parses the
superblock (magic `0xEF53`) and block group descriptors, reads and writes inodes
(12 direct block pointers plus one single-indirect pointer), walks linear
directories, and allocates blocks and inodes via the on-disk bitmaps, keeping the
free counts consistent. It works with 1 KiB blocks and 256-byte inodes.

Volumes are formatted on the host with `mkfs.ext2`, never by the kernel.
Power-loss durability is out of scope for Phase 3 (no journal), which is
acceptable for the QEMU test target. After the kernel test suite writes to an
image, `e2fsck -fn` reports it clean — bitmaps, link counts, and directory
structure all consistent.

## VFS dispatch, capabilities, and syscalls

The kernel ties the servers together through a small **VFS layer** and two new
capability types:

- `CapType::Filesystem { mount_id }` — the right to open paths on a mount (READ
  and/or WRITE).
- `CapType::FileDescriptor { fd, mount_id }` — a per-process handle to an open
  file, returned by `open`.

A **mount table** maps mount IDs to filesystem-server endpoints. Phase 3 mounts
two: `/` → the overlay server, and `/data` → the ext2 server.

Six syscalls (numbers 8–13) expose storage to userspace: `SYS_OPEN`, `SYS_READ`,
`SYS_WRITE`, `SYS_CLOSE`, `SYS_STAT`, `SYS_READDIR`. Each one:

1. Checks the caller's capability (a process with no `Filesystem` capability gets
   `PermissionDenied`; a read-only capability cannot write).
2. Resolves the target mount and forwards the request to that server via IPC.
3. Copies data between the user buffer and the shared region, validating every
   user pointer page-by-page in the caller's own address space (with a transfer
   size cap) so a bad pointer returns an error instead of faulting the kernel.
4. Records the operation in the audit log (`AuditOp::FsAccess`) with the PID,
   operation, and result.

The kernel never interprets filesystem bytes. It checks a capability, copies
bounded buffers, and routes a message — nothing more.

## A read, end to end

Following `cat /version` from the shell shows every layer cooperating:

1. The shell calls `SYS_OPEN("/version")`. The kernel checks the caller's
   `Filesystem` capability for mount `/`, then sends `FS_OPEN` to the **overlay
   server**, writing the path into the client shared region.
2. The overlay finds no `/version` in its RAM upper layer and no whiteout, so it
   forwards `FS_OPEN` to the **SquashFS server**.
3. The SquashFS server resolves the inode. To read the on-disk bytes it sends a
   block request to its **block server** naming an offset in the block shared
   region.
4. The block server calls `VirtioBlk::read_blocks`, which posts a descriptor
   chain to the virtqueue and polls for completion. The blocks land in the shared
   region.
5. The SquashFS server inflates the metadata/data, fills in the file, and replies
   up the chain. The overlay returns a file descriptor; the kernel mints a
   `FileDescriptor` capability and hands the shell an `fd`.
6. `SYS_READ` repeats the forward-and-copy path, and the kernel copies the file
   bytes into the shell's buffer. The shell prints `THEMELIOS_ROOT`.

Two ring-3 servers, one kernel block server, one hardware round-trip — and not a
single byte of filesystem structure parsed inside the kernel.

## Boot sequence

At boot, after PCI and the heap are up, `boot_storage()`:

1. Probes each VirtIO block device, classifying it by on-disk magic (SquashFS vs.
   ext2 vs. an unknown scratch disk).
2. Starts a block server for the SquashFS disk and spawns the **SquashFS server**
   over it.
3. Spawns the **overlay server** with the SquashFS server as its lower layer and
   registers it as mount `/`.
4. Starts a second block server for the ext2 disk, spawns the **ext2 server**, and
   registers it as mount `/data`.
5. Prints the mount table to the serial console.

The debug shell then exposes `mount`, `ls`, `cat`, `stat`, `write`, and `mkdir`
for interactive inspection of the live stack.

## Why it matters for containers

The choices here are not just about Phase 3 — each one pays off later:

| Phase 3 building block | Phase 5+ payoff |
|------------------------|-----------------|
| Compressed read-only SquashFS root | OCI image layers are compressed read-only blobs |
| RAM overlay with copy-up + whiteouts | Exactly the model container image layers stack with |
| Per-mount `Filesystem` capabilities | Each container gets a filesystem view it cannot escape |
| `BlockDevice` trait | NVMe / VirtIO-SCSI on cloud instances, no FS changes |
| Userspace server + IPC pattern | The Linux syscall compat layer is just another server |

Running the parsers in ring 3 is the throughline: a hostile container image is
untrusted input, and the component that unpacks it should never hold kernel
privilege.
