//! # Kernel heap allocator
//!
//! Provides `#[global_allocator]` so that the `alloc` crate's types (`Vec`,
//! `Box`, `String`, etc.) work in the kernel. The heap is backed by a
//! contiguous region of physical frames allocated from the frame allocator
//! and accessed via the HHDM.
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
//! ## Size
//!
//! The heap is fixed at 1 MiB for Phase 1. If it runs out, allocation panics.
//! Dynamic growth is deferred to Phase 2 when we have custom page tables.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;

use linked_list_allocator::Heap;

use crate::println;
use crate::sync::InterruptMutex;
use super::PAGE_SIZE;

/// Heap size: 1 MiB (256 pages of 4 KiB each).
///
/// This is sufficient for Phase 1's needs (task structs, scheduler queues,
/// IPC buffers). If exhausted, allocation panics — dynamic growth is deferred
/// to Phase 2.
const HEAP_SIZE: usize = 1024 * 1024;

/// The kernel heap allocator.
///
/// Wraps `linked_list_allocator::Heap` in an `InterruptMutex` and implements
/// `GlobalAlloc` so the Rust `alloc` crate can use it.
struct KernelHeap(InterruptMutex<Heap>);

/// `GlobalAlloc` is called from any context that allocates — including code
/// that runs with interrupts enabled. The `InterruptMutex` ensures exclusive
/// access by disabling interrupts and spinning on the inner lock.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.0
            .lock()
            .allocate_first_fit(layout)
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
/// Allocates `HEAP_SIZE / PAGE_SIZE` contiguous physical frames, converts the
/// base to a virtual address via the HHDM, and initializes the linked-list
/// heap allocator with that region.
///
/// Must be called exactly once, after `mm::frame::init()` and `mm::init_hhdm()`.
/// After this call, `Vec`, `Box`, `String`, and all other `alloc` types work.
pub fn init() {
    let page_count = HEAP_SIZE / PAGE_SIZE as usize;

    // Allocate contiguous physical frames for the heap. Contiguous physical
    // frames give us a contiguous HHDM virtual region, which is what the
    // linked-list allocator needs.
    let phys_base = super::frame::allocate_contiguous_frames(page_count)
        .expect("Failed to allocate contiguous frames for kernel heap");

    // Convert to virtual address via HHDM.
    let virt_base = phys_base.to_virt();

    // SAFETY: The virtual address points to a valid, contiguous, HHDM-mapped
    // region of HEAP_SIZE bytes backed by physical frames we just allocated.
    // No one else has a reference to this memory.
    unsafe {
        KERNEL_HEAP.0.lock().init(virt_base.as_mut_ptr::<u8>(), HEAP_SIZE);
    }

    println!("Kernel heap initialized: {} KiB at {}", HEAP_SIZE / 1024, virt_base);
}

/// Get the amount of free space remaining in the heap (in bytes).
pub fn free() -> usize {
    KERNEL_HEAP.0.lock().free()
}

/// Get the amount of used space in the heap (in bytes).
pub fn used() -> usize {
    KERNEL_HEAP.0.lock().used()
}
