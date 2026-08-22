# Architecture Overview

ThemeliOS is a **capability-based microkernel**. This page explains the high-level design and the reasoning behind key architectural decisions.

## Microkernel vs monolithic

In a monolithic kernel (like Linux), drivers, filesystems, and networking all run inside the kernel with full hardware access. A bug in any driver can crash or compromise the entire system.

In a microkernel, only the absolute minimum runs in kernel space:

| Kernel space | Userspace |
|-------------|-----------|
| Memory management | Device drivers |
| Process scheduling | Filesystem |
| IPC (message passing) | Network stack |
| Capability enforcement | Container runtime |
| | Management API |

Everything else runs as isolated userspace processes that communicate via IPC. A buggy driver crashes its own process, not the kernel.

**Why microkernel for ThemeliOS?** Since we're building an OS specifically for running untrusted container workloads, minimizing the trusted computing base (the code that can compromise the whole system) is critical. The smaller the kernel, the smaller the attack surface.

## Capability-based security

ThemeliOS does **not** use Linux-style permissions (UID/GID, filesystem permissions) or Linux-style isolation (namespaces, cgroups). Instead, it uses **capabilities**.

### What is a capability?

A capability is an unforgeable token that grants its holder specific permissions on a specific resource. For example:

- "Read and write to memory region 0x1000–0x2000"
- "Send messages to IPC endpoint #42"
- "Access VirtIO block device at MMIO address 0xFE00"

### Key properties

1. **No ambient authority**: A newly created process has zero capabilities. It can't do anything until its parent grants it capabilities.

2. **Unforgeable**: Capabilities are managed by the kernel. Userspace can't create them or guess valid ones.

3. **Transferable**: Capabilities can be passed between processes via IPC, enabling controlled delegation.

4. **Revocable**: A capability can be revoked, immediately cutting off access.

### Why not namespaces?

Linux namespaces are "isolation after the fact" — processes start with broad access and namespaces restrict what they can see. Capabilities are "isolation by default" — processes start with nothing and are explicitly granted only what they need.

For a container OS, this means a compromised container literally cannot access resources it wasn't given capabilities for. There's no kernel interface to probe, no `/proc` to read, no syscall to escalate through — the authority simply doesn't exist.

### Inspiration

- **seL4**: Formally verified capability microkernel. ThemeliOS borrows its capability model.
- **Fuchsia/Zircon**: Google's capability-based OS. Demonstrates the model works at scale.

## Memory model

ThemeliOS uses hardware-enforced memory isolation:

- Each process runs in its own virtual address space (page tables enforced by the MMU).
- The kernel has its own address space that userspace cannot access.
- Shared memory between processes requires explicit capabilities from both sides.

### Physical memory management

A frame allocator tracks free physical memory pages (4 KiB). Frames are allocated to:
- Process page tables
- Kernel heap
- Shared memory regions
- DMA buffers for device drivers

### Virtual memory layout

The virtual address space layout will be defined per-architecture, but the general structure is:

```
0x0000_0000_0000_0000  ┌──────────────────────┐
                        │   Userspace           │
                        │   (per-process)       │
0x0000_7FFF_FFFF_FFFF  └──────────────────────┘
                        ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
                          Non-canonical hole
                        └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘
0xFFFF_8000_0000_0000  ┌──────────────────────┐
                        │   Kernel space        │
                        │   (shared, all procs) │
0xFFFF_FFFF_FFFF_FFFF  └──────────────────────┘
```

(This is the x86_64 layout; aarch64 is similar but with different conventions.)

## IPC

Inter-process communication is the backbone of the microkernel. Since drivers, filesystems, and networking all run in userspace, every system operation involves IPC.

### Synchronous message passing

The primary mechanism: a client sends a message to a server and blocks until it gets a reply. This is used for request/response patterns like "read this file" or "send this network packet."

### Performance consideration

IPC overhead is the classic criticism of microkernels. ThemeliOS will address this by:
- Keeping messages small (pointers to shared memory for bulk data)
- Using register-based fast-path for small messages
- Careful cache-aware scheduling of communicating processes

## Immutability

The OS root filesystem is read-only. The entire OS image is a single artifact that is booted as-is.

- **Updates**: Swap the entire image. No package managers, no apt-get, no partial updates.
- **Configuration**: Injected at boot time via cloud-init-style metadata or the management API.
- **Ephemeral state**: Container images and runtime state live on a RAM-backed ephemeral layer that is lost on reboot.

This model treats nodes as cattle: if a node is unhealthy, replace it with a fresh one. No debugging on the node, no SSHing in, no manual fixes.

## Stopping and restarting a node

Cattle still have to be put down deliberately. A node that vanishes is indistinguishable from one that crashed, and the difference matters to whatever is scheduling work onto it — so `arch::power` exposes two operations, `power_off` and `reset`, surfaced as the shell's `shutdown` and `reboot` and (eventually) as management-API verbs.

**The two architectures are not equally well served here, and the facade deliberately does not hide that.**

| | aarch64 | x86_64 |
|---|---|---|
| Mechanism | PSCI `SYSTEM_OFF` / `SYSTEM_RESET` over `HVC`/`SMC` | ACPI `PM1a_CNT` port write / chipset reset register `0xCF9` |
| Real hardware? | **Yes** — the ARM-standard firmware interface, same call Linux makes | Reset yes; **shutdown no** |
| Fallbacks | none — park the CPU | 8042 pulse, then a deliberate triple fault |

On ARM this is finished work: PSCI is what a Graviton or Ampere instance actually implements, so the same code path that stops QEMU `virt` stops a cloud VM.

On x86 soft-off (ACPI S5) requires reading the `\_S5` object out of the firmware's AML bytecode, which needs an ACPI interpreter this kernel does not have. `power_off` therefore writes the sleep-enable bit to the `PM1a_CNT` addresses that QEMU, Bochs and VirtualBox are each known to fix in place, and if none of them answers it says so on the console before parking the CPU. That is honest but it is not shutdown on physical x86 — closing that gap means an AML interpreter, not a longer table of magic port numbers.

Reset is in better shape on both: `0xCF9` is genuine chipset hardware, and behind it sit the 8042 reset pulse and, last, a deliberate triple fault — which is not a mechanism so much as a consequence, since a CPU that cannot deliver a fault about a fault has no option but to reset.

There is deliberately **no `exit` command**. The debug shell is not a login session; it is a kernel task that owns the only console, so terminating it would leave a running kernel nobody can talk to. Typing `exit` prints a pointer to `shutdown` instead.

## Target platforms

ThemeliOS is designed to run as a virtual machine, with bare-metal support as a secondary goal.

| Platform | Status | Notes |
|----------|--------|-------|
| QEMU/KVM (x86_64) | Primary dev target | Used for all development and testing |
| QEMU (aarch64) | Secondary dev target | ARM64 support |
| AWS (EC2) | Future | Nitro hypervisor |
| GCP (Compute Engine) | Future | KVM-based |
| Azure (VMs) | Future | Hyper-V |
| Bare metal (headless) | Future | Server hardware, no GPU/display |
