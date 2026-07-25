# Milestones

ThemeliOS development is organized into phases. Each phase builds on the previous one and produces a working, testable artifact.

| Phase | Goal | Status |
|-------|------|--------|
| **0** | Boot on QEMU, serial output | Complete |
| **1** | Memory allocator, scheduler, interrupts (x86_64) | Complete |
| **2** | Capability system, process isolation, IPC | Complete |
| **3** | VirtIO block driver, read-only filesystem | Complete |
| **4** | VirtIO net driver, TCP/IP stack | Complete |
| **5** | OCI container support | Not started |
| **6** | Management API (Docker-compatible) | Not started |
| **7** | aarch64 port | Not started |
| **8** | Hyperscaler support (AWS, GCP, Azure) | Not started |
| **9** | Testing and benchmarks | Not started |
| **10** | Kubernetes worker node | Not started |
| **11** | GPU support across clouds | Not started |
| **12** | Production operations (observability, updates) | Not started |

---

## Phase 0 — Boot (Complete)

**Goal**: Get the kernel booting on QEMU and printing to the serial console.

**Deliverables**:
- Bootloader integration (Limine or UEFI)
- Architecture-specific early init (x86_64 first)
- Serial console output (16550 UART on x86_64)
- "Hello from ThemeliOS" printed on boot
- `cargo xtask run` boots the kernel in QEMU end-to-end

## Phase 1 — Kernel basics (Complete)

**Goal**: A kernel that can manage memory and schedule tasks. x86_64 only — aarch64 is deferred to Phase 7.

**Deliverables**:
- Physical frame allocator (bitmap-based)
- Kernel heap allocator
- Interrupt handling (GDT, IDT, 8259 PIC on x86_64)
- Timer-driven preemptive scheduler (round-robin)
- Basic kernel shell over serial (for debugging, will be removed later)
- Automated test infrastructure (`isa-debug-exit`, `cargo xtask test`, GitHub Actions CI)

## Phase 2 — Isolation (Complete)

**Goal**: Implement the capability system and process isolation.

**Deliverables**:
- Custom page tables replacing Limine's (required for per-process address spaces)
- Capability types and capability space (CSpace)
- Process creation with isolated address spaces
- Capability grant, transfer, and revocation
- Synchronous IPC (message passing between processes)
- Audit logging (tamper-evident record of capability usage for compliance and security)
- Reclaim bootloader-reclaimable memory (safe once we own GDT, page tables, and stack)
- First userspace process (init)

## Phase 3 — Storage (Complete)

**Goal**: Read from a virtual disk and present a filesystem, using a hybrid
microkernel design — a thin in-kernel block driver with all filesystem parsing in
userspace servers. See [Storage Architecture](./storage.md) for the full design.

**Deliverables**:
- PCI enumeration and a VirtIO (modern PCI transport + split virtqueue) layer
- `BlockDevice` trait and a VirtIO-blk driver
- Kernel-side block server exposing the disk to userspace over IPC + shared memory
- `CapType::SharedMemory` for bulk data transfer across the privilege boundary
- Userspace server framework (flat-binary embedding, `spawn_server`) and the
  `libthemelios` support library
- SquashFS server (compressed read-only root) running in ring 3
- Overlay server (RAM upper + SquashFS lower, copy-up, whiteouts) — the ephemeral
  writable layer
- ext2 server (read-write persistent data volume) running in ring 3
- VFS dispatch with `Filesystem` / `FileDescriptor` capabilities and the
  filesystem syscalls (open, read, write, close, stat, readdir)
- Audit logging for filesystem operations
- `cargo xtask image` tooling to build the SquashFS root and ext2 data images
- Boot integration (mounts `/` and `/data`) and debug-shell commands
  (`mount`, `ls`, `cat`, `stat`, `write`, `mkdir`)

## Phase 4 — Networking (Complete)

**Goal**: TCP/IP connectivity.

The whole TCP/IP stack (smoltcp) runs in a ring-3 net server; a thin VirtIO-net
driver stays in the kernel and frames cross via a pull-based IPC bridge. Sockets
are capability-checked and kernel-routed. See the
[Network Architecture](./networking.md) doc for the full design.

**Deliverables** (all delivered):
- VirtIO network driver + `NetDevice` trait (the arm64/bus seam)
- Kernel net service (pull-based frame bridge) and `SYS_UPTIME_MS` clock
- Ring-3 smoltcp stack: Ethernet, ARP, IPv4, ICMP
- DHCPv4 client (address, gateway, DNS captured for display)
- Capability-checked socket API (`CapType::Socket`): UDP, TCP, and ICMP
- Shell: `ifconfig`, `sockets`, `ping`, `udpsend`, `tcpconnect`
- Boot integration (NIC + service + net server, DHCP-configured, at boot)
- Integration tests (driver, service, ARP/ICMP, DHCP, UDP echo, socket caps,
  socket listing, TCP client + server) — 35 tests, reliably green
- aarch64 smoltcp compile gate in CI (`cargo xtask arm64-gate`)

## Phase 5 — Containers (Not started)

**Goal**: Run OCI container images.

