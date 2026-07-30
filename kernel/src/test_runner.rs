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
    TestCase { name: "test_heap_growth",      func: test_heap_growth },
    TestCase { name: "test_syscall",          func: test_syscall },
    TestCase { name: "test_capabilities",    func: test_capabilities },
    TestCase { name: "test_process",         func: test_process },
    TestCase { name: "test_ipc",             func: test_ipc },
    TestCase { name: "test_audit",           func: test_audit },
    TestCase { name: "test_userspace_init",  func: test_userspace_init },
    TestCase { name: "test_pci_scan",        func: test_pci_scan },
    TestCase { name: "test_virtio_transport", func: test_virtio_transport },
    TestCase { name: "test_virtio_blk",       func: test_virtio_blk },
    TestCase { name: "test_shared_memory",    func: test_shared_memory },
    TestCase { name: "test_block_server_ipc", func: test_block_server_ipc },
    TestCase { name: "test_server_spawn",     func: test_server_spawn },
    TestCase { name: "test_squashfs_server",  func: test_squashfs_server },
    TestCase { name: "test_overlay_server",   func: test_overlay_server },
    TestCase { name: "test_ext2_read",        func: test_ext2_read },
    TestCase { name: "test_ext2_write",       func: test_ext2_write },
    TestCase { name: "test_vfs_capability",   func: test_vfs_capability },
    TestCase { name: "test_fs_syscalls",      func: test_fs_syscalls },
    TestCase { name: "test_virtio_net",       func: test_virtio_net },
    TestCase { name: "test_net_service",      func: test_net_service },
    TestCase { name: "test_net_server_stack", func: test_net_server_stack },
    TestCase { name: "test_net_icmp_echo",    func: test_net_icmp_echo },
    // Runs before the other persistent net-server tests so it is the sole NIC
    // drainer (no inbound-frame competition) for the host-driven TCP handshake.
    TestCase { name: "test_tcp_server",       func: test_tcp_server },
    TestCase { name: "test_dhcp",             func: test_dhcp },
    TestCase { name: "test_socket_capability", func: test_socket_capability },
    TestCase { name: "test_socket_list",      func: test_socket_list },
    TestCase { name: "test_udp_echo",         func: test_udp_echo },
    TestCase { name: "test_tcp_client",       func: test_tcp_client },
    TestCase { name: "test_elf_exec",         func: test_elf_exec },
    TestCase { name: "test_linux_exec",       func: test_linux_exec },
    TestCase { name: "test_path_resolve",     func: test_path_resolve },
    TestCase { name: "test_linux_fs",         func: test_linux_fs },
    TestCase { name: "test_linux_threads",    func: test_linux_threads },
    TestCase { name: "test_oci_unpack",       func: test_oci_unpack },
    TestCase { name: "test_container_run",    func: test_container_run },
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

/// Test that the kernel heap grows dynamically when exhausted.
///
/// Allocates enough memory to exceed the initial 1 MiB heap, then verifies
/// that the heap grew (growth count > 0) and the allocation succeeded.
fn test_heap_growth() -> Result<(), &'static str> {
    use crate::mm;

    let initial_growth = mm::heap::growth_count();
    let initial_size = mm::heap::total_size();

    // Allocate a series of large blocks that will exceed 1 MiB total.
    // Each block is 128 KiB. 9 blocks = 1152 KiB > 1024 KiB initial heap.
    let mut blocks: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
    for _ in 0..9 {
        let block = alloc::vec![0xABu8; 128 * 1024];
        blocks.push(block);
    }

    // Verify all blocks are intact
    for block in &blocks {
        if block.len() != 128 * 1024 {
            return Err("heap growth: block size mismatch");
        }
        if block[0] != 0xAB || block[block.len() - 1] != 0xAB {
            return Err("heap growth: block content corrupted");
        }
    }

    // Verify the heap grew
    let final_growth = mm::heap::growth_count();
    if final_growth <= initial_growth {
        return Err("heap did not grow despite exceeding initial size");
    }

    let final_size = mm::heap::total_size();
    if final_size <= initial_size {
        return Err("heap total_size did not increase");
    }

    // Clean up — drop the blocks to free heap memory
    drop(blocks);

    Ok(())
}

/// Test the syscall/sysret infrastructure.
///
/// Verifies the complete ring 3 system call path:
/// 1. MSR configuration (EFER.SCE, STAR selectors, LSTAR entry, FMASK mask)
/// 2. Dispatch function (SYS_NULL returns 0, unknown returns -1)
/// 3. Full ring 3 round trip: spawns a task that transitions to ring 3 via
///    iretq, executes SYS_NULL via `syscall`, then SYS_TEST_COMPLETE to
///    report the result back to the kernel
fn test_syscall() -> Result<(), &'static str> {
    crate::arch::x86_64::syscall::test_syscall_round_trip()
}

/// Test the capability system: CSpace, handles, grant, and revoke.
///
/// Verifies:
/// 1. Creating a CSpace and inserting capabilities
/// 2. Looking up capabilities by handle returns correct type and rights
/// 3. Removing a capability increments generation (stale handle detection)
/// 4. Granting with reduced rights succeeds
/// 5. Granting with expanded rights fails (rights escalation)
/// 6. Granting without GRANT right fails
/// 7. Revoking cascades to all descendants
/// 8. Global object registry tracks reference counts
fn test_capabilities() -> Result<(), &'static str> {
    use crate::cap::{Capability, CapHandle, CapRights, CapType};
    use crate::cap::cspace::{CSpace, CapError};
    use crate::cap::object;

    // --- 1. CSpace creation and basic insert/lookup ---

    let mut cspace = CSpace::new();

    // Slot 0 should be the Null capability
    let null_cap = cspace.lookup(CapHandle::NULL)
        .map_err(|_| "lookup of NULL handle failed")?;
    if null_cap.cap_type != CapType::Null {
        return Err("slot 0 is not Null");
    }
    if null_cap.rights != CapRights::NONE {
        return Err("Null capability has non-zero rights");
    }

    // Insert a Memory capability with all rights
    let mem_cap = Capability {
        cap_type: CapType::Memory { base: 0x1000, page_count: 4 },
        rights: CapRights::ALL,
        parent: None,
    };
    let mem_handle = cspace.insert(mem_cap.clone())
        .map_err(|_| "insert Memory cap failed")?;

    // Lookup should return the same capability
    let looked_up = cspace.lookup(mem_handle)
        .map_err(|_| "lookup Memory cap failed")?;
    if looked_up.cap_type != (CapType::Memory { base: 0x1000, page_count: 4 }) {
        return Err("looked up Memory cap has wrong type");
    }
    if looked_up.rights != CapRights::ALL {
        return Err("looked up Memory cap has wrong rights");
    }

    // --- 2. Insert an endpoint capability ---

    let ep_cap = Capability {
        cap_type: CapType::Endpoint { endpoint_id: 42, badge: 0 },
        rights: CapRights::READ | CapRights::WRITE | CapRights::GRANT,
        parent: None,
    };
    let ep_handle = cspace.insert(ep_cap)
        .map_err(|_| "insert Endpoint cap failed")?;

    // Verify it's at a different index than the Memory cap
    if ep_handle.index() == mem_handle.index() {
        return Err("endpoint and memory caps have same index");
    }

    // Active count should be 2 (not counting the Null slot)
    if cspace.active_count() != 2 {
        return Err("active_count should be 2 after two inserts");
    }

    // --- 3. Remove and stale generation detection ---

    let removed = cspace.remove(mem_handle)
        .map_err(|_| "remove Memory cap failed")?;
    if removed.cap_type != (CapType::Memory { base: 0x1000, page_count: 4 }) {
        return Err("removed cap has wrong type");
    }

    // The old handle should now be stale (generation mismatch)
    match cspace.lookup(mem_handle) {
        Err(CapError::StaleGeneration) => { /* expected */ }
        Ok(_) => return Err("stale handle lookup should have failed"),
        Err(_) => return Err("stale handle returned unexpected error"),
    }

    // Inserting a new cap should reuse the freed slot
    let new_cap = Capability {
        cap_type: CapType::Irq { irq_number: 5 },
        rights: CapRights::READ | CapRights::MANAGE,
        parent: None,
    };
    let irq_handle = cspace.insert(new_cap)
        .map_err(|_| "insert IRQ cap failed")?;

    // It should reuse index 1 (the freed Memory slot), but with generation 1
    if irq_handle.index() != mem_handle.index() {
        return Err("insert did not reuse freed slot");
    }
    if irq_handle.generation() != mem_handle.generation() + 1 {
        return Err("reused slot generation not incremented");
    }

    // --- 4. Grant with reduced rights ---

    // First, create a fresh capability with GRANT right for the grant test
    let grant_source = Capability {
        cap_type: CapType::Memory { base: 0x2000, page_count: 8 },
        rights: CapRights::ALL,
        parent: None,
    };
    let source_handle = cspace.insert(grant_source)
        .map_err(|_| "insert grant source failed")?;

    // Grant with reduced rights (READ + WRITE only, no EXECUTE/GRANT/MANAGE)
    let reduced_rights = CapRights::READ | CapRights::WRITE;
    let derived_handle = cspace.grant(source_handle, reduced_rights)
        .map_err(|_| "grant with reduced rights failed")?;

    // Verify the derived capability
    let derived = cspace.lookup(derived_handle)
        .map_err(|_| "lookup derived cap failed")?;
    if derived.rights != reduced_rights {
        return Err("derived cap has wrong rights");
    }
    if derived.cap_type != (CapType::Memory { base: 0x2000, page_count: 8 }) {
        return Err("derived cap has wrong type");
    }
    if derived.parent != Some(source_handle) {
        return Err("derived cap parent does not match source");
    }

    // --- 5. Grant with expanded rights should fail ---

    // Try to grant with rights the source doesn't have — but wait, source
    // has ALL. So let's use the derived cap (which only has READ|WRITE and
    // no GRANT right) to test both escalation and no-grant-right errors.

    // The derived cap doesn't have GRANT, so granting from it should fail
    match cspace.grant(derived_handle, CapRights::READ) {
        Err(CapError::NoGrantRight) => { /* expected */ }
        Ok(_) => return Err("grant without GRANT right should have failed"),
        Err(e) => {
            let _ = e;
            return Err("grant without GRANT right returned unexpected error");
        }
    }

    // Create a cap with GRANT but limited other rights, then try to escalate
    let limited_cap = Capability {
        cap_type: CapType::Process { pid: 1 },
        rights: CapRights::READ | CapRights::GRANT,
        parent: None,
    };
    let limited_handle = cspace.insert(limited_cap)
        .map_err(|_| "insert limited cap failed")?;

    // Try to grant WRITE (which the limited cap doesn't have)
    match cspace.grant(limited_handle, CapRights::READ | CapRights::WRITE) {
        Err(CapError::RightsEscalation) => { /* expected */ }
        Ok(_) => return Err("rights escalation should have been rejected"),
        Err(_) => return Err("rights escalation returned unexpected error"),
    }

    // Granting with equal or fewer rights should work
    let sub_handle = cspace.grant(limited_handle, CapRights::READ)
        .map_err(|_| "grant with equal rights failed")?;
    let sub = cspace.lookup(sub_handle)
        .map_err(|_| "lookup sub-granted cap failed")?;
    if sub.rights != CapRights::READ {
        return Err("sub-granted cap has wrong rights");
    }

    // --- 6. Revocation cascades to descendants ---

    // Build a chain: root -> child -> grandchild
    let root_cap = Capability {
        cap_type: CapType::Endpoint { endpoint_id: 99, badge: 0 },
        rights: CapRights::ALL,
        parent: None,
    };
    let root_handle = cspace.insert(root_cap)
        .map_err(|_| "insert root cap failed")?;

    let child_handle = cspace.grant(root_handle, CapRights::READ | CapRights::WRITE | CapRights::GRANT)
        .map_err(|_| "grant child cap failed")?;

    let grandchild_handle = cspace.grant(child_handle, CapRights::READ | CapRights::GRANT)
        .map_err(|_| "grant grandchild cap failed")?;

    // All three should be lookupable
    cspace.lookup(root_handle).map_err(|_| "root lookup before revoke failed")?;
    cspace.lookup(child_handle).map_err(|_| "child lookup before revoke failed")?;
    cspace.lookup(grandchild_handle).map_err(|_| "grandchild lookup before revoke failed")?;

    let count_before = cspace.active_count();

    // Revoke the root — should cascade to child and grandchild
    cspace.revoke(root_handle).map_err(|_| "revoke failed")?;

    // All three should now be gone
    if cspace.lookup(root_handle).is_ok() {
        return Err("root still accessible after revoke");
    }
    if cspace.lookup(child_handle).is_ok() {
        return Err("child still accessible after revoke");
    }
    if cspace.lookup(grandchild_handle).is_ok() {
        return Err("grandchild still accessible after revoke");
    }

    // Active count should have decreased by 3
    let count_after = cspace.active_count();
    if count_before - count_after != 3 {
        return Err("revoke did not remove exactly 3 capabilities");
    }

    // --- 7. Global object registry ---

    let initial_count = object::object_count();

    let obj_id = object::register(CapType::Endpoint { endpoint_id: 100, badge: 0 });
    if object::object_count() != initial_count + 1 {
        return Err("object registry count not incremented after register");
    }

    // Add a reference (simulating another cap pointing to the same object)
    object::add_ref(obj_id);

    // Release once — should not destroy (ref_count was 2, now 1)
    if object::release(obj_id) {
        return Err("object destroyed with ref_count > 0");
    }
    if object::object_count() != initial_count + 1 {
        return Err("object removed despite remaining references");
    }

    // Release again — should destroy (ref_count drops to 0)
    if !object::release(obj_id) {
        return Err("object not destroyed when ref_count hit 0");
    }
    if object::object_count() != initial_count {
        return Err("object registry count not decremented after destroy");
    }

    // Lookup destroyed object should return None
    if object::lookup(obj_id).is_some() {
        return Err("destroyed object still in registry");
    }

    Ok(())
}

/// Test the process abstraction: create, inspect, and destroy.
///
/// Verifies:
/// 1. Kernel process (PID 0) exists and owns boot-time tasks
/// 2. Creating a new process allocates an address space and CSpace
/// 3. Process list reflects the new process
/// 4. Destroying a process frees all associated resources
/// 5. Frame count is stable across create/destroy cycles (no leaks)
fn test_process() -> Result<(), &'static str> {
    use crate::process;
    use crate::mm;

    // 1. Verify kernel process exists
    let procs = process::process_list();
    let kernel = procs.iter().find(|p| p.pid == process::ProcessId::KERNEL);
    if kernel.is_none() {
        return Err("kernel process (PID 0) not found");
    }
    let kernel = kernel.unwrap();
    if kernel.task_count == 0 {
        return Err("kernel process has no tasks");
    }

    // 2. Create a new process and verify resources are allocated
    let free_before = mm::frame::free_frame_count();
    let initial_proc_count = process::process_count();

    let (pid, _cap_handle) = process::create_process("test-proc", None);

    // Verify the process was created
    if process::process_count() != initial_proc_count + 1 {
        return Err("process count did not increase after create");
    }

    // Creating a process allocates at least 1 frame (the PML4 for the
    // user address space). Verify free count decreased.
    let free_after_create = mm::frame::free_frame_count();
    if free_after_create >= free_before {
        return Err("create_process did not allocate any frames");
    }

    // 3. Verify the process appears in the process list
    let procs = process::process_list();
    let new_proc = procs.iter().find(|p| p.pid == pid);
    if new_proc.is_none() {
        return Err("new process not found in process list");
    }
    let new_proc = new_proc.unwrap();
    if new_proc.name != "test-proc" {
        return Err("new process has wrong name");
    }

    // 4. Destroy the process and verify resources are freed
    if !process::destroy_process(pid) {
        return Err("destroy_process returned false");
    }

    // After destroy, the process should be gone from the list
    let procs = process::process_list();
    if procs.iter().any(|p| p.pid == pid) {
        return Err("destroyed process still in process list");
    }

    // 5. Verify frame count is restored (no leaks)
    let free_after_destroy = mm::frame::free_frame_count();
    if free_after_destroy != free_before {
        return Err("frame leak: free count not restored after destroy");
    }

    // Run multiple create/destroy cycles to stress-test for leaks
    for i in 0..5 {
        let free_loop_before = mm::frame::free_frame_count();
        let (loop_pid, _) = process::create_process("loop-test", None);
        if !process::destroy_process(loop_pid) {
            return Err("destroy in loop failed");
        }
        let free_loop_after = mm::frame::free_frame_count();
        if free_loop_after != free_loop_before {
            let _ = i;
            return Err("frame leak in create/destroy loop");
        }
    }

    // Verify the kernel process cannot be destroyed
    if process::destroy_process(process::ProcessId::KERNEL) {
        return Err("kernel process should not be destroyable");
    }

    Ok(())
}

