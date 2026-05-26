//! # Physical and virtual address types
//!
//! Newtype wrappers around `u64` for physical and virtual addresses. These
//! provide type safety (can't accidentally pass a physical address where a
//! virtual one is expected) and convenient conversion via the HHDM.
//!
//! ## HHDM-based conversion
//!
//! Limine maps all physical memory into the kernel's virtual address space at
//! a fixed offset (the Higher-Half Direct Map). Converting between physical and
//! virtual addresses is just addition/subtraction of this offset:
//!
//! ```text
//! virt = phys + hhdm_offset
//! phys = virt - hhdm_offset
//! ```
//!
//! The `to_virt()` and `to_phys()` methods handle this, reading the globally
//! stored HHDM offset from `mm::hhdm_offset()`.

use core::fmt;

/// A physical memory address.
///
/// Wraps a `u64` representing a byte address in the physical address space.
/// On x86_64, physical addresses are at most 52 bits wide (with 4-level paging)
/// or 57 bits (with 5-level), but we store the full u64 for simplicity.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhysAddr(u64);

/// A virtual memory address.
///
/// Wraps a `u64` representing a byte address in the virtual address space.
/// On x86_64, virtual addresses are 48 bits (4-level paging) or 57 bits
/// (5-level), sign-extended to 64 bits.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VirtAddr(u64);

impl PhysAddr {
    /// Create a new physical address from a raw u64.
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    /// Get the raw u64 value.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Convert to a virtual address via the HHDM.
    ///
    /// Adds the globally stored HHDM offset to get the virtual address where
    /// this physical memory is mapped. The HHDM must be initialized (via
    /// `mm::init_hhdm()`) before calling this.
    pub fn to_virt(self) -> VirtAddr {
        VirtAddr::new(self.0 + super::hhdm_offset())
    }

    /// Get a raw const pointer to this physical address via the HHDM.
    pub fn as_ptr<T>(self) -> *const T {
        self.to_virt().as_ptr()
    }

    /// Get a raw mutable pointer to this physical address via the HHDM.
    pub fn as_mut_ptr<T>(self) -> *mut T {
        self.to_virt().as_mut_ptr()
    }

    /// Check whether this address is aligned to a 4 KiB page boundary.
    pub fn is_page_aligned(self) -> bool {
        self.0 % super::PAGE_SIZE == 0
    }
}

impl VirtAddr {
    /// Create a new virtual address from a raw u64.
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    /// Get the raw u64 value.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Convert to a physical address by subtracting the HHDM offset.
    ///
    /// Only valid for addresses within the HHDM range (i.e., addresses that
    /// were created by adding the HHDM offset to a physical address).
    pub fn to_phys(self) -> PhysAddr {
        PhysAddr::new(self.0 - super::hhdm_offset())
    }

    /// Get a raw const pointer from this virtual address.
    pub fn as_ptr<T>(self) -> *const T {
        self.0 as *const T
    }

    /// Get a raw mutable pointer from this virtual address.
    pub fn as_mut_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }
}

// --- Display and Debug ---

impl fmt::Display for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhysAddr({:#x})", self.0)
    }
}

impl fmt::Debug for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhysAddr({:#018x})", self.0)
    }
}

impl fmt::Display for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VirtAddr({:#x})", self.0)
    }
}

impl fmt::Debug for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VirtAddr({:#018x})", self.0)
    }
}

// --- Alignment utilities ---

/// Round an address up to the nearest multiple of `align`.
/// `align` must be a power of two.
pub const fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

/// Round an address down to the nearest multiple of `align`.
/// `align` must be a power of two.
pub const fn align_down(addr: u64, align: u64) -> u64 {
    addr & !(align - 1)
}
