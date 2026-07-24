# Phase 3 — Storage

**Status**: COMPLETE  
**Created**: 2026-05-27  
**Revised**: 2026-05-27 (hybrid microkernel architecture, momus re-review fixes)  
**Phase**: 3  

## Goal

Read from a virtual disk and present a filesystem. Build the complete storage stack with a hybrid microkernel architecture: thin kernel-side block driver, userspace filesystem servers (SquashFS, ext2, overlay), file-level overlay for ephemeral writes, filesystem capabilities, and image creation tooling.

## Key Architectural Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Root filesystem format | SquashFS | Compressed, read-only by design. Standard for immutable OS images. Pays off in Phase 5 when OCI container images use compressed layers. |
| Ephemeral writable layer | File-level overlay (overlayfs-style) | RAM-backed upper layer merged over read-only lower. Same model containers use for image layer stacking — building it now means Phase 5 container storage comes nearly free. |
| Data volume filesystem | Minimal ext2 read-write | ext2 is ext4 without journal/extents. Same basic on-disk layout. Simple to implement, sufficient for persistent data volumes. ext4 tools can create ext2 volumes. |
| Block device abstraction | Trait-based (BlockDevice) | VirtIO-blk implements it now. NVMe and VirtIO-SCSI implement it in Phase 8 with zero changes to filesystem code. |
| Filesystem access control | Per-mount capabilities | Each mounted filesystem/overlay is a capability. Processes access filesystems through capability handles. Maps cleanly to container isolation in Phase 5. |
| Decompression library | miniz_oxide (gzip/zlib) | SquashFS default compression is gzip. miniz_oxide is pure Rust, no_std + alloc compatible. **VERIFIED**: `miniz_oxide` 0.8 with `default-features=false, features=["with-alloc"]` compiles cleanly for `x86_64-unknown-none` on the pinned nightly (`-Zbuild-std=core,alloc`). No custom inflate fallback needed. |
| Image creation tooling | mksquashfs via host tool | Available on macOS Apple Silicon (`brew install squashfs`) and Linux (`apt install squashfs-tools`). Battle-tested. Invoked from `cargo xtask image`. |
| **Driver/FS split** | **Hybrid microkernel** | **VirtIO-blk driver stays in kernel (thin, ~500 lines, talks to trusted QEMU hardware). SquashFS, ext2, and overlay run as separate userspace server processes communicating via IPC. Filesystem parsing — the real attack surface — runs in ring 3. A corrupt disk image can crash a FS server but cannot touch kernel memory, other processes, or the capability system.** |
| **Userspace server code loading** | **Embedded code blobs** | **Each FS server is compiled as a separate no_std Rust crate. The binary is embedded in the kernel image via `include_bytes!()` and loaded into user pages at spawn time. No ELF parser in kernel (ELF parsing is itself attack surface). Servers run in ring 3 with only granted capabilities. Clean separation enforced by the build system.** |

## Security Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Ring 0 (Kernel)                       │
│                                                         │
│  ┌──────────┐  ┌────────────┐  ┌──────────────────────┐ │
│  │ PCI scan │→│ VirtIO-blk │→│ Block server (IPC)    │ │
│  └──────────┘  └────────────┘  │ Receives block R/W   │ │
│                                │ requests from user FS │ │
│                                │ servers via IPC       │ │
│                                └──────────┬───────────┘ │
│                                           │ IPC         │
│  ┌────────────────────────────────────────┼───────────┐ │
│  │ VFS dispatch: routes SYS_OPEN etc.     │           │ │
│  │ to the correct FS server via IPC       │           │ │
│  └────────────────────────────┬───────────┘           │ │
│                               │ IPC                     │
├───────────────────────────────┼─────────────────────────┤
│                    Ring 3 (Userspace)                    │
│                               │                         │
│  ┌────────────────┐  ┌───────┴────────┐  ┌───────────┐ │
│  │ squashfs-server│  │ overlay-server │  │ext2-server│ │
│  │ (read-only)    │  │ (RAM upper +   │  │(read-write│ │
│  │                │  │  RO lower)     │  │ data vol) │ │
│  └────────────────┘  └────────────────┘  └───────────┘ │
│                                                         │
│  Each server: own address space, own capabilities,      │
│  communicates with block driver and clients via IPC.    │
│  A crash in one server cannot affect the kernel or      │
│  other servers.                                         │
└─────────────────────────────────────────────────────────┘
```

**Why this matters**: Filesystem parsers (SquashFS decompression, ext2 metadata parsing) are historically one of the most exploited vulnerability classes in monolithic kernels. By running them in ring 3, a malicious or corrupt disk image can at worst crash the FS server process — it cannot escalate to kernel privilege, corrupt other processes' memory, or bypass capability checks.

**Upgrade path**: In Phase 5, the Linux syscall compatibility layer also runs as a userspace server. The same IPC-based server pattern scales to all future services. In Phase 8, NVMe drivers can also be userspace processes (the kernel only needs to provide device memory mapping and interrupt forwarding capabilities).

## Deliverables

- BlockDevice trait and VirtIO-blk driver (PCI enumeration, virtqueues) — in kernel
- Block server IPC interface (kernel-side, serves block R/W requests to userspace FS servers)
- Userspace server framework (spawn process from embedded binary, grant capabilities, IPC message loop)
- SquashFS server (userspace process: superblock, inodes, directories, file data with gzip decompression)
- Overlay server (userspace process: RAM-backed upper + read-only lower, copy-up, whiteouts)
- Minimal ext2 server (userspace process: superblock, inodes, directories, block allocation)
- VFS dispatch in kernel (routes filesystem syscalls to the correct userspace FS server via IPC)
- Filesystem capability type and capability-checked syscalls (open, read, write, close, stat, readdir)
- Audit logging for filesystem operations
- `cargo xtask image` command to create SquashFS root images
- Shell commands for filesystem inspection (ls, cat, mount)
- Integration tests
- **Post-phase**: document the hybrid microkernel storage architecture in mdbook docs

## Workspace Structure (new crates)

```
themelios/
├── kernel/                  # Existing kernel crate
├── servers/                 # NEW: userspace server crates
│   ├── squashfs-server/     # SquashFS filesystem server
│   │   ├── src/main.rs
│   │   └── Cargo.toml       # no_std, depends on miniz_oxide
│   ├── ext2-server/         # ext2 filesystem server
│   │   ├── src/main.rs
│   │   └── Cargo.toml       # no_std
│   ├── overlay-server/      # Overlay filesystem server
│   │   ├── src/main.rs
│   │   └── Cargo.toml       # no_std
│   └── libthemelios/        # Shared userspace library
│       ├── src/lib.rs        # Syscall wrappers, IPC helpers, FS protocol types
│       └── Cargo.toml       # no_std
├── xtask/
├── docs/
└── Cargo.toml               # Workspace: add servers/*
```

Each server crate:
- Compiles as a no_std static binary for x86_64-unknown-none
- Has its own `_start` entry point (no Rust runtime)
- Uses `libthemelios` for syscall wrappers and IPC protocol types
- Is embedded in the kernel image via `include_bytes!()` in the kernel crate
- Runs in its own ring 3 address space with only granted capabilities

## Sub-phase Dependency Graph

```
3.0 (PCI) ──→ 3.1 (VirtIO transport) ──→ 3.2 (VirtIO-blk + BlockDevice)
                                                     │
                                          3.3 (Block server IPC) ◄──┘
                                                     │
