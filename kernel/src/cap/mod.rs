//! # Capability system
//!
//! The core security primitive of ThemeliOS. Every resource in the system — memory
//! regions, IPC endpoints, device access, network sockets, filesystem paths — is
//! accessed through **capability tokens**.
//!
//! ## What is a capability?
//!
//! A capability is an unforgeable token that grants its holder permission to
//! perform specific operations on a specific resource. Think of it like a key:
//! you can only open doors you have keys for, and you can't forge new keys.
//!
//! ## Key properties
//!
//! - **No ambient authority**: A process starts with zero capabilities. It can
//!   only access resources that have been explicitly granted to it by its parent
//!   or the kernel.
//! - **Unforgeable**: Capabilities are kernel-managed. Userspace cannot create,
//!   modify, or guess valid capabilities.
//! - **Transferable**: A process can pass a capability to another process via IPC,
//!   enabling controlled sharing.
//! - **Revocable**: The kernel (or a parent process) can revoke a capability,
//!   instantly cutting off access.
//!
//! ## Why capabilities?
//!
//! Traditional OS security (Unix permissions, Linux namespaces) is "deny by default
//! with exceptions." Capabilities are "deny everything, grant explicitly." This
//! makes it much harder for a compromised container to access resources it shouldn't —
//! there's nothing to escalate to if the authority was never ambient in the first place.
//!
//! ## Inspiration
//!
//! This design draws from seL4 (formally verified capability microkernel) and
//! Fuchsia/Zircon (Google's capability-based OS).
