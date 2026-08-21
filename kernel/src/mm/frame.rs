//! # Bitmap frame allocator
//!
//! Tracks which 4 KiB physical memory frames are free or allocated using a
//! bitmap — one bit per frame across the entire physical address space.
//!
//! ## Bitmap encoding
//!
//! - Bit = 1 → frame is **free** (available for allocation)
//! - Bit = 0 → frame is **allocated** (in use or reserved)
//!
//! Frame index `N` corresponds to the physical address `N * 4096`. The bit for
//! frame N is at byte `N / 8`, bit position `N % 8`.
//!
//! ## Bootstrap
//!
//! The allocator faces a chicken-and-egg problem: it needs memory for its bitmap,
//! but it IS the memory allocator. We solve this by:
//!
//! 1. Scanning the Limine memory map to find the highest physical address
//! 2. Computing the bitmap size (1 bit per frame)
//! 3. Finding a USABLE memory region large enough to hold the bitmap
//! 4. Placing the bitmap there (accessed via HHDM)
//! 5. Marking only USABLE frames as free (all others start allocated)
//! 6. Marking the bitmap's own frames as allocated (so they're never handed out)
//!
//! ## Thread safety
//!
//! The `BitmapFrameAllocator` struct itself is not thread-safe. It's wrapped in
//! an `InterruptMutex` by the public API in this module, which disables interrupts
//! while the lock is held to prevent deadlock from timer interrupt handlers.

use limine::memory_map::{Entry, EntryType};

use super::PAGE_SIZE;
use super::addr::{PhysAddr, align_up, align_down};
use crate::sync::InterruptMutex;

/// A bitmap-based physical frame allocator.
///
/// Tracks frame availability using a bitmap stored in HHDM-mapped physical
/// memory. Each bit represents one 4 KiB frame: set = free, clear = allocated.
pub struct BitmapFrameAllocator {
    /// Pointer to the bitmap in HHDM-mapped virtual memory.
    bitmap: *mut u8,
    /// Size of the bitmap in bytes.
    bitmap_len: usize,
    /// Total number of frames tracked (= highest_phys_addr / PAGE_SIZE).
    frame_count: usize,
    /// Frames the memory map reported as usable RAM.
    ///
    /// Distinct from `frame_count`, which is the *span* of the bitmap: it runs from
    /// physical 0 to the highest usable address, so on any machine whose RAM does not
    /// start at 0 it also covers unbacked holes. QEMU `virt` puts RAM at 0x4000_0000,
    /// so a 512 MiB guest has a `frame_count` covering 1.5 GiB and `frame_count -
    /// free_count` reports a gigabyte of phantom "used" memory. Reporting needs the
    /// usable total, not the span.
    usable_count: usize,
    /// Number of currently free frames.
    free_count: usize,
}

// SAFETY: The bitmap pointer targets HHDM-mapped memory that is stable for the
// kernel's lifetime. All access is serialized by the InterruptMutex wrapper,
// so no data races are possible. Send is needed to store this in a static Mutex.
unsafe impl Send for BitmapFrameAllocator {}

