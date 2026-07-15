//! # Network stack
//!
//! TCP/IP networking for ThemeliOS. The network stack serves two purposes:
//!
//! 1. **Container networking**: Providing network connectivity to running containers.
//!    Each container gets an isolated virtual network interface with capabilities
//!    controlling what it can access.
//!
//! 2. **Management API**: The external API for managing the node (starting/stopping
//!    containers, querying status, streaming logs). Since ThemeliOS has no SSH,
//!    this API is the only way to interact with a running node.
//!
//! ## Implementation approach
//!
//! The network stack will be built on top of the VirtIO network driver and will
//! implement:
//! - Ethernet frame handling
//! - ARP (address resolution)
//! - IPv4 (and eventually IPv6)
//! - TCP and UDP
//! - A simple HTTP/gRPC server for the management API
//!
//! ## Microkernel considerations
//!
//! In the full microkernel design, the network stack runs in userspace. The kernel
//! only provides the raw VirtIO device access (via capabilities) and IPC channels
//! for the network service to communicate with other processes.
//!
//! ## Phase 4 status
//!
//! Sub-phase 4.0 adds the [`device`] module: the [`NetDevice`](device::NetDevice)
//! trait (the bus-independent NIC abstraction and the arm64 seam) and the global
//! network device registry. The VirtIO-net driver that implements it lives in
//! [`crate::drivers::virtio::net`]. Higher layers — the kernel net service and the
//! ring-3 TCP/IP stack — build on this in later sub-phases.

/// Network device abstraction (`NetDevice` trait) and the global NIC registry.
pub mod device;

/// Kernel net service: the pull-based IPC + shared-memory bridge between the
/// in-kernel NIC driver and the ring-3 TCP/IP stack.
pub mod net_service;