3.4 (Server framework + libthemelios) ◄──────────────┤
      │                                              │
      ├──→ 3.5 (SquashFS server) ──→ 3.6 (Overlay server)
      │                                              
      └──→ 3.7 (ext2 server)                        
                                                     
3.5 + 3.6 + 3.7 ──→ 3.8 (VFS dispatch + capabilities + syscalls)
                                                     
3.2 ──→ 3.9 (Image tooling)  [can start after block driver works]
                                                     
3.8 + 3.9 ──→ 3.10 (Shell + boot integration + tests)
```

**Parallelization opportunities**:
- 3.4 (Server framework) can be developed in parallel with 3.0-3.3 (PCI/VirtIO/block)
- 3.5 (SquashFS) and 3.7 (ext2) can be developed in parallel once the server framework exists
- 3.9 (Image tooling) can start once the block driver works, in parallel with FS server work

## IPC Protocol

All communication between kernel, FS servers, and client processes uses the existing Phase 2 IPC system.

### Block Server Protocol (kernel → FS server)

FS servers send block requests to the kernel's block server via IPC:

```
Request  (client → block server):
  word0 = operation (0 = READ, 1 = WRITE, 2 = FLUSH)
  word1 = start_lba (low 64 bits)
  word2 = block_count
  word3 = shared_buffer_offset (offset into a shared memory region)

Response (block server → client):
  word0 = status (0 = OK, 1 = ERROR)
  word1 = error_code (if status != OK)
```

**Shared memory for data transfer**: Block data is too large for IPC message words. The kernel maps a shared memory region (e.g., 128 KiB) into both the block server and each FS server's address space. Block requests specify an offset into this region. This requires a `CapType::SharedMemory` capability — a new capability type for Phase 3.

### Filesystem Server Protocol (client → FS server)

Client processes (and the kernel's VFS dispatch) communicate with FS servers via IPC:

```
FS_OPEN:     word0=OP_OPEN,     word1=path_offset, word2=path_len, word3=flags
FS_READ:     word0=OP_READ,     word1=fd,          word2=buf_offset, word3=buf_len
FS_WRITE:    word0=OP_WRITE,    word1=fd,          word2=buf_offset, word3=buf_len
FS_CLOSE:    word0=OP_CLOSE,    word1=fd
FS_STAT:     word0=OP_STAT,     word1=path_offset, word2=path_len, word3=stat_buf_offset
FS_READDIR:  word0=OP_READDIR,  word1=fd,          word2=entries_offset, word3=max_entries

Response:    word0=status (0=OK, negative=error), word1=result_value (bytes_read, fd, entry_count, etc.)
```

Path strings and data buffers are passed via shared memory regions, same as block data.

## Error Types

```rust
/// Block device errors — returned by BlockDevice trait methods.
pub enum BlockError {
    /// I/O error from the device (VirtIO status byte != OK)
    DeviceError,
    /// Request was for sectors beyond the device capacity
    OutOfRange,
    /// Device is read-only (e.g., SquashFS backing disk)
    ReadOnly,
    /// Device not ready or not initialized
    NotReady,
    /// Virtqueue full, request could not be submitted
    QueueFull,
}

