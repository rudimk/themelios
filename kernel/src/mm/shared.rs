//! # Shared memory regions
//!
//! IPC messages in ThemeliOS carry only four 64-bit words — enough for a small
//! request header, but nowhere near enough for a disk block or a filesystem
//! buffer. The storage stack therefore moves bulk data through **shared memory**:
//! a run of physical frames the kernel maps into the address spaces of both
//! communicating parties (for example the block server and a filesystem server).
//! The IPC message then only needs to carry an *offset* into the shared region.
//!
//! A [`SharedRegion`] is the kernel-side handle to such a run of frames. The
//! kernel always reaches it through the HHDM; a userspace process reaches it
//! through a mapping installed by [`SharedRegion::map_into`]. The matching
//! capability type is [`CapType::SharedMemory`](crate::cap::CapType::SharedMemory),
//! which names the region so access can be granted and revoked like any other
//! resource.
//!
//! Regions allocated here are permanent for Phase 3 (the block server's transfer
//! buffers live for the kernel's lifetime), so there is no free path yet.

use super::addr::{align_up, PhysAddr, VirtAddr};
use super::frame;
use super::page_table::{AddressSpace, PageFlags};
use super::PAGE_SIZE;

/// A kernel-owned run of physically-contiguous frames usable as shared memory.
///
/// Contiguity matters: a single mapping (and a single DMA descriptor, when the
/// block driver reads into it) needs one physical base and length.
#[derive(Clone, Copy)]
pub struct SharedRegion {
    /// Physical base address (page-aligned).
    pub phys_base: PhysAddr,
    /// Size in bytes (a whole number of pages).
    pub size: u64,
}

impl SharedRegion {
    /// Allocate a zeroed shared region of at least `size_bytes`.
    ///
    /// Rounds up to whole pages, allocates them contiguously, and zeroes them.
    /// Returns `None` if no contiguous run of that size is free.
    pub fn alloc(size_bytes: u64) -> Option<Self> {
        let size = align_up(size_bytes.max(1), PAGE_SIZE);
        let pages = (size / PAGE_SIZE) as usize;
        let phys_base = frame::allocate_contiguous_frames(pages)?;
        // Zero the region so neither party sees stale frame contents.
        // SAFETY: the frames are HHDM-mapped and owned by this region.
        unsafe {
            core::ptr::write_bytes(phys_base.to_virt().as_mut_ptr::<u8>(), 0, size as usize);
        }
        Some(Self { phys_base, size })
    }

    /// Get a mutable byte slice over the whole region via the HHDM.
    ///
    /// For kernel-side access (the block server reads/writes block data here).
    ///
    /// # Safety
    ///
    /// The caller must ensure no aliasing mutable access races with this slice.
    /// In the block server, access is serialised by the request/reply protocol.
    pub unsafe fn as_slice_mut(&self) -> &'static mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(
                self.phys_base.to_virt().as_mut_ptr::<u8>(),
                self.size as usize,
            )
        }
    }

    /// Map this region into a (user) address space at virtual base `virt`.
    ///
    /// Each page is mapped present, writable, user-accessible, and
    /// non-executable — appropriate for a data buffer a ring-3 server reads and
    /// writes. `virt` must be page-aligned. Used when spawning filesystem
    /// servers (sub-phase 3.4) to give them their block-transfer window.
    pub fn map_into(&self, address_space: &AddressSpace, virt: VirtAddr) {
        let flags = PageFlags::PRESENT
            | PageFlags::WRITABLE
            | PageFlags::USER
            | PageFlags::NO_EXECUTE;
        let pages = self.size / PAGE_SIZE;
        for i in 0..pages {
            let p = PhysAddr::new(self.phys_base.as_u64() + i * PAGE_SIZE);
            let v = VirtAddr::new(virt.as_u64() + i * PAGE_SIZE);
            address_space.map_page(v, p, flags);
        }
    }

    /// Build the [`CapType::SharedMemory`](crate::cap::CapType::SharedMemory)
    /// describing this region, owned by `owner_pid`.
    pub fn cap_type(&self, owner_pid: usize) -> crate::cap::CapType {
        crate::cap::CapType::SharedMemory {
            phys_base: self.phys_base.as_u64(),
            size: self.size,
            owner_pid,
        }
    }
}
