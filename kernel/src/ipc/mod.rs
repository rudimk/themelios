//! # Inter-process communication (IPC)
//!
//! Since ThemeliOS is a microkernel, most system services (drivers, filesystems,
//! networking) run in userspace. IPC is how they communicate — it's the backbone
//! of the entire system.
//!
//! ## IPC mechanisms
//!
//! - **Synchronous message passing**: A process sends a message and blocks until
//!   the receiver replies. This is the primary IPC mechanism, used for request/response
//!   patterns (e.g., "read block 42 from disk").
//!
//! - **Asynchronous notifications** (future): Lightweight signals that don't carry
//!   data, used to wake up processes or signal events without blocking the sender.
//!
//! - **Shared memory** (future): For bulk data transfer, two processes can share
//!   a memory region via capabilities. The IPC message just says "data is ready
//!   in the shared buffer."
//!
//! ## Capability integration
//!
//! IPC endpoints are themselves capabilities. To send a message to a service,
//! you need a capability for that service's endpoint. This means you can't even
//! *address* a service you haven't been granted access to.
