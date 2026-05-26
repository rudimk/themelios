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
//! 1. Limine bootloader loads the kernel ELF into higher-half memory
//! 2. Bootloader sets up page tables, stack, and 64-bit long mode
//! 3. Bootloader jumps to `kmain` (our entry point)
//! 4. `kmain` initializes serial output and prints boot message
//! 5. (Future phases) Initialize subsystems and start scheduler

// This is a freestanding binary — no standard library, no main function.
// `no_std` disables the standard library (which requires an OS).
// `no_main` tells the compiler we provide our own entry point via the linker script.
#![no_std]
#![no_main]

// --- Limine boot protocol setup ---
//
// The Limine bootloader communicates with the kernel through "requests":
// static data structures that the bootloader scans for and fills in at boot time.
// These must be placed in special ELF sections so the bootloader can find them.
//
// The `#[used]` attribute prevents the compiler from optimizing away these statics
// (since nothing in our code reads them directly — the bootloader does).
//
// The `#[link_section]` attribute places the static in a specific ELF section,
// which the linker script maps into the binary. The bootloader scans the region
// between the start and end markers for request structures.

use limine::BaseRevision;
use limine::request::{RequestsStartMarker, RequestsEndMarker};

/// Mark the beginning of the Limine requests region in the ELF binary.
/// The bootloader uses this marker to know where to start scanning for requests.
#[used]
#[link_section = ".requests_start_marker"]
static _REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();

/// Mark the end of the Limine requests region.
/// The bootloader stops scanning for requests when it hits this marker.
#[used]
#[link_section = ".requests_end_marker"]
static _REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

/// Declare the base protocol revision we support.
/// The bootloader checks this and confirms it can speak our protocol version.
/// `BaseRevision::new()` requests the latest revision supported by the crate.
/// After boot, `is_supported()` returns true if the bootloader accepted it.
#[used]
#[link_section = ".requests"]
static BASE_REVISION: BaseRevision = BaseRevision::new();

// ----- Kernel subsystem modules -----

/// Architecture-specific code (x86_64, aarch64).
/// Each architecture implements CPU operations, serial I/O, and other
/// hardware-specific functionality behind a common interface.
mod arch;

/// Kernel synchronization primitives.
/// Provides `InterruptMutex` — a spinlock wrapper that disables interrupts
/// while held, preventing deadlocks when interrupt handlers need to acquire
/// the same lock as the interrupted code.
mod sync;

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

// ----- Kernel entry point -----

/// The kernel entry point. Limine bootloader jumps here after loading the
/// kernel into higher-half memory and setting up a valid stack.
///
/// This function is referenced by the linker script (`ENTRY(kmain)`) and
/// must have C calling convention so the bootloader can call it.
/// `#[no_mangle]` prevents Rust from mangling the symbol name.
///
/// By the time `kmain` runs, the Limine bootloader has already:
/// - Loaded the kernel ELF at 0xffffffff80000000 (higher half)
/// - Set up 4-level page tables with identity + higher-half mappings
/// - Allocated and set up a stack
/// - Entered 64-bit long mode
/// - Filled in our boot protocol request structures
#[no_mangle]
extern "C" fn kmain() -> ! {
    // Verify that the bootloader supports our protocol revision.
    // The base revision request was placed in the .requests section above;
    // Limine fills it in at boot time. If the bootloader doesn't support
    // our revision, we can't rely on any boot info being correct.
    assert!(BASE_REVISION.is_supported());

    // Initialize the global serial writer (COM1 at 0x3F8) for debug output.
    // QEMU maps this to the host terminal via the `-serial stdio` flag.
    // After this call, the `println!` macro works everywhere in the kernel.
    #[cfg(target_arch = "x86_64")]
    arch::x86_64::serial::init();

    // Print the boot banner
    println!();
    println!("============================================");
    println!("  ThemeliOS v{}", env!("CARGO_PKG_VERSION"));
    println!("  An experimental capability-based microkernel");
    println!("============================================");
    println!();
    println!("Kernel booted successfully on x86_64.");
    println!("Limine boot protocol revision supported.");
    println!("Build ULID: {}", env!("BUILD_ULID"));
    println!();

    // --- Initialize kernel subsystems ---

    // Set up the Global Descriptor Table and Task State Segment.
    // This replaces Limine's GDT with our own and configures the TSS
    // with an IST stack for the double-fault handler.
    // Must happen before IDT setup (IDT entries reference GDT selectors and IST indices).
    #[cfg(target_arch = "x86_64")]
    arch::x86_64::gdt::init();

    println!();
    println!("Phase 1 in progress. Halting.");

    // Nothing left to do — halt the CPU in a loop.
    hcf();
}

// ----- Panic handler -----

/// Called when the kernel panics (e.g., `assert!` fails, `unwrap()` on None).
/// In a bare-metal environment there's no OS to catch us, so we print the
/// panic message to serial and halt the CPU.
///
/// Disables interrupts immediately to prevent further execution of interrupt
/// handlers that might panic themselves or corrupt state. The `println!` macro
/// is interrupt-safe (uses `InterruptMutex`), so it's fine to call even after
/// CLI — the lock will simply see that interrupts are already disabled.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // Disable interrupts immediately — we don't want any more interrupt
    // handlers running after a panic. This must happen BEFORE printing,
    // in case a timer or other interrupt tries to use panicked state.
    #[cfg(target_arch = "x86_64")]
    arch::x86_64::cpu::cli();

    // Print the panic info to the global serial writer. If the serial port
    // hasn't been initialized yet (panic very early in boot), the output
    // is silently dropped — but we halt either way.
    println!();
    println!("!!! KERNEL PANIC !!!");
    println!("{}", info);

    hcf();
}

/// Halt and Catch Fire — stop the CPU permanently.
///
/// Disables interrupts (CLI) and then enters an infinite halt loop. With
/// interrupts disabled, `hlt` will never wake up — the CPU is fully stopped.
/// The loop is a safety net in case an NMI (non-maskable interrupt) wakes
/// the CPU despite CLI; we just halt again.
fn hcf() -> ! {
    // Disable interrupts so that `hlt` truly halts — without CLI, any
    // pending interrupt would wake the CPU from `hlt` and we'd resume
    // execution, which we don't want after a panic or clean shutdown.
    #[cfg(target_arch = "x86_64")]
    arch::x86_64::cpu::cli();

    loop {
        #[cfg(target_arch = "x86_64")]
        arch::x86_64::cpu::halt();

        #[cfg(not(target_arch = "x86_64"))]
        core::hint::spin_loop();
    }
}
