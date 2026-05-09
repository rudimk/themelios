# Milestones

ThemeliOS development is organized into phases. Each phase builds on the previous one and produces a working, testable artifact.

| Phase | Goal | Status |
|-------|------|--------|
| **0** | Boot on QEMU, serial output | Complete |
| **1** | Memory allocator, scheduler, interrupts | Not started |
| **2** | Capability system, process isolation, IPC | Not started |
| **3** | VirtIO block driver, read-only filesystem | Not started |
| **4** | VirtIO net driver, TCP/IP stack | Not started |
| **5** | OCI container support | Not started |
| **6** | Management API | Not started |
| **Future** | Kubernetes worker node | Not started |

---

## Phase 0 — Boot (Complete)

**Goal**: Get the kernel booting on QEMU and printing to the serial console.

**Deliverables**:
- Bootloader integration (Limine or UEFI)
- Architecture-specific early init (x86_64 first)
- Serial console output (16550 UART on x86_64)
- "Hello from ThemeliOS" printed on boot
- `cargo xtask run` boots the kernel in QEMU end-to-end

## Phase 1 — Kernel basics (Not started)

**Goal**: A kernel that can manage memory and schedule tasks.

**Deliverables**:
- Physical frame allocator (bitmap-based)
- Virtual memory manager (page table setup, higher-half kernel)
- Kernel heap allocator
- Interrupt handling (IDT on x86_64, GIC on aarch64)
- Timer-driven preemptive scheduler (round-robin)
- Basic kernel shell over serial (for debugging, will be removed later)
- aarch64 port of Phase 0 + Phase 1

## Phase 2 — Isolation (Not started)

**Goal**: Implement the capability system and process isolation.

**Deliverables**:
- Capability types and capability space (CSpace)
- Process creation with isolated address spaces
- Capability grant, transfer, and revocation
- Synchronous IPC (message passing between processes)
- First userspace process (init)

## Phase 3 — Storage (Not started)

**Goal**: Read from a virtual disk and present a filesystem.

**Deliverables**:
- VirtIO block driver (for QEMU's virtual disk)
- Read-only filesystem (simple format, possibly custom or FAT)
- RAM-backed ephemeral writable layer
- Immutable root image creation tooling

## Phase 4 — Networking (Not started)

**Goal**: TCP/IP connectivity.

**Deliverables**:
- VirtIO network driver
- Ethernet, ARP, IPv4
- TCP and UDP
- Basic socket-like API via capabilities
- DHCP client

## Phase 5 — Containers (Not started)

**Goal**: Run OCI container images.

**Deliverables**:
- OCI image format parsing and layer unpacking
- Container lifecycle (create, start, stop, destroy)
- Container-to-capability mapping (each container gets a capability set)
- Container networking (virtual interfaces, isolation)
- Log streaming from containers

## Phase 6 — Management (Not started)

**Goal**: External API for managing the node.

**Deliverables**:
- HTTP or gRPC management API
- Container management endpoints (create, start, stop, list, logs)
- Node status and health reporting
- Configuration injection at boot time
- No SSH — API is the only interface

## Future — Kubernetes (Not started)

**Goal**: Serve as a K8s/K3s worker node.

**Deliverables** (rough):
- CRI-compatible container runtime
- kubelet (or custom equivalent)
- CNI plugin support
- Node registration with K8s control plane
- Pod lifecycle management

This phase is explicitly not v1 and will be scoped in detail after Phase 6 is complete.
