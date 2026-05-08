//! # Architecture abstraction layer
//!
//! ThemeliOS supports multiple CPU architectures. This module provides
//! a common interface that the rest of the kernel uses, with each architecture
//! implementing the specifics behind that interface.
//!
//! ## Supported architectures
//!
//! - `x86_64` — Intel/AMD 64-bit (primary development target)
//! - `aarch64` — ARM 64-bit (secondary target, for cloud ARM instances)
//!
//! ## What lives here
//!
//! Each architecture module provides:
//! - **Boot**: Early initialization, CPU mode setup, stack setup
//! - **Interrupts**: IDT/GIC setup, interrupt/exception handlers
//! - **Paging**: Page table management (architecture-specific format)
//! - **Context switching**: Saving/restoring CPU state for scheduling
//! - **Serial I/O**: Early debug output via UART/COM port

// Conditionally compile the correct architecture module based on the
// target we're building for. Only one of these will be included in
// any given build.

/// x86_64 (Intel/AMD 64-bit) architecture support.
#[cfg(target_arch = "x86_64")]
pub mod x86_64;

/// aarch64 (ARM 64-bit) architecture support.
#[cfg(target_arch = "aarch64")]
pub mod aarch64;