/// Test synchronous IPC: send/receive, call/reply, badge delivery.
///
/// Verifies:
/// 1. Two kernel tasks can exchange messages via an endpoint
/// 2. Sender blocks until receiver calls receive (rendezvous semantics)
/// 3. Badge is correctly delivered to receiver
/// 4. Call/reply pattern: caller blocks, server receives + replies, caller unblocks
/// 5. Endpoint create/destroy lifecycle
fn test_ipc() -> Result<(), &'static str> {
    use core::sync::atomic::{AtomicU64, AtomicBool, Ordering};
    use crate::ipc;
    use crate::sched;

    // ---- Part 1: Basic send/receive ----

    /// Shared state for the send/receive test.
    static SEND_RECV_EP: AtomicU64 = AtomicU64::new(0);
    static RECV_WORD0: AtomicU64 = AtomicU64::new(0);
    static RECV_WORD1: AtomicU64 = AtomicU64::new(0);
    static RECV_BADGE: AtomicU64 = AtomicU64::new(0);
    static RECV_DONE: AtomicBool = AtomicBool::new(false);

    /// Sender task: sends a message with a badge.
    fn sender_task() {
        let ep_id = SEND_RECV_EP.load(Ordering::SeqCst);
        let msg = crate::ipc::IpcMessage::new([0xDEAD, 0xBEEF, 0, 0]);
        crate::ipc::ipc_send(ep_id, msg, 42).expect("send failed");
    }

    /// Receiver task: receives a message and stores the result in globals.
    fn receiver_task() {
        let ep_id = SEND_RECV_EP.load(Ordering::SeqCst);
        let msg = crate::ipc::ipc_receive(ep_id).expect("receive failed");
        RECV_WORD0.store(msg.words[0], Ordering::SeqCst);
        RECV_WORD1.store(msg.words[1], Ordering::SeqCst);
        RECV_BADGE.store(msg.badge, Ordering::SeqCst);
        RECV_DONE.store(true, Ordering::SeqCst);
    }

    // Create an endpoint
    let ep_id = ipc::create_endpoint("test-ep");
    SEND_RECV_EP.store(ep_id, Ordering::SeqCst);
    RECV_DONE.store(false, Ordering::SeqCst);
    RECV_WORD0.store(0, Ordering::SeqCst);
    RECV_WORD1.store(0, Ordering::SeqCst);
    RECV_BADGE.store(0, Ordering::SeqCst);

    // Spawn receiver first (it will block waiting for a sender)
    sched::spawn("ipc-recv", receiver_task);
    // Spawn sender (it will find the blocked receiver and deliver immediately,
    // or block until the receiver arrives)
    sched::spawn("ipc-send", sender_task);

    // Yield to let both tasks run
    for _ in 0..10_000 {
        if RECV_DONE.load(Ordering::SeqCst) {
            break;
        }
        sched::yield_now();
    }

    if !RECV_DONE.load(Ordering::SeqCst) {
        return Err("IPC send/receive: timed out");
    }
    if RECV_WORD0.load(Ordering::SeqCst) != 0xDEAD {
        return Err("IPC send/receive: word[0] mismatch");
    }
    if RECV_WORD1.load(Ordering::SeqCst) != 0xBEEF {
        return Err("IPC send/receive: word[1] mismatch");
    }
    if RECV_BADGE.load(Ordering::SeqCst) != 42 {
        return Err("IPC send/receive: badge mismatch");
    }

    // ---- Part 2: Call/reply (RPC pattern) ----

    static CALL_REPLY_EP: AtomicU64 = AtomicU64::new(0);
    static CALL_RESULT: AtomicU64 = AtomicU64::new(0);
    static CALL_DONE: AtomicBool = AtomicBool::new(false);

    /// Client task: sends a "call" with a number and expects the square back.
    fn client_task() {
        let ep_id = CALL_REPLY_EP.load(Ordering::SeqCst);
        let msg = crate::ipc::IpcMessage::new([7, 0, 0, 0]); // "compute square of 7"
        let reply = crate::ipc::ipc_call(ep_id, msg, 0).expect("call failed");
        CALL_RESULT.store(reply.words[0], Ordering::SeqCst);
        CALL_DONE.store(true, Ordering::SeqCst);
    }

    /// Server task: receives a call, computes the square, and replies.
    fn server_task() {
        let ep_id = CALL_REPLY_EP.load(Ordering::SeqCst);
        let request = crate::ipc::ipc_receive(ep_id).expect("server receive failed");
        let value = request.words[0];
        let reply_token = request.reply_token;

        // Compute and reply
        let reply = crate::ipc::IpcMessage::new([value * value, 0, 0, 0]);
        crate::ipc::ipc_reply(ep_id, reply_token, reply).expect("reply failed");
    }

    let call_ep = ipc::create_endpoint("test-call");
    CALL_REPLY_EP.store(call_ep, Ordering::SeqCst);
    CALL_DONE.store(false, Ordering::SeqCst);
    CALL_RESULT.store(0, Ordering::SeqCst);

    // Spawn server first so it's ready to receive
    sched::spawn("ipc-server", server_task);
    // Spawn client that will call the server
    sched::spawn("ipc-client", client_task);

    for _ in 0..10_000 {
        if CALL_DONE.load(Ordering::SeqCst) {
            break;
        }
        sched::yield_now();
    }

    if !CALL_DONE.load(Ordering::SeqCst) {
        return Err("IPC call/reply: timed out");
    }
    if CALL_RESULT.load(Ordering::SeqCst) != 49 {
        return Err("IPC call/reply: expected 49 (7*7), got wrong value");
    }

    // ---- Part 3: Endpoint lifecycle ----

    // Destroy the test endpoints
    if !ipc::destroy_endpoint(ep_id) {
        return Err("destroy_endpoint failed for send/recv endpoint");
    }
    if !ipc::destroy_endpoint(call_ep) {
        return Err("destroy_endpoint failed for call/reply endpoint");
    }

    // Sending to a destroyed endpoint should fail
    let msg = ipc::IpcMessage::new([0, 0, 0, 0]);
    match ipc::ipc_send(ep_id, msg, 0) {
        Err(ipc::IpcError::InvalidEndpoint) => { /* expected */ }
        Ok(_) => return Err("send to destroyed endpoint should fail"),
        Err(_) => return Err("send to destroyed endpoint returned wrong error"),
    }

    Ok(())
}

// ============================================================
//  test_audit — Audit logging ring buffer
// ============================================================

/// Test the audit logging subsystem.
///
/// Verifies that:
/// 1. Events are logged and can be retrieved
/// 2. Sequence numbers are monotonically increasing
/// 3. Timestamps are non-decreasing
/// 4. Capability operations produce correctly-typed audit entries
/// 5. The total event count increments properly
fn test_audit() -> Result<(), &'static str> {
    use crate::audit;
    use crate::audit::AuditOp;
    use crate::cap::{Capability, CapRights, CapType};
    use crate::cap::cspace::CSpace;

    // Take a snapshot of current state so we only examine events from this test.
    let seq_before = audit::current_seq();
    let count_before = audit::total_event_count();

    // --- Part 1: Verify that CSpace operations generate audit events ---

    let mut cspace = CSpace::new();

    // Insert a capability — should generate a CapCreate event
    let mem_cap = Capability {
        cap_type: CapType::Memory { base: 0xBEEF_0000, page_count: 8 },
        rights: CapRights::READ | CapRights::WRITE | CapRights::GRANT,
        parent: None,
    };
    let handle = cspace.insert(mem_cap)
        .map_err(|_| "audit test: failed to insert capability")?;

    // Grant a derived capability — should generate CapGrant + CapCreate events
    let _child = cspace.grant(handle, CapRights::READ)
        .map_err(|_| "audit test: failed to grant capability")?;

    // Revoke the parent — should generate a CapRevoke event
    cspace.revoke(handle)
        .map_err(|_| "audit test: failed to revoke capability")?;

    // --- Part 2: Verify that IPC operations generate audit events ---

    let ep_id = crate::ipc::create_endpoint("audit-test");

    // We can't do a full send/receive without two tasks, but ipc_send
    // logs before the blocking decision, so the audit event is recorded
    // even though the send will queue and eventually... let's just create
    // an endpoint to trigger the create path. The IPC audit events were
    // already exercised by test_ipc above; we just verify the count grows.

    crate::ipc::destroy_endpoint(ep_id);

    // --- Part 3: Verify events were recorded ---

    let count_after = audit::total_event_count();
    let new_events = count_after - count_before;

    // We expect at minimum: CapCreate (insert) + CapGrant + CapCreate (grant's insert)
    // + CapRevoke = 4 cap events. The exact number may be higher if IPC tests
    // above also generated events, but we should have at least 4 from our
    // cap operations.
    if new_events < 4 {
        return Err("audit: fewer than 4 events generated by cap operations");
    }

    // --- Part 4: Verify the last entries have valid fields ---

    let entries = audit::last_entries(new_events as usize);
    if entries.is_empty() {
        return Err("audit: last_entries returned empty after logging events");
    }

    // Sequence numbers must be monotonically increasing
    for pair in entries.windows(2) {
        if pair[1].seq <= pair[0].seq {
            return Err("audit: sequence numbers not monotonically increasing");
        }
    }

    // Timestamps must be non-decreasing (they could be equal within the same tick)
    for pair in entries.windows(2) {
        if pair[1].timestamp < pair[0].timestamp {
            return Err("audit: timestamps not non-decreasing");
        }
    }

    // All entries from our test should have seq >= seq_before
    for entry in &entries {
        if entry.seq < seq_before {
            return Err("audit: entry has seq below expected range");
        }
    }

    // --- Part 5: Find our specific events in the log ---

    // Look for at least one CapCreate, one CapGrant, and one CapRevoke
    // among entries with seq >= seq_before.
    let our_entries: alloc::vec::Vec<_> = entries.iter()
        .filter(|e| e.seq >= seq_before)
        .collect();

    let has_create = our_entries.iter().any(|e| e.operation == AuditOp::CapCreate);
    let has_grant = our_entries.iter().any(|e| e.operation == AuditOp::CapGrant);
    let has_revoke = our_entries.iter().any(|e| e.operation == AuditOp::CapRevoke);

    if !has_create {
        return Err("audit: no CapCreate event found");
    }
    if !has_grant {
        return Err("audit: no CapGrant event found");
    }
    if !has_revoke {
        return Err("audit: no CapRevoke event found");
    }

    // Verify the CapCreate event has the correct capability type
    let create_entry = our_entries.iter()
        .find(|e| e.operation == AuditOp::CapCreate)
        .unwrap();
    match create_entry.cap_type {
        CapType::Memory { base: 0xBEEF_0000, page_count: 8 } => { /* correct */ }
        _ => return Err("audit: CapCreate event has wrong cap_type"),
    }

    // --- Part 6: Verify current_seq advanced ---

    let seq_after = audit::current_seq();
    if seq_after <= seq_before {
        return Err("audit: current_seq did not advance");
    }

    Ok(())
}

// ============================================================
//  test_userspace_init — First userspace process
// ============================================================

/// Test the init process: a ring 3 process communicating with the kernel via IPC.
///
/// This is the capstone test for Phase 2. It verifies:
/// 1. Init process boots in ring 3 with its own address space
/// 2. Init sends IPC messages to the kernel via syscall
/// 3. The kernel-side server receives the messages
/// 4. Timer preemption works on the init process
fn test_userspace_init() -> Result<(), &'static str> {
    use crate::process;

    // Start the init process (creates process, maps pages, spawns tasks)
    process::init::start();

    // Yield repeatedly to let the init process and server run.
    // The init process sends messages and yields between each one.
    // We need enough yields for the scheduler to cycle through:
    // init task → server task → back to us.
    for _ in 0..500 {
        crate::sched::yield_now();

        // Check early if the server has received messages
        if process::init::server_message_count() >= 3 {
            break;
        }
    }

    // Verify the server received at least one message
    if !process::init::server_has_received() {
        return Err("init server never received a message from userspace");
    }

    let count = process::init::server_message_count();
    if count < 2 {
        return Err("init server received fewer than 2 messages");
    }

    Ok(())
}

// ============================================================
//  test_pci_scan — PCI bus enumeration (Phase 3.0)
// ============================================================

/// Test PCI bus enumeration.
///
/// The scan already ran during boot (in `kmain`), so the global registry is
/// populated. This verifies:
/// 1. At least one PCI device was discovered (QEMU always exposes a host
///    bridge and other Q35 defaults)
/// 2. A VirtIO device (vendor 0x1AF4) is present — QEMU is launched with a
///    VirtIO disk attached
/// 3. The VirtIO device has at least one implemented BAR with a non-zero size
#[cfg(target_arch = "x86_64")]
fn test_pci_scan() -> Result<(), &'static str> {
    use crate::drivers::pci;

    // QEMU's Q35 machine always exposes at least the host bridge and an ISA
    // bridge, so an empty list means config-space access is broken.
    if pci::device_count() == 0 {
        return Err("PCI scan found no devices");
    }

    // The test harness launches QEMU with a VirtIO block disk attached, so we
    // must find at least one VirtIO-vendor device.
    let virtio = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    if virtio.is_empty() {
        return Err("no VirtIO devices found (expected an attached VirtIO disk)");
    }

    // At least one VirtIO device should advertise a usable BAR window — the
    // transport layer (3.1) needs this to reach the device's registers.
    let has_bar = virtio
        .iter()
        .any(|dev| dev.bars.iter().any(|bar| bar.size != 0));
    if !has_bar {
        return Err("VirtIO device has no implemented BAR");
    }

    Ok(())
}

/// Stub for non-x86_64 targets — PCI enumeration is x86-specific in Phase 3
/// (aarch64 uses memory-mapped ECAM, deferred to Phase 7).
#[cfg(not(target_arch = "x86_64"))]
fn test_pci_scan() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_virtio_transport — VirtIO PCI transport (Phase 3.1)
// ============================================================

/// Test the VirtIO PCI transport end to end (short of a real I/O request).
///
/// Finds the attached VirtIO block device on the PCI bus, then exercises the
/// full bring-up path the block driver will rely on:
/// 1. Capability discovery + MMIO mapping (`VirtioTransport::init`)
/// 2. The reset → ACKNOWLEDGE → DRIVER handshake
/// 3. Feature negotiation (VIRTIO_F_VERSION_1)
/// 4. Virtqueue allocation and programming (`setup_queue`)
/// 5. DRIVER_OK
///
/// If any step fails, the device wouldn't be drivable, so this is a strong
/// signal that 3.1 works before the block read/write path (3.2) is built.
#[cfg(target_arch = "x86_64")]
fn test_virtio_transport() -> Result<(), &'static str> {
    use crate::drivers::pci;
    use crate::drivers::virtio::VirtioTransport;

    // VirtIO mass-storage device = PCI class 0x01 (the attached virtio-blk).
    let virtio_devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let blk = virtio_devs
        .iter()
        .find(|d| d.class == 0x01)
        .ok_or("no VirtIO block device on the PCI bus")?;

    // Discover register regions, map them, reset, ACK + DRIVER.
    let transport =
        VirtioTransport::init(blk).map_err(|_| "VirtioTransport::init failed")?;

    // Negotiate features — we want nothing device-specific here, just the
    // mandatory VERSION_1 bit.
    transport
        .negotiate_features(0)
        .map_err(|_| "feature negotiation failed")?;

    // The device must expose at least one virtqueue.
    if transport.num_queues() == 0 {
        return Err("device reports zero virtqueues");
    }

    // Allocate and program virtqueue 0.
    let _queue = transport
        .setup_queue(0)
        .map_err(|_| "virtqueue setup failed")?;

    // Announce the driver is ready.
    transport.set_driver_ok();

    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_virtio_transport() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_virtio_blk — VirtIO block driver round-trip (Phase 3.2)
// ============================================================

/// Test the VirtIO-blk driver and the block device registry.
///
/// Brings up the attached virtio-blk disk, registers it, then exercises the
/// `BlockDevice` interface end to end:
/// 1. Registry lookup returns the device with a sane capacity
/// 2. Single-sector write-then-read-back returns identical data
/// 3. Multi-sector (3-sector) round-trip works
/// 4. A request past the end of the device is rejected with OutOfRange
/// 5. Bad (non-sector-multiple) buffer length is rejected
#[cfg(target_arch = "x86_64")]
fn test_virtio_blk() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use alloc::vec;
    use crate::drivers::{block, pci};
    use crate::drivers::block::BlockError;
    use crate::drivers::virtio::blk::VirtioBlk;

    // Find and initialise the virtio-blk device.
    let virtio_devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let dev = virtio_devs
        .iter()
        .find(|d| d.class == 0x01)
        .ok_or("no VirtIO block device on the PCI bus")?;
    let blk = VirtioBlk::init_from_pci(dev).map_err(|_| "VirtioBlk init failed")?;

    // Register it and fetch it back through the registry.
    let index = block::register("virtio-blk0", Box::new(blk));
    let device = block::get(index).ok_or("registry lookup failed")?;

    let block_size = device.block_size() as usize;
    if block_size != 512 {
        return Err("unexpected block size (expected 512)");
    }
    let capacity = device.block_count();
    if capacity == 0 {
        return Err("device reports zero capacity");
    }

    // --- 1. Single-sector write/read round-trip ---
    let mut write_buf = vec![0u8; block_size];
    for (i, b) in write_buf.iter_mut().enumerate() {
        *b = (i as u8) ^ 0xA5;
    }
    device
        .write_blocks(10, &write_buf)
        .map_err(|_| "single-sector write failed")?;

    let mut read_buf = vec![0u8; block_size];
    device
        .read_blocks(10, &mut read_buf)
        .map_err(|_| "single-sector read failed")?;
    if read_buf != write_buf {
        return Err("single-sector round-trip data mismatch");
    }

    // --- 2. Multi-sector round-trip (3 sectors) ---
    let mut multi_write = vec![0u8; block_size * 3];
    for (i, b) in multi_write.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(7).wrapping_add(1);
    }
    device
        .write_blocks(20, &multi_write)
        .map_err(|_| "multi-sector write failed")?;
    let mut multi_read = vec![0u8; block_size * 3];
    device
        .read_blocks(20, &mut multi_read)
        .map_err(|_| "multi-sector read failed")?;
    if multi_read != multi_write {
        return Err("multi-sector round-trip data mismatch");
    }

    // --- 3. Out-of-range request is rejected ---
    let mut oob = vec![0u8; block_size];
    match device.read_blocks(capacity, &mut oob) {
        Err(BlockError::OutOfRange) => {}
        _ => return Err("out-of-range read should return OutOfRange"),
    }

    // --- 4. Bad buffer length is rejected ---
    let mut bad = vec![0u8; block_size - 1];
    match device.read_blocks(0, &mut bad) {
        Err(BlockError::BadBufferLength) => {}
        _ => return Err("non-sector-multiple buffer should be rejected"),
    }

    // --- 5. Flush succeeds ---
    device.flush().map_err(|_| "flush failed")?;

    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_virtio_blk() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_shared_memory — Shared memory regions (Phase 3.3)
// ============================================================

/// Test shared memory region allocation and mapping into an address space.
///
/// Verifies:
/// 1. A region can be allocated and is zeroed and page-sized
/// 2. The kernel can read/write it via the HHDM
/// 3. It can be mapped into a (user) address space, and `translate` resolves
///    each mapped page to the correct physical frame (the basis for handing a
///    block-transfer window to a ring-3 filesystem server)
fn test_shared_memory() -> Result<(), &'static str> {
    use crate::mm::addr::VirtAddr;
    use crate::mm::page_table::{kernel_address_space, AddressSpace};
    use crate::mm::shared::SharedRegion;

    // 1. Allocate two pages' worth of shared memory.
    let region = SharedRegion::alloc(8192).ok_or("shared alloc failed")?;
    if region.size < 8192 {
        return Err("shared region smaller than requested");
    }

    // Freshly allocated region must be zeroed.
    // SAFETY: we own the region; no other task accesses it during this test.
    unsafe {
        let s = region.as_slice_mut();
        if s.iter().any(|&b| b != 0) {
            return Err("shared region not zeroed");
        }
        // 2. Kernel read/write via HHDM.
        s[0] = 0xAB;
        let last = region.size as usize - 1;
        s[last] = 0xCD;
        if s[0] != 0xAB || s[last] != 0xCD {
            return Err("kernel shared-memory write/read mismatch");
        }
    }

    // 3. Map into a fresh user address space and verify translation.
    let kernel_as = kernel_address_space();
    let user = AddressSpace::new_user(&kernel_as);
    core::mem::forget(kernel_as);

    let virt = VirtAddr::new(0x4000_0000);
    region.map_into(&user, virt);

    let r0 = user.translate(virt);
    let r1 = user.translate(VirtAddr::new(virt.as_u64() + 0x1000));
    // Don't destroy the user AS: it would free the shared region's frames
    // (they're mapped as leaves). Leak the page tables instead — this is a test.
    let ok0 = matches!(r0, Some(p) if p.as_u64() == region.phys_base.as_u64());
    let ok1 = matches!(r1, Some(p) if p.as_u64() == region.phys_base.as_u64() + 0x1000);
    core::mem::forget(user);

    if !ok0 {
        return Err("shared region page 0 translate mismatch");
    }
    if !ok1 {
        return Err("shared region page 1 translate mismatch");
    }

    Ok(())
}

// ============================================================
//  test_block_server_ipc — Block server over IPC (Phase 3.3)
// ============================================================