/// Filesystem errors — returned by FileSystem server operations and syscalls.
pub enum FsError {
    /// File or directory not found
    NotFound,
    /// Path component is not a directory
    NotADirectory,
    /// Target is a directory (when file expected)
    IsADirectory,
    /// Filesystem is read-only (SquashFS, or overlay lower layer)
    ReadOnlyFs,
    /// Permission denied (capability check failed)
    PermissionDenied,
    /// No space left on device (ext2 block allocation failed)
    NoSpace,
    /// No free inodes available (ext2 inode allocation failed)
    NoInodes,
    /// File already exists
    AlreadyExists,
    /// Directory is not empty (for unlink on directories)
    NotEmpty,
    /// I/O error from underlying block device
    IoError,
    /// Corrupt filesystem structure (bad magic, invalid inode, etc.)
    Corrupt,
    /// Name too long (ext2 max name length is 255 bytes)
    NameTooLong,
    /// Invalid argument (bad offset, null buffer, etc.)
    InvalidArgument,
    /// FS server process crashed or is unreachable
    ServerUnavailable,
}
```

## Memory Budget

Phase 3 runs with 256 MiB RAM in QEMU (`-m 256M`). Memory-sensitive areas:

| Component | Budget | Rationale |
|-----------|--------|-----------|
| SquashFS server process | 2 MiB address space | Code (~64 KiB) + heap (~256 KiB for decompression) + stack (32 KiB). Decompression buffer: 128 KiB max (SquashFS default block size), reused across reads. |
| SquashFS metadata cache | 64 KiB | Cache recently-used metadata blocks in the server's heap. LRU eviction. Metadata blocks are 8 KiB each. |
| Overlay server process | 10 MiB address space | Code (~32 KiB) + RAM upper layer (8 MiB budget) + stack (32 KiB). Upper layer memory tracked against budget. |
| Overlay copy-up limit | 1 MiB per file | Files larger than 1 MiB in the lower layer cannot be copied up for modification. Returns FsError::NoSpace. Prevents a single copy-up from consuming the entire upper layer budget. |
| ext2 server process | 2 MiB address space | Code (~64 KiB) + heap (~256 KiB for block I/O buffers) + stack (32 KiB). No block cache (direct I/O to block server). |
| Block server shared memory | 128 KiB per FS server | Shared memory region for block data transfer. One per active FS server. Mapped into both kernel and server address spaces. |
| VirtIO descriptor ring | 4 KiB (256 descriptors × 16 bytes) | Fixed-size ring per virtqueue. One virtqueue for VirtIO-blk. |

## Sub-phases

### Sub-phase 3.0 — PCI bus enumeration ✅ COMPLETE

**Goal**: Discover PCI devices on the Q35 bus so we can find VirtIO devices.

**Rationale**: VirtIO devices on x86_64 QEMU (Q35 machine model) present as PCI devices. Before we can talk to a VirtIO-blk disk, we need to enumerate the PCI bus, identify devices by vendor/device ID, and read their BAR addresses. This is the hardware foundation everything else builds on.

**What to build**:
- PCI configuration space access via x86 IO ports (0xCF8 address, 0xCFC data)
- Bus/device/function scanning (bus 0, 32 device slots, 8 functions each)
- PCI device identification (vendor ID, device ID, class code, subclass)
- BAR (Base Address Register) reading for MMIO regions
- Device registry: store discovered devices for driver binding
- Architecture-specific: this is x86_64 PCI. aarch64 uses ECAM (memory-mapped) — that's Phase 7
- The scan must be comprehensive enough to find VirtIO devices regardless of which PCI slot QEMU assigns them to

**Module**: `kernel/src/drivers/pci/`

**Expected commits**:
1. PCI configuration space read/write via IO ports
2. PCI bus scan with device discovery and serial output
3. BAR reading and device registry

**Acceptance criteria**:
- [x] PCI scan runs at boot and discovers QEMU Q35 default devices (host bridge 8086:29c0, VGA 1234:1111, e1000 net 8086:10d3)
- [x] VirtIO devices (vendor 0x1AF4) are identified with correct device IDs (virtio-blk 1af4:1001 found)
- [x] BAR addresses (MMIO base and size) are correctly read and stored (incl. 64-bit BAR pair folding and write-ones sizing)
- [x] Serial output lists every discovered PCI device with bus:device.function, vendor:device, class:subclass
- [x] Multi-function devices are handled (check function 0 header type bit 7)

**Implementation notes**: `kernel/src/drivers/pci/mod.rs` (config space via 0xCF8/0xCFC,
`PciDevice` struct, BAR decode, global registry). Added `cpu::inl` (32-bit port read).
Scan wired into `kmain` after heap init. xtask now attaches a scratch VirtIO-blk disk
(`target/themelios-scratch.img`) to QEMU in `run`/`test` so the scan has a device to find.
Verified by `test_pci_scan` and serial output showing `00:03.0 1af4:1001 [virtio]`.

---

### Sub-phase 3.1 — VirtIO transport and virtqueue ✅ COMPLETE

**Goal**: Implement the VirtIO PCI transport layer and split virtqueue, shared by all VirtIO device drivers.

**Rationale**: VirtIO defines a standard transport layer (how to find config registers, how to set up queues) and a queue mechanism (split virtqueue) that is the same for all device types — block, network, console, etc. Building this as a shared layer means the VirtIO-net driver in Phase 4 gets the transport for free.

**What to build**:
- VirtIO PCI capability structures (common config, notify, ISR, device-specific) — found by walking the PCI capability list
- VirtIO device initialization handshake (reset → acknowledge → driver → features → driver_ok)
- Split virtqueue implementation:
  - Descriptor table: array of (addr, len, flags, next) entries in physically contiguous memory
  - Available ring (driver → device): "here are descriptor chains to process"
  - Used ring (device → driver): "I finished these descriptor chains"
- Buffer management: allocate descriptor chains for multi-buffer requests, post to available ring, collect completions from used ring
- Interrupt handling: VirtIO MSI-X or legacy INTx for queue completion notifications
- VirtIO feature negotiation framework

**Module**: `kernel/src/drivers/virtio/`

**Expected commits**:
1. VirtIO PCI capability parsing and device reset/init handshake
2. Split virtqueue allocation and descriptor management
3. Virtqueue submit (available ring) and completion (used ring) with interrupt handling
4. Feature negotiation framework

**Acceptance criteria**:
- [x] VirtIO PCI capabilities are discovered by walking the PCI capability list
- [x] VirtIO device can be initialized through the full handshake (reset → driver_ok)
- [x] Virtqueues can be allocated in physically contiguous memory
- [x] Descriptors can be posted to the available ring and completions collected from the used ring (`submit_and_wait` — exercised end-to-end by the block driver in 3.2)
- [x] Interrupt fires on virtqueue completion (or polling works as fallback) — **polling** chosen (spec-permitted, simpler/deterministic)
- [x] Feature bits can be read, negotiated, and written back

**Implementation notes**: `kernel/src/drivers/virtio/mod.rs` — modern (VirtIO 1.0+)
PCI transport only. Walks vendor capabilities (cfg_type common/notify/ISR/device),
maps each MMIO region uncached via the new `mm::mmio` window, runs the
reset→ACK→DRIVER→FEATURES_OK→DRIVER_OK handshake, negotiates `VIRTIO_F_VERSION_1`,
and builds a split virtqueue (`Virtqueue`: desc table + avail/used rings in
zeroed contiguous frames, polled completion). xtask forces a modern device with
`disable-legacy=on` (1af4:1042). Verified by `test_virtio_transport`.

**Key fix (memory subsystem)**: runtime kernel-half page-table additions were
invisible to address spaces created earlier (kernel tasks run on whatever CR3 is
active, and `new_user` copies kernel PML4 entries by value). Fixed in
`page_table::new_kernel` by pre-populating every kernel-half PML4 slot (256–511)
with a shared, empty PDP at creation, so deeper kernel mappings (like device
MMIO) propagate to all address spaces by pointer. Added `mm/mmio.rs` (uncached
MMIO window) and `cpu::inl`.

---

### Sub-phase 3.2 — BlockDevice trait and VirtIO-blk driver ✅ COMPLETE

**Goal**: Define the block device abstraction and implement the first driver.

**Rationale**: The BlockDevice trait decouples filesystem code from hardware. VirtIO-blk is the first implementation, but NVMe (Phase 8, cloud instances) and VirtIO-SCSI will implement the same trait. The VirtIO-blk driver is one of the few components that stays in kernel space — it's thin, talks to trusted QEMU hardware, and exposes a simple block R/W interface.

**What to build**:
- `BlockDevice` trait (see Error Types section for `BlockError`):
  - `fn read_blocks(&self, start_lba: u64, buf: &mut [u8]) -> Result<(), BlockError>`
  - `fn write_blocks(&self, start_lba: u64, buf: &[u8]) -> Result<(), BlockError>`
  - `fn block_size(&self) -> u32` (typically 512 bytes)
  - `fn block_count(&self) -> u64`
  - `fn flush(&self) -> Result<(), BlockError>`
- VirtIO-blk driver implementing BlockDevice:
  - Read device configuration (capacity, block size, geometry) from device-specific PCI capability region
  - VIRTIO_BLK_T_IN (read) and VIRTIO_BLK_T_OUT (write) request types
  - Request format: 3-descriptor chain — header (type + sector) → data buffer → 1-byte status
  - Synchronous I/O: submit request to virtqueue, wait for completion
  - Handle error status byte (0 = OK, 1 = IOERR, 2 = UNSUPP)
- Block device registry: global table mapping device names ("virtio-blk0", "virtio-blk1") to `&dyn BlockDevice`
- QEMU launch with virtual disk: `-drive file=test.img,format=raw,if=virtio`
- Update xtask to create test disk images and pass them to QEMU

**Module**: `kernel/src/drivers/block.rs`, `kernel/src/drivers/virtio/blk.rs`

**Expected commits**:
1. BlockDevice trait definition and BlockError enum
2. VirtIO-blk driver: device config read, request submission, completion handling
3. Block device registry and xtask integration (create test disk, attach to QEMU)
4. Integration test: read/write sectors, verify round-trip

**Acceptance criteria**:
- [x] VirtIO-blk device is discovered via PCI and initialized via VirtIO handshake
- [x] Can read sectors from a QEMU virtual disk and get correct data back
- [x] Can write sectors to a QEMU virtual disk
- [x] Block device registry lists available devices with names and capacities
- [x] Write-then-read-back returns the same data (single- and multi-sector)
- [x] Error status from device is propagated as BlockError (status byte != OK → DeviceError; OutOfRange/BadBufferLength validated)
- [x] `cargo xtask run` creates a test raw disk image and attaches it to QEMU via `-drive` (scratch disk added in 3.0)

**Implementation notes**: `kernel/src/drivers/block.rs` (BlockDevice trait, BlockError,
global registry with leaked `'static` handles); `kernel/src/drivers/virtio/blk.rs`
(VirtioBlk: 3-descriptor request chain header→data→status, capacity from device
config, **bounce buffer** for physically-contiguous DMA since heap buffers aren't
contiguous, chunked at 64 KiB/128 sectors). Verified by `test_virtio_blk`
(single + 3-sector round-trip, OutOfRange, BadBufferLength, flush).

