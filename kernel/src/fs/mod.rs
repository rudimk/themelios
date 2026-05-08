//! # Filesystem layer
//!
//! ThemeliOS uses a minimal filesystem design aligned with its immutable,
//! container-focused architecture:
//!
//! ## Filesystem layout
//!
//! - **Read-only root**: The OS image is a read-only filesystem containing the
//!   kernel, init process, and system services. It's never modified at runtime.
//!   Updates replace the entire image (A/B partition swap).
//!
//! - **Ephemeral writable layer**: A RAM-backed (tmpfs-like) writable layer for
//!   container runtime state — pulled images, running container filesystems,
//!   temporary data. This is lost on reboot, which is fine because nodes are cattle.
//!
//! ## OCI image support (Phase 5)
//!
//! Container images are OCI-format tarballs containing filesystem layers.
//! The filesystem layer will handle unpacking these layers and presenting them
//! as a unified filesystem view to each container, using overlay-style layering.
//!
//! ## No persistent storage for logs
//!
//! Logs are streamed off-node in real-time via the network stack. The filesystem
//! layer does not provide persistent writable storage for logs.