/// Test the block server's IPC interface end to end.
///
/// Brings up a block device, starts the block server on it, then acts as an IPC
/// client: writes a pattern into the shared region and issues a WRITE request,
/// clears the region and issues a READ request, and verifies the data round-trips
/// through the device via IPC + shared memory.
#[cfg(target_arch = "x86_64")]
fn test_block_server_ipc() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use crate::drivers::virtio::blk::VirtioBlk;
    use crate::drivers::{block, block_server, pci};
    use crate::ipc::{self, IpcMessage};
    use crate::sched;

    // Bring up and register a block device for the server to drive.
    let devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let dev = devs
        .iter()
        .find(|d| d.class == 0x01)
        .ok_or("no VirtIO block device")?;
    let blk = VirtioBlk::init_from_pci(dev).map_err(|_| "device init failed")?;
    let idx = block::register("virtio-blk-srv", Box::new(blk));

    // Start the server (spawns its task) and let it reach its receive loop.
    let handle = block_server::start(idx);
    for _ in 0..50 {
        sched::yield_now();
    }

    const BLOCK_SIZE: usize = 512;
    const TEST_LBA: u64 = 50;

    // Fill the shared region [0, 512) with a known pattern, then WRITE it.
    // SAFETY: the server is idle (we hold the only outstanding request) so no
    // concurrent access to the shared region occurs.
    unsafe {
        let s = handle.region.as_slice_mut();
        for (i, b) in s[..BLOCK_SIZE].iter_mut().enumerate() {
            *b = (i as u8) ^ 0x5A;
        }
    }
    // WRITE: op=1, lba=TEST_LBA, count=1, offset=0.
    let write_req = IpcMessage::new([1, TEST_LBA, 1, 0]);
    let resp = ipc::ipc_call(handle.endpoint, write_req, 0)
        .map_err(|_| "WRITE ipc_call failed")?;
    if resp.words[0] != 0 {
        return Err("WRITE request returned error status");
    }

    // Clear the region so a successful READ must repopulate it.
    unsafe {
        let s = handle.region.as_slice_mut();
        for b in s[..BLOCK_SIZE].iter_mut() {
            *b = 0;
        }
    }
    // READ: op=0, lba=TEST_LBA, count=1, offset=0.
    let read_req = IpcMessage::new([0, TEST_LBA, 1, 0]);
    let resp = ipc::ipc_call(handle.endpoint, read_req, 0)
        .map_err(|_| "READ ipc_call failed")?;
    if resp.words[0] != 0 {
        return Err("READ request returned error status");
    }

    // The region should again hold the original pattern.
    // SAFETY: the request completed; no concurrent access.
    unsafe {
        let s = handle.region.as_slice_mut();
        for (i, &b) in s[..BLOCK_SIZE].iter().enumerate() {
            if b != ((i as u8) ^ 0x5A) {
                return Err("data mismatch after block-server round-trip");
            }
        }
    }

    // An out-of-range shared offset must be rejected by the server.
    let bad_req = IpcMessage::new([0, TEST_LBA, 1, handle.region.size]);
    let resp = ipc::ipc_call(handle.endpoint, bad_req, 0)
        .map_err(|_| "bad-offset ipc_call failed")?;
    if resp.words[0] == 0 {
        return Err("out-of-range offset should have been rejected");
    }

    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_block_server_ipc() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_server_spawn — Userspace server framework (Phase 3.4)
// ============================================================

/// Test the userspace server framework end to end with the echo server.
///
/// Spawns the embedded echo-server flat binary into a ring-3 process, then acts
/// as an IPC client and calls it. Verifies:
/// 1. The server boots in ring 3 from the embedded binary (no ELF parsing)
/// 2. Its heap came up (the echo reply's word3 reflects a successful ring-3
///    allocation)
/// 3. The full kernel↔ring-3 IPC round trip works (SYS_RECEIVE + SYS_REPLY)
/// 4. The reply matches the request transformation (word0 + 1)
#[cfg(target_arch = "x86_64")]
fn test_server_spawn() -> Result<(), &'static str> {
    use crate::ipc::{self, IpcMessage};
    use crate::process::embedded;
    use crate::process::server::{spawn_server, ServerConfig};
    use crate::sched;

    // Create the endpoint the echo server will receive on, then spawn it.
    let endpoint = ipc::create_endpoint("echo-test");
    let _pid = spawn_server(ServerConfig {
        name: "echo-server",
        binary: embedded::ECHO_SERVER,
        fs_endpoint: endpoint,
        block_endpoint: 0,
        shared: None,
        client_shared: None,
        heap_bytes: 256 * 1024,
        arg0: 0,
        arg1: 0,
        filesystem_mount: None,
    });

    // Let the ring-3 server reach its receive loop. It must be scheduled, run
    // _start (heap init), and block in SYS_RECEIVE.
    for _ in 0..500 {
        sched::yield_now();
    }

    // Call the echo server: it returns [word0+1, word1, word2, heap_probe].
    let reply = ipc::ipc_call(endpoint, IpcMessage::new([10, 20, 30, 0]), 0)
        .map_err(|_| "ipc_call to echo server failed")?;

    if reply.words[0] != 11 {
        return Err("echo server did not transform word0 (no reply?)");
    }
    if reply.words[1] != 20 || reply.words[2] != 30 {
        return Err("echo server corrupted message words");
    }
    if reply.words[3] != 1 {
        return Err("echo server heap allocation failed in ring 3");
    }

    // A second call confirms the server loops and keeps serving.
    let reply2 = ipc::ipc_call(endpoint, IpcMessage::new([100, 0, 0, 0]), 0)
        .map_err(|_| "second ipc_call failed")?;
    if reply2.words[0] != 101 {
        return Err("echo server did not handle a second request");
    }

    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_server_spawn() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_squashfs_server — SquashFS filesystem server (Phase 3.5)
// ============================================================

/// Test the userspace SquashFS server end to end against the real image.
///
/// Probes the VirtIO disks for the SquashFS magic, starts a block server on it,
/// spawns the SquashFS server (ring 3) with block + client shared regions, then
/// acts as a client: STATs, OPENs/READs files (small fragment-packed and large
/// multi-block), and READDIRs directories — verifying bytes match what
/// `cargo xtask image` packed.
#[cfg(target_arch = "x86_64")]
fn test_squashfs_server() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use crate::drivers::block::BlockDevice;
    use crate::drivers::virtio::blk::VirtioBlk;
    use crate::drivers::{block, block_server, pci};
    use crate::ipc::{self, IpcMessage};
    use crate::mm::shared::SharedRegion;
    use crate::process::embedded;
    use crate::process::server::{spawn_server, ServerConfig};
    use crate::sched;

    // FS protocol opcodes (mirror libthemelios::fs_proto).
    const OP_OPEN: u64 = 1;
    const OP_READ: u64 = 2;
    const OP_STAT: u64 = 5;
    const OP_READDIR: u64 = 6;
    const STATUS_OK: u64 = 0;

    // --- Locate the SquashFS disk by probing each VirtIO-blk device's block 0
    //     for the "hsqs" magic (0x73717368). ---
    let devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let mut sqfs_index = None;
    for dev in devs.iter().filter(|d| d.class == 0x01) {
        let blk = match VirtioBlk::init_from_pci(dev) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut buf = [0u8; 512];
        if blk.read_blocks(0, &mut buf).is_ok() && buf[0..4] == [0x68, 0x73, 0x71, 0x73] {
            sqfs_index = Some(block::register("squashfs-disk", Box::new(blk)));
            break;
        }
        // Not the SquashFS disk; leak this driver instance and keep probing.
    }
    let idx = sqfs_index.ok_or("no SquashFS disk found among VirtIO devices")?;

    // --- Start a block server on the SquashFS device, allocate a client region,
    //     and spawn the SquashFS server. ---
    let block_handle = block_server::start(idx);
    let client = SharedRegion::alloc(128 * 1024).ok_or("client region alloc failed")?;
    let fs_ep = ipc::create_endpoint("squashfs-fs");

    spawn_server(ServerConfig {
        name: "squashfs-server",
        binary: embedded::SQUASHFS_SERVER,
        fs_endpoint: fs_ep,
        block_endpoint: block_handle.endpoint,
        shared: Some(block_handle.region),
        client_shared: Some(client),
        heap_bytes: 2 * 1024 * 1024,
        arg0: 0,
        arg1: 0,
        filesystem_mount: None,
    });

    // Let the server boot and parse the superblock.
    for _ in 0..1000 {
        sched::yield_now();
    }

    // Helper: place a path in the client region and return its byte length.
    let put_path = |path: &str| -> u64 {
        // SAFETY: exclusive access — no outstanding request to the server.
        let s = unsafe { client.as_slice_mut() };
        s[..path.len()].copy_from_slice(path.as_bytes());
        path.len() as u64
    };

    // --- STAT /version ---
    let plen = put_path("/version");
    let r = ipc::ipc_call(fs_ep, IpcMessage::new([OP_STAT, plen, 0, 0]), 0)
        .map_err(|_| "STAT call failed")?;
    if r.words[0] != STATUS_OK {
        return Err("STAT /version returned error");
    }
    if r.words[1] != 15 {
        return Err("STAT /version wrong size (expected 15)");
    }
    if r.words[2] != 0 {
        return Err("STAT /version should not be a directory");
    }

    // --- OPEN + READ /version, verify contents ---
    let plen = put_path("/version");
    let r = ipc::ipc_call(fs_ep, IpcMessage::new([OP_OPEN, plen, 0, 0]), 0)
        .map_err(|_| "OPEN call failed")?;
    if r.words[0] != STATUS_OK {
        return Err("OPEN /version failed");
    }
    let fd = r.words[1];
    let r = ipc::ipc_call(fs_ep, IpcMessage::new([OP_READ, fd, 0, 15]), 0)
        .map_err(|_| "READ call failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != 15 {
        return Err("READ /version returned wrong length");
    }
    {
        // SAFETY: request complete; read the server's output.
        let s = unsafe { client.as_slice_mut() };
        if &s[..15] != b"THEMELIOS_ROOT\n" {
            return Err("/version content mismatch (fragment read)");
        }
    }

    // --- OPEN + READ /big.bin, verify the deterministic pattern across blocks ---
    let plen = put_path("/big.bin");
    let r = ipc::ipc_call(fs_ep, IpcMessage::new([OP_OPEN, plen, 0, 0]), 0)
        .map_err(|_| "OPEN big.bin failed")?;
    if r.words[0] != STATUS_OK {
        return Err("OPEN /big.bin failed");
    }
    let fd = r.words[1];

    // Pattern byte at file offset i = (i*31 + 7) & 0xFF (matches xtask).
    let check_pattern = |base: u64, len: usize| -> Result<(), &'static str> {
        let s = unsafe { client.as_slice_mut() };
        for j in 0..len {
            let i = base + j as u64;
            let expect = ((i as u32).wrapping_mul(31).wrapping_add(7) & 0xFF) as u8;
            if s[j] != expect {
                return Err("big.bin pattern mismatch");
            }
        }
        Ok(())
    };

    // First 4 KiB (start of block 0).
    let r = ipc::ipc_call(fs_ep, IpcMessage::new([OP_READ, fd, 0, 4096]), 0)
        .map_err(|_| "READ big.bin@0 failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != 4096 {
        return Err("READ big.bin@0 wrong length");
    }
    check_pattern(0, 4096)?;

    // 256 bytes at offset 131072 — the start of the second data block.
    let r = ipc::ipc_call(fs_ep, IpcMessage::new([OP_READ, fd, 131072, 256]), 0)
        .map_err(|_| "READ big.bin@128K failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != 256 {
        return Err("READ big.bin@128K wrong length");
    }
    check_pattern(131072, 256)?;

    // --- READDIR / — verify expected names are present ---
    let plen = put_path("/");
    let r = ipc::ipc_call(fs_ep, IpcMessage::new([OP_OPEN, plen, 0, 0]), 0)
        .map_err(|_| "OPEN / failed")?;
    if r.words[0] != STATUS_OK {
        return Err("OPEN / failed");
    }
    let root_fd = r.words[1];
    let r = ipc::ipc_call(fs_ep, IpcMessage::new([OP_READDIR, root_fd, 64, 0]), 0)
        .map_err(|_| "READDIR / failed")?;
    if r.words[0] != STATUS_OK {
        return Err("READDIR / returned error");
    }
    let count = r.words[1] as usize;
    if count < 5 {
        return Err("READDIR / returned too few entries");
    }
    // Parse packed entries: [u16 name_len, u16 type, name bytes]*.
    let mut names: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
    {
        let s = unsafe { client.as_slice_mut() };
        let mut pos = 0usize;
        for _ in 0..count {
            if pos + 4 > s.len() {
                break;
            }
            let nlen = u16::from_le_bytes([s[pos], s[pos + 1]]) as usize;
            pos += 4;
            if pos + nlen > s.len() {
                break;
            }
            names.push(alloc::string::String::from_utf8_lossy(&s[pos..pos + nlen]).into_owned());
            pos += nlen;
        }
    }
    for expect in ["version", "hello.txt", "big.bin", "etc", "docs"] {
        if !names.iter().any(|n| n == expect) {
            return Err("READDIR / missing an expected entry");
        }
    }

    // --- Read a nested file /docs/readme.txt ---
    let plen = put_path("/docs/readme.txt");
    let r = ipc::ipc_call(fs_ep, IpcMessage::new([OP_OPEN, plen, 0, 0]), 0)
        .map_err(|_| "OPEN nested failed")?;
    if r.words[0] != STATUS_OK {
        return Err("OPEN /docs/readme.txt failed");
    }
    let fd = r.words[1];
    let r = ipc::ipc_call(fs_ep, IpcMessage::new([OP_READ, fd, 0, 21]), 0)
        .map_err(|_| "READ nested failed")?;
    if r.words[0] != STATUS_OK {
        return Err("READ /docs/readme.txt failed");
    }
    {
        let s = unsafe { client.as_slice_mut() };
        let n = r.words[1] as usize;
        if &s[..n] != b"nested file in /docs\n" {
            return Err("/docs/readme.txt content mismatch");
        }
    }

    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_squashfs_server() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_overlay_server — Overlay filesystem server (Phase 3.6)
// ============================================================

/// Test the overlay server: a RAM upper layer merged over the SquashFS lower.
///
/// Brings up SquashFS (lower) and the overlay (upper) and verifies the
/// overlayfs semantics end to end:
/// 1. Read-through: a lower file is visible through the overlay
/// 2. Create + write + read: a new file lives only in the RAM upper layer
/// 3. Copy-up: writing a lower file copies it up and modifies the upper copy
///    (the lower image is untouched)
/// 4. Whiteout: deleting a lower file hides it from the merged view
/// 5. Readdir merge: listing shows lower + upper entries, minus whiteouts
#[cfg(target_arch = "x86_64")]
fn test_overlay_server() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use crate::drivers::block::BlockDevice;
    use crate::drivers::virtio::blk::VirtioBlk;
    use crate::drivers::{block, block_server, pci};
    use crate::ipc::{self, IpcMessage};
    use crate::mm::shared::SharedRegion;
    use crate::process::embedded;
    use crate::process::server::{spawn_server, ServerConfig};
    use crate::sched;

    const OP_OPEN: u64 = 1;
    const OP_READ: u64 = 2;
    const OP_WRITE: u64 = 3;
    const OP_STAT: u64 = 5;
    const OP_READDIR: u64 = 6;
    const OP_CREATE: u64 = 7;
    const OP_UNLINK: u64 = 9;
    const STATUS_OK: u64 = 0;

    // --- Locate the SquashFS disk and bring up the lower (SquashFS) server. ---
    let devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let mut sqfs_index = None;
    for dev in devs.iter().filter(|d| d.class == 0x01) {
        let blk = match VirtioBlk::init_from_pci(dev) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut buf = [0u8; 512];
        if blk.read_blocks(0, &mut buf).is_ok() && buf[0..4] == [0x68, 0x73, 0x71, 0x73] {
            sqfs_index = Some(block::register("squashfs-disk-ovl", Box::new(blk)));
            break;
        }
    }
    let idx = sqfs_index.ok_or("no SquashFS disk found")?;
    let block_handle = block_server::start(idx);

    // SquashFS server with its own client region (shared with the overlay).
    let sqfs_client = SharedRegion::alloc(128 * 1024).ok_or("sqfs client alloc failed")?;
    let sqfs_ep = ipc::create_endpoint("ovl-lower-sqfs");
    spawn_server(ServerConfig {
        name: "squashfs-server",
        binary: embedded::SQUASHFS_SERVER,
        fs_endpoint: sqfs_ep,
        block_endpoint: block_handle.endpoint,
        shared: Some(block_handle.region),
        client_shared: Some(sqfs_client),
        heap_bytes: 2 * 1024 * 1024,
        arg0: 0,
        arg1: 0,
        filesystem_mount: None,
    });

    // Overlay server: lower = SquashFS (arg0 = sqfs_ep, block-slot region =
    // sqfs_client so the overlay can forward through it); own client region.
    let ovl_client = SharedRegion::alloc(128 * 1024).ok_or("overlay client alloc failed")?;
    let ovl_ep = ipc::create_endpoint("overlay-fs");
    spawn_server(ServerConfig {
        name: "overlay-server",
        binary: embedded::OVERLAY_SERVER,
        fs_endpoint: ovl_ep,
        block_endpoint: 0,
        shared: Some(sqfs_client),
        client_shared: Some(ovl_client),
        heap_bytes: 4 * 1024 * 1024,
        arg0: sqfs_ep,
        arg1: 0,
        filesystem_mount: None,
    });

    // Let both servers boot (SquashFS parses its superblock; overlay is ready).
    for _ in 0..2000 {
        sched::yield_now();
    }

    let put = |bytes: &[u8]| -> u64 {
        let s = unsafe { ovl_client.as_slice_mut() };
        s[..bytes.len()].copy_from_slice(bytes);
        bytes.len() as u64
    };
    let call = |words: [u64; 4]| ipc::ipc_call(ovl_ep, IpcMessage::new(words), 0);

    // --- 1. Read-through: /version comes from the lower layer ---
    let plen = put(b"/version");
    let r = call([OP_OPEN, plen, 0, 0]).map_err(|_| "open /version failed")?;
    if r.words[0] != STATUS_OK {
        return Err("overlay open /version (read-through) failed");
    }
    let fd = r.words[1];
    let r = call([OP_READ, fd, 0, 15]).map_err(|_| "read /version failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != 15 {
        return Err("overlay read-through wrong length");
    }
    {
        let s = unsafe { ovl_client.as_slice_mut() };
        if &s[..15] != b"THEMELIOS_ROOT\n" {
            return Err("overlay read-through content mismatch");
        }
    }

    // --- 2. Create a new file in the upper layer, write, read back ---
    let plen = put(b"/newfile");
    let r = call([OP_CREATE, plen, 0, 0]).map_err(|_| "create failed")?;
    if r.words[0] != STATUS_OK {
        return Err("overlay create /newfile failed");
    }
    let fd = r.words[1];
    let payload = b"hello overlay";
    let dlen = put(payload);
    let r = call([OP_WRITE, fd, 0, dlen]).map_err(|_| "write failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != payload.len() as u64 {
        return Err("overlay write /newfile failed");
    }
    let r = call([OP_READ, fd, 0, payload.len() as u64]).map_err(|_| "read newfile failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != payload.len() as u64 {
        return Err("overlay read /newfile wrong length");
    }
    {
        let s = unsafe { ovl_client.as_slice_mut() };
        if &s[..payload.len()] != payload {
            return Err("overlay new file content mismatch");
        }
    }

    // --- 3. Copy-up: modify a lower file; the upper copy reflects the change ---
    let plen = put(b"/hello.txt");
    let r = call([OP_OPEN, plen, 0, 0]).map_err(|_| "open hello failed")?;
    if r.words[0] != STATUS_OK {
        return Err("overlay open /hello.txt failed");
    }
    let fd = r.words[1];
    let dlen = put(b"J"); // overwrite first byte: "Hello..." -> "Jello..."
    let r = call([OP_WRITE, fd, 0, dlen]).map_err(|_| "copy-up write failed")?;
    if r.words[0] != STATUS_OK {
        return Err("overlay copy-up write failed");
    }
    let r = call([OP_READ, fd, 0, 21]).map_err(|_| "post-copyup read failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != 21 {
        return Err("overlay post-copy-up read wrong length");
    }
    {
        let s = unsafe { ovl_client.as_slice_mut() };
        if &s[..21] != b"Jello from SquashFS!\n" {
            return Err("overlay copy-up content wrong");
        }
    }

    // --- 4. Whiteout: deleting a lower file hides it ---
    let plen = put(b"/big.bin");
    let r = call([OP_UNLINK, plen, 0, 0]).map_err(|_| "unlink failed")?;
    if r.words[0] != STATUS_OK {
        return Err("overlay unlink /big.bin failed");
    }
    let plen = put(b"/big.bin");
    let r = call([OP_STAT, plen, 0, 0]).map_err(|_| "stat-after-unlink failed")?;
    if r.words[0] == STATUS_OK {
        return Err("overlay whiteout did not hide /big.bin");
    }

    // --- 5. Readdir merge: lower + upper, whiteouts removed ---
    let plen = put(b"/");
    let r = call([OP_OPEN, plen, 0, 0]).map_err(|_| "open / failed")?;
    let root_fd = r.words[1];
    let r = call([OP_READDIR, root_fd, 64, 0]).map_err(|_| "readdir / failed")?;
    if r.words[0] != STATUS_OK {
        return Err("overlay readdir / failed");
    }
    let count = r.words[1] as usize;
    let mut names: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
    {
        let s = unsafe { ovl_client.as_slice_mut() };
        let mut pos = 0usize;
        for _ in 0..count {
            if pos + 4 > s.len() {
                break;
            }
            let nlen = u16::from_le_bytes([s[pos], s[pos + 1]]) as usize;
            pos += 4;
            if pos + nlen > s.len() {
                break;
            }
            names.push(alloc::string::String::from_utf8_lossy(&s[pos..pos + nlen]).into_owned());
            pos += nlen;
        }
    }
    // Upper-created /newfile and lower /version must appear; whiteouted /big.bin
    // must not.
    if !names.iter().any(|n| n == "newfile") {
        return Err("overlay readdir missing upper-created newfile");
    }
    if !names.iter().any(|n| n == "version") {
        return Err("overlay readdir missing lower version");
    }
    if names.iter().any(|n| n == "big.bin") {
        return Err("overlay readdir still shows whiteouted big.bin");
    }

    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_overlay_server() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_ext2_read — ext2 server read path (Phase 3.7, step 1)
