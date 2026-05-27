//! # Page table management
//!
//! x86_64 uses a 4-level page table hierarchy to translate virtual addresses to
//! physical addresses. Each level contains 512 entries of 8 bytes each (one 4 KiB
//! page per table):
//!
//! ```text
//! PML4 (Page Map Level 4)  — 512 entries, each covers 512 GiB
//!   └─► PDP (Page Directory Pointer) — 512 entries, each covers 1 GiB
//!         └─► PD (Page Directory)    — 512 entries, each covers 2 MiB
//!               └─► PT (Page Table)  — 512 entries, each covers 4 KiB
//! ```
//!
//! The CPU walks this hierarchy on every memory access (cached by the TLB).
//! CR3 register holds the physical address of the active PML4.
//!
//! ## Design: HHDM-based table walking
//!
//! We access page table entries through the Higher-Half Direct Map (HHDM).
//! Given a physical address of a page table, we convert it to a virtual
//! address via `phys + hhdm_offset` and read/write entries directly. This
//! avoids the complexity and PML4-slot waste of recursive mapping.
//!
//! ## Kernel vs user address space
//!
//! The upper half of the virtual address space (PML4 indices 256-511) is
//! shared across all address spaces — it maps the kernel code, HHDM, and
//! kernel heap. When creating a new address space, we copy these entries
//! from the kernel PML4 so kernel code is always accessible in ring 0.
//!
//! The lower half (PML4 indices 0-255) is per-process and maps user code,
//! data, and stack.

use crate::arch::x86_64::cpu;
use crate::mm::addr::{PhysAddr, VirtAddr};
use crate::mm::frame;
use crate::mm::PAGE_SIZE;
use crate::println;

/// Number of entries in each page table level (PML4, PDP, PD, PT).
/// x86_64 uses 9 bits per level → 2^9 = 512 entries.
const ENTRIES_PER_TABLE: usize = 512;

/// PML4 index where the kernel half begins (index 256 = virtual address 0xFFFF800000000000).
/// Entries 256-511 are shared across all address spaces.
const KERNEL_PML4_START: usize = 256;

// --- PageFlags ---

/// Bitflags for page table entry attributes.
///
/// These flags control how the CPU treats the mapped page: whether it's
/// present in memory, writable, accessible from userspace, executable, etc.
/// They map directly to bits in the x86_64 page table entry format.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PageFlags(u64);

impl PageFlags {
    /// Page is present in physical memory. If clear, any access triggers a
    /// page fault (#PF). This is the fundamental "is this entry valid" bit.
    pub const PRESENT: Self = Self(1 << 0);

    /// Page is writable. If clear, writes trigger a page fault. Note: in
    /// ring 0 (kernel mode), the CPU ignores this bit unless CR0.WP is set
    /// (which it should be for proper memory protection).
    pub const WRITABLE: Self = Self(1 << 1);

    /// Page is accessible from ring 3 (userspace). If clear, only ring 0
    /// (kernel) code can access this page. This is how we isolate user
    /// processes from kernel memory.
    pub const USER: Self = Self(1 << 2);

    /// Write-through caching. Writes go to both cache and memory immediately
    /// instead of being cached and written back later. Used for memory-mapped
    /// I/O regions where the device needs to see writes immediately.
    pub const WRITE_THROUGH: Self = Self(1 << 3);

    /// Disable caching entirely for this page. Used for memory-mapped I/O
    /// where reads must always hit the device, not a stale cache line.
    pub const CACHE_DISABLE: Self = Self(1 << 4);

    /// The CPU sets this bit when the page is accessed (read or written).
    /// Used by the OS for page replacement algorithms (not needed in Phase 2).
    pub const ACCESSED: Self = Self(1 << 5);

    /// The CPU sets this bit when the page is written to. Used for tracking
    /// dirty pages that need to be written back to disk (not needed in Phase 2).
    pub const DIRTY: Self = Self(1 << 6);

