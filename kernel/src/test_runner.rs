//! # Automated test harness
//!
//! Runs a suite of kernel self-tests when built with `--features test`.
//! Each test function returns `Ok(())` on success or `Err(message)` on failure.
//! The harness prints `[PASS]` or `[FAIL]` for each test, then exits QEMU
//! with a mapped exit code:
//!
//! - All tests pass → `exit_qemu(0x01)` → QEMU exit code `3` → success
//! - Any test fails → `exit_qemu(0x00)` → QEMU exit code `1` → failure
//!
//! ## Test conventions
//!
//! Tests run inside `kmain` after all subsystems are initialized. They have
//! access to the frame allocator, heap, and scheduler, but the shell is not
//! started (there's no interactive input during test runs).
//!
//! Each test is a function `fn() -> Result<(), &'static str>` registered in
//! the `TESTS` array. Tests should be deterministic and complete quickly
//! (the xtask runner has a 30-second timeout).

extern crate alloc;

use crate::println;

/// A single test case: a name for reporting and a function to execute.
struct TestCase {
    name: &'static str,
    func: fn() -> Result<(), &'static str>,
}

/// The test suite. Each entry is run in order; failures are reported
/// but don't abort remaining tests (all tests always run).
static TESTS: &[TestCase] = &[
    TestCase { name: "test_boot",            func: test_boot },
    TestCase { name: "test_frame_allocator",  func: test_frame_allocator },
    TestCase { name: "test_heap",             func: test_heap },
    TestCase { name: "test_scheduler",        func: test_scheduler },
    TestCase { name: "test_interrupts",       func: test_interrupts },
    TestCase { name: "test_page_tables",      func: test_page_tables },
];

/// Run all tests and exit QEMU with the appropriate code.
///
/// Called from `kmain` when the kernel is built with `--features test`.
/// This function does not return — it terminates QEMU via `exit_qemu()`.
pub fn run_tests() -> ! {
    println!();
    println!("========================================");
    println!("  ThemeliOS Test Suite");
    println!("========================================");
    println!();

    let mut passed = 0usize;
    let mut failed = 0usize;

    for test in TESTS {
        match (test.func)() {
            Ok(()) => {
                println!("[PASS] {}", test.name);
                passed += 1;
            }
            Err(reason) => {
                println!("[FAIL] {}: {}", test.name, reason);
                failed += 1;
            }
        }
    }

    println!();
    println!("----------------------------------------");
    println!("  Results: {} passed, {} failed, {} total",
        passed, failed, TESTS.len());
    println!("----------------------------------------");
    println!();

    #[cfg(target_arch = "x86_64")]
    if failed == 0 {
        println!("All tests passed — exiting QEMU with success.");
        crate::arch::x86_64::cpu::exit_qemu(0x01);
    } else {
        println!("Some tests failed — exiting QEMU with failure.");
        crate::arch::x86_64::cpu::exit_qemu(0x00);
    }

    // Fallback for non-x86_64 (shouldn't be reached in Phase 1)
    #[cfg(not(target_arch = "x86_64"))]
    loop {
        core::hint::spin_loop();
    }
}

// ========================================================================
// Test functions
// ========================================================================

/// Smoke test: the kernel booted and reached the test runner.
///
/// If we're executing this code, serial output works, the GDT/IDT are
/// loaded, memory management is initialized, and the scheduler is running.
/// This test can only fail if the kernel panicked before reaching here.
fn test_boot() -> Result<(), &'static str> {
    Ok(())
}

/// Test the physical frame allocator.
///
/// Verifies:
/// 1. Frames can be allocated
/// 2. Allocated frames have distinct physical addresses
/// 3. Frames can be deallocated
/// 4. Free count decreases on alloc and increases on dealloc
fn test_frame_allocator() -> Result<(), &'static str> {
    use crate::mm;

    let initial_free = mm::frame::free_frame_count();
    if initial_free == 0 {
        return Err("no free frames available");
    }

    // Allocate several frames and verify they're distinct
    const NUM_FRAMES: usize = 8;
    let mut frames = alloc::vec::Vec::new();

    for i in 0..NUM_FRAMES {
        let frame = mm::frame::allocate_frame()
            .ok_or("allocate_frame returned None")?;

        // Verify this frame hasn't been returned before
        for (j, &prev) in frames.iter().enumerate() {
            if prev == frame {
                // Can't use format! in a const &str return, but this path
                // means the allocator returned a duplicate address.
                let _ = (i, j); // suppress unused warnings
                return Err("allocate_frame returned duplicate address");
            }
        }

        frames.push(frame);
    }

    // Verify free count decreased
    let after_alloc_free = mm::frame::free_frame_count();
    if after_alloc_free != initial_free - NUM_FRAMES {
        return Err("free count did not decrease by expected amount");
    }

    // Deallocate all frames
    for frame in &frames {
        mm::frame::deallocate_frame(*frame);
    }

    // Verify free count restored
    let after_dealloc_free = mm::frame::free_frame_count();
    if after_dealloc_free != initial_free {
        return Err("free count not restored after deallocation");
    }

    Ok(())
}