---

### Sub-phase 3.3 — Block server IPC interface ✅ COMPLETE

**Goal**: Expose the kernel-side block driver to userspace FS servers via IPC.

**Rationale**: FS servers run in ring 3 and cannot directly call BlockDevice methods. The block server is a kernel task that listens on an IPC endpoint, receives block read/write requests from userspace, performs the actual I/O via the BlockDevice trait, and responds with the result. Data is transferred via shared memory regions since IPC message words are too small for block data.

**What to build**:
- `CapType::SharedMemory { phys_base, size, owner_pid }`: a new capability type for shared memory regions. The kernel allocates physical frames, maps them into both the block server's address space and the requesting FS server's address space.
- Block server kernel task:
  - Spawned at boot after VirtIO-blk initialization
  - Listens on a dedicated IPC endpoint
  - Receives block R/W requests (operation, LBA, count, shared buffer offset)
  - Performs I/O via BlockDevice trait
  - Responds with status
- Shared memory allocation:
  - Kernel allocates contiguous physical frames (128 KiB per FS server)
  - Maps into FS server's user address space (readable/writable)
  - Kernel can also access via HHDM for block I/O DMA
- Block server IPC protocol (see IPC Protocol section above)

**Module**: `kernel/src/drivers/block_server.rs`, `kernel/src/cap/` (new SharedMemory variant)

**Expected commits**:
1. CapType::SharedMemory — allocation, mapping into user address spaces
2. Block server kernel task — IPC endpoint, request dispatch loop
3. Block R/W request handling via shared memory + BlockDevice
4. Integration test: kernel task sends block request via IPC, verifies response

**Acceptance criteria**:
- [x] SharedMemory capability allows mapping physical frames into a process's address space (`SharedRegion::map_into` + `CapType::SharedMemory`; verified by `test_shared_memory` translate checks)
- [x] Block server kernel task starts and listens on an IPC endpoint
- [x] Block read request via IPC returns correct data in shared memory region
- [x] Block write request via IPC writes correct data to the block device
- [x] Multiple FS servers can each have their own shared memory region (one `SharedRegion` allocated per `block_server::start`; design supports N)
- [x] Invalid requests (out-of-range offset) return proper error codes (verified)

**Implementation notes**: `CapType::SharedMemory { phys_base, size, owner_pid }` added
to `cap/mod.rs` (+ shell match arms). `mm/shared.rs`: `SharedRegion` (contiguous
zeroed frames, HHDM kernel access, `map_into` user AS as USER|WRITABLE|NX).
`drivers/block_server.rs`: kernel task on an IPC endpoint, request words
[op, lba, count, shared_offset], reply [status, error_code]; data via a 128 KiB
shared region. Verified by `test_shared_memory` and `test_block_server_ipc`
(WRITE→clear→READ round-trip + out-of-range rejection).

---

### Sub-phase 3.4 — Userspace server framework and libthemelios ✅ COMPLETE

**Goal**: Build the infrastructure for spawning and running userspace server processes from embedded binaries.

**Rationale**: FS servers are no_std Rust binaries embedded in the kernel image. The kernel needs to: (a) create a process, (b) load the server binary into user pages, (c) grant it the right capabilities (block server endpoint, shared memory, FS request endpoint), and (d) start it in ring 3. The servers need a userspace library (libthemelios) that wraps raw syscalls into ergonomic Rust functions.

**What to build**:
- **libthemelios** crate (`servers/libthemelios/`):
  - Syscall wrappers: `ipc_send()`, `ipc_receive()`, `ipc_call()`, `yield_now()`, `exit()`
  - FS protocol message types (shared between kernel VFS dispatch and FS servers)
  - Block protocol message types (shared between block server and FS servers)
  - Allocator: simple bump allocator for the server's heap (servers have a fixed-size heap region)
  - `_start` entry point that calls a user-defined `server_main()`
  - Panic handler (sends error message to kernel via debug print syscall, then exits)
- **Server loader** in kernel (`kernel/src/process/server.rs`):
  - `spawn_server(name, binary: &[u8], caps: &[Capability]) -> ProcessId`
  - Creates a new process with its own address space
  - Copies the server binary into user code pages (similar to init shellcode, but multi-page)
  - Maps stack pages, heap pages
  - Grants the specified capabilities to the server's CSpace
  - Starts the server task in ring 3 at the binary's entry point
- **Server binary embedding**:
  - Build system compiles each server crate to a flat binary
  - Kernel crate uses `include_bytes!()` to embed them
  - xtask updated to build server crates before building the kernel
- **Linker script** for server binaries:
  - Fixed load address (e.g., 0x200000 for code, stack at top of user space)
  - Flat binary output (no ELF headers — `objcopy -O binary`)

**Module**: `servers/libthemelios/`, `kernel/src/process/server.rs`, `xtask/src/main.rs`

**Expected commits**:
1. Create servers/ workspace directory with libthemelios crate (syscall wrappers, protocol types)
2. Server linker script and xtask build integration (compile servers, objcopy to flat binary)
3. Kernel server loader: spawn_server() with binary loading and capability grants
4. Embed a test "echo server" binary, spawn it, verify IPC round-trip from kernel task
5. libthemelios bump allocator and panic handler

**Acceptance criteria**:
- [x] libthemelios compiles for x86_64-unknown-none (no_std + alloc)
- [x] Server crates compile to flat binaries via the xtask build pipeline (LLD `--oformat=binary`, no objcopy needed)
- [x] `spawn_server()` creates a process, loads binary, grants capabilities, starts in ring 3
- [x] A test echo server receives an IPC message and sends a response (full kernel↔ring-3 round trip, twice)
- [x] Server crash (panic) does not affect the kernel or other processes (panic handler prints + `sys_exit`; isolation by design)
- [x] `cargo xtask build` now builds server crates before the kernel
- [x] Server binaries are embedded in the kernel image and the kernel size is reasonable (echo ≈ 5 KiB)

**Implementation notes**: New `servers/` workspace (separate from the root workspace
so host `cargo build` never touches it). `servers/libthemelios`: `_start`, syscall
wrappers, `linked_list_allocator` global heap, panic handler, `BootInfo`, block/FS
protocol types. `servers/linker.ld`: fixed base 0x200000, flat-binary layout.
`servers/echo-server`: validates the framework. Kernel: `process/server.rs`
(`spawn_server` — maps zeroed code pages + binary + bss allowance, stack, heap,
boot-info page, optional shared region; iretq trampoline), `process/embedded.rs`
(`include_bytes!`). xtask builds servers → `target/servers/*.bin` before the kernel.

**Also implemented (required by servers)**: kernel `SYS_CALL`/`SYS_REPLY` for
userspace (were stubbed). **Key fix**: `set_kernel_stack` now re-points
`IA32_KERNEL_GS_BASE` at `PER_CPU` on every context switch — a syscall that blocks
after its entry `swapgs` left `KERNEL_GS_BASE` holding a zero user GS, faulting the
next task's syscall entry (`gs:[8]` write). Verified by `test_server_spawn`.