    /// Huge page flag. At the PD level, this creates a 2 MiB page instead of
    /// pointing to a PT. At the PDP level, this creates a 1 GiB page. We don't
    /// use huge pages in Phase 2 but need to detect them during table walks.
    pub const HUGE_PAGE: Self = Self(1 << 7);

    /// Global page — TLB entry is not flushed on CR3 writes. Used for kernel
    /// pages that are the same across all address spaces. Requires CR4.PGE=1.
    pub const GLOBAL: Self = Self(1 << 8);

    /// No-execute bit (bit 63). If set, code execution from this page triggers
    /// a page fault. Used to prevent data pages from being executed (W^X policy).
    /// Requires IA32_EFER.NXE=1.
    pub const NO_EXECUTE: Self = Self(1 << 63);

    /// Empty flags (no bits set).
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Get the raw u64 value of these flags.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Combine two sets of flags (bitwise OR).
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Check whether all bits in `other` are set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

/// Allow combining PageFlags with the `|` operator for readability:
/// `PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER`
impl core::ops::BitOr for PageFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl core::fmt::Debug for PageFlags {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut first = true;
        let mut flag = |name: &str, bit: PageFlags| {
            if self.contains(bit) {
                if !first {
                    write!(f, " | ")?;
                }
                write!(f, "{}", name)?;
                first = false;
            }
            Ok(())
        };
        flag("PRESENT", Self::PRESENT)?;
        flag("WRITABLE", Self::WRITABLE)?;
        flag("USER", Self::USER)?;
        flag("WRITE_THROUGH", Self::WRITE_THROUGH)?;
        flag("CACHE_DISABLE", Self::CACHE_DISABLE)?;
        flag("ACCESSED", Self::ACCESSED)?;
        flag("DIRTY", Self::DIRTY)?;
        flag("HUGE_PAGE", Self::HUGE_PAGE)?;
        flag("GLOBAL", Self::GLOBAL)?;
        flag("NO_EXECUTE", Self::NO_EXECUTE)?;
        if first {
            write!(f, "(none)")?;
        }
        Ok(())
    }
}

// --- PageTableEntry ---

/// A single entry in any level of the x86_64 page table hierarchy.
///
/// Each entry is 8 bytes (u64) with a well-defined bit layout:
///
/// ```text
/// Bit(s)  | Field
/// --------|------------------------------------------
/// 0       | Present (P)
/// 1       | Read/Write (R/W)
/// 2       | User/Supervisor (U/S)
/// 3       | Page-level write-through (PWT)
/// 4       | Page-level cache disable (PCD)
/// 5       | Accessed (A) — set by CPU
/// 6       | Dirty (D) — set by CPU (PT level only)
/// 7       | Page size (PS) / PAT (level-dependent)
/// 8       | Global (G) (PT level only)
/// 9-11    | Available to OS
/// 12-51   | Physical address of next table / page frame
/// 52-62   | Available to OS / reserved
/// 63      | No Execute (NX) — requires IA32_EFER.NXE
/// ```
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct PageTableEntry(u64);

/// Mask to extract the physical address from a PTE (bits 12-51).
/// Physical addresses in PTEs are always page-aligned, so the low 12 bits
/// are used for flags and the address occupies bits 12 through 51.
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

impl PageTableEntry {
    /// Create a new empty (not present) page table entry.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Create a new entry with the given physical address and flags.
    ///
    /// The physical address must be page-aligned (low 12 bits zero).
    /// Flags are ORed into the entry alongside the address.
    pub const fn new(phys_addr: PhysAddr, flags: PageFlags) -> Self {
        Self((phys_addr.as_u64() & ADDR_MASK) | flags.bits())
    }

    /// Get the raw u64 value of this entry.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Check whether the Present bit is set.
    ///
    /// If not present, the rest of the entry is ignored by the CPU (the OS
    /// can use the remaining bits for swap metadata, etc.).
    pub const fn is_present(self) -> bool {
        self.0 & PageFlags::PRESENT.bits() != 0
    }

