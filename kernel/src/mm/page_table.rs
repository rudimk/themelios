//! # Page table management (architecture-neutral)
//!
//! Both supported architectures translate 48-bit virtual addresses through a 4-level
//! table hierarchy of 512 eight-byte descriptors each (one 4 KiB page per table):
//!
//! ```text
//! x86_64                              aarch64
//! ------                              -------
//! PML4  — 512 entries, 512 GiB each    L0  — 512 entries, 512 GiB each
//!   └─► PDP  — 1 GiB each                └─► L1  — 1 GiB each
//!         └─► PD   — 2 MiB each                └─► L2  — 2 MiB each
//!               └─► PT   — 4 KiB each                └─► L3  — 4 KiB each
//! ```
//!
//! The *walk* is therefore identical, and lives here: index extraction, allocation of
//! missing intermediate tables, kernel/user half handling, map/unmap/translate. What
//! differs — descriptor bit layout, permission encoding, which register holds the
//! root, how the TLB is invalidated — lives behind [`crate::arch::paging`].
//!
//! ## Design: HHDM-based table walking
//!
//! Page table entries are reached through the Higher-Half Direct Map. Given the
//! physical address of a table we form `phys + hhdm_offset` and read/write entries
//! directly, avoiding the complexity and root-slot waste of recursive mapping. Since
//! page tables are ordinary RAM, they are always HHDM-reachable.
//!
//! ## Kernel vs user address space
//!
//! The kernel half is shared across all address spaces — it maps kernel code, the
//! HHDM, and the kernel heap. Where that half *begins* is architecture-specific:
//!
//! - **x86_64**: one root (CR3) split by convention, kernel from index 256. New user
//!   address spaces copy entries 256-511 from the kernel root.
//! - **aarch64**: two roots. The kernel owns the whole `TTBR1_EL1` tree (index 0
//!   onward) and userspace gets an independent `TTBR0_EL1` tree, so there is nothing
//!   to copy.
//!
//! The walker reads [`arch::paging::KERNEL_ROOT_START`] and
//! [`arch::paging::PREPOPULATE_KERNEL_ROOT`] rather than hardcoding either policy.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::paging;
use crate::mm::addr::{PhysAddr, VirtAddr};
use crate::mm::frame;
use crate::println;

/// Portable page-permission flags, supplied by the active architecture.
///
/// Re-exported here so callers keep using `mm::page_table::PageFlags` regardless of
/// which architecture's encoding is underneath.
pub use crate::arch::paging::PageFlags;

/// Physical address of the kernel's root page table. Stored globally so new address
/// spaces can share the kernel half and so the page-table manager is reachable from
/// anywhere in the kernel. Set once by [`init`]; zero means uninitialized.
static KERNEL_ROOT_PHYS: AtomicU64 = AtomicU64::new(0);

/// Number of entries in each page table level (512 on both architectures).
const ENTRIES_PER_TABLE: usize = paging::ENTRIES_PER_TABLE;

/// First root-table index belonging to the kernel half (256 on x86_64, 0 on aarch64).
const KERNEL_ROOT_START: usize = paging::KERNEL_ROOT_START;

/// Initialize the kernel's own page tables and switch away from the bootloader's.
///
/// Builds a root table cloning the bootloader's kernel-half mappings (HHDM, kernel
/// image, heap) and activates it. After this call the bootloader's tables are no
/// longer in use and bootloader-reclaimable memory can be reclaimed.
///
/// Must run after the frame allocator is up (this allocates tables) and before
/// anything depends on kernel-owned mappings.
///
/// The activation is the critical moment: if the new root is missing a mapping for
/// the executing code or stack, the CPU faults immediately and unrecoverably.
/// Reaching the `println!` afterwards is itself the proof that it worked.
pub fn init() {
    let kernel_as = AddressSpace::new_kernel();
    let root_phys = kernel_as.root_phys().as_u64();

    // Publish before switching so `kernel_address_space()` works immediately after.
    KERNEL_ROOT_PHYS.store(root_phys, Ordering::Relaxed);

    // SAFETY: `new_kernel` cloned every kernel-half entry from the live root, so all
    // currently executing code, the stack, and the HHDM remain mapped identically.
    unsafe {
        paging::activate(root_phys);
    }

    println!(
        "[pgtable] Switched to kernel page tables (root = {:#x})",
        root_phys
    );

    // Verify by reading the root register back.
    let readback = paging::current_root();
    assert!(
        readback == root_phys,
        "page-table root readback mismatch: expected {:#x}, got {:#x}",
        root_phys,
        readback
    );

    // The kernel address space lives forever and is never destroyed; its physical
    // address is kept in KERNEL_ROOT_PHYS. Deliberately leak the handle.
    core::mem::forget(kernel_as);
}

