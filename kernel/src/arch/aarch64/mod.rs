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

// Phase 7.0b: minimal boot-to-banner support.
//
// - `boot`   — early EL1 init: enable FP, map the PL011, print the banner (7.0b).
// - `serial` — PL011 UART driver + `_print` (backs the `println!` facade).
// - `irq`    — `DAIF`/`wfi` local-interrupt control (backs `arch::irq`).
// - `time`   — monotonic tick (backs `arch::time`; timer ISR lands in 7.2).
//
// Still to come: `mmu` (7.1), `exceptions`+`gic`+`timer` (7.2), `context` (7.3).
pub mod boot;
pub mod irq;
pub mod serial;
pub mod time;