    /// Check whether this entry has the Huge Page bit set.
    ///
    /// At the PD level, a huge page means this entry maps a 2 MiB page
    /// directly (no PT level). At the PDP level, it maps 1 GiB.
    pub const fn is_huge(self) -> bool {
        self.0 & PageFlags::HUGE_PAGE.bits() != 0
    }

    /// Extract the physical address from this entry (bits 12-51).
    ///
    /// Returns the physical address of the next-level page table (for
    /// PML4/PDP/PD entries) or the mapped page frame (for PT entries).
    pub const fn phys_addr(self) -> PhysAddr {
        PhysAddr::new(self.0 & ADDR_MASK)
    }

    /// Extract the flags from this entry (all bits except the address).
    pub const fn flags(self) -> PageFlags {
        PageFlags(self.0 & !ADDR_MASK)
    }

    /// Set this entry to the given raw value.
    pub fn set(&mut self, value: u64) {
        self.0 = value;
    }

    /// Clear this entry (set to zero / not present).
    pub fn clear(&mut self) {
        self.0 = 0;
    }
}

impl core::fmt::Debug for PageTableEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.is_present() {
            write!(
                f,
                "PTE({:#x} -> {:?}, flags={:?})",
                self.0,
                self.phys_addr(),
                self.flags()
            )
        } else {
            write!(f, "PTE(not present)")
        }
    }
}

// --- PageTable ---

/// A single page table: an array of 512 entries, occupying exactly one 4 KiB page.
///
/// This struct is used for ALL levels of the page table hierarchy (PML4, PDP,
/// PD, and PT). The interpretation of each entry depends on which level it's at:
///
/// - PML4 entry → points to a PDP table
/// - PDP entry  → points to a PD table (or maps a 1 GiB huge page)
/// - PD entry   → points to a PT table (or maps a 2 MiB huge page)
/// - PT entry   → maps a 4 KiB page frame
#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [PageTableEntry; ENTRIES_PER_TABLE],
}

impl PageTable {
    /// Create a new page table with all entries cleared (not present).
    pub const fn empty() -> Self {
        Self {
            entries: [PageTableEntry::empty(); ENTRIES_PER_TABLE],
        }
    }
}

// --- AddressSpace ---

/// An address space, identified by the physical address of its PML4 table.
///
/// Each process gets its own AddressSpace with private lower-half mappings
/// (user pages) and shared upper-half mappings (kernel). The kernel itself
/// runs in a special AddressSpace that replaces Limine's boot-time page tables.
pub struct AddressSpace {
    /// Physical address of this address space's PML4 (the value loaded into CR3).
    pml4_phys: PhysAddr,
}

impl AddressSpace {
    /// Create the kernel's address space by cloning Limine's upper-half mappings.
    ///
    /// Allocates a fresh PML4 frame and copies all entries from Limine's PML4
    /// for the upper half (indices 256-511, covering the HHDM and kernel image).
    /// The lower half (indices 0-255) is left empty — the kernel doesn't use
    /// user-space addresses.
    ///
    /// After calling this, write the returned PML4's physical address to CR3
    /// to switch away from Limine's page tables.
    pub fn new_kernel() -> Self {
        // Read Limine's current PML4 physical address from CR3.
        let limine_pml4_phys = PhysAddr::new(cpu::read_cr3() & !0xFFF);
        let limine_pml4: &PageTable =
            // SAFETY: Limine's PML4 is at a valid physical address mapped via HHDM.
            // We only read from it (copying entries), never write.
            unsafe { &*limine_pml4_phys.as_ptr::<PageTable>() };

        // Allocate a fresh frame for our new PML4.
        let new_pml4_phys = frame::allocate_frame()
            .expect("new_kernel: failed to allocate PML4 frame");
        let new_pml4: &mut PageTable =
            // SAFETY: the newly allocated frame is valid physical memory mapped via HHDM.
            // We have exclusive access because it was just allocated.
            unsafe { &mut *new_pml4_phys.as_mut_ptr::<PageTable>() };

        // Zero the entire table first (lower half will be empty).
        for entry in new_pml4.entries.iter_mut() {
            entry.clear();
        }

        // Copy the upper half (kernel mappings) from Limine's PML4.
        // These entries point to PDP tables that contain the HHDM, kernel
        // image, and kernel heap mappings. By sharing these PML4 entries,
        // all address spaces see the same kernel memory.
        for i in KERNEL_PML4_START..ENTRIES_PER_TABLE {
            new_pml4.entries[i].set(limine_pml4.entries[i].as_u64());
        }

        println!(
            "[pgtable] Kernel address space created: PML4 at {:#x}",
            new_pml4_phys.as_u64()
        );

        Self {
            pml4_phys: new_pml4_phys,
        }
    }