**Deliverables**:
- Linux syscall compatibility layer (translate Linux syscalls to capability-checked ThemeliOS operations)
- OCI image format parsing and layer unpacking
- Container lifecycle (create, start, stop, destroy)
- Container exec (spawn processes inside a running container's isolation boundary)
- PTY support for interactive terminal sessions
- Container-to-capability mapping (each container gets a capability set)
- Container networking (virtual interfaces, isolation)
- Log streaming from containers (stdout/stderr capture)
- Resource limits (CPU, memory) enforced via capabilities
- Container image registry support (Docker Hub, ECR, GCR, ACR)
- Registry authentication, TLS, and cloud-specific credential helpers

## Phase 6 — Management (Not started)

**Goal**: Docker-compatible management API for the node.

**Deliverables**:
- Docker Engine API compatible subset (containers, exec, images, logs, networks)
- Bidirectional streaming for interactive exec sessions (websocket)
- Capability-based authorization (API clients mapped to capability sets)
- TLS client certificate and API token authentication
- Node status and health reporting
- Configuration injection at boot time
- No SSH — API is the only interface
- Standard Docker tooling works out of the box (`docker exec`, `docker ps`, `docker logs`, etc.)

## Phase 7 — aarch64 port (Not started)

**Goal**: Port all Phase 0 and Phase 1 functionality to aarch64 (ARM64), enabling ThemeliOS to run on ARM-based hardware and cloud instances (e.g., AWS Graviton).

**Deliverables**:
- aarch64 boot via Limine (UEFI on ARM)
- PL011 UART serial driver for debug output
- GIC (Generic Interrupt Controller) initialization and exception handling
- ARM generic timer for scheduler preemption
- Physical frame allocator (same bitmap design, architecture-independent)
- Kernel heap (architecture-independent, just works)
- Scheduler and context switch for aarch64 (different register set, different calling convention)
- Serial debug shell (architecture-independent, just works)
- `cargo xtask run --arch aarch64` boots and passes all tests
- Automated tests on aarch64 QEMU in CI

## Phase 8 — Hyperscaler support (Not started)

**Goal**: Boot and run on AWS, GCP, and Azure.

**Deliverables**:
- Instance metadata service (IMDS) clients for all three providers
- Cloud-aware configuration injection at boot time
- Machine image tooling (`cargo xtask image --cloud aws/gcp/azure`)
- AMI creation for AWS (raw disk import via `aws ec2 import-image`)
- GCP image creation (raw disk tarball + `gcloud compute images create`)
- Azure VHD image creation
- UEFI Secure Boot chain verification and kernel image signing
- Measured boot (TPM support)
- Boot validation on each provider's compute instances
- GitHub Actions workflow to build downloadable QEMU ISOs (x86_64, aarch64)
- GitHub Actions workflows to build and publish cloud-specific machine images

## Phase 9 — Testing and benchmarks (Not started)

**Goal**: Comprehensive test suite and performance benchmarks to validate the OS works correctly end-to-end.

**Deliverables**:
- CI infrastructure (GitHub Actions with QEMU, `isa-debug-exit` device for pass/fail exit codes)
- Boot smoke tests (kernel boots, reaches known-good state, no panic)
- Kernel unit tests (allocator, scheduler, capability enforcement tested in isolation)
- Kernel integration tests (spawn process + grant capability + IPC message + verify result)
- Security and isolation tests (capability violations, unauthorized memory access, process escape attempts — all must fail cleanly)
- Container runtime tests with standard images (alpine, busybox, nginx)
- Custom test images (memory stress, network connectivity, filesystem I/O, multi-process isolation)
- Container lifecycle tests (create, start, stop, restart, destroy, exec)
- Multi-container isolation validation
- Container networking tests
- Resource limit enforcement tests
- Cloud validation tests (boot on each hyperscaler, IMDS, networking, container workloads)
- Benchmarks: boot time, context switch latency, IPC throughput, memory allocation speed, container cold-start time
- Benchmark history tracking for regression detection

## Phase 10 — Kubernetes (Not started)

**Goal**: Full drop-in K8s/K3s/RKE2 worker node. Any pod that runs on an Ubuntu or Flatcar node must run identically on ThemeliOS.

**Deliverables**:
- Full Linux syscall coverage for real-world K8s workloads (databases, language runtimes, service meshes, logging agents, init systems)
- CRI (Container Runtime Interface) gRPC API implementation
- CNI (Container Network Interface) plugin support (Flannel, Calico, Cilium)
- CSI (Container Storage Interface) driver support for persistent volumes
- Pod semantics (groups of containers sharing network and storage namespaces)
- kubelet (standard binary or compatible custom implementation)
- kube-proxy equivalent for service networking and load balancing
- Node registration, capacity reporting, and health conditions
- `kubectl exec -it` with full interactive shell support
- `kubectl logs`, `kubectl cp`, `kubectl port-forward`
- Pod resource management (CPU/memory requests and limits, QoS classes)
- DNS resolution for K8s service discovery

## Phase 11 — GPU support (Not started)

**Goal**: GPU passthrough and accelerator support for containerized workloads across all major cloud providers.

**Deliverables**:
- VFIO/IOMMU support for GPU device passthrough to containers
- NVIDIA driver ioctl compatibility in the syscall layer
- K8s device plugin API support for GPU resource scheduling
- GPU resource requests and limits in pod specs
- Validation on AWS GPU instances (P/G series)
- Validation on GCP GPU instances (A2/G2 series)
- Validation on Azure GPU instances (NC/ND series)
- Cloud-specific accelerator support (AWS Inferentia/Trainium, GCP TPU, Azure AMD GPUs)

## Phase 12 — Production operations (Not started)

**Goal**: Day-2 operational tooling for running ThemeliOS nodes in production.

**Deliverables**:
- Metrics export in Prometheus format (node-exporter compatible)
- Log forwarding to external collectors (CloudWatch, Stackdriver, Fluentd)
- Health endpoints for load balancers and orchestrators
- Distributed tracing support for container workloads
- A/B partition scheme for whole-image OS updates
- Automatic rollback on failed updates
- Zero-downtime node upgrades (drain → swap image → rejoin cluster)
- OS update tooling (`cargo xtask image --update` or equivalent)
- Update coordination with K8s (respect PodDisruptionBudgets during upgrades)