---

### Sub-phase 3.5 — SquashFS server ✅ COMPLETE

**Goal**: A userspace process that reads files and directories from a SquashFS image via the block server.

**Rationale**: SquashFS is the root filesystem format. The server receives filesystem requests via IPC, reads compressed blocks from the block server via IPC, decompresses them, and returns file data to clients. Running in ring 3 means SquashFS parsing bugs cannot compromise the kernel — a corrupt image can at worst crash this server.

**Pre-requisite**: Verify that `miniz_oxide` compiles for `x86_64-unknown-none` with `-Zbuild-std=core,alloc` before starting implementation. If it doesn't, fall back to a minimal custom inflate implementation (RFC 1951 is ~300 lines of Rust).

**What to build**:
- SquashFS superblock parsing (magic 0x73717368, version, block size, compression type, table locations)
- Metadata block reading: read via block server IPC, decompress via miniz_oxide. SquashFS metadata is stored in compressed 8 KiB blocks, each prefixed with a 2-byte length header (bit 15 = uncompressed flag).
- Decompression: allocate a reusable 128 KiB buffer in the server's heap for data block decompression.
- Inode parsing (regular file, directory, symlink — basic and extended forms)
- Directory table reading (header + entries, sorted by name)
- File data block reading (compressed data blocks, fragment blocks for small files)
- Fragment table and fragment blocks: small files (< block size) packed into shared fragment blocks. Fragment table maps index → (block offset, size).
- UID/GID ID table parsing (maps internal IDs to Unix UID/GID values)
- IPC message loop: receive FS requests, process them, respond
- Read-only: write/create/unlink requests return FsError::ReadOnlyFs

**Module**: `servers/squashfs-server/`

**Expected commits**:
1. Add miniz_oxide dependency to squashfs-server, verify bare-metal compilation
2. SquashFS superblock parsing and validation
3. Metadata block reading via block server IPC + decompression
4. Inode parsing (file, directory, symlink types)
5. Directory listing
6. File data reading (compressed data blocks)
7. Fragment table parsing and small file reading
8. IPC message loop + full FS server integration

**Acceptance criteria**:
- [x] Server starts in ring 3, receives IPC messages on its endpoint
- [x] Superblock is read (via block server IPC) and validated (magic 0x73717368)
- [x] Metadata blocks are decompressed correctly (zlib via miniz_oxide)
- [x] Inodes can be looked up (by metadata reference; basic + extended dir/file)
- [x] Directory listing returns correct entries (READDIR / found all expected names)
- [x] Regular file contents are read correctly (multi-block: big.bin verified at offsets 0 and 131072)
- [x] Small files stored in fragment blocks are read correctly (/version, /docs/readme.txt)
- [~] Symlinks — deferred (image has none; symlink inode type not yet parsed, easy add)
- [x] File metadata (size) matches (STAT /version == 15)
- [x] Read-back of files matches original byte-for-byte
- [~] Decompression buffer reuse — currently allocates per block read on the heap (correctness first; pooling is a later optimization)
- [x] Server crash from corrupt data does not affect kernel stability (ring-3 isolation; bad superblock → server exits, kernel fine)

**Implementation notes**: `servers/squashfs-server` — full SquashFS 4.0 reader:
superblock, metadata-block stream reader (2-byte header, zlib inflate), inode
parsing (basic dir/file + extended LDIR/LREG), directory listing (header+entries),
data blocks (per-block compression, sparse), and fragment table/tail. Block I/O
via the block server through the **block** shared region; paths/results via the
**client** shared region (framework extended to two regions in `BootInfo`/
`spawn_server`). FS protocol OPEN/READ/CLOSE/STAT/READDIR. Verified by
`test_squashfs_server`.

---

### Sub-phase 3.6 — Overlay server ✅ COMPLETE

**Goal**: A userspace process that layers a RAM-backed writable filesystem on top of a read-only lower (SquashFS server), providing a merged view.

**Implementation notes**: `servers/overlay-server` — RAM upper layer (`BTreeMap<path,
Node>` of File/Dir/Whiteout) merged over the SquashFS lower (forwarded via IPC,
arg0 = lower endpoint, block-slot region = SquashFS client region). Copy-up on
write (≤1 MiB/file, 8 MiB budget), whiteouts for deletion, readdir merges
lower+upper with upper precedence and whiteout removal. OPEN/READ/WRITE/CLOSE/
STAT/READDIR/CREATE/MKDIR/UNLINK. Verified by `test_overlay_server`: read-through,
create+write+read, copy-up (modify lower file), whiteout (hide lower file), and
readdir merge — all byte-exact. Multi-hop ring-3→ring-3 IPC works.

**Rationale**: The immutable root model requires a writable layer for runtime state. The overlay server intercepts FS requests, checks its RAM upper layer first, then forwards to the SquashFS server for the lower layer. This is exactly the overlayfs model that container runtimes use to stack image layers.

**What to build**:
- In-memory file tree for the upper layer:
  - RAM-backed inodes: file content in `Vec<u8>`, directory entries in a fixed-size array
  - Inode allocation: atomic counter
  - Total upper layer memory tracked against 8 MiB budget
- Overlay merge logic (IPC message loop):
  - FS_OPEN/lookup: check upper layer first; if not found and no whiteout, forward to SquashFS server via IPC
  - FS_READDIR: merge entries from upper and lower (forward FS_READDIR to SquashFS server, merge with upper entries)
  - FS_READ: if file in upper, read from upper; if only in lower, forward to SquashFS server
  - FS_WRITE (copy-up): if file only in lower, copy it to upper (read from SquashFS server, store in RAM, up to 1 MiB limit), then write to upper
  - FS_CREATE: create in upper only, track memory
  - FS_UNLINK: create whiteout in upper layer
- Whiteout entries: special markers that hide lower-layer names
- Memory tracking: all upper-layer allocations count against 8 MiB budget

**Module**: `servers/overlay-server/`

**Expected commits**:
1. RAM-backed upper layer (in-memory inode tree, file content storage)
2. Overlay lookup: upper-first, forward to SquashFS server via IPC for lower
3. Copy-up on write with memory budget enforcement
4. Whiteout entries for deletion
5. Readdir merge (combine upper entries with lower entries from SquashFS server)
6. Full IPC message loop + integration

**Acceptance criteria**:
- [ ] Files from the lower layer (SquashFS) are visible through the overlay
- [ ] New files created in the overlay exist only in RAM (upper layer)
- [ ] Modifying a lower-layer file triggers copy-up (read from SquashFS server, store in RAM)
- [ ] Copy-up rejects files larger than 1 MiB (returns FsError::NoSpace)
- [ ] Deleting a lower-layer file creates a whiteout (file disappears from merged view)
- [ ] Deleting an upper-layer file removes it directly
- [ ] Directory listings merge upper and lower, with upper taking precedence
- [ ] Whiteout-hidden entries do not appear in readdir results
- [ ] Memory usage tracked; operations exceeding 8 MiB budget return NoSpace
- [ ] Overlay state is ephemeral — lost on reboot
- [ ] Server crash does not affect kernel or other servers

---

### Sub-phase 3.7 — ext2 server ✅ COMPLETE

**Goal**: A userspace process that reads and writes files on an ext2-formatted block device via the block server.