    /// Create a new user address space with shared kernel mappings.
    ///
    /// The upper half (PML4 indices 256-511) is copied from the kernel's PML4
    /// so kernel code remains accessible during ring 0 execution. The lower
    /// half starts completely empty — the caller must map user code, stack,
    /// and data pages before scheduling a task in this address space.
    pub fn new_user(kernel: &AddressSpace) -> Self {
        let kernel_pml4: &PageTable =
            // SAFETY: kernel's PML4 is at a valid physical address mapped via HHDM.
            unsafe { &*kernel.pml4_phys.as_ptr::<PageTable>() };

        let new_pml4_phys = frame::allocate_frame()
            .expect("new_user: failed to allocate PML4 frame");
        let new_pml4: &mut PageTable =
            // SAFETY: newly allocated frame, exclusive access.
            unsafe { &mut *new_pml4_phys.as_mut_ptr::<PageTable>() };

        // Zero the lower half (user pages — per-process, starts empty).
        for i in 0..KERNEL_PML4_START {
            new_pml4.entries[i].clear();
        }

        // Copy the upper half from the kernel PML4 (shared kernel mappings).
        for i in KERNEL_PML4_START..ENTRIES_PER_TABLE {
            new_pml4.entries[i].set(kernel_pml4.entries[i].as_u64());
        }

        Self {
            pml4_phys: new_pml4_phys,
        }
    }

    /// Get the physical address of this address space's PML4 table.
    ///
    /// This is the value to write to CR3 to activate this address space.
    pub fn pml4_phys(&self) -> PhysAddr {
        self.pml4_phys
    }

    /// Map a single 4 KiB virtual page to a physical frame.
    ///
    /// Walks the 4-level page table hierarchy, allocating intermediate tables
    /// (PDP, PD, PT) as needed. Sets the final PT entry to point to `phys`
    /// with the given `flags`.
    ///
    /// # Panics
    ///
    /// Panics if the virtual address is already mapped (the PT entry is present).
    /// Callers should unmap first if remapping is intended.
    pub fn map_page(&self, virt: VirtAddr, phys: PhysAddr, flags: PageFlags) {
        assert!(
            virt.is_page_aligned(),
            "map_page: virtual address {:#x} is not page-aligned",
            virt.as_u64()
        );
        assert!(
            phys.is_page_aligned(),
            "map_page: physical address {:#x} is not page-aligned",
            phys.as_u64()
        );

        let pml4_idx = virt.pml4_index();
        let pdp_idx = virt.pdp_index();
        let pd_idx = virt.pd_index();
        let pt_idx = virt.pt_index();

        // Walk or allocate: PML4 → PDP
        let pml4: &mut PageTable =
            // SAFETY: our PML4 is at a valid phys address mapped via HHDM.
            unsafe { &mut *self.pml4_phys.as_mut_ptr::<PageTable>() };
        let pdp_phys = Self::ensure_table(&mut pml4.entries[pml4_idx]);

        // Walk or allocate: PDP → PD
        let pdp: &mut PageTable =
            // SAFETY: pdp_phys is a valid frame (either existing or just allocated).
            unsafe { &mut *pdp_phys.as_mut_ptr::<PageTable>() };
        let pd_phys = Self::ensure_table(&mut pdp.entries[pdp_idx]);

        // Walk or allocate: PD → PT
        let pd: &mut PageTable =
            // SAFETY: pd_phys is a valid frame.
            unsafe { &mut *pd_phys.as_mut_ptr::<PageTable>() };
        let pt_phys = Self::ensure_table(&mut pd.entries[pd_idx]);

        // Set the final PT entry to map the page.
        let pt: &mut PageTable =
            // SAFETY: pt_phys is a valid frame.
            unsafe { &mut *pt_phys.as_mut_ptr::<PageTable>() };

        assert!(
            !pt.entries[pt_idx].is_present(),
            "map_page: virtual address {:#x} is already mapped",
            virt.as_u64()
        );

        pt.entries[pt_idx] = PageTableEntry::new(phys, flags | PageFlags::PRESENT);
    }