/// Get an [`AddressSpace`] handle for the kernel's page tables.
///
/// Used by heap setup and growth, MMIO mapping, and process creation.
///
/// # Panics
///
/// Panics if called before [`init`].
pub fn kernel_address_space() -> AddressSpace {
    let phys = KERNEL_ROOT_PHYS.load(Ordering::Relaxed);
    assert!(
        phys != 0,
        "kernel_address_space: page table manager not initialized"
    );
    AddressSpace {
        root_phys: PhysAddr::new(phys),
    }
}

// --- PageTableEntry ---

/// A single descriptor in any level of the hierarchy.
///
/// Eight bytes whose bit layout is architecture-specific; all interpretation is
/// delegated to [`crate::arch::paging`]. Keeping the type here (rather than in the
/// arch modules) lets the walker treat tables as plain `[PageTableEntry; 512]` arrays
/// on both architectures.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    /// An empty (invalid) descriptor.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Build a **leaf** descriptor mapping `phys` with `flags`.
    ///
    /// Leaf and table descriptors are distinct on aarch64 (bit 1 means "page" at L3
    /// but "table" at L0-L2), so the two constructors are not interchangeable.
    pub fn new_leaf(phys_addr: PhysAddr, flags: PageFlags) -> Self {
        Self(paging::encode_leaf(phys_addr.as_u64(), flags))
    }

    /// Build an **intermediate** descriptor pointing at the next-level table.
    pub fn new_table(phys_addr: PhysAddr) -> Self {
        Self(paging::encode_table(phys_addr.as_u64()))
    }

    /// Build an intermediate descriptor for a table in the **kernel half**.
    ///
    /// Distinct from [`Self::new_table`] because x86_64 withholds `USER` here: these
    /// are the root entries every user address space copies by value, and they should
    /// not advertise ring-3 reachability. On aarch64 the two are identical.
    pub fn new_kernel_table(phys_addr: PhysAddr) -> Self {
        Self(paging::encode_kernel_table(phys_addr.as_u64()))
    }

    /// Raw descriptor value.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Whether this descriptor is valid/present.
    pub const fn is_present(self) -> bool {
        paging::is_valid(self.0)
    }

    /// Whether this intermediate descriptor maps memory directly (a huge page on
    /// x86_64, a block on aarch64) instead of pointing at a next-level table.
    ///
    /// Only meaningful at levels 0-2, which is where the walker consults it.
    pub const fn is_huge(self) -> bool {
        paging::is_block(self.0)
    }

    /// Output physical address of this descriptor.
    pub const fn phys_addr(self) -> PhysAddr {
        PhysAddr::new(paging::addr_of(self.0))
    }

    /// Decode this descriptor's attributes into portable flags.
    pub fn flags(self) -> PageFlags {
        paging::flags_of(self.0)
    }

    /// Overwrite with a raw descriptor value.
    pub fn set(&mut self, value: u64) {
        self.0 = value;
    }

    /// Clear this descriptor (invalid).
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

/// One page table: 512 descriptors occupying exactly one 4 KiB page.
///
/// Used at every level; the meaning of an entry depends on the level it sits at.
#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [PageTableEntry; ENTRIES_PER_TABLE],
}

impl PageTable {
    /// A page table with every descriptor cleared.
    pub const fn empty() -> Self {
        Self {
            entries: [PageTableEntry::empty(); ENTRIES_PER_TABLE],
        }
    }
}

// --- AddressSpace ---