/// Test the kernel heap allocator.
///
/// Verifies Vec, Box, and String work correctly via the global allocator.
fn test_heap() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;

    // Test Vec: push values and verify sum
    let mut v: Vec<i32> = Vec::new();
    for i in 0..100 {
        v.push(i);
    }
    let sum: i32 = v.iter().sum();
    if sum != 4950 {
        return Err("Vec sum incorrect (expected 4950)");
    }

    // Test Box: heap-allocate a value and verify
    let b = Box::new(0xDEAD_BEEFu64);
    if *b != 0xDEAD_BEEF {
        return Err("Box value mismatch");
    }

    // Test String: construct and verify length/content
    let s = String::from("ThemeliOS test string");
    if s.len() != 21 {
        return Err("String length mismatch");
    }
    if !s.starts_with("ThemeliOS") {
        return Err("String content mismatch");
    }

    // Test larger allocation: Vec of 1000 Boxes
    let mut boxes: Vec<Box<u64>> = Vec::new();
    for i in 0..1000u64 {
        boxes.push(Box::new(i * i));
    }
    if *boxes[999] != 999 * 999 {
        return Err("large allocation: Box[999] value mismatch");
    }

    Ok(())
}

/// Test the scheduler.
///
/// Spawns several tasks that each increment a shared atomic counter, then
/// yields repeatedly until all tasks complete. Verifies the counter reaches
/// the expected total — proving all tasks ran to completion.
fn test_scheduler() -> Result<(), &'static str> {
    use core::sync::atomic::{AtomicUsize, Ordering};
    use crate::sched;

    /// Number of test tasks to spawn.
    const TASK_COUNT: usize = 5;
    /// Each task increments the counter this many times.
    const INCREMENTS_PER_TASK: usize = 10;

    /// Shared counter — all test tasks increment this.
    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Entry function for scheduler test tasks.
    fn counter_task() {
        for _ in 0..INCREMENTS_PER_TASK {
            TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
            crate::sched::yield_now();
        }
    }

    // Reset counter (in case tests run multiple times somehow)
    TEST_COUNTER.store(0, Ordering::SeqCst);

    // Spawn tasks
    for _ in 0..TASK_COUNT {
        sched::spawn("test-sched", counter_task);
    }

    // Yield repeatedly to let the spawned tasks run. The timer interrupt
    // will also preempt us, but explicit yields ensure forward progress
    // even if the timer rate is very low. We bound iterations to avoid
    // infinite loops on a broken scheduler.
    let expected = TASK_COUNT * INCREMENTS_PER_TASK;
    for _ in 0..10_000 {
        if TEST_COUNTER.load(Ordering::SeqCst) >= expected {
            break;
        }
        sched::yield_now();
    }

    let final_count = TEST_COUNTER.load(Ordering::SeqCst);
    if final_count != expected {
        return Err("scheduler test: not all tasks completed");
    }

    Ok(())
}

/// Test that timer interrupts are firing.
///
/// Reads the tick counter, halts the CPU a few times (each `hlt` wakes on
/// the next timer interrupt), then verifies the counter advanced — proving
/// the PIT timer and IRQ0 handler are working.
fn test_interrupts() -> Result<(), &'static str> {
    #[cfg(target_arch = "x86_64")]
    {
        use crate::arch::x86_64::idt::tick_count;
        use crate::arch::x86_64::cpu;

        let before = tick_count();

        // halt() suspends the CPU until the next interrupt. At 100 Hz,
        // each halt wakes after ~10ms when the timer fires. Five halts
        // gives the timer plenty of chances to advance the tick counter.
        for _ in 0..5 {
            cpu::halt();
        }

        let after = tick_count();

        if after <= before {
            return Err("tick counter did not advance");
        }

        Ok(())
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        Err("interrupt test not implemented for this architecture")
    }
}

