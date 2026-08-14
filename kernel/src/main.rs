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

// Enable the `alloc` crate so we can use Vec, Box, String, etc.
// This requires a #[global_allocator] to be defined (see mm/heap.rs)
// and `-Zbuild-std=core,alloc` in the build flags (see xtask).
extern crate alloc;

// On x86_64 the global allocator lives in `mm::heap`. On aarch64 the memory subsystem
// is not yet ported (Phase 7.1), so provide a placeholder allocator that keeps the
// crate linkable. The Phase 7.0b boot-to-banner path performs **no** heap allocation;
// any allocation before 7.1 is a bug and panics loudly here rather than corrupting.
#[cfg(target_arch = "aarch64")]
mod aarch64_alloc_placeholder {
    use core::alloc::{GlobalAlloc, Layout};
    struct NullAlloc;
    // SAFETY: never hands out memory; alloc always diverges, dealloc is a no-op.
    unsafe impl GlobalAlloc for NullAlloc {
        unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
            panic!("aarch64: heap allocation attempted before the Phase 7.1 allocator");
        }
        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
    }
    #[global_allocator]
    static ALLOCATOR: NullAlloc = NullAlloc;
}

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
use limine::request::{
    RequestsStartMarker, RequestsEndMarker,
    MemoryMapRequest, HhdmRequest, ExecutableAddressRequest,
};

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

/// Request the memory map from the bootloader.
/// The response contains an array of memory region descriptors, each with a
/// physical base address, length, and type (USABLE, RESERVED, BOOTLOADER_RECLAIMABLE,
/// EXECUTABLE_AND_MODULES, etc.). We use this to initialize the frame allocator.
#[used]
#[link_section = ".requests"]
static MEMORY_MAP_REQUEST: MemoryMapRequest = MemoryMapRequest::new();

/// Request the Higher-Half Direct Map (HHDM) offset from the bootloader.
/// Limine maps ALL physical memory into the virtual address space at a
/// high offset: virt_addr = phys_addr + hhdm_offset. This gives the kernel
/// a convenient way to access any physical address without setting up custom
/// page table entries. The offset is typically 0xffff800000000000 or similar.
#[used]
#[link_section = ".requests"]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

/// Request the physical and virtual base addresses of the loaded kernel image.
/// Used for diagnostics and to verify that kernel frames are classified as
/// EXECUTABLE_AND_MODULES (not USABLE) in the memory map — ensuring the
/// frame allocator never hands out kernel memory.
#[used]
#[link_section = ".requests"]
static EXECUTABLE_ADDRESS_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();

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
#[cfg(target_arch = "x86_64")]
mod mm;

/// Process scheduler.
/// Manages kernel and userspace tasks, implements scheduling policies,
/// and handles context switching.
#[cfg(target_arch = "x86_64")]
mod sched;

/// Interactive debug shell.
/// Runs as a scheduler task, provides commands for inspecting memory,
/// tasks, and kernel state at runtime via the serial console.
#[cfg(target_arch = "x86_64")]
mod shell;

/// Automated test harness.
/// When built with `--features test`, the kernel runs self-tests instead of
/// the interactive shell and exits QEMU with a pass/fail exit code.
#[cfg(feature = "test")]
#[cfg(target_arch = "x86_64")]
mod test_runner;

/// Capability system.
/// The core security primitive of ThemeliOS. All resource access is mediated
/// by unforgeable capability tokens — processes can only use resources they've
/// been explicitly granted access to.
#[cfg(target_arch = "x86_64")]
mod cap;

/// Process abstraction.
/// Bundles an address space, capability space, and task list into a single
/// unit of isolation. The kernel process (PID 0) owns all boot-time tasks.
#[cfg(target_arch = "x86_64")]
mod process;

/// Inter-process communication.
/// Message passing between processes. Since ThemeliOS is a microkernel,
/// IPC is the backbone — drivers, filesystems, and services all communicate
/// through IPC channels.
#[cfg(target_arch = "x86_64")]
mod ipc;

/// Audit logging.
/// A tamper-evident ring buffer recording all security-relevant kernel
/// operations: capability create/grant/revoke, IPC send/receive, and
/// syscall invocations. Kernel-internal only in Phase 2.
#[cfg(target_arch = "x86_64")]
mod audit;