/// An address space, identified by the physical address of its root table.
///
/// On x86_64 that root is the PML4 loaded into CR3. On aarch64 it is the L0 table
/// loaded into `TTBR1_EL1` for the kernel (userspace, when it lands, will get its own
/// `TTBR0_EL1` root).
pub struct AddressSpace {
    /// Physical address of this address space's root table.
    root_phys: PhysAddr,
}

impl AddressSpace {
    /// Create the kernel's address space by cloning the bootloader's kernel-half
    /// mappings.
    ///
    /// Allocates a fresh root frame and copies every kernel-half entry from the live
    /// root, so the new tables describe kernel memory identically. The user half (on
    /// architectures that share a root) is left empty.
    ///
    /// After this, activate the returned root to stop using the bootloader's tables.
    pub fn new_kernel() -> Self {
        // Read the bootloader's live root.
        let boot_root_phys = PhysAddr::new(paging::current_root());
        let boot_root: &PageTable =
            // SAFETY: the live root is valid physical memory reachable via the HHDM.
            // We only read from it.
            unsafe { &*boot_root_phys.as_ptr::<PageTable>() };

        let new_root_phys =
            frame::allocate_frame().expect("new_kernel: failed to allocate root table frame");
        let new_root: &mut PageTable =
            // SAFETY: freshly allocated frame, exclusive access, HHDM-reachable.
            unsafe { &mut *new_root_phys.as_mut_ptr::<PageTable>() };

        // Start from a fully empty table (this also clears the user half, if any).
        for entry in new_root.entries.iter_mut() {
            entry.clear();
        }

        // Clone the kernel half. On aarch64 KERNEL_ROOT_START is 0, so this copies
        // the entire TTBR1 tree; on x86_64 it copies indices 256-511.
        for i in KERNEL_ROOT_START..ENTRIES_PER_TABLE {
            new_root.entries[i].set(boot_root.entries[i].as_u64());
        }

        // Where user address spaces copy kernel root entries by value (x86_64), every
        // kernel slot must already be present so later-added kernel mappings are
        // shared by pointer. See `paging::PREPOPULATE_KERNEL_ROOT`.
        if paging::PREPOPULATE_KERNEL_ROOT {
            for i in KERNEL_ROOT_START..ENTRIES_PER_TABLE {
                if !new_root.entries[i].is_present() {
                    let table_phys = frame::allocate_frame()
                        .expect("new_kernel: failed to allocate shared kernel table");
                    // SAFETY: freshly allocated frame, exclusive access via HHDM.
                    let table: &mut PageTable =
                        unsafe { &mut *table_phys.as_mut_ptr::<PageTable>() };
                    for e in table.entries.iter_mut() {
                        e.clear();
                    }
                    // Kernel-half encoding: no USER bit on x86_64. These entries are
                    // copied by value into every user address space.
                    new_root.entries[i] = PageTableEntry::new_kernel_table(table_phys);
                }
            }
        }

        println!(
            "[pgtable] Kernel address space created: root at {:#x}",
            new_root_phys.as_u64()
        );

        Self {
            root_phys: new_root_phys,
        }
    }

    /// Create a new user address space sharing the kernel's mappings.
    ///
    /// The kernel half is copied from the kernel root so kernel code stays reachable
    /// during privileged execution; the user half starts empty.
    ///
    /// Only meaningful on architectures with a shared root. aarch64 userspace is
    /// deferred with the rest of the EL0 port and will use an independent `TTBR0_EL1`
    /// tree instead of copying anything.
    pub fn new_user(kernel: &AddressSpace) -> Self {
        let kernel_root: &PageTable =
            // SAFETY: the kernel root is valid physical memory reachable via HHDM.
            unsafe { &*kernel.root_phys.as_ptr::<PageTable>() };

        let new_root_phys =
            frame::allocate_frame().expect("new_user: failed to allocate root table frame");
        let new_root: &mut PageTable =
            // SAFETY: newly allocated frame, exclusive access.
            unsafe { &mut *new_root_phys.as_mut_ptr::<PageTable>() };

        // User half — per-process, starts empty.
        for i in 0..KERNEL_ROOT_START {
            new_root.entries[i].clear();
        }

        // Kernel half — shared.
        for i in KERNEL_ROOT_START..ENTRIES_PER_TABLE {
            new_root.entries[i].set(kernel_root.entries[i].as_u64());
        }

        Self {
            root_phys: new_root_phys,
        }
    }