    /// Unmap a single 4 KiB virtual page.
    ///
    /// Clears the PT entry for the given virtual address and invalidates the
    /// TLB entry so the CPU stops using the stale translation.
    ///
    /// Returns the physical address that was previously mapped, or `None` if
    /// the page was not mapped. Does not free the physical frame — the caller
    /// is responsible for that if desired.
    pub fn unmap_page(&self, virt: VirtAddr) -> Option<PhysAddr> {
        assert!(
            virt.is_page_aligned(),
            "unmap_page: virtual address {:#x} is not page-aligned",
            virt.as_u64()
        );

        let pml4_idx = virt.pml4_index();
        let pdp_idx = virt.pdp_index();
        let pd_idx = virt.pd_index();
        let pt_idx = virt.pt_index();

        // Walk the existing hierarchy — if any level is not present, the page
        // is not mapped and we return None (no-op).
        let pml4: &mut PageTable =
            unsafe { &mut *self.pml4_phys.as_mut_ptr::<PageTable>() };
        if !pml4.entries[pml4_idx].is_present() {
            return None;
        }

        let pdp: &mut PageTable =
            unsafe { &mut *pml4.entries[pml4_idx].phys_addr().as_mut_ptr::<PageTable>() };
        if !pdp.entries[pdp_idx].is_present() {
            return None;
        }

        let pd: &mut PageTable =
            unsafe { &mut *pdp.entries[pdp_idx].phys_addr().as_mut_ptr::<PageTable>() };
        if !pd.entries[pd_idx].is_present() {
            return None;
        }

        let pt: &mut PageTable =
            unsafe { &mut *pd.entries[pd_idx].phys_addr().as_mut_ptr::<PageTable>() };
        if !pt.entries[pt_idx].is_present() {
            return None;
        }

        // Save the old physical address before clearing.
        let old_phys = pt.entries[pt_idx].phys_addr();
        pt.entries[pt_idx].clear();

        // Invalidate the TLB entry for this virtual address so the CPU
        // doesn't use the stale cached translation.
        // SAFETY: we've just unmapped this page — invalidating its TLB entry
        // is correct and expected.
        unsafe {
            cpu::invlpg(virt.as_u64());
        }

        Some(old_phys)
    }

