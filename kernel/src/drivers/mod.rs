//! # Device drivers
//!
//! Drivers for hardware devices. In ThemeliOS's microkernel architecture,
//! most drivers will eventually run in userspace. However, some minimal drivers
//! need to be in the kernel for bootstrapping:
//!
//! - **Serial/UART**: Debug output from the earliest boot stages.
//! - **Timer**: Drives the scheduler's preemption tick.
//! - **Interrupt controller**: Routes hardware interrupts to the right handler.
//!
//! ## VirtIO drivers
//!
//! Since ThemeliOS targets virtual machines (QEMU/KVM, cloud hypervisors),
//! VirtIO is the primary device interface:
//!
//! - **virtio-blk**: Block device (disk I/O)
//! - **virtio-net**: Network interface
//! - **virtio-console**: Serial console
//!
//! VirtIO devices use a standardized transport (MMIO or PCI) and a ring buffer
//! protocol (virtqueues) for efficient host-guest communication.
//!
//! ## Microkernel driver model
//!
//! In later phases, drivers will be moved to userspace processes that communicate
//! with the kernel via IPC. The kernel grants them capabilities for the specific
//! hardware resources (MMIO regions, interrupt lines) they need — nothing more.

/// Block device abstraction and registry.
///
/// The `BlockDevice` trait every storage driver implements, plus the global
/// registry that maps device names/indices to drivers. Decouples filesystem and
/// block-server code from the specific hardware (VirtIO-blk now; NVMe later).
pub mod block;

/// Block server — kernel-side IPC endpoint exposing block I/O to userspace
/// filesystem servers. Receives read/write/flush requests over IPC and performs
/// them via the in-kernel `BlockDevice`, transferring data through shared memory.
pub mod block_server;

/// PCI bus enumeration (x86_64).
///
/// Discovers the virtual hardware QEMU exposes on the PCI bus — most importantly
/// VirtIO devices — by walking PCI configuration space via the 0xCF8/0xCFC I/O
/// ports. The foundation the VirtIO transport and block driver build on.
#[cfg(target_arch = "x86_64")]
pub mod pci;

/// VirtIO PCI transport and split virtqueue.
///
/// The device-type-agnostic layer every VirtIO driver builds on: capability
/// discovery, the device init handshake, feature negotiation, and the split
/// virtqueue used to exchange buffers with the host. Sits in ring 0 as the thin
/// trusted transport of the Phase 3 hybrid-microkernel storage design.
///
/// Arch-neutral since 8.3: the register-level transport is a trait with a virtio-PCI
/// implementation on x86_64 and a virtio-mmio one on aarch64.
pub mod virtio;