    /// Physical address of this address space's root table — the value to load into
    /// the architecture's translation-base register.
    pub fn root_phys(&self) -> PhysAddr {
        self.root_phys
    }

    /// Map a single 4 KiB virtual page to a physical frame.
    ///
    /// Walks the hierarchy, allocating intermediate tables as needed, then writes the
    /// leaf descriptor. The new translation is published with the architecture's TLB
    /// maintenance so the mapping is usable on return.
    ///
    /// # Panics
    ///
    /// Panics if either address is misaligned, or if the virtual address is already
    /// mapped — callers must unmap first if a remap is intended.
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

        let l0_idx = virt.pml4_index();
        let l1_idx = virt.pdp_index();
        let l2_idx = virt.pd_index();
        let l3_idx = virt.pt_index();

        // Level 0 → 1
        let l0: &mut PageTable =
            // SAFETY: our root is a valid physical address reachable via HHDM.
            unsafe { &mut *self.root_phys.as_mut_ptr::<PageTable>() };
        let l1_phys = Self::ensure_table(&mut l0.entries[l0_idx]);

        // Level 1 → 2
        let l1: &mut PageTable =
            // SAFETY: valid frame, existing or just allocated.
            unsafe { &mut *l1_phys.as_mut_ptr::<PageTable>() };
        let l2_phys = Self::ensure_table(&mut l1.entries[l1_idx]);

        // Level 2 → 3
        let l2: &mut PageTable =
            // SAFETY: valid frame.
            unsafe { &mut *l2_phys.as_mut_ptr::<PageTable>() };
        let l3_phys = Self::ensure_table(&mut l2.entries[l2_idx]);

        // Level 3: the leaf.
        let l3: &mut PageTable =
            // SAFETY: valid frame.
            unsafe { &mut *l3_phys.as_mut_ptr::<PageTable>() };

        assert!(
            !l3.entries[l3_idx].is_present(),
            "map_page: virtual address {:#x} is already mapped",
            virt.as_u64()
        );

        l3.entries[l3_idx] = PageTableEntry::new_leaf(phys, flags | PageFlags::PRESENT);

