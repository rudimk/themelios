//! # Kernel heap allocator
//!
//! Provides `#[global_allocator]` so that the `alloc` crate's types (`Vec`,
//! `Box`, `String`, etc.) work in the kernel. The heap starts as a 1 MiB region
//! backed by contiguous physical frames and grows dynamically when exhausted.
//!
//! ## Design
//!
//! We use `linked_list_allocator::Heap` (the non-locked variant) wrapped in
//! our own `InterruptMutex`. This is critical for correctness:
//!
//! - `Heap`'s built-in `LockedHeap` uses a plain spinlock.
//! - If the timer interrupt fires while the spinlock is held (e.g., during a
//!   `Vec::push` in normal code), and the interrupt handler tries to allocate,
//!   it will spin forever → **deadlock**.
//! - Our `InterruptMutex` disables interrupts while the lock is held, so
//!   the timer handler can never run during an allocation.
//!
//! ## Dynamic growth
//!
//! When the heap runs out of space, the allocator automatically grows by
//! mapping additional physical frames to contiguous virtual addresses at the
//! end of the current heap region. The physical frames do NOT need to be
//! contiguous — the page table manager maps arbitrary physical frames to
//! consecutive virtual pages. Growth occurs in 256 KiB increments (64 pages).
//! The heap is capped at 16 MiB total to prevent runaway allocations.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use linked_list_allocator::Heap;

use crate::println;
use crate::sync::InterruptMutex;
use super::PAGE_SIZE;

/// Initial heap size: 1 MiB (256 pages).
const INITIAL_HEAP_SIZE: usize = 1024 * 1024;

/// Heap growth increment: 256 KiB (64 pages) per growth event.
const GROWTH_INCREMENT: usize = 256 * 1024;

/// Maximum total heap size: 16 MiB. Growth beyond this is denied.
const MAX_HEAP_SIZE: usize = 16 * 1024 * 1024;

/// The current size of the heap in bytes. Updated atomically on each growth.
static HEAP_CURRENT_SIZE: AtomicUsize = AtomicUsize::new(0);

/// The virtual address of the top of the heap (first byte AFTER the heap).
/// Growth maps new pages starting at this address and then advances it.
static HEAP_TOP_VIRT: AtomicU64 = AtomicU64::new(0);

/// Number of times the heap has been grown.
static GROWTH_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The kernel heap allocator.
///
/// Wraps `linked_list_allocator::Heap` in an `InterruptMutex` and implements
/// `GlobalAlloc` so the Rust `alloc` crate can use it.
struct KernelHeap(InterruptMutex<Heap>);

/// `GlobalAlloc` is called from any context that allocates — including code
/// that runs with interrupts enabled. The `InterruptMutex` ensures exclusive
/// access by disabling interrupts and spinning on the inner lock.
///
/// On allocation failure (OOM), the allocator attempts to grow the heap by
/// mapping additional physical frames, then retries the allocation. If growth
/// fails (out of frames or at max size), returns null (triggering a panic
/// via the default alloc error handler).
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut heap = self.0.lock();

        // First attempt.
        if let Ok(nn) = heap.allocate_first_fit(layout) {
            return nn.as_ptr();
        }

        // OOM — try to grow the heap.
        if !grow_heap_locked(&mut heap) {
            return core::ptr::null_mut();
        }

        // Retry after growth.
        heap.allocate_first_fit(layout)
            .ok()
            .map_or(core::ptr::null_mut(), |nn| nn.as_ptr())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            self.0
                .lock()
                .deallocate(NonNull::new_unchecked(ptr), layout);
        }
    }
}

/// The global heap allocator instance.
///
/// Starts as an empty heap (no backing memory). `init()` allocates physical
/// frames and initializes the heap with a contiguous virtual memory region.
/// Until `init()` is called, any allocation will return null (which the default
/// alloc error handler turns into a panic).
#[global_allocator]
static KERNEL_HEAP: KernelHeap = KernelHeap(InterruptMutex::new(Heap::empty()));