// ============================================================

/// Test the ext2 server's read path against a pre-populated ext2 image.
///
/// Probes the VirtIO disks for the ext2 magic, brings up the ext2 server, and
/// verifies: STAT, file read (small + a large file spanning direct AND
/// single-indirect blocks), directory listing, and nested-path resolution.
#[cfg(target_arch = "x86_64")]
fn test_ext2_read() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use crate::drivers::block::BlockDevice;
    use crate::drivers::virtio::blk::VirtioBlk;
    use crate::drivers::{block, block_server, pci};
    use crate::ipc::{self, IpcMessage};
    use crate::mm::shared::SharedRegion;
    use crate::process::embedded;
    use crate::process::server::{spawn_server, ServerConfig};
    use crate::sched;

    const OP_OPEN: u64 = 1;
    const OP_READ: u64 = 2;
    const OP_STAT: u64 = 5;
    const OP_READDIR: u64 = 6;
    const STATUS_OK: u64 = 0;

    // --- Locate the ext2 disk: superblock magic 0xEF53 at byte 1080 (sector 2,
    //     offset 56). ---
    let devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let mut ext2_index = None;
    for dev in devs.iter().filter(|d| d.class == 0x01) {
        let blk = match VirtioBlk::init_from_pci(dev) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut buf = [0u8; 512];
        // Sector 2 = bytes 1024..1536; ext2 magic (0xEF53) at offset 56.
        if blk.read_blocks(2, &mut buf).is_ok() && buf[56] == 0x53 && buf[57] == 0xEF {
            ext2_index = Some(block::register("ext2-disk", Box::new(blk)));
            break;
        }
    }
    let idx = ext2_index.ok_or("no ext2 disk found among VirtIO devices")?;

    let block_handle = block_server::start(idx);
    let client = SharedRegion::alloc(128 * 1024).ok_or("client region alloc failed")?;
    let fs_ep = ipc::create_endpoint("ext2-fs");
    spawn_server(ServerConfig {
        name: "ext2-server",
        binary: embedded::EXT2_SERVER,
        fs_endpoint: fs_ep,
        block_endpoint: block_handle.endpoint,
        shared: Some(block_handle.region),
        client_shared: Some(client),
        heap_bytes: 2 * 1024 * 1024,
        arg0: 0,
        arg1: 0,
        filesystem_mount: None,
    });

    for _ in 0..1000 {
        sched::yield_now();
    }

    let put = |bytes: &[u8]| -> u64 {
        let s = unsafe { client.as_slice_mut() };
        s[..bytes.len()].copy_from_slice(bytes);
        bytes.len() as u64
    };
    let call = |w: [u64; 4]| ipc::ipc_call(fs_ep, IpcMessage::new(w), 0);

    // --- STAT /hello.txt (17 bytes) ---
    let p = put(b"/hello.txt");
    let r = call([OP_STAT, p, 0, 0]).map_err(|_| "STAT call failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != 17 {
        return Err("ext2 STAT /hello.txt wrong size");
    }

    // --- OPEN + READ /hello.txt ---
    let p = put(b"/hello.txt");
    let r = call([OP_OPEN, p, 0, 0]).map_err(|_| "OPEN failed")?;
    if r.words[0] != STATUS_OK {
        return Err("ext2 OPEN /hello.txt failed");
    }
    let fd = r.words[1];
    let r = call([OP_READ, fd, 0, 17]).map_err(|_| "READ failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != 17 {
        return Err("ext2 READ /hello.txt wrong length");
    }
    {
        let s = unsafe { client.as_slice_mut() };
        if &s[..17] != b"Hello from ext2!\n" {
            return Err("ext2 /hello.txt content mismatch");
        }
    }

    // --- /data.bin: verify pattern across direct AND single-indirect blocks ---
    let p = put(b"/data.bin");
    let r = call([OP_OPEN, p, 0, 0]).map_err(|_| "OPEN data.bin failed")?;
    if r.words[0] != STATUS_OK {
        return Err("ext2 OPEN /data.bin failed");
    }
    let fd = r.words[1];
    // Pattern byte at offset i = (i*37 + 11) & 0xFF (matches xtask).
    let check = |base: u64, len: usize| -> Result<(), &'static str> {
        let s = unsafe { client.as_slice_mut() };
        for j in 0..len {
            let i = base + j as u64;
            let expect = ((i as u32).wrapping_mul(37).wrapping_add(11) & 0xFF) as u8;
            if s[j] != expect {
                return Err("ext2 data.bin pattern mismatch");
            }
        }
        Ok(())
    };
    // First 4 KiB: direct blocks.
    let r = call([OP_READ, fd, 0, 4096]).map_err(|_| "READ data.bin@0 failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != 4096 {
        return Err("ext2 READ data.bin@0 wrong length");
    }
    check(0, 4096)?;
    // Offset 13000: past the 12 direct blocks (12 KiB) — single-indirect region.
    let r = call([OP_READ, fd, 13000, 256]).map_err(|_| "READ data.bin@13000 failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != 256 {
        return Err("ext2 READ data.bin@13000 wrong length (single-indirect)");
    }
    check(13000, 256)?;

    // --- READDIR / ---
    let p = put(b"/");
    let r = call([OP_OPEN, p, 0, 0]).map_err(|_| "OPEN / failed")?;
    let root_fd = r.words[1];
    let r = call([OP_READDIR, root_fd, 64, 0]).map_err(|_| "READDIR / failed")?;
    if r.words[0] != STATUS_OK {
        return Err("ext2 READDIR / failed");
    }
    let count = r.words[1] as usize;
    let mut names: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
    {
        let s = unsafe { client.as_slice_mut() };
        let mut pos = 0usize;
        for _ in 0..count {
            if pos + 4 > s.len() {
                break;
            }
            let nlen = u16::from_le_bytes([s[pos], s[pos + 1]]) as usize;
            pos += 4;
            if pos + nlen > s.len() {
                break;
            }
            names.push(alloc::string::String::from_utf8_lossy(&s[pos..pos + nlen]).into_owned());
            pos += nlen;
        }
    }
    for expect in ["hello.txt", "sub", "data.bin", "lost+found"] {
        if !names.iter().any(|n| n == expect) {
            return Err("ext2 READDIR / missing an expected entry");
        }
    }

    // --- Nested: /sub/nested.txt ---
    let p = put(b"/sub/nested.txt");
    let r = call([OP_OPEN, p, 0, 0]).map_err(|_| "OPEN nested failed")?;
    if r.words[0] != STATUS_OK {
        return Err("ext2 OPEN /sub/nested.txt failed");
    }
    let fd = r.words[1];
    let r = call([OP_READ, fd, 0, 17]).map_err(|_| "READ nested failed")?;
    if r.words[0] != STATUS_OK {
        return Err("ext2 READ /sub/nested.txt failed");
    }
    {
        let s = unsafe { client.as_slice_mut() };
        let n = r.words[1] as usize;
        if &s[..n] != b"nested ext2 file\n" {
            return Err("ext2 /sub/nested.txt content mismatch");
        }
    }

    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_ext2_read() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_ext2_write — ext2 server write path (Phase 3.7, step 2)
// ============================================================

/// Test the ext2 server's write path: create, write (incl. indirect blocks),
/// mkdir, unlink, and inode/block reuse after unlink.
#[cfg(target_arch = "x86_64")]
fn test_ext2_write() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use crate::drivers::block::BlockDevice;
    use crate::drivers::virtio::blk::VirtioBlk;
    use crate::drivers::{block, block_server, pci};
    use crate::ipc::{self, IpcMessage};
    use crate::mm::shared::SharedRegion;
    use crate::process::embedded;
    use crate::process::server::{spawn_server, ServerConfig};
    use crate::sched;

    const OP_OPEN: u64 = 1;
    const OP_READ: u64 = 2;
    const OP_WRITE: u64 = 3;
    const OP_STAT: u64 = 5;
    const OP_READDIR: u64 = 6;
    const OP_CREATE: u64 = 7;
    const OP_MKDIR: u64 = 8;
    const OP_UNLINK: u64 = 9;
    const STATUS_OK: u64 = 0;

    // Bring up the ext2 server on the ext2 disk.
    let devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let mut ext2_index = None;
    for dev in devs.iter().filter(|d| d.class == 0x01) {
        let blk = match VirtioBlk::init_from_pci(dev) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut buf = [0u8; 512];
        if blk.read_blocks(2, &mut buf).is_ok() && buf[56] == 0x53 && buf[57] == 0xEF {
            ext2_index = Some(block::register("ext2-disk-w", Box::new(blk)));
            break;
        }
    }
    let idx = ext2_index.ok_or("no ext2 disk found")?;
    let block_handle = block_server::start(idx);
    let client = SharedRegion::alloc(128 * 1024).ok_or("client region alloc failed")?;
    let fs_ep = ipc::create_endpoint("ext2-fs-w");
    spawn_server(ServerConfig {
        name: "ext2-server",
        binary: embedded::EXT2_SERVER,
        fs_endpoint: fs_ep,
        block_endpoint: block_handle.endpoint,
        shared: Some(block_handle.region),
        client_shared: Some(client),
        heap_bytes: 2 * 1024 * 1024,
        arg0: 0,
        arg1: 0,
        filesystem_mount: None,
    });
    for _ in 0..1000 {
        sched::yield_now();
    }

    let put = |bytes: &[u8]| -> u64 {
        let s = unsafe { client.as_slice_mut() };
        s[..bytes.len()].copy_from_slice(bytes);
        bytes.len() as u64
    };
    let call = |w: [u64; 4]| ipc::ipc_call(fs_ep, IpcMessage::new(w), 0);

    // --- 1. Create a file, write, read back ---
    let p = put(b"/wtest");
    let r = call([OP_CREATE, p, 0, 0]).map_err(|_| "create failed")?;
    if r.words[0] != STATUS_OK {
        return Err("ext2 create /wtest failed");
    }
    let fd = r.words[1];
    let msg = b"ext2 write works!";
    let dlen = put(msg);
    let r = call([OP_WRITE, fd, 0, dlen]).map_err(|_| "write failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != msg.len() as u64 {
        return Err("ext2 write /wtest failed");
    }
    let r = call([OP_READ, fd, 0, msg.len() as u64]).map_err(|_| "read failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != msg.len() as u64 {
        return Err("ext2 read-back /wtest wrong length");
    }
    {
        let s = unsafe { client.as_slice_mut() };
        if &s[..msg.len()] != msg {
            return Err("ext2 /wtest content mismatch");
        }
    }
    // STAT reflects the new size.
    let p = put(b"/wtest");
    let r = call([OP_STAT, p, 0, 0]).map_err(|_| "stat failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != msg.len() as u64 {
        return Err("ext2 STAT /wtest wrong size");
    }

    // --- 2. Large write spanning direct + single-indirect blocks ---
    let p = put(b"/wbig");
    let r = call([OP_CREATE, p, 0, 0]).map_err(|_| "create wbig failed")?;
    let fd = r.words[1];
    // Write 20000 bytes in 4 KiB chunks (client region holds the chunk).
    let pat = |i: u64| ((i as u32).wrapping_mul(53).wrapping_add(3) & 0xFF) as u8;
    const BIG: usize = 20000;
    let mut off = 0usize;
    while off < BIG {
        let len = (BIG - off).min(4096);
        {
            let s = unsafe { client.as_slice_mut() };
            for j in 0..len {
                s[j] = pat((off + j) as u64);
            }
        }
        let r = call([OP_WRITE, fd, off as u64, len as u64]).map_err(|_| "wbig write failed")?;
        if r.words[0] != STATUS_OK || r.words[1] != len as u64 {
            return Err("ext2 wbig write wrong length");
        }
        off += len;
    }
    // Read back a slice from the single-indirect region (offset > 12 KiB).
    let r = call([OP_READ, fd, 13000, 512]).map_err(|_| "wbig read failed")?;
    if r.words[0] != STATUS_OK || r.words[1] != 512 {
        return Err("ext2 wbig read wrong length (indirect)");
    }
    {
        let s = unsafe { client.as_slice_mut() };
        for j in 0..512 {
            if s[j] != pat(13000 + j as u64) {
                return Err("ext2 wbig indirect pattern mismatch");
            }
        }
    }

    // --- 3. mkdir + readdir ---
    let p = put(b"/wdir");
    let r = call([OP_MKDIR, p, 0, 0]).map_err(|_| "mkdir failed")?;
    if r.words[0] != STATUS_OK {
        return Err("ext2 mkdir /wdir failed");
    }
    let p = put(b"/wdir");
    let r = call([OP_OPEN, p, 0, 0]).map_err(|_| "open wdir failed")?;
    let dfd = r.words[1];
    let r = call([OP_READDIR, dfd, 16, 0]).map_err(|_| "readdir wdir failed")?;
    if r.words[0] != STATUS_OK || r.words[1] < 2 {
        return Err("ext2 new dir missing . and ..");
    }

    // --- 4. Unlink, verify gone ---
    let p = put(b"/wtest");
    let r = call([OP_UNLINK, p, 0, 0]).map_err(|_| "unlink failed")?;
    if r.words[0] != STATUS_OK {
        return Err("ext2 unlink /wtest failed");
    }
    let p = put(b"/wtest");
    let r = call([OP_STAT, p, 0, 0]).map_err(|_| "stat-after-unlink failed")?;
    if r.words[0] == STATUS_OK {
        return Err("ext2 /wtest still present after unlink");
    }

    // --- 5. Reuse after unlink: create again, write, read back ---
    let p = put(b"/wtest2");
    let r = call([OP_CREATE, p, 0, 0]).map_err(|_| "recreate failed")?;
    if r.words[0] != STATUS_OK {
        return Err("ext2 recreate failed");
    }
    let fd = r.words[1];
    let msg2 = b"reused";
    let dlen = put(msg2);
    let r = call([OP_WRITE, fd, 0, dlen]).map_err(|_| "rewrite failed")?;
    if r.words[0] != STATUS_OK {
        return Err("ext2 rewrite failed");
    }
    let r = call([OP_READ, fd, 0, msg2.len() as u64]).map_err(|_| "reread failed")?;
    {
        let s = unsafe { client.as_slice_mut() };
        if r.words[1] != msg2.len() as u64 || &s[..msg2.len()] != msg2 {
            return Err("ext2 reuse-after-unlink content mismatch");
        }
    }

    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_ext2_write() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_vfs_capability — VFS dispatch + capability gating (Phase 3.8)
// ============================================================