impl BitmapFrameAllocator {
    /// Create a new frame allocator from the Limine memory map.
    ///
    /// This is the bootstrap sequence:
    /// 1. Find the highest physical address → compute bitmap size
    /// 2. Find a USABLE region for the bitmap itself
    /// 3. Zero the bitmap (all frames marked allocated)
    /// 4. Walk the memory map, marking USABLE frames as free
    /// 5. Mark the bitmap's own frames as allocated
    /// 6. Count free frames
    ///
    /// The `hhdm_offset` is used to access the bitmap via virtual addresses.
    /// The `kernel_phys_base` is used to verify the kernel isn't in a USABLE region.
    pub fn new(entries: &[&Entry], hhdm_offset: u64, kernel_phys_base: u64) -> Self {
        // --- Step 1: Determine bitmap size ---

        // Find the highest physical address in the memory map. This determines
        // how many frames we need to track (and thus how large the bitmap is).
        let max_phys = entries.iter()
            .map(|e| e.base + e.length)
            .max()
            .expect("Memory map is empty");

        let frame_count = (max_phys / PAGE_SIZE) as usize;
        // Round up: if frame_count isn't a multiple of 8, we need an extra byte.
        // The trailing bits in the last byte will stay 0 (allocated) since no
        // real frames correspond to them.
        let bitmap_len = (frame_count + 7) / 8;

        // --- Step 2: Find a home for the bitmap ---

        // The bitmap needs to live in a USABLE region (since that's the only
        // memory we're allowed to use). We find the first region large enough
        // and place the bitmap at its page-aligned start.
        let bitmap_phys = Self::find_bitmap_region(entries, bitmap_len);

        // Access the bitmap through the HHDM. The bootloader mapped all physical
        // memory at virtual address (phys + hhdm_offset), so we can just add.
        let bitmap_virt = (bitmap_phys + hhdm_offset) as *mut u8;

        // --- Step 3: Zero the bitmap (all frames = allocated) ---

        // SAFETY: bitmap_virt points to HHDM-mapped memory within a USABLE
        // region of size >= bitmap_len.
        unsafe {
            core::ptr::write_bytes(bitmap_virt, 0, bitmap_len);
        }

        let mut allocator = Self {
            bitmap: bitmap_virt,
            bitmap_len,
            frame_count,
            free_count: 0,
            usable_count: 0,
        };

        // --- Verify kernel frames are not USABLE ---

        // The plan's design decision: Limine classifies kernel frames as
        // EXECUTABLE_AND_MODULES, not USABLE. If this assumption is wrong,
        // we'd hand out kernel memory as free frames → instant corruption.
        for entry in entries.iter() {
            if entry.entry_type == EntryType::USABLE {
                let entry_end = entry.base + entry.length;
                assert!(
                    !(kernel_phys_base >= entry.base && kernel_phys_base < entry_end),
                    "BUG: Kernel physical base ({:#x}) found in a USABLE memory region! \
                     Expected EXECUTABLE_AND_MODULES.",
                    kernel_phys_base
                );
            }
        }

        // --- Step 4: Mark USABLE frames as free ---

        // Walk every USABLE entry in the memory map and set the corresponding
        // bits. Entries are page-aligned per the plan: round start up, end down.
        for entry in entries.iter() {
            if entry.entry_type == EntryType::USABLE {
                let start = align_up(entry.base, PAGE_SIZE);
                let end = align_down(entry.base + entry.length, PAGE_SIZE);
                let mut addr = start;
                while addr < end {
                    allocator.mark_free(addr / PAGE_SIZE);
                    addr += PAGE_SIZE;
                }
            }
        }

        // --- Step 5: Mark bitmap's own frames as allocated ---

        // The bitmap sits inside a USABLE region, so step 4 marked its frames
        // as free. We undo that here so the allocator never hands out the
        // memory it's using for its own bookkeeping.
        let bitmap_pages = align_up(bitmap_len as u64, PAGE_SIZE) / PAGE_SIZE;
        for i in 0..bitmap_pages {
            allocator.mark_allocated((bitmap_phys + i * PAGE_SIZE) / PAGE_SIZE);
        }

        // --- Step 6: Count free frames ---

        allocator.free_count = allocator.bitmap_slice()
            .iter()
            .map(|byte| byte.count_ones() as usize)
            .sum();

        // Nothing has been handed out yet, so every frame that is free right now is a
        // frame the memory map called usable. Capturing it here is what lets `mem`
        // report against real RAM rather than against the bitmap's address span.
        allocator.usable_count = allocator.free_count;

        allocator
    }

    /// Find the first USABLE region large enough to hold `bitmap_len` bytes.
    ///
    /// Returns the page-aligned physical start address for the bitmap.
    /// Panics if no suitable region exists.
    fn find_bitmap_region(entries: &[&Entry], bitmap_len: usize) -> u64 {
        let bitmap_size = align_up(bitmap_len as u64, PAGE_SIZE);

        for entry in entries.iter() {
            if entry.entry_type == EntryType::USABLE {
                let start = align_up(entry.base, PAGE_SIZE);
                let end = align_down(entry.base + entry.length, PAGE_SIZE);
                if end > start && end - start >= bitmap_size {
                    return start;
                }
            }
        }

        panic!(
            "No usable memory region large enough for the frame bitmap ({} bytes needed)",
            bitmap_len
        );
    }

    /// Get a read-only view of the bitmap.
    fn bitmap_slice(&self) -> &[u8] {
        // SAFETY: bitmap points to HHDM-mapped memory of bitmap_len bytes,
        // and all access is serialized by the InterruptMutex wrapper.
        unsafe { core::slice::from_raw_parts(self.bitmap, self.bitmap_len) }
    }

    /// Get a mutable view of the bitmap.
    fn bitmap_slice_mut(&mut self) -> &mut [u8] {
        // SAFETY: same as bitmap_slice, plus we have &mut self.
        unsafe { core::slice::from_raw_parts_mut(self.bitmap, self.bitmap_len) }
    }