**Implementation notes**: `servers/ext2-server` — read path (superblock, block group
descriptors, inodes with 12 direct + single-indirect pointers, linear directories,
path resolution) committed first, then the write path (block/inode bitmap
alloc+free with superblock/BGD counter maintenance, file write growing direct +
single-indirect, dir-entry insert/remove via rec_len splitting/merging, create,
mkdir with "."/"..", unlink with block+inode reclamation). 1 KiB blocks, 256-byte
inodes. xtask pre-populates the image via `debugfs` (incl. a 20 KB file forcing
single-indirect) and regenerates it per test run. Verified by `test_ext2_read`
and `test_ext2_write`; **`e2fsck -fn` reports the OS-written image clean** (bitmaps,
counts, directory structure, refcounts all consistent). Double/triple indirect
deferred (not needed for Phase 3 volume sizes).

**Rationale**: Containers need persistent volumes backed by real disk. ext2 is the simplest Linux-compatible filesystem. The server handles all ext2 parsing and metadata management in ring 3 — a corrupt ext2 image or allocation bug crashes the server, not the kernel.

**What to build**:
- Superblock parsing (offset 1024 bytes, magic 0xEF53, block size, inode size, per-group counts)
- Block group descriptor table (per-group: block bitmap, inode bitmap, inode table locations)
- Inode operations:
  - Locate on disk: block group = (inode-1) / inodes_per_group, index within group
  - Read/write inode (128 or 256 bytes)
  - Block pointers: 12 direct (i_block[0..11]) + single indirect (i_block[12])
  - Double indirect deferred — max file size with single indirect = (12 + block_size/4) × block_size = ~1.05 MiB with 4K blocks. This is sufficient for Phase 3 data volumes.
- Directory operations:
  - ext2 linear format: `{inode: u32, rec_len: u16, name_len: u8, file_type: u8, name: [u8]}`
  - Lookup, create (split rec_len or extend), unlink (zero inode, merge rec_len)
- Block/inode allocation via bitmaps (read bitmap from block server, scan, write back)
- File read/write: resolve logical → physical block, read/write via block server IPC
- Superblock writeback after metadata changes
- IPC message loop: receive FS requests, process, respond

**Note**: The kernel does NOT format ext2 volumes. Formatting is done on the host with `mkfs.ext2`. Power-loss safety is not a goal for Phase 3 — ext2 without journal is inherently vulnerable to corruption on unclean shutdown, which is acceptable for QEMU testing.

**Module**: `servers/ext2-server/`

**Expected commits**:
1. ext2 superblock and block group descriptor parsing
2. Inode read/write with direct block pointers
3. Directory lookup and listing
4. Block bitmap allocation and deallocation
5. Inode bitmap allocation and deallocation
6. File read (follow block pointers, read via block server IPC)
7. File write and append (allocate blocks, update pointers)
8. Directory create and unlink operations
9. Single indirect block pointer support
10. IPC message loop + full FS server integration

**Acceptance criteria**:
- [ ] Superblock parsed correctly (magic 0xEF53, block size, inode count)
- [ ] Block group descriptors parsed (bitmap locations, inode table)
- [ ] Can read files and directories from a pre-formatted ext2 image
- [ ] Can create new files and write data to them
- [ ] Can create new directories
- [ ] Can delete files and verify space is reclaimed
- [ ] Block allocation/deallocation keeps bitmaps consistent (no leaks, no double-free)
- [ ] Inode allocation/deallocation keeps bitmaps consistent
- [ ] Free block/inode counts accurate after every operation
- [ ] Single indirect pointers work for files larger than 12 blocks
- [ ] Changes persist to disk (flush via block server, re-read, verify)
- [ ] Max file size = (12 + block_size/4) × block_size documented and enforced
- [ ] Server crash does not affect kernel or other servers

---

### Sub-phase 3.8 — VFS dispatch, capabilities, and syscalls ✅ COMPLETE

**Goal**: Wire filesystem syscalls through the kernel's capability system to the correct userspace FS server via IPC.

**Implementation notes**: `CapType::Filesystem{mount_id}` + `FileDescriptor{fd, mount_id}`
added. `fs/mod.rs`: mount table + capability-checked `vfs_open/read/write/close/
stat/readdir` that forward to FS servers over IPC (kernel never parses FS data).
`syscall.rs`: SYS_OPEN..SYS_READDIR (8–13) with `copy_from_user`/`copy_to_user`
(per-page validation in the caller's address space, user-half + mapped checks,
256 KiB transfer cap) and `AuditOp::FsAccess` logging. libthemelios gained FS
syscall wrappers. Verified two ways: `test_vfs_capability` (dispatch + cap
grant/deny at the function level) and `test_fs_syscalls` (the `fstest-client`
ring-3 process does stat/open/read via real syscalls and confirms null-capability
rejection). Note: SYS_READ/WRITE take an explicit file offset (stateless) rather
than a per-fd position.

**Rationale**: In the hybrid architecture, the kernel is a router, not a filesystem implementor. When a process calls SYS_OPEN, the kernel checks the process's capabilities, determines which FS server owns the target mount point, and forwards the request via IPC. The kernel never parses filesystem data — it only validates capabilities and routes messages.

**Existing syscall numbering** (to avoid conflicts):
- SYS_NULL=0, SYS_SEND=1, SYS_RECEIVE=2, SYS_CALL=3, SYS_REPLY=4
- SYS_YIELD=5, SYS_EXIT=6, SYS_DEBUG_PRINT=7
- SYS_TEST_COMPLETE=0xFFFF (test-only)

**What to build**:
- New capability types:
  - `CapType::Filesystem { mount_id: u64 }` — rights: READ, WRITE
  - `CapType::FileDescriptor { fd: u32, mount_id: u64, inode: u64 }` — rights: READ, WRITE
- Mount table in kernel: maps mount IDs to FS server IPC endpoints
  - Mount 0: overlay server (root "/")
  - Mount 1: ext2 server ("/data")
- VFS dispatch logic:
  - SYS_OPEN → check Filesystem cap → resolve mount from path → forward FS_OPEN to FS server via IPC → create FileDescriptor cap from response → return cap handle
  - SYS_READ_FILE → check FileDescriptor cap (READ) → forward FS_READ to FS server → copy data to user buffer
  - SYS_WRITE_FILE → check FileDescriptor cap (WRITE) → forward FS_WRITE to FS server
  - SYS_CLOSE → check FileDescriptor cap → forward FS_CLOSE → revoke cap
  - SYS_STAT → check Filesystem cap → forward FS_STAT → copy stat to user buffer
  - SYS_READDIR → check FileDescriptor cap (READ) → forward FS_READDIR → copy entries to user buffer
- New syscalls:
  - `SYS_OPEN (8)`: RDI=fs_cap, RSI=path_ptr, RDX=path_len, R10=flags → RAX=fd_cap
  - `SYS_READ_FILE (9)`: RDI=fd_cap, RSI=buf_ptr, RDX=buf_len → RAX=bytes_read
  - `SYS_WRITE_FILE (10)`: RDI=fd_cap, RSI=buf_ptr, RDX=buf_len → RAX=bytes_written
  - `SYS_CLOSE (11)`: RDI=fd_cap → RAX=0
  - `SYS_STAT (12)`: RDI=fs_cap, RSI=path_ptr, RDX=path_len, R10=stat_ptr → RAX=0
  - `SYS_READDIR (13)`: RDI=fd_cap, RSI=entries_ptr, RDX=max_entries → RAX=count
