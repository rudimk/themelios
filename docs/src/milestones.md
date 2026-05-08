# Milestones

ThemeliOS development is organized into phases. Each phase builds on the previous one and produces a working, testable artifact.

## Phase 0 — Boot

**Goal**: Get the kernel booting on QEMU and printing to the serial console.

**Deliverables**:
- Bootloader integration (Limine or UEFI)
- Architecture-specific early init (x86_64 first)
- Serial console output (16550 UART on x86_64)
- "Hello from ThemeliOS" printed on boot
- `cargo xtask run` boots the kernel in QEMU end-to-end

**What you'll learn**: Bare-metal Rust, the boot process, how hardware/QEMU works at the lowest level.

## Phase 1 — Kernel basics

**Goal**: A kernel that can manage memory and schedule tasks.

**Deliverables**:
- Physical frame allocator (bitmap-based)
- Virtual memory manager (page table setup, higher-half kernel)
- Kernel heap allocator
- Interrupt handling (IDT on x86_64, GIC on aarch64)
- Timer-driven preemptive scheduler (round-robin)
- Basic kernel shell over serial (for debugging, will be removed later)
- aarch64 port of Phase 0 + Phase 1

## Phase 2 — Isolation

**Goal**: Implement the capability system and process isolation.

**Deliverables**:
- Capability types and capability space (CSpace)
- Process creation with isolated address spaces
- Capability grant, transfer, and revocation
- Synchronous IPC (message passing between processes)
- First userspace process (init)

## Phase 3 — Storage

**Goal**: Read from a virtual disk and present a filesystem.

**Deliverables**:
- VirtIO block driver (for QEMU's virtual disk)
- Read-only filesystem (simple format, possibly custom or FAT)
- RAM-backed ephemeral writable layer
- Immutable root image creation tooling

## Phase 4 — Networking

**Goal**: TCP/IP connectivity.

**Deliverables**:
- VirtIO network driver
- Ethernet, ARP, IPv4
- TCP and UDP
- Basic socket-like API via capabilities
- DHCP client

## Phase 5 — Containers

**Goal**: Run OCI container images.

**Deliverables**:
- OCI image format parsing and layer unpacking
- Container lifecycle (create, start, stop, destroy)
- Container-to-capability mapping (each container gets a capability set)
- Container networking (virtual interfaces, isolation)
- Log streaming from containers

## Phase 6 — Management

**Goal**: External API for managing the node.

**Deliverables**:
- HTTP or gRPC management API
- Container management endpoints (create, start, stop, list, logs)
- Node status and health reporting
- Configuration injection at boot time
- No SSH — API is the only interface

## Future — Kubernetes

**Goal**: Serve as a K8s/K3s worker node.

**Deliverables** (rough):
- CRI-compatible container runtime
- kubelet (or custom equivalent)
- CNI plugin support
- Node registration with K8s control plane
- Pod lifecycle management

This phase is explicitly not v1 and will be scoped in detail after Phase 6 is complete.