    /// Mark a frame as free (set its bit to 1).
    fn mark_free(&mut self, frame_idx: u64) {
        let idx = frame_idx as usize;
        if idx < self.frame_count {
            self.bitmap_slice_mut()[idx / 8] |= 1 << (idx % 8);
        }
    }

    /// Mark a frame as allocated (clear its bit to 0).
    fn mark_allocated(&mut self, frame_idx: u64) {
        let idx = frame_idx as usize;
        if idx < self.frame_count {
            self.bitmap_slice_mut()[idx / 8] &= !(1 << (idx % 8));
        }
    }

    /// Check whether a frame is free.
    fn is_free(&self, frame_idx: usize) -> bool {
        if frame_idx >= self.frame_count {
            return false;
        }
        self.bitmap_slice()[frame_idx / 8] & (1 << (frame_idx % 8)) != 0
    }

    /// Allocate `count` contiguous 4 KiB physical frames.
    ///
    /// Scans the bitmap for a run of `count` consecutive free frames, marks them
    /// all as allocated, and returns the physical address of the first frame.
    /// Returns `None` if no contiguous run of sufficient length exists.
    ///
    /// Used by the heap allocator to get a contiguous virtual region (contiguous
    /// physical frames = contiguous HHDM virtual addresses).
    pub fn allocate_contiguous_frames(&mut self, count: usize) -> Option<PhysAddr> {
        if count == 0 {
            return None;
        }

        let mut run_start = 0;
        let mut run_len = 0;

        for frame_idx in 0..self.frame_count {
            if self.is_free(frame_idx) {
                if run_len == 0 {
                    run_start = frame_idx;
                }
                run_len += 1;
                if run_len == count {
                    for i in run_start..run_start + count {
                        self.mark_allocated(i as u64);
                    }
                    self.free_count -= count;
                    return Some(PhysAddr::new(run_start as u64 * PAGE_SIZE));
                }
            } else {
                run_len = 0;
            }
        }

        None
    }

    /// Allocate a single 4 KiB physical frame.
    ///
    /// Scans the bitmap for the first free frame, marks it as allocated, and
    /// returns its physical address. Returns `None` if no free frames remain.
    ///
    /// Uses a simple linear scan — O(n) in the number of frames. This is fine
    /// for Phase 1; a more sophisticated allocator (buddy system, free lists)
    /// can be added later if allocation speed becomes a bottleneck.
    pub fn allocate_frame(&mut self) -> Option<PhysAddr> {
        for byte_idx in 0..self.bitmap_len {
            let byte = self.bitmap_slice()[byte_idx];
            if byte != 0 {
                // Found a byte with at least one free frame. Find the
                // lowest-numbered free frame in this byte.
                let bit = byte.trailing_zeros() as usize;
                let frame_idx = byte_idx * 8 + bit;

                if frame_idx >= self.frame_count {
                    return None;
                }

                self.bitmap_slice_mut()[byte_idx] &= !(1 << bit);
                self.free_count -= 1;

                return Some(PhysAddr::new(frame_idx as u64 * PAGE_SIZE));
            }
        }
        None
    }

    /// Return a previously allocated frame to the free pool.
    ///
    /// The address must be page-aligned and must have been previously returned
    /// by `allocate_frame()`. Double-freeing a frame panics.
    pub fn deallocate_frame(&mut self, addr: PhysAddr) {
        assert!(addr.is_page_aligned(), "Deallocating non-page-aligned address: {}", addr);

        let frame_idx = (addr.as_u64() / PAGE_SIZE) as usize;
        assert!(frame_idx < self.frame_count, "Frame index {} out of range (max {})", frame_idx, self.frame_count);
        assert!(!self.is_free(frame_idx), "Double free of frame at {}", addr);

        self.bitmap_slice_mut()[frame_idx / 8] |= 1 << (frame_idx % 8);
        self.free_count += 1;
    }

    /// Get the number of currently free frames.
    pub fn free_frame_count(&self) -> usize {
        self.free_count
    }

    /// Get the total number of frames tracked by this allocator.
    ///
    /// This is the bitmap's *span*, holes included — see [`usable_frame_count`] for
    /// the amount of real RAM.
    ///
    /// [`usable_frame_count`]: Self::usable_frame_count
    pub fn total_frame_count(&self) -> usize {
        self.frame_count
    }

    /// Frames backed by real RAM, per the bootloader's memory map.
    pub fn usable_frame_count(&self) -> usize {
        self.usable_count
    }