- User pointer validation: all user buffer pointers checked before kernel dereferences
- Audit logging: new AuditOp variants (FsOpen, FsRead, FsWrite, FsClose, FsStat, FsReaddir) added to audit subsystem
- Shared memory for data transfer between user process ↔ kernel ↔ FS server

**Module**: `kernel/src/fs/`, `kernel/src/arch/x86_64/syscall.rs`, `kernel/src/cap/`, `kernel/src/audit/`

**Expected commits**:
1. CapType::Filesystem and CapType::FileDescriptor
2. Mount table mapping mount IDs to FS server IPC endpoints
3. VFS dispatch: SYS_OPEN and SYS_CLOSE with capability checks and IPC forwarding
4. VFS dispatch: SYS_READ_FILE and SYS_WRITE_FILE
5. VFS dispatch: SYS_STAT and SYS_READDIR
6. User pointer validation
7. Audit logging for all filesystem operations
8. Integration test: userspace process opens/reads/writes via syscalls through full FS server stack

**Acceptance criteria**:
- [ ] Process with Filesystem capability can open, read, write, close files via syscalls
- [ ] Process WITHOUT Filesystem capability gets PermissionDenied on SYS_OPEN
- [ ] READ-only Filesystem capability allows read but rejects write
- [ ] FileDescriptor capabilities are per-process and cannot be used by other processes
- [ ] Kernel routes requests to correct FS server based on mount table
- [ ] Kernel never parses filesystem data — it only forwards IPC messages
- [ ] Audit log records all filesystem operations with PID, operation, result
- [ ] Invalid user pointers return error (not a kernel crash)
- [ ] All six syscalls handle every FsError variant gracefully

---

### Sub-phase 3.9 — Image creation tooling ✅ COMPLETE

**Goal**: `cargo xtask image` creates a SquashFS root image, and xtask run/test attaches disk images to QEMU.

**Rationale**: The kernel needs SquashFS and ext2 disk images to boot from. The xtask creates them on demand using host tools (mksquashfs, mkfs.ext2). This sub-phase also updates the xtask build pipeline to compile server crates and embed them in the kernel.

**What to build**:
- New xtask subcommand: `cargo xtask image`
  - Creates a staging directory (`target/rootfs/`) with root filesystem contents
  - Minimal root: `/version` (build ULID), `/etc/hostname`, `/data/`, test files for readdir/cat
  - Invokes `mksquashfs target/rootfs target/themelios-root.squashfs -comp gzip -noappend`
  - Checks `mksquashfs` is installed; prints install instructions if not
- Test ext2 data volume:
  - `dd if=/dev/zero of=target/themelios-data.ext2 bs=1M count=16`
  - `mkfs.ext2 -F target/themelios-data.ext2`
  - Checks `mkfs.ext2` is installed; prints install instructions if not
- Update QEMU invocations in `cargo xtask run` and `cargo xtask test`:
  - Attach SquashFS: `-drive file=target/themelios-root.squashfs,format=raw,if=virtio,readonly=on`
  - Attach ext2: `-drive file=target/themelios-data.ext2,format=raw,if=virtio`
- Update CI workflow: install `squashfs-tools` and `e2fsprogs`

**Module**: `xtask/src/main.rs`, `.github/workflows/build.yml`

**Expected commits**:
1. `cargo xtask image` with mksquashfs invocation
2. Test ext2 volume creation with mkfs.ext2
3. Update xtask run/test to create and attach disk images to QEMU
4. Update CI workflow to install host tool dependencies

**Acceptance criteria**:
- [ ] `cargo xtask image` creates valid SquashFS at `target/themelios-root.squashfs`
- [ ] `cargo xtask run` boots with SquashFS and ext2 disks attached as VirtIO devices
- [ ] `cargo xtask test` creates test images and attaches them
- [ ] Works on macOS Apple Silicon (brew packages)
- [ ] Works on Linux CI (apt packages)
- [ ] Missing tools produce clear error messages with install instructions
- [ ] Images only recreated when missing (not on every build)

---

### Sub-phase 3.10 — Shell integration, boot sequence, and integration tests ✅ COMPLETE

**Goal**: The kernel boots, spawns FS servers, mounts the SquashFS root with overlay, mounts ext2 /data, and exposes everything through the debug shell. All tests pass.

**Implementation notes**: Boot sequence (`fs::boot_storage`) classifies VirtIO
disks by on-disk magic, starts a block server per disk, spawns SquashFS+overlay
(root `/`) and ext2 (`/data`), registers the mount table, and prints it. Shell
commands `mount`/`ls`/`cat`/`stat`/`write`/`mkdir` operate on the live mounts.
Live QEMU verification (piping commands over `-serial stdio`) drove out three
bugs the 25-test suite missed — each isolated test starts only one block server
and never re-creates an existing name:
- **block server was a global singleton** — boot starts two (SquashFS + ext2) and
  the second `start()` clobbered the first, so the root was unreadable at boot.
  Fixed with per-instance config slots (`d9a2f9e`).
- **ext2 create/mkdir added duplicate dirents** for an existing name (dir
  corruption). Fixed: reuse+truncate existing file, `mkdir` → AlreadyExists
  (`cb11256`); `e2fsck -fn` clean after OS writes.
- **`ls` showed every entry as a directory** — servers emitted disagreeing native
  readdir type codes. Fixed with canonical `fs_proto::DT_*` codes (`89889d4`).
Post-fix: root (overlay/SquashFS) + `/data` (ext2) both readable at boot; `ls`
distinguishes files from dirs; create/mkdir idempotent-safe; 25 tests pass. Also
documented the storage architecture in mdbook (`docs/src/storage.md`) and flipped
the milestone to Complete.

**Rationale**: This is the integration sub-phase that wires everything together and proves the storage stack works end-to-end in the hybrid microkernel architecture.

**What to build**:
- Boot sequence update in `kernel/src/main.rs`:
  1. PCI scan → discover VirtIO-blk devices
  2. Initialize VirtIO-blk drivers
  3. Start block server kernel task
  4. Spawn SquashFS server (embedded binary) with block endpoint + shared memory caps
  5. Spawn overlay server with SquashFS server endpoint cap
  6. Spawn ext2 server (embedded binary) with block endpoint + shared memory caps
  7. Register mount table: "/" → overlay server, "/data" → ext2 server
  8. Print mount table and server status to serial
- Shell commands:
  - `mount`: list mounted filesystems with type, path, server PID, status
  - `ls <path>`: list directory contents (name, type, size)
  - `cat <path>`: print file contents to serial
  - `stat <path>`: show file metadata
  - `write <path> <content>`: write to a file (overlay or ext2)
  - `mkdir <path>`: create a directory