/// Test the kernel VFS layer: capability-checked open/read/stat that route to a
/// filesystem server through the mount table, and denial without the capability.
#[cfg(target_arch = "x86_64")]
fn test_vfs_capability() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use crate::cap::{Capability, CapHandle, CapRights, CapType};
    use crate::drivers::block::BlockDevice;
    use crate::drivers::virtio::blk::VirtioBlk;
    use crate::drivers::{block, block_server, pci};
    use crate::fs::{self, FsError};
    use crate::ipc;
    use crate::mm::shared::SharedRegion;
    use crate::process::{self, embedded};
    use crate::process::server::{spawn_server, ServerConfig};
    use crate::sched;

    // Bring up a SquashFS server and register it as a mount.
    let devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let mut sqfs_index = None;
    for dev in devs.iter().filter(|d| d.class == 0x01) {
        let blk = match VirtioBlk::init_from_pci(dev) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut buf = [0u8; 512];
        if blk.read_blocks(0, &mut buf).is_ok() && buf[0..4] == [0x68, 0x73, 0x71, 0x73] {
            sqfs_index = Some(block::register("squashfs-vfs", Box::new(blk)));
            break;
        }
    }
    let idx = sqfs_index.ok_or("no SquashFS disk for VFS test")?;
    let block_handle = block_server::start(idx);
    let sqfs_client = SharedRegion::alloc(128 * 1024).ok_or("client alloc failed")?;
    let sqfs_ep = ipc::create_endpoint("vfs-sqfs");
    spawn_server(ServerConfig {
        name: "squashfs-server",
        binary: embedded::SQUASHFS_SERVER,
        fs_endpoint: sqfs_ep,
        block_endpoint: block_handle.endpoint,
        shared: Some(block_handle.region),
        client_shared: Some(sqfs_client),
        heap_bytes: 2 * 1024 * 1024,
        arg0: 0,
        arg1: 0,
        filesystem_mount: None,
    });
    for _ in 0..1000 {
        sched::yield_now();
    }

    // Register the mount. The kernel↔server channel is the server's client region.
    let mount_id = fs::register_mount(sqfs_ep, sqfs_client);

    // --- A process WITH a Filesystem capability can open/read/stat ---
    let (pid, _) = process::create_process("vfs-test", None);
    let fs_handle = process::with_cspace_mut(pid, |cspace| {
        cspace.insert(Capability {
            cap_type: CapType::Filesystem { mount_id },
            rights: CapRights::READ | CapRights::WRITE,
            parent: None,
        })
    })
    .ok_or("no cspace")?
    .map_err(|_| "insert fs cap failed")?;

    // stat /version
    let (size, is_dir) = fs::vfs_stat(pid, fs_handle, b"/version").map_err(|_| "vfs_stat failed")?;
    if size != 15 || is_dir {
        return Err("vfs_stat /version wrong result");
    }

    // open + read /version
    let fd_raw = fs::vfs_open(pid, fs_handle, b"/version").map_err(|_| "vfs_open failed")?;
    let fd_handle = CapHandle::from_raw(fd_raw);
    let mut buf = [0u8; 15];
    let n = fs::vfs_read(pid, fd_handle, 0, &mut buf).map_err(|_| "vfs_read failed")?;
    if n != 15 || &buf != b"THEMELIOS_ROOT\n" {
        return Err("vfs_read /version content mismatch");
    }
    fs::vfs_close(pid, fd_handle).map_err(|_| "vfs_close failed")?;

    // --- A process WITHOUT the capability is denied ---
    let (pid2, _) = process::create_process("vfs-noperm", None);
    // No Filesystem capability inserted; the NULL handle resolves to nothing.
    match fs::vfs_open(pid2, CapHandle::NULL, b"/version") {
        Err(FsError::PermissionDenied) => {}
        _ => return Err("open without capability should be PermissionDenied"),
    }

    // A non-Filesystem capability also must not grant access.
    let bogus = process::with_cspace_mut(pid2, |cspace| {
        cspace.insert(Capability {
            cap_type: CapType::Endpoint { endpoint_id: 1, badge: 0 },
            rights: CapRights::READ | CapRights::WRITE,
            parent: None,
        })
    })
    .ok_or("no cspace")?
    .map_err(|_| "insert bogus failed")?;
    match fs::vfs_open(pid2, bogus, b"/version") {
        Err(FsError::PermissionDenied) => {}
        _ => return Err("open with wrong cap type should be PermissionDenied"),
    }

    process::destroy_process(pid);
    process::destroy_process(pid2);
    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_vfs_capability() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_fs_syscalls — FS syscalls from ring 3 (Phase 3.8, step 2)
// ============================================================

/// Test the filesystem syscalls end to end from a real ring-3 process.
///
/// Brings up a SquashFS mount, spawns the `fstest-client` server with a granted
/// `Filesystem` capability, and waits for its result code. The client performs
/// `stat`/`open`/`read_file` syscalls (exercising user-pointer copy in/out and
/// capability checks) and confirms a null-capability call is rejected.
#[cfg(target_arch = "x86_64")]
fn test_fs_syscalls() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use crate::drivers::block::BlockDevice;
    use crate::drivers::virtio::blk::VirtioBlk;
    use crate::drivers::{block, block_server, pci};
    use crate::fs;
    use crate::ipc;
    use crate::mm::shared::SharedRegion;
    use crate::process::embedded;
    use crate::process::server::{spawn_server, ServerConfig};
    use crate::sched;

    // SquashFS mount (lower-level data plane already proven; reused here).
    let devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let mut sqfs_index = None;
    for dev in devs.iter().filter(|d| d.class == 0x01) {
        let blk = match VirtioBlk::init_from_pci(dev) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut buf = [0u8; 512];
        if blk.read_blocks(0, &mut buf).is_ok() && buf[0..4] == [0x68, 0x73, 0x71, 0x73] {
            sqfs_index = Some(block::register("squashfs-sys", Box::new(blk)));
            break;
        }
    }
    let idx = sqfs_index.ok_or("no SquashFS disk for syscall test")?;
    let block_handle = block_server::start(idx);
    let sqfs_client = SharedRegion::alloc(128 * 1024).ok_or("client alloc failed")?;
    let sqfs_ep = ipc::create_endpoint("sys-sqfs");
    spawn_server(ServerConfig {
        name: "squashfs-server",
        binary: embedded::SQUASHFS_SERVER,
        fs_endpoint: sqfs_ep,
        block_endpoint: block_handle.endpoint,
        shared: Some(block_handle.region),
        client_shared: Some(sqfs_client),
        heap_bytes: 2 * 1024 * 1024,
        arg0: 0,
        arg1: 0,
        filesystem_mount: None,
    });
    for _ in 0..1000 {
        sched::yield_now();
    }
    let mount_id = fs::register_mount(sqfs_ep, sqfs_client);

    // Spawn the ring-3 client, granting it a Filesystem capability for the mount
    // and a result endpoint to report back on.
    let result_ep = ipc::create_endpoint("fstest-result");
    spawn_server(ServerConfig {
        name: "fstest-client",
        binary: embedded::FSTEST_CLIENT,
        fs_endpoint: result_ep,
        block_endpoint: 0,
        shared: None,
        client_shared: None,
        heap_bytes: 256 * 1024,
        arg0: 0,
        arg1: 0,
        filesystem_mount: Some(mount_id),
    });

    // The client reports its result code on result_ep. 0 = all syscalls passed.
    let msg = ipc::ipc_receive(result_ep).map_err(|_| "no result from fstest-client")?;
    match msg.words[0] {
        0 => Ok(()),
        1 => Err("fstest-client: SYS_STAT failed"),
        2 => Err("fstest-client: stat size wrong"),
        3 => Err("fstest-client: SYS_OPEN failed"),
        4 => Err("fstest-client: SYS_READ_FILE wrong length"),
        5 => Err("fstest-client: read content mismatch"),
        6 => Err("fstest-client: null-capability open was NOT rejected"),
        100 => Err("fstest-client: no Filesystem capability granted"),
        _ => Err("fstest-client: unknown failure"),
    }
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_fs_syscalls() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_virtio_net — VirtIO-net driver TX/RX round-trip (Phase 4.0)
// ============================================================

/// Test the VirtIO-net driver end-to-end by ARP-resolving the slirp gateway.
///
/// Brings up the NIC, registers it, transmits a broadcast ARP request for the
/// user-mode-networking gateway (10.0.2.2), and polls the receive path for the
/// gateway's ARP reply. A correct reply proves the whole driver works: feature
/// negotiation, both virtqueues, TX (frame consumed by the device) and polled RX
/// (a frame delivered into a pre-posted buffer), and the 12-byte VirtIO-net
/// header handling (a wrong header length would misalign the parsed EtherType).
#[cfg(target_arch = "x86_64")]
fn test_virtio_net() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use alloc::vec;
    use crate::drivers::pci;
    use crate::drivers::virtio::net::VirtioNet;
    use crate::net::device;
    use crate::sched;

    // Find the VirtIO network device (vendor 0x1AF4, PCI class 0x02 = network).
    let virtio_devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let dev = virtio_devs
        .iter()
        .find(|d| d.class == 0x02)
        .ok_or("no VirtIO network device on the PCI bus")?;
    let nic = VirtioNet::init_from_pci(dev).map_err(|_| "VirtioNet init failed")?;

    // Register it and fetch it back through the registry.
    let index = device::register("virtio-net0", Box::new(nic));
    let netdev = device::get(index).ok_or("net registry lookup failed")?;

    let mac = netdev.mac();
    if mac == [0u8; 6] {
        return Err("device reports an all-zero MAC");
    }
    if netdev.mtu() < 576 {
        return Err("device reports an implausible MTU");
    }

    // --- Build an ARP request: "who has 10.0.2.2?" (the slirp gateway) ---
    // Ethernet header (14 bytes) + ARP (28 bytes) = 42 bytes.
    let mut frame = vec![0u8; 42];
    frame[0..6].copy_from_slice(&[0xff; 6]); // dst = broadcast
    frame[6..12].copy_from_slice(&mac); // src = our MAC
    frame[12..14].copy_from_slice(&[0x08, 0x06]); // EtherType = ARP
    frame[14..16].copy_from_slice(&[0x00, 0x01]); // htype = Ethernet
    frame[16..18].copy_from_slice(&[0x08, 0x00]); // ptype = IPv4
    frame[18] = 6; // hlen
    frame[19] = 4; // plen
    frame[20..22].copy_from_slice(&[0x00, 0x01]); // op = request
    frame[22..28].copy_from_slice(&mac); // sender hardware addr
    frame[28..32].copy_from_slice(&[10, 0, 2, 15]); // sender protocol addr
    // target hardware addr stays zeroed (unknown)
    frame[38..42].copy_from_slice(&[10, 0, 2, 2]); // target protocol addr = gateway

    netdev.transmit(&frame).map_err(|_| "ARP transmit failed")?;

    // --- Poll the receive path for the gateway's ARP reply ---
    let mut rxbuf = [0u8; 2048];
    let mut got_reply = false;
    for _ in 0..8000 {
        let n = netdev.receive(&mut rxbuf).map_err(|_| "receive failed")?;
        if n >= 42 {
            let is_arp = rxbuf[12] == 0x08 && rxbuf[13] == 0x06;
            let is_reply = rxbuf[20] == 0x00 && rxbuf[21] == 0x02;
            let sender_is_gateway = rxbuf[28..32] == [10, 0, 2, 2];
            if is_arp && is_reply && sender_is_gateway {
                got_reply = true;
                break;
            }
        }
        // Yield so the round-trip has time to complete (RX is polled).
        sched::yield_now();
    }

    if !got_reply {
        return Err("no ARP reply from gateway — TX/RX round-trip failed");
    }
    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_virtio_net() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_net_service — kernel net service frame bridge (Phase 4.1)
// ============================================================

/// Test the kernel net service by playing the role of the ring-3 net server.
///
/// Brings up a NIC, starts the net service on it, then drives the service purely
/// through IPC `ipc_call`s (as the ring-3 server will): transmits an ARP request
/// via `MSG_TX_FRAME` (frame staged in the TX shared region) and polls with
/// `MSG_POLL` until the gateway's ARP reply comes back in the RX shared region.
/// Also checks the `MSG_POLL` reply carries a monotonic, non-decreasing timestamp
/// (the same tick source `SYS_UPTIME_MS` exposes). Exercises the whole pull-based
/// bridge — endpoint, both shared regions, RX buffering, and TX — end to end.
#[cfg(target_arch = "x86_64")]
fn test_net_service() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use crate::arch::x86_64::idt;
    use crate::drivers::pci;
    use crate::drivers::virtio::net::VirtioNet;
    use crate::ipc::{self, IpcMessage};
    use crate::net::device::{self, NetDevice};
    use crate::net::net_service::{self, MSG_POLL, MSG_TX_FRAME, STATUS_OK};
    use crate::sched;

    // Bring up a fresh NIC and start the net service on it. (Re-initialising the
    // VirtIO device resets it cleanly; any driver from an earlier test is
    // orphaned but unused.)
    let virtio_devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let dev = virtio_devs
        .iter()
        .find(|d| d.class == 0x02)
        .ok_or("no VirtIO network device on the PCI bus")?;
    let nic = VirtioNet::init_from_pci(dev).map_err(|_| "VirtioNet init failed")?;
    let mac = nic.mac();
    let index = device::register("virtio-net-svc", Box::new(nic));
    let handle = net_service::start(index).ok_or("net_service start failed")?;

    // Let the service task reach its receive loop.
    for _ in 0..1000 {
        sched::yield_now();
    }

    // --- Stage an ARP request for the gateway in the TX shared region ---
    {
        // SAFETY: kernel-side access to the shared region via the HHDM.
        let tx = unsafe { handle.tx_region.as_slice_mut() };
        tx[..42].fill(0);
        tx[0..6].copy_from_slice(&[0xff; 6]); // dst = broadcast
        tx[6..12].copy_from_slice(&mac); // src = our MAC
        tx[12..14].copy_from_slice(&[0x08, 0x06]); // EtherType = ARP
        tx[14..16].copy_from_slice(&[0x00, 0x01]); // htype = Ethernet
        tx[16..18].copy_from_slice(&[0x08, 0x00]); // ptype = IPv4
        tx[18] = 6;
        tx[19] = 4;
        tx[20..22].copy_from_slice(&[0x00, 0x01]); // op = request
        tx[22..28].copy_from_slice(&mac);
        tx[28..32].copy_from_slice(&[10, 0, 2, 15]);
        tx[38..42].copy_from_slice(&[10, 0, 2, 2]);
    }
    let reply = ipc::ipc_call(handle.endpoint, IpcMessage::new([MSG_TX_FRAME, 42, 0, 0]), 0)
        .map_err(|_| "MSG_TX_FRAME call failed")?;
    if reply.words[0] != STATUS_OK {
        return Err("net service refused to transmit the frame");
    }

    // --- Poll the service for the gateway's ARP reply ---
    let mut got_reply = false;
    let mut last_ts = 0u64;
    for _ in 0..8000 {
        let r = ipc::ipc_call(handle.endpoint, IpcMessage::new([MSG_POLL, 0, 0, 0]), 0)
            .map_err(|_| "MSG_POLL call failed")?;
        // Timestamp (word2) must never go backwards.
        if r.words[2] < last_ts {
            return Err("net service timestamp went backwards");
        }
        last_ts = r.words[2];

        let n = r.words[1] as usize;
        if n >= 42 {
            // SAFETY: kernel-side access to the RX shared region via the HHDM.
            let rx = unsafe { handle.rx_region.as_slice_mut() };
            let is_arp = rx[12] == 0x08 && rx[13] == 0x06;
            let is_reply = rx[20] == 0x00 && rx[21] == 0x02;
            let sender_is_gateway = rx[28..32] == [10, 0, 2, 2];
            if is_arp && is_reply && sender_is_gateway {
                got_reply = true;
                break;
            }
        }
        sched::yield_now();
    }

    if !got_reply {
        return Err("no ARP reply via the net service — bridge round-trip failed");
    }
    // By now the timer has ticked; the timestamp source must be live.
    if last_ts == 0 && idt::tick_count() > 0 {
        return Err("net service reported a zero timestamp after ticks elapsed");
    }
    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_net_service() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_net_server_stack — ring-3 smoltcp stack round-trip (Phase 4.2)
// ============================================================

/// Test the real ring-3 net server (smoltcp) end to end by playing the role of
/// the kernel net service.
///
/// Spawns the net server wired to test-controlled endpoint + shared regions,
/// then acts as the service: on the server's first `MSG_POLL`, inject an ARP
/// request "who has 10.0.2.15?" (the server's configured IP); the server's
/// smoltcp stack must process it and `MSG_TX_FRAME` an ARP *reply* carrying the
/// server's MAC and IP. Proves the whole 4.2 path — the net server boots in ring
/// 3, initialises the Interface, and the IPC Device moves a frame in (MSG_POLL)
/// and out (MSG_TX_FRAME) with smoltcp processing in between.
#[cfg(target_arch = "x86_64")]
fn test_net_server_stack() -> Result<(), &'static str> {
    use crate::arch::x86_64::idt;
    use crate::ipc::{self, IpcMessage};
    use crate::mm::shared::SharedRegion;
    use crate::net::net_service::{MSG_POLL, MSG_TX_FRAME, STATUS_OK};
    use crate::process::embedded;
    use crate::process::server::{spawn_server, ServerConfig};
    use crate::sched;

    // Play the net service: allocate the RX/TX frame regions and an endpoint.
    let rx_region = SharedRegion::alloc(2048).ok_or("rx region alloc failed")?;
    let tx_region = SharedRegion::alloc(2048).ok_or("tx region alloc failed")?;
    let svc_ep = ipc::create_endpoint("test-net-svc");
    let fs_ep = ipc::create_endpoint("test-net-server-fs");

    // The MAC the net server will use (packed into arg1); smoltcp answers ARP for
    // 10.0.2.15 with this MAC.
    let mac = [0x52u8, 0x54, 0x00, 0x12, 0x34, 0x56];
    let packed_mac = {
        let mut v = 0u64;
        for (i, b) in mac.iter().enumerate() {
            v |= (*b as u64) << (8 * i);
        }
        v
    };

    spawn_server(ServerConfig {
        name: "net-server",
        binary: embedded::NET_SERVER,
        fs_endpoint: fs_ep,
        block_endpoint: 0,
        shared: Some(rx_region),
        client_shared: Some(tx_region),
        heap_bytes: 8 * 1024 * 1024,
        arg0: svc_ep,
        arg1: packed_mac,
        filesystem_mount: None,
    });

    // An ARP request from the gateway asking for the net server's IP.
    let sender_mac = [0x52u8, 0x55, 0x0a, 0x00, 0x02, 0x02];

    let mut injected = false;
    let mut got_reply = false;
    for _ in 0..20000 {
        // Serve one request from the net server (blocks until it polls us).
        let req = match ipc::ipc_receive(svc_ep) {
            Ok(m) => m,
            Err(_) => {
                sched::yield_now();
                continue;
            }
        };
        let now = idt::tick_count().wrapping_mul(10);
        match req.words[0] {
            MSG_POLL => {
                if !injected {
                    // SAFETY: kernel-side access to the RX region via the HHDM.
                    let b = unsafe { rx_region.as_slice_mut() };
                    b[..42].fill(0);
                    b[0..6].copy_from_slice(&[0xff; 6]); // dst broadcast
                    b[6..12].copy_from_slice(&sender_mac); // src
                    b[12..14].copy_from_slice(&[0x08, 0x06]); // ARP
                    b[14..16].copy_from_slice(&[0x00, 0x01]); // htype
                    b[16..18].copy_from_slice(&[0x08, 0x00]); // ptype
                    b[18] = 6;
                    b[19] = 4;
                    b[20..22].copy_from_slice(&[0x00, 0x01]); // op = request
                    b[22..28].copy_from_slice(&sender_mac); // sha
                    b[28..32].copy_from_slice(&[10, 0, 2, 2]); // spa (gateway)
                    b[38..42].copy_from_slice(&[10, 0, 2, 15]); // tpa (net server)
                    injected = true;
                    let _ = ipc::ipc_reply(svc_ep, req.reply_token, IpcMessage::new([STATUS_OK, 42, now, 0]));
                } else {
                    let _ = ipc::ipc_reply(svc_ep, req.reply_token, IpcMessage::new([STATUS_OK, 0, now, 0]));
                }
            }
            MSG_TX_FRAME => {
                let len = req.words[1] as usize;
                // SAFETY: kernel-side access to the TX region via the HHDM.
                let b = unsafe { tx_region.as_slice_mut() };
                if len >= 42
                    && b[12] == 0x08
                    && b[13] == 0x06 // ARP
                    && b[20] == 0x00
                    && b[21] == 0x02 // reply
                    && b[28..32] == [10, 0, 2, 15] // sender IP = net server
                    && b[22..28] == mac
                // sender MAC = net server MAC
                {
                    got_reply = true;
                }
                let _ = ipc::ipc_reply(svc_ep, req.reply_token, IpcMessage::new([STATUS_OK, 0, 0, 0]));
            }
            _ => {
                let _ = ipc::ipc_reply(svc_ep, req.reply_token, IpcMessage::new([STATUS_OK, 0, 0, 0]));
            }
        }
        if got_reply {
            break;
        }
    }

    if !got_reply {
        return Err("net server did not answer ARP for its IP — stack round-trip failed");
    }
    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_net_server_stack() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_net_icmp_echo — ring-3 smoltcp answers ICMP echo (Phase 4.3)