    /// Translate a virtual address to a physical address by walking the page tables.
    ///
    /// Returns `Some(PhysAddr)` if the address is mapped (all 4 levels present),
    /// or `None` if any level is not present. The returned physical address includes
    /// the page offset (low 12 bits of the virtual address).
    pub fn translate(&self, virt: VirtAddr) -> Option<PhysAddr> {
        let pml4_idx = virt.pml4_index();
        let pdp_idx = virt.pdp_index();
        let pd_idx = virt.pd_index();
        let pt_idx = virt.pt_index();

        let pml4: &PageTable =
            unsafe { &*self.pml4_phys.as_ptr::<PageTable>() };
        if !pml4.entries[pml4_idx].is_present() {
            return None;
        }

        let pdp: &PageTable =
            unsafe { &*pml4.entries[pml4_idx].phys_addr().as_ptr::<PageTable>() };
        if !pdp.entries[pdp_idx].is_present() {
            return None;
        }
        // Check for 1 GiB huge page at PDP level.
        if pdp.entries[pdp_idx].is_huge() {
            let base = pdp.entries[pdp_idx].phys_addr().as_u64();
            let offset = virt.as_u64() & 0x3FFF_FFFF; // low 30 bits
            return Some(PhysAddr::new(base + offset));
        }

        let pd: &PageTable =
            unsafe { &*pdp.entries[pdp_idx].phys_addr().as_ptr::<PageTable>() };
        if !pd.entries[pd_idx].is_present() {
            return None;
        }
        // Check for 2 MiB huge page at PD level.
        if pd.entries[pd_idx].is_huge() {
            let base = pd.entries[pd_idx].phys_addr().as_u64();
            let offset = virt.as_u64() & 0x1F_FFFF; // low 21 bits
            return Some(PhysAddr::new(base + offset));
        }

        let pt: &PageTable =
            unsafe { &*pd.entries[pd_idx].phys_addr().as_ptr::<PageTable>() };
        if !pt.entries[pt_idx].is_present() {
            return None;
        }

        // Final 4 KiB page: combine the frame base with the page offset.
        let frame_base = pt.entries[pt_idx].phys_addr().as_u64();
        let offset = virt.page_offset() as u64;
        Some(PhysAddr::new(frame_base + offset))
    }

    /// Destroy this address space, freeing all page table frames for the user half.
    ///
    /// Walks the lower-half PML4 entries (indices 0-255) and recursively frees
    /// all PDP, PD, and PT frames. Does NOT free the pages themselves (the mapped
    /// physical frames) — only the page table infrastructure.
    ///
    /// The upper-half entries (kernel mappings) are NOT freed because they're
    /// shared across all address spaces.
    ///
    /// Finally, frees the PML4 frame itself.
    pub fn destroy(self) {
        let pml4: &PageTable =
            unsafe { &*self.pml4_phys.as_ptr::<PageTable>() };

        // Only free user-half page table frames (indices 0-255).
        for pml4_idx in 0..KERNEL_PML4_START {
            if !pml4.entries[pml4_idx].is_present() {
                continue;
            }
            let pdp_phys = pml4.entries[pml4_idx].phys_addr();
            let pdp: &PageTable =
                unsafe { &*pdp_phys.as_ptr::<PageTable>() };

            for pdp_idx in 0..ENTRIES_PER_TABLE {
                if !pdp.entries[pdp_idx].is_present() || pdp.entries[pdp_idx].is_huge() {
                    continue;
                }
                let pd_phys = pdp.entries[pdp_idx].phys_addr();
                let pd: &PageTable =
                    unsafe { &*pd_phys.as_ptr::<PageTable>() };

                for pd_idx in 0..ENTRIES_PER_TABLE {
                    if !pd.entries[pd_idx].is_present() || pd.entries[pd_idx].is_huge() {
                        continue;
                    }
                    // Free the PT frame.
                    let pt_phys = pd.entries[pd_idx].phys_addr();
                    frame::deallocate_frame(pt_phys);
                }
                // Free the PD frame.
                frame::deallocate_frame(pd_phys);
            }
            // Free the PDP frame.
            frame::deallocate_frame(pdp_phys);
        }

        // Free the PML4 frame itself.
        frame::deallocate_frame(self.pml4_phys);
    }

