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

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use limine::memory_map::EntryType;

use crate::sync::InterruptMutex;

pub mod addr;
pub mod frame;
pub mod heap;
pub mod mmio;
pub mod page_table;
pub mod shared;

extern crate alloc;

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

// --- Saved memory map ---
//
// Limine's memory map lives in bootloader-reclaimable memory. We must copy
// the entries we need into kernel-owned storage BEFORE reclaiming that memory.
// This saved copy is used by reclaim_bootloader_memory() to know which regions
// to reclaim.

/// A saved memory map entry — just the fields we need.
///
/// We store the limine `EntryType` directly (it's `Copy` + `PartialEq`)
/// rather than extracting the private inner `u64`. This lets us compare
/// against the public constants like `EntryType::BOOTLOADER_RECLAIMABLE`.
#[derive(Clone)]
pub struct SavedMemoryRegion {
    pub base: u64,
    pub length: u64,
    pub entry_type: EntryType,
}

/// Saved copy of the Limine memory map, stored in kernel heap memory.
static SAVED_MEMORY_MAP: InterruptMutex<Option<Vec<SavedMemoryRegion>>> =
    InterruptMutex::new(None);

/// Save the Limine memory map into kernel-owned heap storage.
///
/// Must be called AFTER the heap is initialized but BEFORE reclaiming
/// bootloader memory (which invalidates the Limine response structures).
pub fn save_memory_map(entries: &[&limine::memory_map::Entry]) {
    let saved: Vec<SavedMemoryRegion> = entries
        .iter()
        .map(|e| SavedMemoryRegion {
            base: e.base,
            length: e.length,
            entry_type: e.entry_type,
        })
        .collect();
    *SAVED_MEMORY_MAP.lock() = Some(saved);
}

/// Reclaim bootloader-reclaimable memory regions.
///
/// Walks the saved memory map and marks all BOOTLOADER_RECLAIMABLE regions
/// as free in the frame allocator. Must be called AFTER:
/// - The memory map has been saved (save_memory_map)
/// - The kernel has switched to its own page tables (page_table::init)
/// - The kernel has its own GDT loaded (gdt::init — already done in Phase 1)
///
/// After this call, the physical frames that held Limine's boot structures
/// (page tables, GDT, stack, etc.) are available for general allocation.
///
/// Returns the total number of frames reclaimed.
pub fn reclaim_bootloader_memory() -> usize {
    let guard = SAVED_MEMORY_MAP.lock();
    let map = guard.as_ref().expect("Memory map not saved — call save_memory_map() first");

    let mut total_reclaimed = 0usize;
    for region in map {
        if region.entry_type == EntryType::BOOTLOADER_RECLAIMABLE {
            let reclaimed = frame::reclaim_region(region.base, region.length);
            total_reclaimed += reclaimed;
        }
    }

    total_reclaimed
}
