//! # ThemeliOS Kernel
//!
//! The core kernel for ThemeliOS, an experimental capability-based microkernel
//! designed for secure container workloads. This is a bare-metal kernel — it runs
//! directly on hardware (or a hypervisor) with no underlying OS.
//!
//! ## Architecture
//!
//! ThemeliOS is a microkernel. Only the bare minimum runs in kernel space:
//! - Memory management (physical and virtual)
//! - Process scheduling
//! - Inter-process communication (IPC)
//! - Capability enforcement
//!
//! Everything else — drivers, filesystems, networking — runs in userspace
//! and communicates with the kernel via IPC and capabilities.
//!
//! ## Boot Flow
//!
//! 1. Bootloader loads the kernel and hands off control
//! 2. Architecture-specific init (`arch::init`) sets up CPU state, page tables, interrupts
//! 3. Kernel main (`kmain`) initializes subsystems in order
//! 4. Scheduler starts and begins running userspace processes

// This is a freestanding binary — no standard library, no main function.
// The `no_std` attribute disables the standard library (which requires an OS).
// The `no_main` attribute tells the compiler we provide our own entry point.
#![no_std]
#![no_main]

// ----- Kernel subsystem modules -----

/// Architecture-specific code (x86_64, aarch64).
/// Each architecture implements CPU init, interrupt handling, paging, and other
/// hardware-specific functionality behind a common interface.
mod arch;

/// Memory management subsystem.
/// Handles physical frame allocation, virtual address spaces (page tables),
/// and the kernel heap allocator.
mod mm;

/// Process scheduler.
/// Manages kernel and userspace tasks, implements scheduling policies,
/// and handles context switching.
mod sched;

/// Capability system.
/// The core security primitive of ThemeliOS. All resource access is mediated
/// by unforgeable capability tokens — processes can only use resources they've
/// been explicitly granted access to.
mod cap;

/// Inter-process communication.
/// Message passing between processes. Since ThemeliOS is a microkernel,
/// IPC is the backbone — drivers, filesystems, and services all communicate
/// through IPC channels.
mod ipc;

/// Device drivers.
/// VirtIO drivers for block devices, network interfaces, and console.
/// Platform-specific drivers for timers, interrupt controllers, etc.
mod drivers;

/// Filesystem layer.
/// Read-only root filesystem for the immutable OS image, plus ephemeral
/// writable layers for container runtime state.
mod fs;

/// Network stack.
/// TCP/IP implementation for container networking and the management API.
mod net;

// ----- Panic handler -----

/// Called when the kernel panics. In a bare-metal environment there's no OS to
/// catch us, so we halt the CPU. In later phases this will print diagnostic
/// information to the serial console before halting.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // TODO(phase-0): Print panic info to serial console before halting.
    loop {
        // Halt the CPU to save power while we spin.
        // Architecture-specific halt will be used once arch module is implemented.
        core::hint::spin_loop();
    }
}

/// The architecture-independent kernel entry point. Called by the arch-specific
/// boot code after basic hardware initialization is complete.
///
/// By the time `kmain` runs, the following should be set up:
/// - A valid stack
/// - Serial/console output (for debug printing)
/// - Basic interrupt handling
/// - Identity-mapped or higher-half page tables
///
/// `kmain` then initializes the rest of the kernel subsystems and starts
/// the scheduler.
fn kmain() -> ! {
    // This is where the kernel comes to life. Each phase will add
    // initialization steps here:
    //
    // Phase 0: Print "Hello from ThemeliOS" to serial console
    // Phase 1: Initialize memory allocator, scheduler, interrupts
    // Phase 2: Initialize capability system, create first processes
    // Phase 3: Mount root filesystem, initialize block driver
    // Phase 4: Bring up network stack
    // Phase 5: Start container runtime
    // Phase 6: Start management API server

    loop {
        core::hint::spin_loop();
    }
}