/// Device drivers.
/// VirtIO drivers for block devices, network interfaces, and console.
/// Platform-specific drivers for timers, interrupt controllers, etc.
#[cfg(target_arch = "x86_64")]
mod drivers;

/// Filesystem layer.
/// Read-only root filesystem for the immutable OS image, plus ephemeral
/// writable layers for container runtime state.
#[cfg(target_arch = "x86_64")]
mod fs;

/// Network stack.
/// TCP/IP implementation for container networking and the management API.
#[cfg(target_arch = "x86_64")]
mod net;

/// Linux compatibility layer (Phase 5): ELF loader and, later, the Linux syscall
/// personality that lets ThemeliOS run unmodified OCI container binaries.
#[cfg(target_arch = "x86_64")]
mod linux;

/// OCI / docker-save image unpacking (Phase 5.4): tar + JSON + layer assembly.
/// Dependency-light (alloc-only) so it can lift into a ring-3 `oci-server`. Used
/// by the Phase 5.5 container runtime.
#[allow(dead_code)] // a complete tar/JSON reader; callers use a subset
#[cfg(target_arch = "x86_64")]
mod oci;

/// Container runtime (Phase 5.5): unpack an image, assemble its rootfs, and launch
/// its entrypoint as a capability-isolated Linux process.
#[cfg(target_arch = "x86_64")]
mod container;

/// Minimal HTTP/1.1 primitives (Phase 6.0): shared byte-scanning helpers plus an
/// HTTP **request** parser for the management API. Dependency-light (alloc-only)
/// so it lifts into the ring-3 `api-server`; the registry client reuses the shared
/// helpers for its response parsing.
#[allow(dead_code)] // request parser + helpers; the api-server (6.5) uses the full surface
#[cfg(target_arch = "x86_64")]
mod http;

/// Container management ABI (Phase 6.3): the capability-guarded ring-0 surface the
/// ring-3 `api-server` drives to serve the Docker Engine API — list/inspect/create/
/// start/stop/logs/node_info + an inbound-TCP listener, each gated on the
/// `CapType::Management` sentinel and audited. The syscall/IPC wrappers and the
/// sentinel-cap grant land in Phase 6.4.
#[allow(dead_code)] // driven by the api-server (6.4/6.5); tested directly in 6.3
#[cfg(target_arch = "x86_64")]
mod mgmt;

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
/// Kernel entry point (linker `ENTRY(kmain)`). Dispatches to the per-architecture
/// bring-up path. Both branches diverge, so exactly one architecture's boot code is
/// compiled into any given build.
#[no_mangle]
extern "C" fn kmain() -> ! {
    // aarch64 (Phase 7.0b): minimal boot-to-banner. Limine hands off at EL1 with the
    // MMU on; the aarch64 path enables FP, maps the PL011, and prints a banner. The
    // full bring-up (mm/sched/etc.) lands in 7.1+.
    #[cfg(target_arch = "aarch64")]
    {
        let hhdm = HHDM_REQUEST.get_response().map(|r| r.offset()).unwrap_or(0);
        let (phys_base, virt_base) = EXECUTABLE_ADDRESS_REQUEST
            .get_response()
            .map(|r| (r.physical_base(), r.virtual_base()))
            .unwrap_or((0, 0));
        crate::arch::aarch64::boot::kmain_aarch64(
            hhdm,
            crate::arch::aarch64::boot::KernelAddr { phys_base, virt_base },
        );
    }

    #[cfg(target_arch = "x86_64")]
    kmain_x86_64()
}