    /// Mark a range of physical frames as free (reclaimable).
    ///
    /// Used to reclaim bootloader-reclaimable memory regions after the kernel
    /// has set up its own page tables, GDT, and stack (so Limine's structures
    /// are no longer needed). Only marks frames that are currently allocated
    /// (avoids double-free of frames we already freed during init).
    ///
    /// `base` must be page-aligned. `length` is in bytes (rounded to page boundaries).
    /// Returns the number of frames reclaimed.
    pub fn reclaim_region(&mut self, base: u64, length: u64) -> usize {
        let start_frame = (align_up(base, PAGE_SIZE) / PAGE_SIZE) as usize;
        let end_frame = (align_down(base + length, PAGE_SIZE) / PAGE_SIZE) as usize;
        let mut reclaimed = 0;

        for frame_idx in start_frame..end_frame {
            if frame_idx < self.frame_count && !self.is_free(frame_idx) {
                self.mark_free(frame_idx as u64);
                self.free_count += 1;
                reclaimed += 1;
            }
        }

        reclaimed
    }
}

// ==========================================================================
// Global frame allocator behind InterruptMutex
// ==========================================================================
//
// The frame allocator is a global resource: any kernel code that needs a
// physical frame calls `frame::allocate_frame()`. We protect it with an
// InterruptMutex (not a plain spinlock) because the timer interrupt handler
// may indirectly trigger allocation in future phases (e.g., scheduler
// creating a new task). If a plain spinlock were held during allocation
// and the timer fired, the handler would spin forever → deadlock.

/// Global frame allocator, protected by an interrupt-disabling mutex.
///
/// Starts as `None` and is initialized by `init()`. After initialization,
/// every call to `allocate_frame()` / `deallocate_frame()` locks this mutex,
/// which disables interrupts for the duration of the operation.
static FRAME_ALLOCATOR: InterruptMutex<Option<BitmapFrameAllocator>> = InterruptMutex::new(None);

/// Initialize the global frame allocator from the Limine memory map.
///
/// Must be called exactly once during early boot, after `mm::init_hhdm()`.
/// After this call, `allocate_frame()` and `deallocate_frame()` are available.
pub fn init(entries: &[&Entry], hhdm_offset: u64, kernel_phys_base: u64) {
    let allocator = BitmapFrameAllocator::new(entries, hhdm_offset, kernel_phys_base);
    *FRAME_ALLOCATOR.lock() = Some(allocator);
}

/// Allocate `count` contiguous 4 KiB physical frames.
///
/// Returns the physical address of the first frame, or `None` if no
/// contiguous run of that size exists.
pub fn allocate_contiguous_frames(count: usize) -> Option<PhysAddr> {
    FRAME_ALLOCATOR
        .lock()
        .as_mut()
        .expect("Frame allocator not initialized")
        .allocate_contiguous_frames(count)
}

/// Allocate a single 4 KiB physical frame.
///
/// Returns `Some(PhysAddr)` on success, `None` if memory is exhausted.
/// Disables interrupts while the allocator lock is held.
///
/// Panics if the frame allocator hasn't been initialized yet.
pub fn allocate_frame() -> Option<PhysAddr> {
    FRAME_ALLOCATOR
        .lock()
        .as_mut()
        .expect("Frame allocator not initialized")
        .allocate_frame()
}

/// Return a physical frame to the free pool.
///
/// The address must be page-aligned and previously returned by `allocate_frame()`.
/// Panics on double-free or if the allocator is uninitialized.
pub fn deallocate_frame(addr: PhysAddr) {
    FRAME_ALLOCATOR
        .lock()
        .as_mut()
        .expect("Frame allocator not initialized")
        .deallocate_frame(addr)
}

/// Get the number of currently free physical frames.
pub fn free_frame_count() -> usize {
    FRAME_ALLOCATOR
        .lock()
        .as_ref()
        .expect("Frame allocator not initialized")
        .free_frame_count()
}

/// Frames backed by real RAM, per the bootloader's memory map.
pub fn usable_frame_count() -> usize {
    FRAME_ALLOCATOR
        .lock()
        .as_ref()
        .expect("Frame allocator not initialized")
        .usable_frame_count()
}

/// Get the total number of physical frames tracked by the allocator.
pub fn total_frame_count() -> usize {
    FRAME_ALLOCATOR
        .lock()
        .as_ref()
        .expect("Frame allocator not initialized")
        .total_frame_count()
}

/// Reclaim a physical memory region, marking its frames as free.
///
/// Used to reclaim bootloader-reclaimable memory after the kernel owns
/// all hardware structures (GDT, page tables, stack). Returns the number
/// of frames reclaimed.
pub fn reclaim_region(base: u64, length: u64) -> usize {
    FRAME_ALLOCATOR
        .lock()
        .as_mut()
        .expect("Frame allocator not initialized")
        .reclaim_region(base, length)
}