- Integration tests:
  - `test_pci_scan`: PCI bus scanned, VirtIO devices found
  - `test_virtio_blk`: block read/write round-trip
  - `test_block_server_ipc`: block server responds to IPC requests
  - `test_server_spawn`: FS server spawned from embedded binary, responds to IPC
  - `test_squashfs_mount`: SquashFS root mounted, `/version` readable, content matches ULID
  - `test_squashfs_readdir`: directory listing returns expected entries
  - `test_overlay_read_through`: SquashFS files visible through overlay
  - `test_overlay_write`: create file through overlay, read back
  - `test_overlay_copyup`: modify SquashFS file through overlay
  - `test_overlay_whiteout`: delete SquashFS file through overlay
  - `test_ext2_create_read`: create file on ext2, write, read back
  - `test_ext2_mkdir`: create directory, verify
  - `test_ext2_unlink`: delete file, verify free counts
  - `test_ext2_persistence`: write, flush, re-read, verify
  - `test_fs_capability_grant`: process with cap can open files
  - `test_fs_capability_deny`: process without cap gets PermissionDenied
  - `test_server_crash_isolation`: crash a FS server, verify kernel and other servers unaffected

**Module**: `kernel/src/main.rs`, `kernel/src/shell/commands.rs`, `kernel/src/test_runner.rs`

**Expected commits**:
1. Boot sequence: PCI scan, VirtIO init, block server, FS server spawn
2. Mount table registration and VFS routing
3. Shell commands: mount, ls, cat, stat
4. Shell commands: write, mkdir
5. Integration tests: PCI, VirtIO, block server
6. Integration tests: SquashFS server
7. Integration tests: overlay (read-through, write, copy-up, whiteout)
8. Integration tests: ext2 (create, read, mkdir, unlink, persistence)
9. Integration tests: capabilities and server crash isolation

**Acceptance criteria**:
- [ ] Kernel boots, spawns all three FS servers, prints mount table
- [ ] `ls /` through overlay shows SquashFS contents
- [ ] `cat /version` returns the build ULID
- [ ] `write /tmp/test hello` + `cat /tmp/test` works (overlay upper layer)
- [ ] `ls /data` shows ext2 volume
- [ ] `write /data/foo bar` + `cat /data/foo` works (ext2 persistent)
- [ ] All 17 integration tests pass
- [ ] `cargo xtask test` passes end-to-end with disk images
- [ ] All existing Phase 1 and Phase 2 tests still pass
- [ ] Server crash test: FS server crashes, kernel logs it, other servers unaffected

---

## Dependencies

| Crate | Used by | Version | Purpose | no_std? |
|-------|---------|---------|---------|---------|
| miniz_oxide | squashfs-server | latest | gzip/zlib decompression | Yes (with alloc) — must verify before 3.5 |

## Host Tool Dependencies

| Tool | macOS (Homebrew) | Linux (apt) | Used by |
|------|-----------------|-------------|---------|
| mksquashfs | `brew install squashfs` | `apt install squashfs-tools` | `cargo xtask image` |
| mkfs.ext2 | `brew install e2fsprogs` | `apt install e2fsprogs` | `cargo xtask image` |
| qemu-system-x86_64 | `brew install qemu` | `apt install qemu-system-x86` | existing |
| xorriso | `brew install xorriso` | `apt install xorriso` | existing |

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| VirtIO PCI transport complexity | Medium | High | Isolated in sub-phases 3.0-3.2 with independent tests at each layer |
| SquashFS format complexity (metadata compression, fragments) | Medium | Medium | Use simplest options (gzip, no xattrs). Test with known-good mksquashfs images |
| ext2 corruption bugs (bitmap desync) | High | Medium (server crash, not kernel crash) | Exhaustive free count tests. Run e2fsck on images after kernel tests. ext2 bugs crash the server, not the kernel — this is the security benefit of the hybrid architecture |
| Overlay copy-up edge cases | Medium | Medium | File-only copy-up in Phase 3. Directory copy-up deferred. Test each case individually |
| Userspace server IPC overhead | Medium | Low | Synchronous IPC is simple. Shared memory avoids copying block data. Performance optimization in later phases |
| Server framework complexity (binary embedding, spawn, caps) | Medium | High | Sub-phase 3.4 is dedicated to this. Test with a simple echo server before building FS servers |
| miniz_oxide doesn't compile for bare-metal | ~~Low~~ RESOLVED | High | ~~Verify before 3.5.~~ Verified: compiles for x86_64-unknown-none on nightly with `with-alloc`. |
| Shared memory security (FS server could corrupt shared region) | Low | Medium | Each FS server gets its own shared region. Kernel validates data from shared memory before acting on it. Regions are bounded in size. |
| Phase scope (11 sub-phases, ~65 commits) | Medium | Medium | Natural off-ramp at 3.6 (SquashFS + overlay working). ext2 and full syscall integration are additive. |

## Estimated Effort

| Sub-phase | Estimated commits | Complexity |
|-----------|------------------|------------|
| 3.0 PCI enumeration | 3 | Medium |
| 3.1 VirtIO transport | 4 | High |
| 3.2 VirtIO-blk + BlockDevice | 4 | Medium |
| 3.3 Block server IPC | 4 | Medium |
| 3.4 Server framework + libthemelios | 5 | High |
| 3.5 SquashFS server | 8 | High |
| 3.6 Overlay server | 6 | Medium-High |
| 3.7 ext2 server | 10 | High |
| 3.8 VFS dispatch + caps + syscalls | 8 | Medium-High |
| 3.9 Image tooling | 4 | Low |
| 3.10 Integration + shell | 9 | Medium |
| **Total** | **~65** | |

## Verification Checklist

- [ ] PCI bus enumeration discovers VirtIO devices
- [ ] VirtIO-blk driver reads/writes blocks correctly
- [ ] Block server IPC interface serves block requests to userspace
- [ ] SharedMemory capabilities allow data transfer between kernel and userspace
- [ ] Server framework spawns embedded binaries in ring 3
- [ ] SquashFS server reads all files correctly (data blocks + fragments + decompression)
- [ ] Overlay server merges upper (RAM) and lower (SquashFS) correctly
- [ ] Overlay copy-up works with 1 MiB limit enforced
- [ ] Overlay whiteouts hide deleted lower-layer files
- [ ] Overlay memory budget (8 MiB) enforced
- [ ] ext2 server reads/writes files and directories
- [ ] ext2 block/inode allocation consistent (free counts always accurate)
- [ ] ext2 max file size documented and enforced (single indirect only)
- [ ] ext2 changes persist to disk
- [ ] e2fsck reports clean on ext2 images after kernel test suite
- [ ] VFS dispatch routes syscalls to correct FS server
- [ ] Kernel never parses filesystem data — only routes IPC
- [ ] Filesystem capabilities enforce access control
- [ ] All filesystem syscalls (8-13) work from userspace
- [ ] User pointer validation prevents kernel crashes from bad arguments
- [ ] Audit log captures filesystem operations
- [ ] Server crash does not affect kernel or other servers
- [ ] `cargo xtask image` creates valid images
- [ ] `cargo xtask test` passes all new and existing tests
- [ ] Shell commands (mount, ls, cat, stat, write, mkdir) work
- [ ] Works on macOS Apple Silicon and Linux CI
- [ ] No regressions in Phase 1 or Phase 2 tests
- [ ] milestones.md and CLAUDE.md updated
- [ ] **Post-phase**: hybrid microkernel storage architecture documented in mdbook
