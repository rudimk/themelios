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