    /// Walk the page table for a virtual address and print each level's entry.
    ///
    /// Used by the `pgtable` shell command for debugging. Prints the PML4, PDP,
    /// PD, and PT entries with their physical addresses and flags.
    pub fn walk_and_print(&self, virt: VirtAddr) {
        let pml4_idx = virt.pml4_index();
        let pdp_idx = virt.pdp_index();
        let pd_idx = virt.pd_index();
        let pt_idx = virt.pt_index();

        println!("Page table walk for {:#x}:", virt.as_u64());
        println!(
            "  Indices: PML4[{}] PDP[{}] PD[{}] PT[{}] offset={:#x}",
            pml4_idx,
            pdp_idx,
            pd_idx,
            pt_idx,
            virt.page_offset()
        );

        let pml4: &PageTable =
            unsafe { &*self.pml4_phys.as_ptr::<PageTable>() };
        let pml4_entry = pml4.entries[pml4_idx];
        println!("  PML4[{}]: {:?}", pml4_idx, pml4_entry);
        if !pml4_entry.is_present() {
            println!("  (walk ends — PML4 entry not present)");
            return;
        }

        let pdp: &PageTable =
            unsafe { &*pml4_entry.phys_addr().as_ptr::<PageTable>() };
        let pdp_entry = pdp.entries[pdp_idx];
        println!("  PDP[{}]:  {:?}", pdp_idx, pdp_entry);
        if !pdp_entry.is_present() {
            println!("  (walk ends — PDP entry not present)");
            return;
        }
        if pdp_entry.is_huge() {
            println!("  => 1 GiB huge page at {:#x}", pdp_entry.phys_addr().as_u64());
            return;
        }

        let pd: &PageTable =
            unsafe { &*pdp_entry.phys_addr().as_ptr::<PageTable>() };
        let pd_entry = pd.entries[pd_idx];
        println!("  PD[{}]:   {:?}", pd_idx, pd_entry);
        if !pd_entry.is_present() {
            println!("  (walk ends — PD entry not present)");
            return;
        }
        if pd_entry.is_huge() {
            println!("  => 2 MiB huge page at {:#x}", pd_entry.phys_addr().as_u64());
            return;
        }

        let pt: &PageTable =
            unsafe { &*pd_entry.phys_addr().as_ptr::<PageTable>() };
        let pt_entry = pt.entries[pt_idx];
        println!("  PT[{}]:   {:?}", pt_idx, pt_entry);
        if pt_entry.is_present() {
            println!(
                "  => 4 KiB page: phys {:#x}, flags {:?}",
                pt_entry.phys_addr().as_u64(),
                pt_entry.flags()
            );
        } else {
            println!("  (walk ends — PT entry not present)");
        }
    }

    /// Ensure that a page table entry points to a valid next-level table.
    ///
    /// If the entry is already present, returns the physical address it points to.
    /// If not present, allocates a new frame, zeros it, sets the entry with
    /// PRESENT | WRITABLE | USER flags, and returns the new frame's address.
    ///
    /// The USER flag is set on intermediate entries because the final PT entry
    /// controls the actual access permission. Intermediate entries must be at
    /// least as permissive as the final entry — setting USER on all intermediates
    /// means we can map both kernel and user pages through the same hierarchy.
    fn ensure_table(entry: &mut PageTableEntry) -> PhysAddr {
        if entry.is_present() {
            return entry.phys_addr();
        }

        // Allocate a new frame for the next-level table.
        let frame_phys = frame::allocate_frame()
            .expect("ensure_table: out of physical frames for page table");

        // Zero the new table (all entries not present).
        let table: &mut PageTable =
            // SAFETY: newly allocated frame, exclusive access, mapped via HHDM.
            unsafe { &mut *frame_phys.as_mut_ptr::<PageTable>() };
        for e in table.entries.iter_mut() {
            e.clear();
        }

        // Set the parent entry to point to the new table.
        // PRESENT | WRITABLE | USER on intermediate entries — the final PT entry
        // controls actual permissions. Intermediates must be permissive enough
        // to allow any leaf permission combination.
        *entry = PageTableEntry::new(
            frame_phys,
            PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER,
        );

        frame_phys
    }
}
