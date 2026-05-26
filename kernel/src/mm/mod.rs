//! # Memory management subsystem
//!
//! Responsible for all memory-related operations in the kernel:
//!
//! - **Physical frame allocator** (`frame`): Tracks which 4 KiB frames of physical
//!   memory are free or in use. Hands out frames to the heap allocator and (later)
//!   page table manager.
//!
//! - **Address types** (`addr`): Type-safe `PhysAddr` and `VirtAddr` wrappers with
//!   HHDM-based conversion between physical and virtual addresses.
//!
//! - **Kernel heap allocator** (`heap`, future): Provides `alloc`-style dynamic
//!   allocation for kernel data structures backed by physical frames.
//!
//! ## HHDM (Higher-Half Direct Map)
//!
//! All physical memory access in the kernel goes through the HHDM — a Limine-
//! provided mapping of the entire physical address space into the kernel's virtual
//! address space at a fixed offset. The formula is:
//!
//! ```text
//! virtual_addr = physical_addr + hhdm_offset
//! physical_addr = virtual_addr - hhdm_offset
//! ```
//!
//! The HHDM offset is provided by Limine at boot and stored globally via
//! `init_hhdm()`. All code that needs to access physical memory (frame allocator
//! bitmap, page tables, DMA buffers) uses this offset for conversion.
//!
//! ## Design notes
//!
//! Memory isolation is critical for ThemeliOS's security model. Each container
//! process will run in its own address space (Phase 2), and the capability system
//! controls which memory regions a process can share or access. The MM subsystem
//! enforces this at the hardware level via page table permissions.

use core::sync::atomic::{AtomicU64, Ordering};

pub mod addr;

/// Page size: 4 KiB (4096 bytes).
///
/// This is the smallest unit of memory the x86_64 MMU can map individually
/// and the granularity of our physical frame allocator. Every allocation
/// and mapping operates in multiples of this size.
pub const PAGE_SIZE: u64 = 4096;

/// Globally stored HHDM offset, set once during early boot.
///
/// Initialized by `init_hhdm()` from the Limine `HhdmResponse`. After that,
/// all physical-to-virtual conversions (`PhysAddr::to_virt()`) read this value.
///
/// Uses `Relaxed` ordering because it's written once during single-threaded
/// boot before any readers exist, and on our single-core kernel there's no
/// need for cross-core synchronization.
static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Store the HHDM offset for use by physical-to-virtual address conversions.
///
/// Must be called exactly once during early boot with the offset from
/// `HhdmResponse::offset()`. After this call, `PhysAddr::to_virt()` and
/// `VirtAddr::to_phys()` will produce correct results.
pub fn init_hhdm(offset: u64) {
    HHDM_OFFSET.store(offset, Ordering::Relaxed);
}

/// Read the globally stored HHDM offset.
///
/// Returns the offset that converts physical addresses to virtual addresses:
/// `virt = phys + hhdm_offset()`. Panics if `init_hhdm()` hasn't been called
/// yet (offset is still 0, which is never a valid HHDM offset — Limine places
/// the HHDM in the upper half of the 64-bit address space).
pub fn hhdm_offset() -> u64 {
    let offset = HHDM_OFFSET.load(Ordering::Relaxed);
    debug_assert!(offset != 0, "HHDM offset not initialized — call mm::init_hhdm() first");
    offset
}
