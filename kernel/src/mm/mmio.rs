//! # MMIO region mapping
//!
//! Device registers (the memory-mapped BARs discovered during PCI enumeration)
//! live at physical addresses in the hole below 4 GiB that the chipset routes
//! to devices instead of RAM. To touch them from kernel code we need a virtual
//! mapping — and crucially one that is **cache-disabled**, because device
//! registers have side effects: a cached read could return a stale value and a
//! cached write might never reach the device.
//!
//! ## Why not just use the HHDM?
//!
//! The Higher-Half Direct Map already covers physical memory, so in principle a
//! BAR at `0xfebd5000` is reachable at `0xfebd5000 + hhdm_offset`. Two problems:
//!
//! 1. **Cache attributes.** The HHDM is mapped as normal write-back memory.
//!    MMIO must be uncached (`CACHE_DISABLE`).
//! 2. **Huge pages.** Limine maps the HHDM with 2 MiB / 1 GiB huge pages.
//!    Re-mapping an individual 4 KiB page inside a huge-page region with our
//!    4 KiB `map_page` would misinterpret the huge-page directory entry as a
//!    page-table pointer and corrupt the tables.
//!
//! So device MMIO gets its own dedicated virtual window, mapped fresh as 4 KiB
//! uncached pages.
//!
//! ## Allocation strategy
//!
//! A simple bump allocator hands out virtual addresses from a reserved region
//! in the kernel's upper half (PML4 slot 509, which neither the HHDM nor the
//! kernel image use). MMIO mappings live for the lifetime of the kernel — they
//! are never unmapped — so a bump pointer is all we need. Each request is
//! rounded up to whole pages and given a fresh, page-aligned virtual base.

use core::sync::atomic::{AtomicU64, Ordering};

use super::PAGE_SIZE;
use super::addr::{PhysAddr, VirtAddr, align_down, align_up};
use super::page_table::{kernel_address_space, PageFlags};

/// Base of the dedicated MMIO virtual window.
///
/// PML4 index 509 → `0xFFFF_FE80_0000_0000`. The HHDM sits lower (Limine places
/// it around PML4 256) and the kernel image is at PML4 511, so this slot is
/// free for our use. Each device BAR is bump-allocated upward from here.
const MMIO_VIRT_BASE: u64 = 0xFFFF_FE80_0000_0000;

/// Next free virtual address in the MMIO window (bump allocator cursor).
static NEXT_MMIO_VIRT: AtomicU64 = AtomicU64::new(MMIO_VIRT_BASE);

/// Map a physical MMIO region into the kernel's virtual address space.
///
/// Maps `size` bytes starting at `phys` as uncached, writable, non-executable
/// 4 KiB pages, and returns the virtual address corresponding to `phys` (the
/// returned address preserves any sub-page offset of `phys`).
///
/// The mapping is permanent for the kernel's lifetime. Intended for device BARs
/// found during PCI enumeration.
pub fn map(phys: PhysAddr, size: usize) -> VirtAddr {
    // Align the physical base down to a page boundary, and grow the length to
    // cover the original end. The caller's sub-page offset is re-applied to the
    // returned virtual address.
    let phys_raw = phys.as_u64();
    let page_base = align_down(phys_raw, PAGE_SIZE);
    let offset = phys_raw - page_base;
    let total = align_up(offset + size as u64, PAGE_SIZE);
    let page_count = total / PAGE_SIZE;

    // Reserve a virtual range from the bump allocator.
    let virt_base = NEXT_MMIO_VIRT.fetch_add(total, Ordering::Relaxed);

    // MMIO mapping flags: present, writable, uncached so reads/writes reach the
    // device, and no-execute (device registers are never code).
    let flags = PageFlags::PRESENT
        | PageFlags::WRITABLE
        | PageFlags::CACHE_DISABLE
        | PageFlags::NO_EXECUTE;

    let kernel_as = kernel_address_space();
    for i in 0..page_count {
        let p = PhysAddr::new(page_base + i * PAGE_SIZE);
        let v = VirtAddr::new(virt_base + i * PAGE_SIZE);
        kernel_as.map_page(v, p, flags);
    }
    // kernel_address_space() returns a borrowed handle to the global kernel
    // PML4; don't run its Drop (which would tear down page tables).
    core::mem::forget(kernel_as);

    VirtAddr::new(virt_base + offset)
}