/// Test the page table manager.
///
/// Verifies:
/// 1. The kernel is running on custom page tables (not Limine's)
/// 2. Kernel addresses translate correctly (HHDM + kernel image)
/// 3. Mapping a new page works and the data is accessible
/// 4. Unmapping a page works and translate returns None
/// 5. Creating and destroying a user address space doesn't leak frames
fn test_page_tables() -> Result<(), &'static str> {
    use crate::mm;
    use crate::mm::addr::{PhysAddr, VirtAddr};
    use crate::mm::page_table::{AddressSpace, PageFlags, kernel_address_space};

    // 1. Verify we're on custom page tables (CR3 matches our kernel PML4)
    let kernel_as = kernel_address_space();
    let cr3 = crate::arch::x86_64::cpu::read_cr3() & !0xFFF;
    if cr3 != kernel_as.pml4_phys().as_u64() {
        core::mem::forget(kernel_as);
        return Err("CR3 does not match kernel PML4");
    }

    // 2. Verify kernel HHDM addresses translate correctly.
    // Pick a known physical address (the first page of usable memory)
    // and verify translate() resolves the HHDM virtual address correctly.
    let hhdm = mm::hhdm_offset();
    let test_phys = PhysAddr::new(0x100000); // 1 MiB — should be in USABLE range
    let test_virt = VirtAddr::new(test_phys.as_u64() + hhdm);
    match kernel_as.translate(test_virt) {
        Some(resolved) => {
            // The resolved physical address should match (including page offset = 0).
            if resolved.as_u64() != test_phys.as_u64() {
                core::mem::forget(kernel_as);
                return Err("translate: HHDM address resolved to wrong physical address");
            }
        }
        None => {
            core::mem::forget(kernel_as);
            return Err("translate: HHDM address not mapped");
        }
    }

    // 3. Map a new page, write to it, read back.
    // Pick a virtual address in the kernel space that's unlikely to be mapped.
    // We'll use an address in the upper half but not in the HHDM or kernel image.
    // PML4 index 510, PDP index 0, PD index 0, PT index 0 → 0xFFFFFF0000000000
    let map_virt = VirtAddr::new(0xFFFF_FF00_0000_0000);
    let map_phys = mm::frame::allocate_frame()
        .ok_or("test_page_tables: failed to allocate frame for map test")?;

    // Map the page as writable
    kernel_as.map_page(map_virt, map_phys, PageFlags::PRESENT | PageFlags::WRITABLE);

    // Verify translate works for the newly mapped page
    match kernel_as.translate(map_virt) {
        Some(resolved) => {
            if resolved.as_u64() != map_phys.as_u64() {
                core::mem::forget(kernel_as);
                return Err("translate: mapped page resolved to wrong address");
            }
        }
        None => {
            core::mem::forget(kernel_as);
            return Err("translate: mapped page not found");
        }
    }

    // Write a magic value through the mapped virtual address and read it back.
    // SAFETY: we just mapped this page as writable, so the virtual address is valid.
    unsafe {
        let ptr = map_virt.as_u64() as *mut u64;
        core::ptr::write_volatile(ptr, 0xDEAD_BEEF_CAFE_BABE);
        let readback = core::ptr::read_volatile(ptr);
        if readback != 0xDEAD_BEEF_CAFE_BABE {
            core::mem::forget(kernel_as);
            return Err("mapped page: write/read mismatch");
        }
    }

    // 4. Unmap the page and verify translate returns None
    let unmapped_phys = kernel_as.unmap_page(map_virt);
    if unmapped_phys.is_none() {
        core::mem::forget(kernel_as);
        return Err("unmap_page returned None for a mapped page");
    }
    if unmapped_phys.unwrap().as_u64() != map_phys.as_u64() {
        core::mem::forget(kernel_as);
        return Err("unmap_page returned wrong physical address");
    }

    // After unmap, translate should return None
    if kernel_as.translate(map_virt).is_some() {
        core::mem::forget(kernel_as);
        return Err("translate returned Some after unmap");
    }

    // Free the physical frame we used for the test
    mm::frame::deallocate_frame(map_phys);

    // 5. Test user address space create/destroy doesn't leak frames
    let free_before = mm::frame::free_frame_count();

    let user_as = AddressSpace::new_user(&kernel_as);
    // Just creating a user address space allocates 1 frame (the PML4)
    let free_after_create = mm::frame::free_frame_count();
    if free_after_create >= free_before {
        core::mem::forget(kernel_as);
        return Err("new_user did not allocate any frames");
    }

    // Destroy it and verify frames are returned
    user_as.destroy();
    let free_after_destroy = mm::frame::free_frame_count();
    if free_after_destroy != free_before {
        core::mem::forget(kernel_as);
        return Err("destroy did not return all frames");
    }

    // Don't drop the kernel AddressSpace (it's a global handle, not owned).
    core::mem::forget(kernel_as);

    Ok(())
}