// ============================================================

/// Standard 16-bit one's-complement Internet checksum.
#[cfg(target_arch = "x86_64")]
fn inet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Test IPv4/ICMP: inject a ping (ICMP echo request) to the net server's IP and
/// verify its smoltcp stack answers with a correct echo reply.
///
/// Plays the net service (like `test_net_server_stack`). First injects an ARP
/// request for the server's IP (which both draws an ARP reply and seeds the
/// server's neighbour cache with the sender's MAC), then injects an ICMP echo
/// request from 10.0.2.2 to the server's 10.0.2.15; the server auto-replies with
/// an ICMP echo reply, transmitted straight back. Proves Ethernet + ARP + IPv4 +
/// ICMP end to end through the ring-3 stack.
#[cfg(target_arch = "x86_64")]
fn test_net_icmp_echo() -> Result<(), &'static str> {
    use alloc::collections::VecDeque;
    use alloc::vec;
    use alloc::vec::Vec;
    use crate::arch::x86_64::idt;
    use crate::ipc::{self, IpcMessage};
    use crate::mm::shared::SharedRegion;
    use crate::net::net_service::{MSG_POLL, MSG_TX_FRAME, STATUS_OK};
    use crate::process::embedded;
    use crate::process::server::{spawn_server, ServerConfig};
    use crate::sched;

    let rx_region = SharedRegion::alloc(2048).ok_or("rx region alloc failed")?;
    let tx_region = SharedRegion::alloc(2048).ok_or("tx region alloc failed")?;
    let svc_ep = ipc::create_endpoint("test-icmp-svc");
    let fs_ep = ipc::create_endpoint("test-icmp-fs");

    let net_mac = [0x52u8, 0x54, 0x00, 0x12, 0x34, 0x56];
    let gw_mac = [0x52u8, 0x55, 0x0a, 0x00, 0x02, 0x02];
    let packed_mac = {
        let mut v = 0u64;
        for (i, b) in net_mac.iter().enumerate() {
            v |= (*b as u64) << (8 * i);
        }
        v
    };

    spawn_server(ServerConfig {
        name: "net-server",
        binary: embedded::NET_SERVER,
        fs_endpoint: fs_ep,
        block_endpoint: 0,
        shared: Some(rx_region),
        client_shared: Some(tx_region),
        heap_bytes: 8 * 1024 * 1024,
        arg0: svc_ep,
        arg1: packed_mac,
        filesystem_mount: None,
    });

    // Frame 1: ARP request "who has 10.0.2.15?" from the gateway (seeds cache).
    let mut arp = vec![0u8; 42];
    arp[0..6].copy_from_slice(&[0xff; 6]);
    arp[6..12].copy_from_slice(&gw_mac);
    arp[12..14].copy_from_slice(&[0x08, 0x06]);
    arp[14..16].copy_from_slice(&[0x00, 0x01]);
    arp[16..18].copy_from_slice(&[0x08, 0x00]);
    arp[18] = 6;
    arp[19] = 4;
    arp[20..22].copy_from_slice(&[0x00, 0x01]);
    arp[22..28].copy_from_slice(&gw_mac);
    arp[28..32].copy_from_slice(&[10, 0, 2, 2]);
    arp[38..42].copy_from_slice(&[10, 0, 2, 15]);

    // Frame 2: ICMP echo request from 10.0.2.2 to 10.0.2.15.
    let mut icmp = vec![0u8; 42];
    icmp[0..6].copy_from_slice(&net_mac);
    icmp[6..12].copy_from_slice(&gw_mac);
    icmp[12..14].copy_from_slice(&[0x08, 0x00]); // IPv4
    icmp[14] = 0x45; // version 4, IHL 5
    icmp[16..18].copy_from_slice(&28u16.to_be_bytes()); // total length (20 + 8)
    icmp[22] = 64; // TTL
    icmp[23] = 1; // protocol = ICMP
    icmp[26..30].copy_from_slice(&[10, 0, 2, 2]); // src IP
    icmp[30..34].copy_from_slice(&[10, 0, 2, 15]); // dst IP
    let ip_csum = inet_checksum(&icmp[14..34]);
    icmp[24..26].copy_from_slice(&ip_csum.to_be_bytes());
    icmp[34] = 8; // ICMP type = echo request
    icmp[38..40].copy_from_slice(&0x1234u16.to_be_bytes()); // identifier
    icmp[40..42].copy_from_slice(&1u16.to_be_bytes()); // sequence
    let icmp_csum = inet_checksum(&icmp[34..42]);
    icmp[36..38].copy_from_slice(&icmp_csum.to_be_bytes());

    let mut inject: VecDeque<Vec<u8>> = VecDeque::new();
    inject.push_back(arp);
    inject.push_back(icmp);

    let mut got_reply = false;
    for _ in 0..20000 {
        let req = match ipc::ipc_receive(svc_ep) {
            Ok(m) => m,
            Err(_) => {
                sched::yield_now();
                continue;
            }
        };
        let now = idt::tick_count().wrapping_mul(10);
        match req.words[0] {
            MSG_POLL => {
                if let Some(frame) = inject.pop_front() {
                    // SAFETY: kernel-side access to the RX region via the HHDM.
                    let b = unsafe { rx_region.as_slice_mut() };
                    let n = frame.len().min(b.len());
                    b[..n].copy_from_slice(&frame[..n]);
                    let _ = ipc::ipc_reply(svc_ep, req.reply_token, IpcMessage::new([STATUS_OK, n as u64, now, 0]));
                } else {
                    let _ = ipc::ipc_reply(svc_ep, req.reply_token, IpcMessage::new([STATUS_OK, 0, now, 0]));
                }
            }
            MSG_TX_FRAME => {
                let len = req.words[1] as usize;
                // SAFETY: kernel-side access to the TX region via the HHDM.
                let b = unsafe { tx_region.as_slice_mut() };
                // ICMP echo reply: IPv4, protocol ICMP, type 0, from us to gateway.
                if len >= 42
                    && b[12] == 0x08
                    && b[13] == 0x00 // IPv4
                    && b[23] == 1 // protocol ICMP
                    && b[34] == 0 // ICMP echo reply (20-byte IP header → ICMP at 34)
                    && b[26..30] == [10, 0, 2, 15] // src IP = net server
                    && b[30..34] == [10, 0, 2, 2] // dst IP = gateway
                {
                    got_reply = true;
                }
                let _ = ipc::ipc_reply(svc_ep, req.reply_token, IpcMessage::new([STATUS_OK, 0, 0, 0]));
            }
            _ => {
                let _ = ipc::ipc_reply(svc_ep, req.reply_token, IpcMessage::new([STATUS_OK, 0, 0, 0]));
            }
        }
        if got_reply {
            break;
        }
    }

    if !got_reply {
        return Err("net server did not answer ICMP echo — IPv4/ICMP round-trip failed");
    }
    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_net_icmp_echo() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_dhcp — DHCPv4 address acquisition end to end (Phase 4.4)
// ============================================================

/// Test the DHCPv4 client by bringing the full stack up against QEMU's slirp
/// DHCP server, exactly as the live boot does.
///
/// Unlike the earlier net-server tests (which *play* the kernel service and feed
/// scripted frames), this test wires the real ring-3 net server to the **real**
/// kernel net service on a real NIC and lets it run — so its smoltcp DHCP client
/// exchanges DISCOVER/OFFER/REQUEST/ACK with slirp's built-in DHCP server. When
/// the lease is acquired the server reports it via `MSG_CONFIG`, which the net
/// service records in [`net_service::status`]. The test polls that status until
/// the interface is configured, then checks slirp's well-known assignment
/// (`10.0.2.15`, gateway `10.0.2.2`). Proves address/gateway acquisition and the
/// whole config-reporting path end to end.
#[cfg(target_arch = "x86_64")]
fn test_dhcp() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use crate::drivers::pci;
    use crate::drivers::virtio::net::VirtioNet;
    use crate::net::{self, device, net_service};
    use crate::net::device::NetDevice;
    use crate::process::embedded;
    use crate::process::server::{spawn_server, ServerConfig};
    use crate::sched;

    // Bring up a fresh NIC and start the real net service on it.
    let virtio_devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let dev = virtio_devs
        .iter()
        .find(|d| d.class == 0x02)
        .ok_or("no VirtIO network device on the PCI bus")?;
    let nic = VirtioNet::init_from_pci(dev).map_err(|_| "VirtioNet init failed")?;
    let mac = nic.mac();
    let index = device::register("virtio-net-dhcp", Box::new(nic));
    let handle = net_service::start(index).ok_or("net_service start failed")?;

    // Let the service task reach its receive loop and publish MAC/MTU.
    for _ in 0..1000 {
        sched::yield_now();
    }

    // Spawn the real ring-3 net server in DHCP mode. arg1 packs the MAC in the
    // low 48 bits with NET_ARG_DHCP (bit 48) set — the same wiring `boot_net`
    // uses live, so this exercises the production DHCP path.
    let fs_ep = crate::ipc::create_endpoint("test-dhcp-fs");
    let packed_mac = {
        let mut v = 0u64;
        for (i, b) in mac.iter().enumerate() {
            v |= (*b as u64) << (8 * i);
        }
        v
    };
    spawn_server(ServerConfig {
        name: "net-server",
        binary: embedded::NET_SERVER,
        fs_endpoint: fs_ep,
        block_endpoint: 0,
        shared: Some(handle.rx_region),
        client_shared: Some(handle.tx_region),
        heap_bytes: 8 * 1024 * 1024,
        arg0: handle.endpoint,
        arg1: packed_mac | net::NET_ARG_DHCP,
        filesystem_mount: None,
    });

    // Drive the scheduler until the DHCP client acquires a lease. slirp answers
    // DISCOVER immediately, so acquisition takes a handful of poll round-trips;
    // the budget is generous to absorb scheduling jitter.
    let mut acquired = false;
    for _ in 0..300_000 {
        let cfg = net_service::status().config;
        if cfg.configured && cfg.addr != [0, 0, 0, 0] {
            // slirp's built-in DHCP server hands out 10.0.2.15 with gateway
            // 10.0.2.2. Anything else means the acquisition path is wrong.
            if cfg.addr != [10, 0, 2, 15] {
                return Err("DHCP acquired an unexpected address");
            }
            match cfg.gateway {
                Some([10, 0, 2, 2]) => {}
                _ => return Err("DHCP did not configure the expected gateway"),
            }
            acquired = true;
            break;
        }
        sched::yield_now();
    }

    if !acquired {
        return Err("net server did not acquire a DHCP lease from slirp");
    }
    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_dhcp() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  Socket API tests (Phase 4.5)
// ============================================================

/// Bring up a NIC + kernel net service + ring-3 net server, map the socket
/// payload region, and register the kernel socket router — the same wiring
/// `boot_net` does. Returns the net server's request endpoint. `dhcp` selects
/// DHCP vs the static fallback address.
#[cfg(target_arch = "x86_64")]
fn spawn_net_server_with_sockets(dhcp: bool, nic_name: &'static str) -> Result<u64, &'static str> {
    use alloc::boxed::Box;
    use crate::drivers::pci;
    use crate::drivers::virtio::net::VirtioNet;
    use crate::mm::addr::VirtAddr;
    use crate::mm::shared::SharedRegion;
    use crate::net::device::NetDevice;
    use crate::net::{self, device, net_service, socket};
    use crate::process::server::{spawn_server, ServerConfig, SERVER_SOCKET_BYTES, SERVER_SOCKET_VIRT};
    use crate::process::{self, embedded};
    use crate::sched;

    let devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let dev = devs
        .iter()
        .find(|d| d.class == 0x02)
        .ok_or("no VirtIO network device on the PCI bus")?;
    let nic = VirtioNet::init_from_pci(dev).map_err(|_| "VirtioNet init failed")?;
    let mac = nic.mac();
    let index = device::register(nic_name, Box::new(nic));
    let handle = net_service::start(index).ok_or("net_service start failed")?;
    for _ in 0..500 {
        sched::yield_now();
    }

    let fs_ep = crate::ipc::create_endpoint("test-sock-net");
    let mut packed = 0u64;
    for (i, b) in mac.iter().enumerate() {
        packed |= (*b as u64) << (8 * i);
    }
    if dhcp {
        packed |= net::NET_ARG_DHCP;
    }

    let pid = spawn_server(ServerConfig {
        name: "net-server",
        binary: embedded::NET_SERVER,
        fs_endpoint: fs_ep,
        block_endpoint: 0,
        shared: Some(handle.rx_region),
        client_shared: Some(handle.tx_region),
        heap_bytes: 8 * 1024 * 1024,
        arg0: handle.endpoint,
        arg1: packed,
        filesystem_mount: None,
    });

    // Map the socket payload region and register the router (as boot_net does).
    let sock_region = SharedRegion::alloc(SERVER_SOCKET_BYTES).ok_or("socket region alloc failed")?;
    process::with_address_space(pid, |a| {
        sock_region.map_into(a, VirtAddr::new(SERVER_SOCKET_VIRT));
    })
    .ok_or("map socket region failed")?;
    socket::init(fs_ep, sock_region);

    // Let the net server reach its poll/serve loop.
    for _ in 0..2000 {
        sched::yield_now();
    }
    Ok(fs_ep)
}

/// Test the capability-checked socket API: a process holding the network
/// authority can create/bind/close a UDP socket; one without it is denied at
/// `sys_socket`; a wrong-type capability is also denied; and a socket capability
/// is revoked on close. Mirrors `test_vfs_capability` for the socket layer.
///
/// Deterministic — no external network is needed to create/bind/close a socket
/// (the round-trip is exercised separately by `test_udp_echo`).
#[cfg(target_arch = "x86_64")]
fn test_socket_capability() -> Result<(), &'static str> {
    use crate::cap::{CapHandle, CapRights, CapType, Capability, SOCKET_FACTORY};
    use crate::net::socket::{self, SockError};
    use crate::process;

    let _fs_ep = spawn_net_server_with_sockets(false, "virtio-net-sock")?;

    // --- A process WITH the network authority can create/bind/close a socket ---
    let (pid, _) = process::create_process("sock-test", None);
    let factory = process::with_cspace_mut(pid, |cs| {
        cs.insert(Capability {
            cap_type: CapType::Socket { socket_id: SOCKET_FACTORY },
            rights: CapRights::READ | CapRights::WRITE,
            parent: None,
        })
    })
    .ok_or("no cspace")?
    .map_err(|_| "insert factory cap failed")?;

    let sh_raw = socket::sys_socket(pid, factory, socket::SOCK_TYPE_UDP, 15)
        .map_err(|_| "sys_socket denied for an authorized process")?;
    let sh = CapHandle::from_raw(sh_raw);
    socket::sys_bind(pid, sh, [0, 0, 0, 0], 12345, 16).map_err(|_| "sys_bind failed")?;
    socket::sys_close(pid, sh, 19).map_err(|_| "sys_close failed")?;

    // The socket capability is revoked on close: reusing it is denied.
    match socket::sys_bind(pid, sh, [0, 0, 0, 0], 12345, 16) {
        Err(SockError::PermissionDenied) => {}
        _ => return Err("bind on a closed socket should be PermissionDenied"),
    }

    // --- A process WITHOUT the authority is denied at sys_socket ---
    let (pid2, _) = process::create_process("sock-noperm", None);
    match socket::sys_socket(pid2, CapHandle::NULL, socket::SOCK_TYPE_UDP, 15) {
        Err(SockError::PermissionDenied) => {}
        _ => return Err("sys_socket without capability should be PermissionDenied"),
    }
    // A wrong-type capability must not authorize socket creation either.
    let bogus = process::with_cspace_mut(pid2, |cs| {
        cs.insert(Capability {
            cap_type: CapType::Endpoint { endpoint_id: 1, badge: 0 },
            rights: CapRights::READ | CapRights::WRITE,
            parent: None,
        })
    })
    .ok_or("no cspace")?
    .map_err(|_| "insert bogus cap failed")?;
    match socket::sys_socket(pid2, bogus, socket::SOCK_TYPE_UDP, 15) {
        Err(SockError::PermissionDenied) => {}
        _ => return Err("sys_socket with a wrong-type cap should be PermissionDenied"),
    }

    process::destroy_process(pid);
    process::destroy_process(pid2);
    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_socket_capability() -> Result<(), &'static str> {
    Ok(())
}

/// Test the `sockets` listing path (`OP_SOCK_LIST`) and the ICMP socket /
/// ping-send plumbing (`OP_SOCK_PING`), both introduced in sub-phase 4.7.
///
/// Deterministic: it only opens sockets, binds, lists, and *emits* one ICMP echo
/// request — none of which needs an external peer or an inbound frame (so it does
/// not compete with `test_tcp_server` for the NIC). Whether an echo *reply* comes
/// back is left to the best-effort `ping` shell command (slirp's ICMP proxy may
/// lack host privileges); here we assert the send is accepted, which exercises the
/// whole ICMP socket build/bind/emit path in the ring-3 stack.
#[cfg(target_arch = "x86_64")]
fn test_socket_list() -> Result<(), &'static str> {
    use crate::net::socket;

    // Static-config server (10.0.2.15/24, gateway 10.0.2.2) — no DHCP needed.
    let _fs_ep = spawn_net_server_with_sockets(false, "virtio-net-list")?;

    // Open one socket of each kind through the kernel-internal path.
    let udp = socket::ksocket_open().map_err(|_| "ksocket_open (udp) failed")?;
    socket::ksocket_bind(udp, [0, 0, 0, 0], 40001).map_err(|_| "ksocket_bind (udp) failed")?;
    let tcp = socket::ksocket_open_tcp().map_err(|_| "ksocket_open_tcp failed")?;
    let icmp = socket::ksocket_open_icmp().map_err(|_| "ksocket_open_icmp failed")?;

    // The listing must show all three, with the right kinds and the bound UDP port.
    let list = socket::ksocket_list().map_err(|_| "ksocket_list failed")?;
    if list.len() < 3 {
        return Err("ksocket_list returned fewer than the three open sockets");
    }
    let udp_e = list.iter().find(|s| s.id == udp).ok_or("udp socket missing from listing")?;
    if udp_e.kind != 0 {
        return Err("udp socket reported wrong kind code");
    }
    if udp_e.local_port != 40001 {
        return Err("udp socket reported wrong bound local port");
    }
    let tcp_e = list.iter().find(|s| s.id == tcp).ok_or("tcp socket missing from listing")?;
    if tcp_e.kind != 1 {
        return Err("tcp socket reported wrong kind code");
    }
    let icmp_e = list.iter().find(|s| s.id == icmp).ok_or("icmp socket missing from listing")?;
    if icmp_e.kind != 2 {
        return Err("icmp socket reported wrong kind code");
    }

    // Emit one echo request to the gateway. The send is accepted (buffered) even
    // though no reply may arrive in CI — this exercises OP_SOCK_PING end to end.
    socket::ksocket_ping(icmp, [10, 0, 2, 2], 1).map_err(|_| "ksocket_ping send rejected")?;
    // Give the poll loop a chance to emit the frame; a reply is best-effort, so we
    // only require that recv does not hard-error (WouldBlock is the normal result).
    for _ in 0..2000 {
        match socket::ksocket_ping_recv(icmp) {
            Ok(_) => break,                                   // a reply came back
            Err(socket::SockError::WouldBlock) => crate::sched::yield_now(),
            Err(_) => return Err("ksocket_ping_recv hard-errored"),
        }
    }

    let _ = socket::ksocket_close(udp);
    let _ = socket::ksocket_close(tcp);
    let _ = socket::ksocket_close(icmp);
    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_socket_list() -> Result<(), &'static str> {
    Ok(())
}