/// Initialize the kernel heap with physical frames from the frame allocator.
///
/// Allocates `INITIAL_HEAP_SIZE / PAGE_SIZE` contiguous physical frames,
/// converts the base to a virtual address via the HHDM, and initializes the
/// linked-list heap allocator with that region.
///
/// Must be called exactly once, after `mm::frame::init()` and `mm::init_hhdm()`.
/// After this call, `Vec`, `Box`, `String`, and all other `alloc` types work.
pub fn init() {
    let page_count = INITIAL_HEAP_SIZE / PAGE_SIZE as usize;

    // Allocate contiguous physical frames for the initial heap. Contiguous
    // frames give us a contiguous HHDM virtual region, which is what the
    // linked-list allocator needs for its initial region.
    let phys_base = super::frame::allocate_contiguous_frames(page_count)
        .expect("Failed to allocate contiguous frames for kernel heap");

    // Convert to virtual address via HHDM.
    let virt_base = phys_base.to_virt();
    let virt_top = virt_base.as_u64() + INITIAL_HEAP_SIZE as u64;

    // SAFETY: The virtual address points to a valid, contiguous, HHDM-mapped
    // region of INITIAL_HEAP_SIZE bytes backed by physical frames we just allocated.
    // No one else has a reference to this memory.
    unsafe {
        KERNEL_HEAP.0.lock().init(virt_base.as_mut_ptr::<u8>(), INITIAL_HEAP_SIZE);
    }

    HEAP_CURRENT_SIZE.store(INITIAL_HEAP_SIZE, Ordering::Relaxed);
    HEAP_TOP_VIRT.store(virt_top, Ordering::Relaxed);

    println!("Kernel heap initialized: {} KiB at {}", INITIAL_HEAP_SIZE / 1024, virt_base);
}

/// Grow the heap by mapping additional physical frames at contiguous virtual
/// addresses above the current heap top.
///
/// Called from inside `GlobalAlloc::alloc` while the heap lock is held.
/// Returns `true` if growth succeeded, `false` if we're at the max size
/// or out of physical frames.
fn grow_heap_locked(heap: &mut Heap) -> bool {
    let current_size = HEAP_CURRENT_SIZE.load(Ordering::Relaxed);
    let new_size = current_size + GROWTH_INCREMENT;

    if new_size > MAX_HEAP_SIZE {
        return false;
    }

    let heap_top = HEAP_TOP_VIRT.load(Ordering::Relaxed);
    let pages_needed = GROWTH_INCREMENT / PAGE_SIZE as usize;

    // Get the kernel address space for mapping new pages.
    let kernel_as = super::page_table::kernel_address_space();

    // Map each new page individually. The physical frames don't need to be
    // contiguous — the page table maps them to consecutive virtual addresses.
    for i in 0..pages_needed {
        let virt = super::addr::VirtAddr::new(heap_top + (i as u64 * PAGE_SIZE));
        let phys = match super::frame::allocate_frame() {
            Some(f) => f,
            None => {
                // Out of physical frames. We may have already mapped some pages
                // for this growth — that's fine, we'll extend the heap by what
                // we managed to map so far.
                if i > 0 {
                    let partial_bytes = i * PAGE_SIZE as usize;
                    // SAFETY: we just mapped `i` pages at heap_top, which is
                    // contiguous with the existing heap region.
                    unsafe { heap.extend(partial_bytes); }
                    HEAP_CURRENT_SIZE.store(current_size + partial_bytes, Ordering::Relaxed);
                    HEAP_TOP_VIRT.store(heap_top + partial_bytes as u64, Ordering::Relaxed);
                    GROWTH_COUNT.fetch_add(1, Ordering::Relaxed);
                }
                core::mem::forget(kernel_as);
                return i > 0;
            }
        };

        kernel_as.map_page(
            virt,
            phys,
            super::page_table::PageFlags::PRESENT | super::page_table::PageFlags::WRITABLE,
        );
    }

    core::mem::forget(kernel_as);

    // Extend the linked-list allocator into the newly mapped region.
    // SAFETY: the `GROWTH_INCREMENT` bytes starting at `heap_top` are now
    // valid mapped memory, contiguous with the existing heap region.
    unsafe {
        heap.extend(GROWTH_INCREMENT);
    }

    HEAP_CURRENT_SIZE.store(new_size, Ordering::Relaxed);
    HEAP_TOP_VIRT.store(heap_top + GROWTH_INCREMENT as u64, Ordering::Relaxed);
    GROWTH_COUNT.fetch_add(1, Ordering::Relaxed);

    true
}

/// Get the amount of free space remaining in the heap (in bytes).
pub fn free() -> usize {
    KERNEL_HEAP.0.lock().free()
}

/// Get the amount of used space in the heap (in bytes).
pub fn used() -> usize {
    KERNEL_HEAP.0.lock().used()
}

/// Get the total current size of the heap (in bytes).
pub fn total_size() -> usize {
    HEAP_CURRENT_SIZE.load(Ordering::Relaxed)
}

/// Get the number of times the heap has been grown.
pub fn growth_count() -> usize {
    GROWTH_COUNT.load(Ordering::Relaxed)
}
