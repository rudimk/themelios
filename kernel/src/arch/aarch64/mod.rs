//! # aarch64 architecture support
//!
//! This module contains all code specific to the aarch64 (ARM 64-bit)
//! architecture. It handles:
//!
//! - **Boot sequence**: Transition from bootloader to kernel
//! - **Exception levels**: EL1 (kernel) configuration
//! - **MMU**: Translation tables (4 KiB granule, 4-level)
//! - **GIC**: Generic Interrupt Controller setup
//! - **PL011 UART**: Serial output for debug printing
//! - **Context switching**: Register save/restore for task switching
//!
//! ## Memory model
//!
//! aarch64 uses a 4-level translation table (similar to x86_64's paging)
//! with 4 KiB granule. Virtual addresses are 48-bit. The kernel runs at
//! Exception Level 1 (EL1).
//!
//! ## Status
//!
//! aarch64 support is a secondary target. The x86_64 implementation will
//! be completed first, then ported here. The architecture abstraction layer
//! in `arch/mod.rs` ensures the rest of the kernel doesn't need to know
//! which architecture it's running on.

// Sub-modules will be added after x86_64 implementation is stable:
// pub mod boot;    — entry point, EL1 setup
// pub mod gic;     — Generic Interrupt Controller
// pub mod uart;    — PL011 UART driver
// pub mod mmu;     — translation table management
