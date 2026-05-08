//! # x86_64 architecture support
//!
//! This module contains all code specific to the x86_64 (Intel/AMD 64-bit)
//! architecture. It handles:
//!
//! - **Boot sequence**: Transition from bootloader to kernel, stack setup
//! - **GDT/IDT**: Global Descriptor Table and Interrupt Descriptor Table
//! - **Paging**: 4-level page tables (PML4 → PDPT → PD → PT)
//! - **Interrupts**: APIC configuration, exception handlers, IRQ routing
//! - **Serial output**: 16550 UART (COM1) for early debug printing
//! - **Context switching**: Register save/restore for task switching
//!
//! ## Memory model
//!
//! x86_64 uses 4-level paging with 48-bit virtual addresses (or 5-level
//! with 57-bit, which we may support later). Each page table entry is 8 bytes,
//! each table has 512 entries, and the standard page size is 4 KiB.
//!
//! ## Boot protocol
//!
//! The bootloader (TBD — likely Limine or a custom UEFI loader) loads the
//! kernel ELF into memory, sets up an initial page table and stack, and
//! jumps to our entry point with boot information (memory map, framebuffer, etc.).

// Sub-modules will be added as Phase 0 implementation proceeds:
// pub mod boot;    — entry point, early init
// pub mod gdt;     — Global Descriptor Table setup
// pub mod idt;     — Interrupt Descriptor Table, exception handlers
// pub mod serial;  — 16550 UART driver for debug output
// pub mod paging;  — page table management
