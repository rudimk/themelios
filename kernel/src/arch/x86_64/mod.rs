//! # x86_64 architecture support
//!
//! This module contains all code specific to the x86_64 (Intel/AMD 64-bit)
//! architecture. It handles:
//!
//! - **CPU operations**: I/O port access, halt, and other low-level instructions
//! - **Serial output**: 16550 UART (COM1) for early debug printing
//!
//! ## Memory model
//!
//! x86_64 uses 4-level paging with 48-bit virtual addresses (or 5-level
//! with 57-bit, which we may support later). Each page table entry is 8 bytes,
//! each table has 512 entries, and the standard page size is 4 KiB.
//!
//! ## Boot protocol
//!
//! The Limine bootloader loads the kernel ELF into the higher half
//! (0xffffffff80000000), sets up initial page tables and a stack, then
//! jumps to our `kmain` entry point.

/// Low-level CPU instructions (I/O ports, halt, etc.).
pub mod cpu;

/// 16550 UART serial driver for debug output via COM1.
pub mod serial;

/// Global Descriptor Table and Task State Segment setup.
/// Defines kernel code/data segments and the TSS (with IST stacks for
/// critical exception handlers like double fault).
pub mod gdt;

/// Interrupt Descriptor Table and exception handlers.
/// Handles CPU exceptions (divide error, page fault, GPF, double fault, etc.)
/// and hardware IRQs (timer, serial, etc.) via ISR stubs and a common handler.
pub mod idt;

// Future sub-modules:
// pub mod paging;  — page table management
