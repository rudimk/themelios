//! # Memory management subsystem
//!
//! Responsible for all memory-related operations in the kernel:
//!
//! - **Physical frame allocator**: Tracks which 4 KiB frames of physical memory
//!   are free or in use. Hands out frames to the page table manager and kernel heap.
//!
//! - **Virtual memory / paging**: Creates and manages page tables for the kernel
//!   and for each userspace process. Each process gets its own virtual address space,
//!   enforced by the hardware MMU.
//!
//! - **Kernel heap allocator**: Provides `alloc`-style dynamic allocation for kernel
//!   data structures (e.g., process control blocks, capability tables, IPC buffers).
//!
//! ## Design notes
//!
//! Memory isolation is critical for ThemeliOS's security model. Each container process
//! runs in its own address space, and the capability system controls which memory
//! regions a process can share or access. The MM subsystem enforces this at the
//! hardware level via page table permissions.