/// x86_64 kernel bring-up: serial, GDT/IDT, memory, scheduler, drivers, servers, and
/// the interactive shell. (The original `kmain` body, unchanged.)
#[cfg(target_arch = "x86_64")]
fn kmain_x86_64() -> ! {
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

    // Set up the Interrupt Descriptor Table with exception handlers.
    // After this, CPU exceptions (page fault, GPF, double fault, etc.) will
    // be caught and print diagnostic info instead of silently triple-faulting.
    // Must happen after GDT (IDT entries reference kernel code segment selector).
    #[cfg(target_arch = "x86_64")]
    arch::x86_64::idt::init();

    // Test the breakpoint exception handler. INT3 should trigger vector 3,
    // the handler should print diagnostics and return, and execution should
    // continue here. If this hangs or triple-faults, the IDT is broken.
    println!();
    println!("Testing breakpoint exception (INT3)...");
    #[cfg(target_arch = "x86_64")]
    arch::x86_64::cpu::int3();
    println!("Breakpoint handler returned — IDT is working!");

    // --- Memory management initialization ---
    //
    // Extract all needed info from Limine boot protocol responses and
    // initialize the memory management subsystem. This must happen before
    // interrupts are enabled, and before anything that needs physical frames.
    //
    // Limine responses live in bootloader-reclaimable memory, so we extract
    // everything we need here and never reference the responses again.

    // Get the HHDM offset — the virtual address offset where Limine maps
    // all physical memory. Every physical address access in the kernel goes
    // through this: virt = phys + hhdm_offset.
    let hhdm_response = HHDM_REQUEST.get_response()
        .expect("Limine HHDM request not answered");
    let hhdm_offset = hhdm_response.offset();
    mm::init_hhdm(hhdm_offset);
    println!("HHDM offset: {:#x}", hhdm_offset);

    // Get the kernel's physical and virtual base addresses (for diagnostics
    // and to verify kernel frames aren't classified as USABLE).
    let exec_response = EXECUTABLE_ADDRESS_REQUEST.get_response()
        .expect("Limine ExecutableAddress request not answered");
    println!("Kernel physical base: {:#x}", exec_response.physical_base());
    println!("Kernel virtual base:  {:#x}", exec_response.virtual_base());

    // Get the memory map and print it for diagnostics.
    let mmap_response = MEMORY_MAP_REQUEST.get_response()
        .expect("Limine MemoryMap request not answered");
    let entries = mmap_response.entries();

    println!();
    println!("Memory map ({} entries):", entries.len());
    for entry in entries.iter() {
        let entry_type = match entry.entry_type {
            limine::memory_map::EntryType::USABLE => "Usable",
            limine::memory_map::EntryType::RESERVED => "Reserved",
            limine::memory_map::EntryType::ACPI_RECLAIMABLE => "ACPI Reclaimable",
            limine::memory_map::EntryType::ACPI_NVS => "ACPI NVS",
            limine::memory_map::EntryType::BAD_MEMORY => "Bad Memory",
            limine::memory_map::EntryType::BOOTLOADER_RECLAIMABLE => "Bootloader Reclaimable",
            limine::memory_map::EntryType::EXECUTABLE_AND_MODULES => "Executable & Modules",
            limine::memory_map::EntryType::FRAMEBUFFER => "Framebuffer",
            _ => "Unknown",
        };
        let size_kb = entry.length / 1024;
        println!("  {:#012x} - {:#012x} ({:>7} KiB) {}",
            entry.base,
            entry.base + entry.length,
            size_kb,
            entry_type,
        );
    }

    // Initialize the physical frame allocator from the memory map.
    mm::frame::init(entries, hhdm_offset, exec_response.physical_base());

    let free = mm::frame::free_frame_count();
    let total = mm::frame::total_frame_count();
    let free_mib = (free as u64 * mm::PAGE_SIZE) / (1024 * 1024);
    let total_mib = (total as u64 * mm::PAGE_SIZE) / (1024 * 1024);
    println!();
    println!("Frame allocator initialized:");
    println!("  Total frames: {} ({} MiB address space)", total, total_mib);
    println!("  Free frames:  {} ({} MiB usable)", free, free_mib);

    // Quick smoke test: allocate and deallocate a frame to verify the
    // allocator works end-to-end.
    let test_frame = mm::frame::allocate_frame()
        .expect("Failed to allocate test frame");
    println!("  Test alloc:   {} (OK)", test_frame);
    mm::frame::deallocate_frame(test_frame);
    println!("  Test dealloc: {} (OK)", test_frame);

    let free_after = mm::frame::free_frame_count();
    assert!(free_after == free, "Free count mismatch after alloc/dealloc cycle");
    println!("  Alloc/dealloc cycle verified — count restored to {}", free_after);

    // --- Page table initialization ---
    //
    // Switch from Limine's page tables to our own: a fresh root that clones Limine's
    // kernel mappings (HHDM + kernel image) and is loaded into the architecture's
    // translation-base register. After this we own the page tables and can:
    // - Map/unmap individual pages (guard pages, the kernel heap, MMIO windows)
    // - Create per-process address spaces (Phase 2 isolation)
    // - Reclaim bootloader-reclaimable memory (Sub-phase 2.2)
    //
    // This must run *before* heap init, because the heap is mapped into its own
    // virtual window rather than the HHDM (see `mm::heap::HEAP_VIRT_BASE`) and so
    // needs working page tables. It allocates only from the frame allocator and
    // prints via serial, so it has no heap dependency of its own.
    mm::page_table::init();

    // Initialize the kernel heap (1 MiB) in its own mapped virtual window.
    // After this, Vec, Box, String, and all alloc types are usable.
    mm::heap::init();

    // Smoke test the heap with Vec, Box, and String to verify dynamic allocation.
    {
        use alloc::vec::Vec;
        use alloc::boxed::Box;
        use alloc::string::String;

        let mut v = Vec::new();
        for i in 0..10 {
            v.push(i);
        }
        assert!(v.iter().sum::<i32>() == 45, "Vec sum mismatch");

        let b = Box::new(42u64);
        assert!(*b == 42, "Box value mismatch");

        let s = String::from("ThemeliOS heap works!");
        assert!(s.len() == 21, "String length mismatch");

        println!("Heap smoke test passed (Vec, Box, String all working)");
        println!("  Heap used: {} bytes, free: {} bytes",
            mm::heap::used(), mm::heap::free());
    }

    // Save the Limine memory map into kernel-owned storage before we reclaim
    // bootloader memory (which would invalidate Limine's response structures).
    mm::save_memory_map(entries);


    // Reclaim bootloader-reclaimable memory now that we own the GDT, page
    // tables, and stack. Limine's boot structures in those regions are no
    // longer referenced — the frames can be returned to the free pool.
    let free_before_reclaim = mm::frame::free_frame_count();
    let reclaimed = mm::reclaim_bootloader_memory();
    let free_after_reclaim = mm::frame::free_frame_count();
    let reclaimed_kib = (reclaimed as u64 * mm::PAGE_SIZE) / 1024;
    println!(
        "[reclaim] {} frames ({} KiB) reclaimed from bootloader regions ({} -> {} free)",
        reclaimed, reclaimed_kib, free_before_reclaim, free_after_reclaim
    );

    // Initialize the 8259 PIC and remap hardware IRQs to vectors 32-47.
    // Must happen after IDT setup (so IRQ vectors have handlers installed).
    // After this, all IRQ lines are masked — individual IRQs are unmasked
    // as their devices are initialized.
    #[cfg(target_arch = "x86_64")]
    arch::x86_64::pic::init();

    // Configure the PIT to fire IRQ0 at ~100 Hz, then unmask IRQ0 so the
    // timer interrupts can reach the CPU.
    #[cfg(target_arch = "x86_64")]
    {
        arch::x86_64::pit::init();
        arch::x86_64::pic::unmask(0);
    }

    // --- PCI bus enumeration ---
    //
    // Walk the PCI bus to discover the virtual hardware QEMU exposes — most
    // importantly the VirtIO block device we'll drive in Phase 3. This is a
    // read-only scan of PCI config space; it allocates a Vec of discovered
    // devices, so it must run after heap init. It has no dependency on the
    // scheduler or interrupts, so we do it here.
    #[cfg(target_arch = "x86_64")]
    drivers::pci::scan();

    // --- Scheduler initialization ---
    //
    // Set up the preemptive round-robin scheduler. This creates the bootstrap
    // task (representing this execution context) and the idle task. After
    // this, spawn() can be used to create new tasks.
    //
    // Must happen after heap init (scheduler uses Vec/VecDeque) and after
    // PIC/PIT init (so timer-driven preemption will work once we enable
    // interrupts).
    sched::init();

    // --- Process table initialization ---
    //
    // Create the process table and the kernel process (PID 0). The kernel
    // process owns all boot-time tasks (bootstrap, idle). The shell task
    // is assigned later when it's spawned.
    //
    // Must happen after scheduler init (so we know the bootstrap and idle
    // task IDs) and before syscall init.
    process::init();
    // Assign existing tasks to the kernel process (PID 0).
    // Task 0 = bootstrap (main), Task 1 = idle.
    process::assign_task_to_kernel(0);
    process::assign_task_to_kernel(1);

    // --- Syscall/sysret initialization ---
    //
    // Configure the MSRs that enable the fast syscall path: EFER.SCE,
    // STAR (segment selectors), LSTAR (entry point), FMASK (interrupt mask),
    // and IA32_KERNEL_GS_BASE (PerCpu struct address). After this, ring 3
    // code can use `syscall` to enter the kernel.
    //
    // Must happen after GDT init (segment selectors must be valid) and
    // after scheduler init (PerCpu.kernel_stack_top is set per-task).
    #[cfg(target_arch = "x86_64")]
    arch::x86_64::syscall::init();

    // Enable maskable interrupts. From this point on:
    // - PIT timer (IRQ0) fires at ~100 Hz for preemptive scheduling
    // - COM1 serial (IRQ4) fires when the user types in the QEMU terminal
    #[cfg(target_arch = "x86_64")]
    crate::arch::irq::enable();

    // --- Test mode vs interactive mode ---
    //
    // When built with --features test, run automated tests and exit QEMU
    // instead of starting the interactive shell.
    #[cfg(feature = "test")]
    {
        println!();
        println!("Running in test mode...");
        test_runner::run_tests();
        // run_tests() exits QEMU — never returns
    }

    // --- Normal mode: interactive shell ---
    #[cfg(not(feature = "test"))]
    {
        // Initialize the interactive debug shell. This enables serial receive
        // interrupts (IRQ4), unmasks IRQ4 in the PIC, and spawns the shell task.
        // The shell task blocks until input arrives via IRQ4.
        shell::init();

        // Boot the first userspace init process. This creates a ring 3 process
        // with its own address space, maps code and stack pages, grants IPC
        // capabilities, and spawns the init task plus a kernel-side server.
        process::init::start();

        // Bring up the storage stack: discover the VirtIO disks, spawn the
        // block + filesystem servers (SquashFS root under an overlay, ext2
        // data volume), and register the "/" and "/data" mounts. The debug
        // shell's filesystem commands operate on these mounts.
        fs::boot_storage();
        fs::print_mount_status();

        // Bring up the network stack: discover the VirtIO NIC, start the kernel
        // net service, and spawn the ring-3 net server (smoltcp).
        net::boot_net();

        // Bring up the container management API (Phase 6.5): a ring-3 server
        // holding the Management sentinel cap, serving the Docker Engine API on the
        // default port. It listens via the management ABI once the net server is up
        // (it retries while that comes up). Only reachable when a route/hostfwd is
        // configured — unauthenticated for now (auth lands in 6.6), so it must not
        // be exposed on a real NIC until then. This spawn is not exercised by CI
        // (the test path runs `test_api_server` instead); the binary, grant, listen,
        // and serve are all covered there.
        {
            let api_ep = ipc::create_endpoint("api-server");
            process::server::spawn_server(process::server::ServerConfig {
                name: "api-server",
                binary: process::embedded::API_SERVER,
                fs_endpoint: api_ep,
                block_endpoint: 0,
                shared: None,
                client_shared: None,
                heap_bytes: 256 * 1024,
                arg0: 0, // 0 → the api-server's default Docker port (2375)
                arg1: 0,
                filesystem_mount: None,
                grant_management: true,
            });
        }

        println!();
        println!("Interrupts enabled — scheduler is running.");
        println!("Type 'help' for available commands.");

        // The main task (bootstrap, task 0) enters an idle loop. The timer will
        // preempt it and schedule other tasks. The shell task handles input.
        loop {
            #[cfg(target_arch = "x86_64")]
            crate::arch::irq::halt();

            #[cfg(not(target_arch = "x86_64"))]
            core::hint::spin_loop();
        }
    }
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
    crate::arch::irq::disable();

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
    crate::arch::irq::disable();

    loop {
        #[cfg(target_arch = "x86_64")]
        crate::arch::irq::halt();

        #[cfg(not(target_arch = "x86_64"))]
        core::hint::spin_loop();
    }
}