/// Test the Phase 5.0 ELF loader end to end: load the embedded `elf-smoke` ELF
/// into a fresh process, build its initial stack with a known argv, map a result
/// page, run it in ring 3, and verify it wrote the expected proof words back.
///
/// This validates the whole loader path — ELF header/phdr parsing, `PT_LOAD`
/// segment mapping (W^X), the SysV initial stack (argc + argv pointers), and a
/// clean ring-3 entry + `SYS_EXIT` — using the **native** ABI, independent of the
/// Linux syscall personality (5.1). Deterministic: no external I/O.
#[cfg(target_arch = "x86_64")]
fn test_elf_exec() -> Result<(), &'static str> {
    use crate::linux::elf::{self, SliceSource};
    use crate::mm::addr::VirtAddr;
    use crate::mm::shared::SharedRegion;
    use crate::process::{self, embedded};
    use crate::sched;

    // Must match elf-smoke: the result page vaddr, magic, and the argv we pass.
    const RESULT_VADDR: u64 = 0x0600_0000;
    const RESULT_MAGIC: u64 = 0xE1FC_0DE1_2345_6789;

    // --- Malformed ELF is rejected without a fault ---
    let (bad_pid, _) = process::create_process("elf-bad", None);
    if elf::load_into(bad_pid, &SliceSource(&[0x7f, b'E', 0, 0, 1, 2, 3, 4])).is_ok() {
        return Err("loader accepted a malformed ELF");
    }
    process::destroy_process(bad_pid);

    // --- Load and run the real smoke-test ELF ---
    let (pid, _) = process::create_process("elf-smoke", None);
    let img = elf::load_into(pid, &SliceSource(embedded::ELF_SMOKE))
        .map_err(|_| "elf load_into failed")?;
    // argv = ["elf-smoke", "AB"] → argc must come back as 2, argv[0][0] as 'e'.
    let rsp = elf::build_initial_stack(pid, &img, &["elf-smoke", "AB"], &[])
        .map_err(|_| "build_initial_stack failed")?;

    // Map a shared result page at the fixed vaddr the binary writes to. The kernel
    // reads the same frames back via the region's HHDM alias.
    let region = SharedRegion::alloc(4096).ok_or("result region alloc failed")?;
    process::with_address_space(pid, |a| {
        region.map_into(a, VirtAddr::new(RESULT_VADDR));
    })
    .ok_or("mapping result region failed")?;

    process::set_user_entry(pid, img.entry, rsp);
    crate::linux::spawn_loaded("elf-smoke", pid);

    // Yield until the binary signals (magic written) or we give up.
    let mut signalled = false;
    for _ in 0..200_000 {
        // SAFETY: kernel-owned region frames; the guest writes the same physical
        // memory via its RESULT_VADDR mapping.
        let s = unsafe { region.as_slice_mut() };
        let magic = u64::from_le_bytes(s[0..8].try_into().unwrap());
        if magic == RESULT_MAGIC {
            let argc = u64::from_le_bytes(s[8..16].try_into().unwrap());
            let argv0_c = u64::from_le_bytes(s[16..24].try_into().unwrap());
            if argc != 2 {
                return Err("elf-smoke saw wrong argc (initial stack incorrect)");
            }
            if argv0_c != b'e' as u64 {
                return Err("elf-smoke saw wrong argv[0][0] (argv pointers incorrect)");
            }
            signalled = true;
            break;
        }
        sched::yield_now();
    }
    process::destroy_process(pid);
    if !signalled {
        return Err("elf-smoke did not run (no magic written to the result page)");
    }
    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_elf_exec() -> Result<(), &'static str> {
    Ok(())
}

/// Test the Phase 5.1 Linux syscall personality end to end: load the `linux-smoke`
/// ELF, mark the process `Personality::Linux`, run it, and verify it self-checked
/// the core Linux syscall surface — `arch_prctl(SET_FS)` + `%fs` TLS, `brk` growth,
/// anonymous `mmap`, and `write` — reporting success to a result page.
///
/// This exercises: the personality branch in the syscall dispatcher, per-task
/// FS-base save/restore, and the write/brk/mmap/arch_prctl translators. The probe
/// speaks the Linux ABI (`write`=1, `exit_group`=231, etc.), so a native process
/// would misinterpret those numbers — proving the personality routing works.
/// Deterministic: no external I/O.
#[cfg(target_arch = "x86_64")]
fn test_linux_exec() -> Result<(), &'static str> {
    use crate::linux::elf::{self, SliceSource};
    use crate::mm::addr::VirtAddr;
    use crate::mm::shared::SharedRegion;
    use crate::process::{self, embedded, Personality};
    use crate::sched;

    const RESULT_VADDR: u64 = 0x0600_0000;
    const RESULT_MAGIC: u64 = 0x5A11_D0C5_10AD_ED11;

    let (pid, _) = process::create_process("linux-smoke", None);
    let img = elf::load_into(pid, &SliceSource(embedded::LINUX_SMOKE))
        .map_err(|_| "linux-smoke elf load failed")?;
    let rsp = elf::build_initial_stack(pid, &img, &["linux-smoke"], &[])
        .map_err(|_| "linux-smoke stack build failed")?;

    // The probe writes its result (and uses a TLS slot at +0x800) in this page.
    let region = SharedRegion::alloc(4096).ok_or("result region alloc failed")?;
    process::with_address_space(pid, |a| {
        region.map_into(a, VirtAddr::new(RESULT_VADDR));
    })
    .ok_or("mapping result region failed")?;

    process::set_user_entry(pid, img.entry, rsp);
    // The distinguishing step for 5.1: route this process's syscalls through the
    // Linux table.
    process::set_personality(pid, Personality::Linux);
    crate::linux::spawn_loaded("linux-smoke", pid);

    let mut result_code: Option<u64> = None;
    for _ in 0..300_000 {
        // SAFETY: kernel-owned region frames; the guest writes the same memory.
        let s = unsafe { region.as_slice_mut() };
        let magic = u64::from_le_bytes(s[0..8].try_into().unwrap());
        if magic == RESULT_MAGIC {
            result_code = Some(u64::from_le_bytes(s[8..16].try_into().unwrap()));
            break;
        }
        sched::yield_now();
    }
    process::destroy_process(pid);

    match result_code {
        None => Err("linux-smoke did not run (no magic written)"),
        Some(0) => Ok(()),
        Some(1) => Err("linux-smoke: arch_prctl(SET_FS)/TLS check failed"),
        Some(2) => Err("linux-smoke: brk growth check failed"),
        Some(3) => Err("linux-smoke: anonymous mmap check failed"),
        Some(_) => Err("linux-smoke: unknown failure code"),
    }
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_linux_exec() -> Result<(), &'static str> {
    Ok(())
}

/// Unit-test the security-critical Linux path resolver (Phase 5.2): `..` must be
/// clamped at the root so a container path can never escape its rootfs.
#[cfg(target_arch = "x86_64")]
fn test_path_resolve() -> Result<(), &'static str> {
    use crate::linux::fs::resolve_path;
    let cases: &[(&str, &str, &str)] = &[
        ("/", "/hello.txt", "/hello.txt"),
        ("/", "hello.txt", "/hello.txt"),
        // Escape attempts clamp at the root — the whole point.
        ("/", "../../../hello.txt", "/hello.txt"),
        ("/", "..", "/"),
        ("/", "../..", "/"),
        ("/foo", "bar", "/foo/bar"),
        ("/foo/bar", "../baz", "/foo/baz"),
        ("/", "a/./b/../c", "/a/c"),
        ("/x", "/etc/../y", "/y"),
        ("/a/b/c", "../../../../../etc/passwd", "/etc/passwd"),
    ];
    for (cwd, path, expect) in cases {
        let got = resolve_path(cwd, path);
        if got != *expect {
            crate::println!("  resolve_path({:?}, {:?}) = {:?}, want {:?}", cwd, path, got, expect);
            return Err("path resolution mismatch (possible container escape)");
        }
    }
    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_path_resolve() -> Result<(), &'static str> {
    Ok(())
}

/// Test the Phase 5.2 Linux filesystem syscalls end to end: stage a file on an
/// ext2 mount, use it as a Linux process's container rootfs, and run `fs-smoke`,
/// which `openat`/`read`s the file and checks path clamping. Verifies the bytes
/// the container read match what we staged.
#[cfg(target_arch = "x86_64")]
fn test_linux_fs() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use crate::drivers::block::BlockDevice;
    use crate::drivers::virtio::blk::VirtioBlk;
    use crate::drivers::{block, block_server, pci};
    use crate::ipc;
    use crate::linux::elf::{self, SliceSource};
    use crate::mm::addr::VirtAddr;
    use crate::mm::shared::SharedRegion;
    use crate::process::{self, embedded, Personality};
    use crate::process::server::{spawn_server, ServerConfig};
    use crate::sched;

    const RESULT_VADDR: u64 = 0x0600_0000;
    const RESULT_MAGIC: u64 = 0xF5C0_DE55_10AD_ED55;
    const CONTENT: &[u8] = b"THEMELIOS_FS_OK\n"; // 16 bytes staged at /hello.txt

    // --- Bring up an ext2 server on the ext2 disk and register it as a mount ---
    let devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let mut ext2_index = None;
    for dev in devs.iter().filter(|d| d.class == 0x01) {
        let blk = match VirtioBlk::init_from_pci(dev) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut buf = [0u8; 512];
        if blk.read_blocks(2, &mut buf).is_ok() && buf[56] == 0x53 && buf[57] == 0xEF {
            ext2_index = Some(block::register("ext2-disk-linuxfs", Box::new(blk)));
            break;
        }
    }
    let idx = ext2_index.ok_or("no ext2 disk found")?;
    let block_handle = block_server::start(idx);
    let client = SharedRegion::alloc(128 * 1024).ok_or("client region alloc failed")?;
    let fs_ep = ipc::create_endpoint("ext2-linuxfs");
    spawn_server(ServerConfig {
        name: "ext2-server",
        binary: embedded::EXT2_SERVER,
        fs_endpoint: fs_ep,
        block_endpoint: block_handle.endpoint,
        shared: Some(block_handle.region),
        client_shared: Some(client),
        heap_bytes: 2 * 1024 * 1024,
        arg0: 0,
        arg1: 0,
        filesystem_mount: None,
    });
    for _ in 0..1000 {
        sched::yield_now();
    }
    let mount = crate::fs::register_mount(fs_ep, client);

    // --- Stage /hello.txt with known content (overwrite if it already exists) ---
    let fd = match crate::fs::kcreate(mount, b"/hello.txt") {
        Ok(fd) => fd,
        Err(_) => crate::fs::kopen(mount, b"/hello.txt").map_err(|_| "stage: open hello.txt")?,
    };
    crate::fs::kwrite(mount, fd, 0, CONTENT).map_err(|_| "stage: kwrite hello.txt")?;
    let _ = crate::fs::kclose(mount, fd);

    // --- Run fs-smoke with this mount as its rootfs ---
    let (pid, _) = process::create_process("fs-smoke", None);
    let img = elf::load_into(pid, &SliceSource(embedded::FS_SMOKE))
        .map_err(|_| "fs-smoke elf load failed")?;
    let rsp = elf::build_initial_stack(pid, &img, &["fs-smoke"], &[])
        .map_err(|_| "fs-smoke stack build failed")?;
    let region = SharedRegion::alloc(4096).ok_or("result region alloc failed")?;
    process::with_address_space(pid, |a| {
        region.map_into(a, VirtAddr::new(RESULT_VADDR));
    })
    .ok_or("mapping result region failed")?;
    process::set_user_entry(pid, img.entry, rsp);
    process::set_personality(pid, Personality::Linux);
    process::set_rootfs_mount(pid, mount);
    crate::linux::spawn_loaded("fs-smoke", pid);

    let mut done: Option<(u64, u64, u64)> = None; // (code, first8, len)
    for _ in 0..400_000 {
        // SAFETY: kernel-owned region; the guest writes the same memory.
        let s = unsafe { region.as_slice_mut() };
        if u64::from_le_bytes(s[0..8].try_into().unwrap()) == RESULT_MAGIC {
            done = Some((
                u64::from_le_bytes(s[8..16].try_into().unwrap()),
                u64::from_le_bytes(s[16..24].try_into().unwrap()),
                u64::from_le_bytes(s[24..32].try_into().unwrap()),
            ));
            break;
        }
        sched::yield_now();
    }
    process::destroy_process(pid);

    match done {
        None => Err("fs-smoke did not run (no magic written)"),
        Some((1, _, _)) => Err("fs-smoke: openat(/hello.txt) failed"),
        Some((2, _, _)) => Err("fs-smoke: read(/hello.txt) failed/empty"),
        Some((3, _, _)) => Err("fs-smoke: '..' clamp broke (clamped-open failed)"),
        Some((4, _, _)) => Err("fs-smoke: nonexistent path opened (phantom file)"),
        Some((0, first8, len)) => {
            let expect = u64::from_le_bytes(CONTENT[0..8].try_into().unwrap());
            if first8 != expect {
                return Err("fs-smoke: read wrong file content");
            }
            if len < CONTENT.len() as u64 {
                return Err("fs-smoke: short read");
            }
            Ok(())
        }
        Some(_) => Err("fs-smoke: unknown failure code"),
    }
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_linux_fs() -> Result<(), &'static str> {
    Ok(())
}

/// Test the Phase 5.3 threads + futex path end to end: run `threads-smoke`, which
/// `clone`s a thread (sharing the address space, with its own TLS), has the child
/// write a magic and `exit`, then **joins** by `futex`-waiting on the
/// `CLONE_CHILD_CLEARTID` word until the kernel clears + wakes it on thread exit.
///
/// A green result exercises: `clone(CLONE_THREAD)` + the thread trampoline
/// (ring-3 entry with `rax=0` on the child stack), per-task FS base, the
/// address-keyed `futex` WAIT/WAKE queue, and `CLONE_CHILD_SETTID`/`CLEARTID`.
/// Deterministic.
#[cfg(target_arch = "x86_64")]
fn test_linux_threads() -> Result<(), &'static str> {
    use crate::linux::elf::{self, SliceSource};
    use crate::mm::addr::VirtAddr;
    use crate::mm::shared::SharedRegion;
    use crate::process::{self, embedded, Personality};
    use crate::sched;

    const RESULT_VADDR: u64 = 0x0600_0000;
    const RESULT_MAGIC: u64 = 0x7C0D_E57C_0DE5_7C01;

    let (pid, _) = process::create_process("threads-smoke", None);
    let img = elf::load_into(pid, &SliceSource(embedded::THREADS_SMOKE))
        .map_err(|_| "threads-smoke elf load failed")?;
    let rsp = elf::build_initial_stack(pid, &img, &["threads-smoke"], &[])
        .map_err(|_| "threads-smoke stack build failed")?;
    let region = SharedRegion::alloc(4096).ok_or("result region alloc failed")?;
    process::with_address_space(pid, |a| {
        region.map_into(a, VirtAddr::new(RESULT_VADDR));
    })
    .ok_or("mapping result region failed")?;
    process::set_user_entry(pid, img.entry, rsp);
    process::set_personality(pid, Personality::Linux);
    crate::linux::spawn_loaded("threads-smoke", pid);

    let mut code: Option<u64> = None;
    for _ in 0..400_000 {
        // SAFETY: kernel-owned region; the guest threads write the same memory.
        let s = unsafe { region.as_slice_mut() };
        if u64::from_le_bytes(s[0..8].try_into().unwrap()) == RESULT_MAGIC {
            code = Some(u64::from_le_bytes(s[8..16].try_into().unwrap()));
            break;
        }
        sched::yield_now();
    }
    process::destroy_process(pid);

    match code {
        None => Err("threads-smoke did not finish (no magic; possible futex-join hang)"),
        Some(0) => Ok(()),
        Some(1) => Err("threads-smoke: child thread did not run (clone/trampoline)"),
        Some(2) => Err("threads-smoke: clone/mmap failed"),
        Some(_) => Err("threads-smoke: unknown failure code"),
    }
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_linux_threads() -> Result<(), &'static str> {
    Ok(())
}

/// Build one 512-byte USTAR header for `name`/`size`/`typeflag` (test helper).
fn tar_header(name: &str, size: usize, typeflag: u8) -> [u8; 512] {
    let mut h = [0u8; 512];
    let nb = name.as_bytes();
    let n = nb.len().min(100);
    h[..n].copy_from_slice(&nb[..n]);
    h[100..108].copy_from_slice(b"0000644\0"); // mode
    h[108..116].copy_from_slice(b"0000000\0"); // uid
    h[116..124].copy_from_slice(b"0000000\0"); // gid
    // size: 11 octal digits + NUL
    let s = alloc::format!("{:011o}\0", size);
    h[124..136].copy_from_slice(s.as_bytes());
    h[136..148].copy_from_slice(b"00000000000\0"); // mtime
    h[156] = typeflag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    // checksum: spaces in the field, sum all bytes, then write "%06o\0 ".
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    let cs = alloc::format!("{:06o}\0 ", sum);
    h[148..156].copy_from_slice(cs.as_bytes());
    h
}

/// Assemble a tar archive from `(name, data, typeflag)` entries (test helper).
fn make_tar(entries: &[(&str, &[u8], u8)]) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::new();
    for (name, data, tf) in entries {
        out.extend_from_slice(&tar_header(name, data.len(), *tf));
        out.extend_from_slice(data);
        // pad the data to a 512-byte block
        let pad = (512 - (data.len() % 512)) % 512;
        out.extend(core::iter::repeat(0u8).take(pad));
    }
    // two zero blocks terminate the archive
    out.extend(core::iter::repeat(0u8).take(1024));
    out
}