        // Publish the new translation. x86_64 does not strictly require this for a
        // previously-absent entry, but aarch64 may hold a cached negative translation,
        // and issuing it unconditionally makes the map/unmap contract uniform.
        // SAFETY: the descriptor store above is the change being published.
        unsafe {
            paging::flush_page(virt.as_u64());
        }
    }

    /// Unmap a single 4 KiB virtual page.
    ///
    /// Clears the leaf descriptor and invalidates the stale translation. Returns the
    /// physical address that was mapped, or `None` if it was not mapped. Does not
    /// free the physical frame — that is the caller's business.
    pub fn unmap_page(&self, virt: VirtAddr) -> Option<PhysAddr> {
        assert!(
            virt.is_page_aligned(),
            "unmap_page: virtual address {:#x} is not page-aligned",
            virt.as_u64()
        );

        let l0_idx = virt.pml4_index();
        let l1_idx = virt.pdp_index();
        let l2_idx = virt.pd_index();
        let l3_idx = virt.pt_index();

        // Walk the existing hierarchy; a missing level means "not mapped".
        // SAFETY (all four): each address came from a present descriptor and is
        // therefore a valid table frame reachable via the HHDM.
        let l0: &mut PageTable = unsafe { &mut *self.root_phys.as_mut_ptr::<PageTable>() };
        if !l0.entries[l0_idx].is_present() {
            return None;
        }

        let l1: &mut PageTable =
            unsafe { &mut *l0.entries[l0_idx].phys_addr().as_mut_ptr::<PageTable>() };
        // A block here maps 1 GiB directly; there is no 4 KiB leaf to clear, and
        // walking into it would reinterpret mapped data as descriptors.
        if !l1.entries[l1_idx].is_present() || l1.entries[l1_idx].is_huge() {
            return None;
        }

        let l2: &mut PageTable =
            unsafe { &mut *l1.entries[l1_idx].phys_addr().as_mut_ptr::<PageTable>() };
        // Likewise for a 2 MiB block at level 2.
        if !l2.entries[l2_idx].is_present() || l2.entries[l2_idx].is_huge() {
            return None;
        }

        let l3: &mut PageTable =
            unsafe { &mut *l2.entries[l2_idx].phys_addr().as_mut_ptr::<PageTable>() };
        if !l3.entries[l3_idx].is_present() {
            return None;
        }

        let old_phys = l3.entries[l3_idx].phys_addr();
        l3.entries[l3_idx].clear();

        // Drop the stale translation so the CPU stops using it.
        // SAFETY: the page was just unmapped; invalidating it is the required
        // completion of that operation.
        unsafe {
            paging::flush_page(virt.as_u64());
        }

        Some(old_phys)
    }

    /// Translate a virtual address by walking the tables.
    ///
    /// Returns `Some(PhysAddr)` including the page offset if mapped, `None` if any
    /// level is absent. Handles block/huge mappings at levels 1 and 2, which the
    /// bootloader uses for the HHDM.
    pub fn translate(&self, virt: VirtAddr) -> Option<PhysAddr> {
        let l0_idx = virt.pml4_index();
        let l1_idx = virt.pdp_index();
        let l2_idx = virt.pd_index();
        let l3_idx = virt.pt_index();

        // SAFETY (all four): each table address comes from a present descriptor and
        // is HHDM-reachable; we only read.
        let l0: &PageTable = unsafe { &*self.root_phys.as_ptr::<PageTable>() };
        if !l0.entries[l0_idx].is_present() {
            return None;
        }

        let l1: &PageTable =
            unsafe { &*l0.entries[l0_idx].phys_addr().as_ptr::<PageTable>() };
        if !l1.entries[l1_idx].is_present() {
            return None;
        }
        // 1 GiB block/huge page at level 1.
        if l1.entries[l1_idx].is_huge() {
            let base = l1.entries[l1_idx].phys_addr().as_u64();
            let offset = virt.as_u64() & 0x3FFF_FFFF; // low 30 bits
            return Some(PhysAddr::new(base + offset));
        }

        let l2: &PageTable =
            unsafe { &*l1.entries[l1_idx].phys_addr().as_ptr::<PageTable>() };
        if !l2.entries[l2_idx].is_present() {
            return None;
        }
        // 2 MiB block/huge page at level 2.
        if l2.entries[l2_idx].is_huge() {
            let base = l2.entries[l2_idx].phys_addr().as_u64();
            let offset = virt.as_u64() & 0x1F_FFFF; // low 21 bits
            return Some(PhysAddr::new(base + offset));
        }

        let l3: &PageTable =
            unsafe { &*l2.entries[l2_idx].phys_addr().as_ptr::<PageTable>() };
        if !l3.entries[l3_idx].is_present() {
            return None;
        }

        let frame_base = l3.entries[l3_idx].phys_addr().as_u64();
        let offset = virt.page_offset() as u64;
        Some(PhysAddr::new(frame_base + offset))
    }

    /// Destroy this address space, freeing its user-half table frames.
    ///
    /// Frees the intermediate tables beneath the user half and then the root frame.
    /// Does **not** free the mapped pages themselves, nor any kernel-half tables
    /// (those are shared across all address spaces).
    pub fn destroy(self) {
        // SAFETY: our root is a valid, HHDM-reachable table; we only read it to find
        // the frames to release.
        let root: &PageTable = unsafe { &*self.root_phys.as_ptr::<PageTable>() };

        for l0_idx in 0..KERNEL_ROOT_START {
            if !root.entries[l0_idx].is_present() {
                continue;
            }
            let l1_phys = root.entries[l0_idx].phys_addr();
            // SAFETY: present descriptor ⇒ valid table frame.
            let l1: &PageTable = unsafe { &*l1_phys.as_ptr::<PageTable>() };

            for l1_idx in 0..ENTRIES_PER_TABLE {
                if !l1.entries[l1_idx].is_present() || l1.entries[l1_idx].is_huge() {
                    continue;
                }
                let l2_phys = l1.entries[l1_idx].phys_addr();
                // SAFETY: present, non-block descriptor ⇒ valid table frame.
                let l2: &PageTable = unsafe { &*l2_phys.as_ptr::<PageTable>() };

                for l2_idx in 0..ENTRIES_PER_TABLE {
                    if !l2.entries[l2_idx].is_present() || l2.entries[l2_idx].is_huge() {
                        continue;
                    }
                    frame::deallocate_frame(l2.entries[l2_idx].phys_addr());
                }
                frame::deallocate_frame(l2_phys);
            }
            frame::deallocate_frame(l1_phys);
        }

        frame::deallocate_frame(self.root_phys);
    }

    /// Walk the tables for a virtual address and print each level's descriptor.
    ///
    /// Backs the `pgtable` shell command. Level labels come from the architecture, so
    /// the output reads `PML4/PDP/PD/PT` on x86_64 and `L0/L1/L2/L3` on aarch64.
    pub fn walk_and_print(&self, virt: VirtAddr) {
        let idx = [
            virt.pml4_index(),
            virt.pdp_index(),
            virt.pd_index(),
            virt.pt_index(),
        ];
        let names = paging::LEVEL_NAMES;

        println!("Page table walk for {:#x}:", virt.as_u64());
        println!(
            "  Indices: {}[{}] {}[{}] {}[{}] {}[{}] offset={:#x}",
            names[0], idx[0], names[1], idx[1], names[2], idx[2], names[3], idx[3],
            virt.page_offset()
        );

        // Walk down, printing each descriptor and stopping at the first absent level
        // or block mapping.
        let mut table_phys = self.root_phys;
        for level in 0..4 {
            // SAFETY: `table_phys` is the root or came from a present, non-block
            // descriptor; either way it is a valid HHDM-reachable table.
            let table: &PageTable = unsafe { &*table_phys.as_ptr::<PageTable>() };
            let entry = table.entries[idx[level]];
            println!("  {}[{}]: {:?}", names[level], idx[level], entry);

            if !entry.is_present() {
                println!("  (walk ends — {} entry not present)", names[level]);
                return;
            }
            // Blocks only exist at levels 1 and 2.
            if level > 0 && level < 3 && entry.is_huge() {
                let size = if level == 1 { "1 GiB" } else { "2 MiB" };
                println!(
                    "  => {} block at {:#x}",
                    size,
                    entry.phys_addr().as_u64()
                );
                return;
            }
            if level == 3 {
                println!(
                    "  => 4 KiB page: phys {:#x}, flags {:?}",
                    entry.phys_addr().as_u64(),
                    entry.flags()
                );
                return;
            }
            table_phys = entry.phys_addr();
        }
    }

    /// Ensure a descriptor points at a valid next-level table, allocating one if not.
    ///
    /// (Placed last so the public surface reads top-down.)
    ///
    /// Returns the physical address of the next-level table. Newly created tables are
    /// zeroed and linked with the architecture's intermediate encoding, which is
    /// deliberately permissive — the leaf descriptor decides the effective permission
    /// on both architectures.
    fn ensure_table(entry: &mut PageTableEntry) -> PhysAddr {
        if entry.is_present() {
            // A present descriptor that maps memory directly is *not* a table
            // pointer. Walking into a block/huge entry would treat mapped data as
            // page-table descriptors and corrupt whatever lives there, with no fault
            // at the point of the mistake. Refuse loudly instead — callers wanting a
            // 4 KiB mapping inside a huge region must split it first (which is why
            // `mm::mmio` allocates from its own window rather than re-mapping HHDM
            // addresses).
            assert!(
                !entry.is_huge(),
                "ensure_table: descriptor {:#x} maps a block/huge page — cannot \
                 install a 4 KiB mapping beneath it",
                entry.as_u64()
            );
            return entry.phys_addr();
        }

        let frame_phys =
            frame::allocate_frame().expect("ensure_table: out of physical frames for page table");

        // SAFETY: newly allocated frame, exclusive access, HHDM-reachable.
        let table: &mut PageTable = unsafe { &mut *frame_phys.as_mut_ptr::<PageTable>() };
        for e in table.entries.iter_mut() {
            e.clear();
        }

        *entry = PageTableEntry::new_table(frame_phys);

        frame_phys
    }
}
