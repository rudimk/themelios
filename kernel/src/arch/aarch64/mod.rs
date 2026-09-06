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

/// Exception vectors (`VBAR_EL1`), syndrome decoding, and fault reporting.
pub mod exceptions;

/// Task context switching (callee-saved register save/restore, task bootstrap).
pub mod context;

/// GICv2 interrupt controller (distributor + CPU interface).
pub mod gic;

/// ARM generic timer (`CNTV`) driving the 100 Hz tick.
pub mod timer;

pub mod irq;

/// EL0 syscall entry: the ABI, and dispatch from the lower-EL synchronous vector.
pub mod syscall;

/// Copying across the EL0 boundary, bounded by the hardware's own T0SZ.
pub mod uaccess;

/// The EL0 preemption soak: two userspace tasks in separate address spaces, every syscall
/// return checked, with predicates that fail differently for a frozen timer and for a
/// scheduler that does not interleave. Phase 8's riskiest unknown.
pub mod el0_soak;

/// Per-task FPSIMD state (`v0`-`v31` + `FPCR`/`FPSR`), and the `CPACR_EL1.FPEN` policy
/// that makes userspace hardfloat possible. Phase 8.4e.
pub mod fpsimd;

/// PSCI power control — how the machine stops (the `isa-debug-exit` analog).
pub mod psci;

/// Per-CPU data addressed through `TPIDR_EL1` (the GS-base analog).
pub mod percpu;

/// Page-table descriptor format, TTBR control, and TLB maintenance.
pub mod paging;
pub mod serial;
pub mod time;