/// Test the Phase 5.4 OCI / docker-save image unpacker: synthesize a bundle
/// in-memory (manifest + config + uncompressed layer tars), unpack it, and verify
/// the assembled rootfs files, the parsed runtime config, and whiteout handling.
///
/// Deterministic and self-contained — no `docker`, no disk. Exercises the tar
/// reader, the JSON parser, and the layer/whiteout assembly that the ring-3
/// `oci-server` will use in 5.5.
#[cfg(target_arch = "x86_64")]
fn test_oci_unpack() -> Result<(), &'static str> {
    use crate::oci;

    const HELLO: &[u8] = b"#!hello binary\n";
    const MOTD: &[u8] = b"welcome to the container\n";
    const WORLD: &[u8] = b"#!world binary\n";

    // Layer 0: /bin/hello, /etc (dir), /etc/motd.
    let layer0 = make_tar(&[
        ("bin/hello", HELLO, b'0'),
        ("etc/", b"", b'5'),
        ("etc/motd", MOTD, b'0'),
    ]);
    // Layer 1: whiteout /bin/hello and add /bin/world.
    let layer1 = make_tar(&[
        ("bin/.wh.hello", b"", b'0'),
        ("bin/world", WORLD, b'0'),
    ]);
    let config = br#"{"config":{"Entrypoint":["/bin/world","--flag"],"Cmd":["arg1"],"Env":["PATH=/bin","TERM=linux"],"WorkingDir":"/root"}}"#;
    let manifest = br#"[{"Config":"config.json","RepoTags":["test:latest"],"Layers":["layer0.tar","layer1.tar"]}]"#;
    let bundle = make_tar(&[
        ("manifest.json", manifest, b'0'),
        ("config.json", config, b'0'),
        ("layer0.tar", &layer0, b'0'),
        ("layer1.tar", &layer1, b'0'),
    ]);

    let image = oci::unpack(&bundle).map_err(|_| "oci::unpack failed on a valid bundle")?;

    // --- Files: layer1 whiteout removed hello; world + motd remain ---
    let find = |p: &str| image.files.iter().find(|f| f.path == p);
    if find("/bin/hello").is_some() {
        return Err("whiteout did not remove /bin/hello");
    }
    let world = find("/bin/world").ok_or("/bin/world missing from assembled rootfs")?;
    if world.data != WORLD {
        return Err("/bin/world has wrong contents");
    }
    let motd = find("/etc/motd").ok_or("/etc/motd missing from assembled rootfs")?;
    if motd.data != MOTD {
        return Err("/etc/motd has wrong contents");
    }
    match find("/etc") {
        Some(d) if d.is_dir => {}
        _ => return Err("/etc directory missing from assembled rootfs"),
    }

    // --- Config parsed correctly ---
    let c = &image.config;
    if c.entrypoint != ["/bin/world", "--flag"] {
        return Err("image entrypoint parsed incorrectly");
    }
    if c.cmd != ["arg1"] {
        return Err("image cmd parsed incorrectly");
    }
    if !c.env.iter().any(|e| e == "PATH=/bin") {
        return Err("image env missing PATH");
    }
    if c.cwd != "/root" {
        return Err("image WorkingDir parsed incorrectly");
    }

    // --- A garbage bundle is rejected, not a panic ---
    if oci::unpack(b"not a tar at all, just bytes").is_ok() {
        return Err("unpack accepted a non-tar bundle");
    }

    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_oci_unpack() -> Result<(), &'static str> {
    Ok(())
}

/// Bring up an ext2 server on the ext2 disk and register it as a VFS mount,
/// returning the mount id. Shared by the FS/container tests.
#[cfg(target_arch = "x86_64")]
fn bring_up_ext2_mount(block_name: &'static str, ep_name: &'static str) -> Result<u64, &'static str> {
    use alloc::boxed::Box;
    use crate::drivers::block::BlockDevice;
    use crate::drivers::virtio::blk::VirtioBlk;
    use crate::drivers::{block, block_server, pci};
    use crate::ipc;
    use crate::mm::shared::SharedRegion;
    use crate::process::embedded;
    use crate::process::server::{spawn_server, ServerConfig};
    use crate::sched;

    let devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
    let mut ext2_index = None;
    for dev in devs.iter().filter(|d| d.class == 0x01) {
        let blk = match VirtioBlk::init_from_pci(dev) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut buf = [0u8; 512];
        if blk.read_blocks(2, &mut buf).is_ok() && buf[56] == 0x53 && buf[57] == 0xEF {
            ext2_index = Some(block::register(block_name, Box::new(blk)));
            break;
        }
    }
    let idx = ext2_index.ok_or("no ext2 disk found")?;
    let block_handle = block_server::start(idx);
    let client = SharedRegion::alloc(128 * 1024).ok_or("client region alloc failed")?;
    let fs_ep = ipc::create_endpoint(ep_name);
    spawn_server(ServerConfig {
        name: "ext2-server",
        binary: embedded::EXT2_SERVER,
        fs_endpoint: fs_ep,
        block_endpoint: block_handle.endpoint,
        shared: Some(block_handle.region),
        client_shared: Some(client),
        heap_bytes: 2 * 1024 * 1024,
        arg0: 0,
        arg1: 0,
        filesystem_mount: None,
    });
    for _ in 0..1000 {
        sched::yield_now();
    }
    Ok(crate::fs::register_mount(fs_ep, client))
}

/// Test the Phase 5.5 container runtime end to end: synthesize an image whose
/// entrypoint `/init` is a real Linux ELF, assemble its rootfs on an ext2 mount,
/// launch it as a capability-isolated Linux container, and verify it ran and
/// exited cleanly.
///
/// This is the payoff of Phases 5.0–5.4: the entrypoint ELF round-trips **image
/// bundle → written to the rootfs → loaded back from the rootfs via the VFS →
/// run as a Linux process rooted at that rootfs**. The `linux-smoke` probe serves
/// as the entrypoint (it self-checks the Linux syscall surface and reports to a
/// result page), so a green run also proves syscalls work inside the container.
#[cfg(target_arch = "x86_64")]
fn test_container_run() -> Result<(), &'static str> {
    use crate::container;
    use crate::mm::addr::VirtAddr;
    use crate::mm::shared::SharedRegion;
    use crate::process::{self, embedded};
    use crate::sched;

    // linux-smoke's result page + success magic (it is our entrypoint here).
    const RESULT_VADDR: u64 = 0x0600_0000;
    const RESULT_MAGIC: u64 = 0x5A11_D0C5_10AD_ED11;

    let mount = bring_up_ext2_mount("ext2-disk-container", "ext2-container")?;

    // Build an image bundle whose single layer contains /init = the linux-smoke
    // ELF, with an Entrypoint of ["/init"].
    let layer = make_tar(&[("init", embedded::LINUX_SMOKE, b'0')]);
    let config = br#"{"config":{"Entrypoint":["/init"],"Env":["PATH=/bin"],"WorkingDir":"/"}}"#;
    let manifest = br#"[{"Config":"config.json","Layers":["layer.tar"]}]"#;
    let bundle = make_tar(&[
        ("manifest.json", manifest, b'0'),
        ("config.json", config, b'0'),
        ("layer.tar", &layer, b'0'),
    ]);

    // Assemble the rootfs + create the container process (not yet running).
    let pid = container::create(&bundle, mount).map_err(|_| "container::create failed")?;

    // Map the entrypoint's result page before launch.
    let region = SharedRegion::alloc(4096).ok_or("result region alloc failed")?;
    process::with_address_space(pid, |a| {
        region.map_into(a, VirtAddr::new(RESULT_VADDR));
    })
    .ok_or("mapping result region failed")?;

    container::start(pid);

    // Wait for the container to signal success on its result page.
    let mut signalled = false;
    for _ in 0..400_000 {
        // SAFETY: kernel-owned region; the container writes the same memory.
        let s = unsafe { region.as_slice_mut() };
        if u64::from_le_bytes(s[0..8].try_into().unwrap()) == RESULT_MAGIC {
            if u64::from_le_bytes(s[8..16].try_into().unwrap()) != 0 {
                process::destroy_process(pid);
                return Err("container entrypoint reported a failure code");
            }
            signalled = true;
            break;
        }
        sched::yield_now();
    }
    if !signalled {
        process::destroy_process(pid);
        return Err("container entrypoint did not run (loaded from rootfs?)");
    }

    // The exit status was captured by exit_group.
    let status = process::exit_status(pid);
    process::destroy_process(pid);
    match status {
        Some(0) => Ok(()),
        Some(_) => Err("container exit status was non-zero"),
        None => Err("container exit status was not captured"),
    }
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_container_run() -> Result<(), &'static str> {
    Ok(())
}

/// Build a minimal DNS A-record query for "example.com" (29 bytes).
#[cfg(target_arch = "x86_64")]
fn build_dns_query() -> [u8; 29] {
    let mut q = [0u8; 29];
    q[0] = 0x12; // transaction id (high)
    q[1] = 0x34; // transaction id (low)
    q[2] = 0x01; // flags: recursion desired
    q[5] = 0x01; // QDCOUNT = 1
    // QNAME: 7"example" 3"com" 0
    q[12] = 7;
    q[13..20].copy_from_slice(b"example");
    q[20] = 3;
    q[21..24].copy_from_slice(b"com");
    q[24] = 0;
    q[26] = 0x01; // QTYPE = A
    q[28] = 0x01; // QCLASS = IN
    q
}

/// Test UDP send/receive through the socket API end to end.
///
/// Brings the stack up with DHCP, then creates a UDP socket, binds it, and sends
/// a DNS query to slirp's DNS server (10.0.2.3:53). The **send path** — socket
/// creation, bind, and datagram transmission through the kernel router and the
/// ring-3 net server — is the hard assertion. The **reply** is best-effort:
/// slirp's DNS proxy resolves via the host, which a sandboxed CI may not permit;
/// the UDP transport round-trip itself is already proven deterministically by
/// `test_dhcp` (DHCP is UDP). When a reply does arrive it is validated.
#[cfg(target_arch = "x86_64")]
fn test_udp_echo() -> Result<(), &'static str> {
    use crate::net::{net_service, socket};
    use crate::sched;

    let _fs_ep = spawn_net_server_with_sockets(true, "virtio-net-udp")?;

    // Wait for DHCP so the interface has an address and default route.
    let mut configured = false;
    for _ in 0..300_000 {
        if net_service::status().config.configured {
            configured = true;
            break;
        }
        sched::yield_now();
    }
    if !configured {
        return Err("DHCP did not configure before the UDP test");
    }

    // Create + bind a UDP socket via the kernel-internal socket API.
    let id = socket::ksocket_open().map_err(|_| "ksocket_open failed")?;
    socket::ksocket_bind(id, [0, 0, 0, 0], 5353).map_err(|_| "ksocket_bind failed")?;

    // Send a DNS query to slirp's DNS server. The send must be fully accepted.
    let query = build_dns_query();
    let sent = socket::ksocket_sendto(id, &query, [10, 0, 2, 3], 53)
        .map_err(|_| "ksocket_sendto failed")?;
    if sent != query.len() {
        return Err("sendto did not accept the whole datagram");
    }

    // Best-effort: try to receive the DNS response (see the doc comment).
    let mut buf = [0u8; 512];
    let mut got = false;
    for _ in 0..200_000 {
        if let Ok((n, ip, port)) = socket::ksocket_recvfrom(id, &mut buf) {
            if n > 0 {
                if ip == [10, 0, 2, 3]
                    && port == 53
                    && n >= 12
                    && buf[0] == query[0]
                    && buf[1] == query[1]
                    && (buf[2] & 0x80) != 0
                {
                    got = true;
                }
                break;
            }
        }
        sched::yield_now();
    }
    let _ = socket::ksocket_close(id);

    if got {
        crate::println!("  [test_udp_echo] DNS round-trip via slirp OK");
    } else {
        crate::println!(
            "  [test_udp_echo] send OK; no DNS reply (best-effort — transport proven by test_dhcp)"
        );
    }
    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_udp_echo() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_tcp_client — outbound TCP connect through the socket API (Phase 4.6)
// ============================================================

/// Test the TCP client path end to end: create a TCP socket, connect out, and
/// drive the handshake through the net server + smoltcp.
///
/// Brings the stack up with DHCP, then drives an outbound TCP connect to
/// slirp's DNS address (10.0.2.3:53) and probes it, exercising the full client
/// plumbing: socket create, `connect`, and the non-blocking send/recv state
/// machine routed through the kernel router and the ring-3 net server.
///
/// The **hard assertions** are that every socket call returns a *sensible* result
/// (never a plumbing error) and drives the smoltcp state machine — connect is
/// accepted, and the send probe returns `WouldBlock` while the handshake is in
/// flight. The connection *outcome* is best-effort: slirp has no general TCP
/// listener and its DNS proxy does not answer TCP SYNs, so establishment /
/// refusal / a silently-dropped SYN are all acceptable (a deterministic TCP
/// round-trip against a real listener lands with the server path in 4.7). The
/// probe budget is bounded so a dropped SYN cannot hang the test.
#[cfg(target_arch = "x86_64")]
fn test_tcp_client() -> Result<(), &'static str> {
    use crate::net::socket::SockError;
    use crate::net::{net_service, socket};
    use crate::sched;

    let _fs_ep = spawn_net_server_with_sockets(true, "virtio-net-tcp")?;

    // Wait for DHCP so the interface has an address and default route.
    let mut configured = false;
    for _ in 0..300_000 {
        if net_service::status().config.configured {
            configured = true;
            break;
        }
        sched::yield_now();
    }
    if !configured {
        return Err("DHCP did not configure before the TCP test");
    }

    // Create a TCP socket and begin connecting to slirp's DNS address over TCP.
    let id = socket::ksocket_open_tcp().map_err(|_| "ksocket_open_tcp failed")?;
    socket::ksocket_connect(id, [10, 0, 2, 3], 53).map_err(|_| "ksocket_connect failed")?;

    // Probe the connection with a small bounded budget. Each probe is a full IPC
    // round-trip to the net server, so the budget is kept modest — it is ample
    // for a real handshake to complete (a few round-trips) while a silently
    // dropped SYN just exhausts it quickly. Every reply must be one of the
    // expected outcomes: a plumbing error fails the test; `WouldBlock` (handshake
    // in flight) is expected and required at least once.
    let mut established = false;
    let mut refused = false;
    let mut saw_wouldblock = false;
    for _ in 0..2_000 {
        match socket::ksocket_send(id, &[]) {
            Ok(_) => {
                established = true;
                break;
            }
            Err(SockError::WouldBlock) => {
                saw_wouldblock = true;
                sched::yield_now();
            }
            Err(SockError::ConnectionRefused) => {
                refused = true;
                break;
            }
            Err(_) => {
                let _ = socket::ksocket_close(id);
                return Err("TCP send probe returned an unexpected error (bad plumbing)");
            }
        }
    }

    if established {
        // Established — attempt a best-effort DNS-over-TCP round-trip.
        let query = build_dns_query();
        let mut framed = [0u8; 31];
        framed[0..2].copy_from_slice(&(query.len() as u16).to_be_bytes());
        framed[2..].copy_from_slice(&query);
        let mut sent = 0usize;
        for _ in 0..2_000 {
            match socket::ksocket_send(id, &framed[sent..]) {
                Ok(n) => {
                    sent += n;
                    if sent >= framed.len() {
                        break;
                    }
                }
                Err(SockError::WouldBlock) => sched::yield_now(),
                Err(_) => break,
            }
        }
        let mut buf = [0u8; 512];
        let mut got = 0usize;
        for _ in 0..2_000 {
            match socket::ksocket_recv(id, &mut buf) {
                Ok(n) if n > 0 => {
                    got = n;
                    break;
                }
                Ok(_) => break,
                Err(SockError::WouldBlock) => sched::yield_now(),
                Err(_) => break,
            }
        }
        crate::println!("  [test_tcp_client] TCP established to 10.0.2.3:53; reply {} bytes", got);
    } else if refused {
        crate::println!("  [test_tcp_client] slirp refused TCP :53; client path + refusal detection OK");
    } else {
        // The send probe must have exercised the non-blocking path at least once.
        if !saw_wouldblock {
            let _ = socket::ksocket_close(id);
            return Err("TCP send probe never returned WouldBlock (state machine not advancing)");
        }
        crate::println!("  [test_tcp_client] no TCP listener at 10.0.2.3:53 (SYN dropped); client plumbing OK");
    }

    let _ = socket::ksocket_close(id);
    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_tcp_client() -> Result<(), &'static str> {
    Ok(())
}

// ============================================================
//  test_tcp_server — inbound TCP listen/accept + echo (Phase 4.6)
// ============================================================

/// Test the TCP server path with a real inbound connection driven by the host.
///
/// Brings the stack up with DHCP, listens on port 7, and accepts a connection
/// that the xtask harness's host-side peer makes through a QEMU `hostfwd` rule
/// (host 127.0.0.1:15007 → guest :7). The guest receives the peer's payload,
/// verifies it, and echoes it back — a deterministic, bidirectional TCP round
/// trip against an external endpoint, exercising `bind`/`listen`/`accept` and the
/// per-connection socket, `recv`, and `send`.
///
/// Registered before the other persistent net-server tests so it is the only
/// interface draining the NIC when it runs — otherwise accumulated net servers
/// would compete for the inbound frames.
#[cfg(target_arch = "x86_64")]
fn test_tcp_server() -> Result<(), &'static str> {
    use crate::net::socket::SockError;
    use crate::net::{net_service, socket};
    use crate::sched;

    let _fs_ep = spawn_net_server_with_sockets(true, "virtio-net-tcpsrv")?;

    // Wait for DHCP: the host's hostfwd targets the guest's DHCP address.
    let mut configured = false;
    for _ in 0..300_000 {
        if net_service::status().config.configured {
            configured = true;
            break;
        }
        sched::yield_now();
    }
    if !configured {
        return Err("DHCP did not configure before the TCP server test");
    }

    // Listen on port 7 (the hostfwd target).
    let listener = socket::ksocket_open_tcp().map_err(|_| "ksocket_open_tcp failed")?;
    socket::ksocket_bind_port(listener, 7).map_err(|_| "ksocket_bind_port failed")?;
    socket::ksocket_listen(listener).map_err(|_| "ksocket_listen failed")?;

    // Accept the host peer's inbound connection. Each poll cycles the net server,
    // so the handshake completes within a bounded number of attempts.
    let mut conn: Option<u64> = None;
    for _ in 0..40_000 {
        match socket::ksocket_accept(listener) {
            Ok((cid, _ip, _port)) => {
                conn = Some(cid);
                break;
            }
            Err(SockError::WouldBlock) => sched::yield_now(),
            Err(_) => {
                let _ = socket::ksocket_close(listener);
                return Err("TCP accept returned an unexpected error");
            }
        }
    }
    let conn = match conn {
        Some(c) => c,
        None => {
            let _ = socket::ksocket_close(listener);
            return Err("no inbound TCP connection accepted (host peer did not connect)");
        }
    };

    // Receive the host peer's payload.
    let expect: &[u8] = b"THEMELIOS_TCP_PING";
    let mut buf = [0u8; 64];
    let mut got = 0usize;
    for _ in 0..40_000 {
        match socket::ksocket_recv(conn, &mut buf) {
            Ok(n) if n > 0 => {
                got = n;
                break;
            }
            Ok(_) => sched::yield_now(),
            Err(SockError::WouldBlock) => sched::yield_now(),
            Err(_) => break,
        }
    }
    if got != expect.len() || &buf[..got] != expect {
        let _ = socket::ksocket_close(conn);
        let _ = socket::ksocket_close(listener);
        return Err("TCP server received an unexpected payload");
    }

    // Echo it back to the host.
    let mut sent = 0usize;
    for _ in 0..20_000 {
        match socket::ksocket_send(conn, &buf[sent..got]) {
            Ok(n) => {
                sent += n;
                if sent >= got {
                    break;
                }
            }
            Err(SockError::WouldBlock) => sched::yield_now(),
            Err(_) => break,
        }
    }

    let _ = socket::ksocket_close(conn);
    let _ = socket::ksocket_close(listener);
    crate::println!("  [test_tcp_server] accepted inbound TCP, echoed {} bytes", got);
    Ok(())
}

/// Stub for non-x86_64 targets.
#[cfg(not(target_arch = "x86_64"))]
fn test_tcp_server() -> Result<(), &'static str> {
    Ok(())
}

