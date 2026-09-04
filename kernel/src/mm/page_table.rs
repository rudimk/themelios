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

/// One past the last root entry a *user* address space owns.
///
/// The counterpart to [`KERNEL_ROOT_START`], and not simply derivable from it, because
/// the two architectures partition a root differently:
///
/// - **x86_64** — one root, split. User owns `0..KERNEL_ROOT_START` (256), kernel owns
///   the rest, and the kernel half's tables are shared with every other process.
/// - **aarch64** — two roots. A user space *is* a `TTBR0_EL1` tree, so it owns all 512
///   entries and shares none of them; `KERNEL_ROOT_START` is 0 here because the kernel's
///   own tree, in `TTBR1_EL1`, starts at index 0 of a *different* root.
///
/// Writing "the user half is `0..KERNEL_ROOT_START`" is correct on x86 and silently
/// empty on aarch64 — the shape of the `destroy` leak this constant exists to prevent.
const USER_ROOT_END: usize = if KERNEL_ROOT_START == 0 {
    ENTRIES_PER_TABLE
} else {
    KERNEL_ROOT_START
};

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
        // The kernel's own tree is never installed in TTBR0 and never ASID-tagged: its
        // TTBR1 entries are global to every context. Zero marks "not a user space".
        //
        // `activate_user` carries a `debug_assert_ne!(asid, 0)`, which catches this in the
        // dev profile the kernel actually ships — but it is a `debug_assert`, so it is not
        // a guarantee in a release build. Saying this "cannot" be installed by accident
        // would be a claim about the current build profile, not about the code. The real
        // protection is that `activate_user` is `unsafe` and has one caller.
        #[cfg(target_arch = "aarch64")]
        asid: 0,
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
/// loaded into `TTBR1_EL1` for the kernel; a user space is an independent `TTBR0_EL1`
/// root with its own ASID (8.4).
pub struct AddressSpace {
    /// Physical address of this address space's root table.
    root_phys: PhysAddr,

    /// The ASID this space's TLB entries are tagged with (aarch64 user spaces only).
    ///
    /// x86_64 has no equivalent: it invalidates the whole non-global TLB on every CR3
    /// load, so there is nothing to tag. On aarch64 the tag is what lets a context
    /// switch skip invalidation entirely — and what makes reuse after a rollover the
    /// one case that needs it. Held here rather than looked up at switch time so the
    /// space and its tag cannot get separated.
    #[cfg(target_arch = "aarch64")]
    asid: u16,
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
            // Not a user space — see `kernel_address_space`.
            #[cfg(target_arch = "aarch64")]
            asid: 0,
        }
    }

    /// Create a new user address space.
    ///
    /// ## Two architectures, two shapes — and the shared version had an aarch64 bug
    ///
    /// **x86_64** has one root per address space, split down the middle: entries
    /// `0..256` are the user half, `256..512` the kernel half. A user space must
    /// therefore *copy* the kernel half by value so kernel code stays mapped while the
    /// CPU is running on this root, and start the user half empty.
    ///
    /// **aarch64 has two roots.** `TTBR1_EL1` holds the kernel, `TTBR0_EL1` holds
    /// userspace, and they are separate trees selected by the top bits of the virtual
    /// address. A user space is a *whole independent root*, all 512 entries of it, and
    /// nothing may be copied into it at all.
    ///
    /// The single shared implementation got this exactly backwards. `KERNEL_ROOT_START`
    /// is `0` on aarch64, so the "user half" clear loop was `0..0` — a no-op — and the
    /// "kernel half" copy loop was `0..512`, which copied **the entire kernel tree into
    /// the user root**.
    ///
    /// The consequence is *not* the obvious one, and an earlier version of this comment
    /// got it wrong. EL0 would **not** have gained access to kernel memory: `encode_leaf`
    /// grants `AP[1]` only to `USER` pages and sets `UXN` on everything else, and table
    /// descriptors impose no `APTable` restriction, so EL0 data access to those pages
    /// faults and EL0 execution is blocked. The AP bits are exactly what would have caught
    /// it.
    ///
    /// The real hazard is at **EL1**: with `EPD0` cleared, low virtual addresses would
    /// translate through this tree and alias kernel memory at `kernel_VA & 0x0000_FFFF_FFFF_FFFF`.
    /// A null or small-integer pointer dereference in the kernel would then silently
    /// succeed into kernel structures instead of faulting — losing the guard-page
    /// behaviour that makes such bugs findable.
    ///
    /// Splitting the function per architecture — rather than parameterising the loops —
    /// is deliberate. The bug was a range expression that read as correct on one
    /// architecture and inverted on the other, and no amount of care with the *bounds*
    /// removes that. Two bodies, each stating what its architecture actually does.
    #[cfg(target_arch = "x86_64")]
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

        // Kernel half — shared, copied by value. `new_kernel` pre-populated every slot
        // (see `PREPOPULATE_KERNEL_ROOT`) so a kernel mapping added later is picked up
        // through the shared next-level table rather than needing a copy-back.
        for i in KERNEL_ROOT_START..ENTRIES_PER_TABLE {
            new_root.entries[i].set(kernel_root.entries[i].as_u64());
        }

        Self {
            root_phys: new_root_phys,
        }
    }

    /// Create a new user address space: an empty `TTBR0_EL1` tree.
    ///
    /// See the x86_64 sibling for why these are separate functions.
    ///
    /// **Takes no kernel address space**, and that is the point rather than an
    /// omission. There is nothing for it to do here — the kernel lives in `TTBR1_EL1`
    /// and is reached by the hardware selecting a different root, not by anything this
    /// tree contains. A `kernel: &AddressSpace` parameter that the body ignored would be
    /// an invitation to "use" it, which is precisely how the copy-everything bug this
    /// replaces would come back.
    #[cfg(target_arch = "aarch64")]
    pub fn new_user() -> Self {
        let new_root_phys =
            frame::allocate_frame().expect("new_user: failed to allocate root table frame");
        let new_root: &mut PageTable =
            // SAFETY: newly allocated frame, exclusive access via the HHDM.
            unsafe { &mut *new_root_phys.as_mut_ptr::<PageTable>() };

        // Every entry, empty. `frame::allocate_frame` does not promise zeroed memory,
        // so this is the clear that makes the tree empty, not a formality.
        for entry in new_root.entries.iter_mut() {
            entry.clear();
        }

        Self {
            root_phys: new_root_phys,
            asid: paging::allocate_asid(),
        }
    }

    /// The ASID this space's TLB entries are tagged with.
    #[cfg(target_arch = "aarch64")]
    pub fn asid(&self) -> u16 {
        self.asid
    }

    /// Install this space in `TTBR0_EL1`, making its mappings the live low half.
    ///
    /// # Safety
    ///
    /// Changes what every low virtual address translates through, for EL0 and for EL1
    /// accesses alike. The caller must be installing the space the current task should
    /// be running on, and must not hold references derived from the outgoing one.
    #[cfg(target_arch = "aarch64")]
    pub unsafe fn activate_user(&self) {
        // SAFETY: forwarded to the caller. `root_phys` is our own root, page-aligned by
        // the frame allocator, and `asid` was allocated nonzero by `new_user`.
        unsafe { paging::activate_user(self.root_phys.as_u64(), self.asid) }
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

    /// Decode the leaf descriptor's attributes for a mapped virtual address.
    ///
    /// Returns `None` if the address is not mapped by a 4 KiB leaf (absent, or covered
    /// by a block/huge mapping). Round-tripping through the architecture's
    /// `encode_leaf`/`flags_of` pair is the only way to observe permission encoding
    /// without taking a fault, which matters on aarch64 where a mis-encoded
    /// `AP[2]`/`AttrIndx` is silent rather than fatal.
    pub fn leaf_flags(&self, virt: VirtAddr) -> Option<PageFlags> {
        self.leaf_raw(virt).map(paging::flags_of)
    }

    /// The raw leaf descriptor for a mapped virtual address, undecoded.
    ///
    /// [`Self::leaf_flags`] runs the descriptor back through the architecture's own
    /// decoder, which makes it blind to a *consistent* encode/decode error. Tests that
    /// need to check a field against an independent source of truth — the live
    /// `MAIR_EL1`, say, rather than the same helper that chose the index — need the
    /// undecoded value.
    pub fn leaf_raw(&self, virt: VirtAddr) -> Option<u64> {
        let idx = [
            virt.pml4_index(),
            virt.pdp_index(),
            virt.pd_index(),
            virt.pt_index(),
        ];
        let mut table_phys = self.root_phys;
        for level in 0..4 {
            // SAFETY: the root, or an address from a present non-block descriptor;
            // either way a valid HHDM-reachable table. Read-only.
            let table: &PageTable = unsafe { &*table_phys.as_ptr::<PageTable>() };
            let entry = table.entries[idx[level]];
            if !entry.is_present() {
                return None;
            }
            if level == 3 {
                return Some(entry.as_u64());
            }
            if level > 0 && entry.is_huge() {
                return None; // block mapping — not a 4 KiB leaf
            }
            table_phys = entry.phys_addr();
        }
        None
    }

    /// Destroy this address space, freeing the table frames it owns.
    ///
    /// Frees the intermediate tables this space owns and then the root frame. Does
    /// **not** free the mapped pages themselves, nor any table shared with other
    /// address spaces.
    ///
    /// ## What "owns" means differs by architecture, and the shared bound was wrong
    ///
    /// On **x86_64** the root is shared between user and kernel, so only entries
    /// `0..KERNEL_ROOT_START` — the user half — belong to this space. The kernel half
    /// was copied by value from the kernel root and its tables are shared by every
    /// process; freeing them would pull the kernel's own page tables out from under it.
    ///
    /// On **aarch64** the whole root is this space's: it is a `TTBR0_EL1` tree with no
    /// kernel entries in it at all. [`USER_ROOT_END`] is therefore `ENTRIES_PER_TABLE`
    /// here and `KERNEL_ROOT_START` on x86.
    ///
    /// This loop previously ran `0..KERNEL_ROOT_START` on both, which is `0..0` on
    /// aarch64 — so a destroyed user space would free its root frame and leak every
    /// intermediate table beneath it.
    ///
    /// Precisely: it was **latent, not live**. No aarch64 user space had ever had anything
    /// mapped into it, so there were no intermediate tables to leak, and the frame
    /// accounting in `test_page_tables` balanced (one frame allocated, one freed). The
    /// `0..0` bound was not leaking — it was accidentally the only bound that avoided
    /// freeing the *kernel's* tables, since the old `new_user` filled the user root with
    /// copies of the kernel's L0 entries. Fixing `new_user` is what makes widening this
    /// bound safe, and widening this bound is what makes a mapped user space reclaimable.
    pub fn destroy(self) {
        // Never tear down the kernel's own tree.
        //
        // `kernel_address_space()` hands out an `AddressSpace` wrapping the kernel root,
        // and `destroy` is `pub` and takes `self` by value — so `kernel_address_space().destroy()`
        // compiles. Before 8.4 that was survivable on aarch64 by accident: the teardown
        // bound was `0..0`, so it freed only the root frame. Widening the bound to the
        // whole root (correct for user spaces) turned the same call into "free every
        // kernel page table", which would corrupt memory the walker is actively using and
        // surface as an unrelated fault much later.
        //
        // An `assert!`, not a `debug_assert!`: this is a "cannot be allowed to happen"
        // guard rather than a "should not happen" one, and the release-build behaviour of
        // a `debug_assert` here is precisely the catastrophic case. The callers that
        // legitimately hold a kernel handle use `core::mem::forget`; this catches the ones
        // that forget to.
        let kernel_root = KERNEL_ROOT_PHYS.load(Ordering::Relaxed);
        assert!(
            kernel_root == 0 || self.root_phys.as_u64() != kernel_root,
            "destroy() called on the kernel address space (root {:#x}) — this would free \
             the kernel's own page tables",
            kernel_root
        );

        // SAFETY: our root is a valid, HHDM-reachable table; we only read it to find
        // the frames to release.
        let root: &PageTable = unsafe { &*self.root_phys.as_ptr::<PageTable>() };

        for l0_idx in 0..USER_ROOT_END {
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

// --- Self-test ---

/// Virtual address used by [`selftest`] as a scratch mapping window.
///
/// Must be in the kernel half, page-aligned, and unmapped. Root index 506 on both
/// architectures: below the MMIO window (`mm::mmio`, index 509) and well clear of the
/// HHDM and the kernel image, so it collides with nothing. The test asserts the
/// address really is unmapped before touching it, so a bad choice fails loudly rather
/// than silently clobbering a live mapping.
const SELFTEST_VIRT: u64 = 0xFFFF_FD00_0000_0000;

/// Byte pattern written through the test mapping. Asymmetric and non-trivial so a
/// half-working mapping (wrong offset, stale TLB entry) does not accidentally pass.
const SELFTEST_PATTERN: u64 = 0x5445_4D45_4C49_4F53; // "TEMELIOS" in ASCII

/// Exercise the page-table implementation end to end: map, translate, read/write
/// through the mapping, verify the physical frame really changed, unmap, and confirm
/// the translation is gone.
///
/// This is the acceptance check for the MMU work. It is deliberately arch-neutral —
/// it drives only the portable [`AddressSpace`] API — so the same assertions validate
/// the x86_64 and aarch64 descriptor encodings. It runs on **both**: from the aarch64
/// boot path (`arch::aarch64::boot`) and from the amd64 suite (`test_runner`), so a
/// regression in the shared walker is caught on the architecture that has a real test
/// suite, not only on the one being ported.
///
/// On aarch64 it is the first thing that proves the ARM descriptor format, the
/// `MAIR`-derived attributes, and the TLB barrier discipline are actually right rather
/// than merely compiling.
///
/// Mapping cycles cover the encodings whose failure modes are *silent*: writable+NX
/// and read-only (the `AP[2]` inversion) on both architectures, plus uncached/Device
/// (the `MAIR`/`AttrIndx` selection the plan calls the riskiest unknown) on aarch64
/// only — see the comment on that cycle for why running it on x86_64 would trade a
/// documented memory-type alias for coverage that architecture does not need.
///
/// ## What this does not prove
///
/// Every check here is on descriptor contents, because without exception handlers
/// (7.2) a wrong permission cannot be *observed* as a fault. So this catches a wrong
/// bit position, a missing inversion, or an attribute index that does not resolve to
/// Device memory — but it cannot show that a write to a read-only page actually
/// faults, that PXN/UXN are enforced, or that Device memory behaves as Device (which
/// on aarch64 would show up as an unaligned access faulting). Those need 7.2, and the
/// acceptance claim for this sub-phase should be read with that limit in mind.
///
/// Returns `true` if every check passed. Prints a line per stage so a failure in CI
/// identifies the exact step rather than just "it hung".
///
/// # Panics
///
/// Panics if called before [`init`], or if the scratch window is already mapped.
pub fn selftest() -> bool {
    let space = kernel_address_space();
    let virt = VirtAddr::new(SELFTEST_VIRT);

    // The window must start out unmapped, or the rest of the test proves nothing.
    assert!(
        space.translate(virt).is_none(),
        "paging selftest: scratch window {:#x} is already mapped",
        SELFTEST_VIRT
    );

    let phys = match frame::allocate_frame() {
        Some(f) => f,
        None => {
            println!("[selftest] paging: FAIL — could not allocate a test frame");
            core::mem::forget(space);
            return false;
        }
    };

    let mut ok = true;
    let mut check = |cond: bool, what: &str| {
        if !cond {
            println!("[selftest] paging: FAIL at {}", what);
            ok = false;
        }
    };

    // 1. Map it writable, non-executable.
    space.map_page(
        virt,
        phys,
        PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::NO_EXECUTE,
    );

    // 2. The walker must agree with what we just installed.
    check(
        space.translate(virt) == Some(phys),
        "translate-after-map (walker disagrees with the descriptor it wrote)",
    );

    // 3. Write through the new mapping. On aarch64 this is where a missing Access
    //    Flag, a botched AP[2] inversion, or a wrong MAIR index shows up — as a fault
    //    or as a write that never lands.
    // SAFETY: `virt` is a page we just mapped writable; nothing else refers to it.
    unsafe {
        core::ptr::write_volatile(virt.as_mut_ptr::<u64>(), SELFTEST_PATTERN);
    }

    // 4. Read it back through the same mapping.
    // SAFETY: same freshly mapped, writable page.
    let readback = unsafe { core::ptr::read_volatile(virt.as_ptr::<u64>()) };
    check(
        readback == SELFTEST_PATTERN,
        "read-back through the mapping",
    );

    // 5. Read the *physical* frame through the HHDM. This is the check that the
    //    mapping points where we think it does: step 4 alone would still pass if the
    //    write had gone to some other frame entirely.
    // SAFETY: HHDM covers all physical RAM; `phys` is a frame we own.
    let via_hhdm = unsafe { core::ptr::read_volatile(phys.as_ptr::<u64>()) };
    check(
        via_hhdm == SELFTEST_PATTERN,
        "physical frame contents via HHDM (mapping points at the wrong frame)",
    );

    // 6. Unmap, and confirm we get the same frame back.
    let returned = space.unmap_page(virt);
    check(returned == Some(phys), "unmap returned the wrong frame");

    // 7. The translation must be gone.
    check(
        space.translate(virt).is_none(),
        "translate-after-unmap (stale entry left behind)",
    );

    // 8. Read-only mapping. The write permission is the one encoding that fails
    //    *silently* on aarch64 if botched: `AP[2]` is read-only-when-set, so an
    //    omitted inversion yields a writable page where read-only was requested — a
    //    protection hole, not a fault. Cycle 1 only ever asked for WRITABLE, so a
    //    stubbed-out inversion would have passed every assertion above. Assert on the
    //    decoded descriptor, since without exception handlers (7.2) we cannot take the
    //    fault that would otherwise prove it.
    space.map_page(virt, phys, PageFlags::PRESENT | PageFlags::NO_EXECUTE);
    match space.leaf_flags(virt) {
        Some(f) => check(
            !f.contains(PageFlags::WRITABLE),
            "read-only mapping decoded as writable (permission encoding inverted)",
        ),
        None => check(false, "read-only mapping produced no leaf descriptor"),
    }
    space.unmap_page(virt);

    // 9. Uncached / Device mapping — **aarch64 only**, deliberately.
    //
    //    This is the path the plan calls the riskiest unknown in the sub-phase:
    //    "MAIR/AttrIndx cacheability (wrong attrs = silent corruption or DMA-only
    //    faults later)". On aarch64 the attribute is selected indirectly through
    //    MAIR_EL1, nothing else exercises it (`mm::mmio` has no ARM callers yet), and
    //    a wrong index is silent — so the cycle earns its place.
    //
    //    The mismatched-attribute alias this creates is **not** an x86-only problem,
    //    and it would be dishonest to imply otherwise. Every RAM frame is already
    //    mapped write-back through the bootloader's HHDM, so mapping one uncached
    //    gives the same physical page two mappings with different memory types on
    //    either architecture — undefined per Intel SDM 11.12.4, and CONSTRAINED
    //    UNPREDICTABLE per the ARM ARM's "Mismatched memory attributes", which is if
    //    anything the stricter rule. There is no way to avoid it (the HHDM covers all
    //    RAM) short of cache maintenance.
    //
    //    The reason to run it on aarch64 anyway is cost/benefit, not safety: this is
    //    the only coverage of the MAIR/AttrIndx selection the plan names the riskiest
    //    unknown in the sub-phase, which is worth a UP-benign alias. On x86_64 there
    //    is no such payoff — CACHE_DISABLE is one directly encoded bit (PCD), and the
    //    PCD path is exercised end-to-end by `mm::mmio` for VirtIO — so the alias
    //    would buy nothing.
    //
    //    To keep the alias as narrow as possible we only *inspect* the descriptor and
    //    never access memory through the uncached mapping: the checks below are what
    //    actually prove the attribute selection, and reading through the mapping would
    //    add a genuinely undefined access without adding evidence.
    #[cfg(target_arch = "aarch64")]
    {
        space.map_page(
            virt,
            phys,
            PageFlags::PRESENT
                | PageFlags::WRITABLE
                | PageFlags::CACHE_DISABLE
                | PageFlags::NO_EXECUTE,
        );
        match space.leaf_flags(virt) {
            Some(f) => check(
                f.contains(PageFlags::CACHE_DISABLE),
                "uncached mapping did not decode as CACHE_DISABLE (wrong memory attribute)",
            ),
            None => check(false, "uncached mapping produced no leaf descriptor"),
        }
        // The check above only proves the attribute field round-trips through our own
        // encoder and decoder — a consistently wrong index would satisfy it. Verify
        // the selected attribute against an independent source of truth: the
        // descriptor's raw AttrIndx, resolved through the live MAIR_EL1 register.
        match space.leaf_raw(virt) {
            Some(raw) => {
                let attr_idx = (raw >> 2) & 0x7;
                let mair = crate::arch::aarch64::paging::read_mair();
                let attr = (mair >> (attr_idx * 8)) & 0xff;
                check(
                    attr == 0x00,
                    "uncached mapping selected a MAIR attribute that is not \
                     Device-nGnRnE (device memory would be cacheable)",
                );
            }
            None => check(false, "uncached mapping produced no raw leaf descriptor"),
        }
        space.unmap_page(virt);
    }

    check(
        space.translate(virt).is_none(),
        "translate-after-final-unmap (stale entry left behind)",
    );

    frame::deallocate_frame(phys);

    // Match the surrounding convention of not letting a borrowed kernel-root handle
    // escape into a teardown path. (`AddressSpace` has no `Drop` today — teardown is
    // the explicit, consuming `destroy()` — so this is intent, not a load-bearing
    // guard.)
    core::mem::forget(space);

    if ok {
        println!(
            "[selftest] paging: PASS (map/translate/rw/unmap at {:#x} -> {:#x})",
            SELFTEST_VIRT,
            phys.as_u64()
        );
    }
    ok
}

// --- User address space self-test (Phase 8.4, aarch64) ---

/// Two user virtual addresses used by [`user_selftest`], far enough apart that the walk
/// must allocate distinct L2 and L3 tables for each rather than reusing one leaf table.
///
/// Precisely: `A = 2^28` is L0 0 / L1 0, `B = 2^38` is L0 0 / L1 256. They share the root
/// *and the L1 table*, diverging at the L1 entry — so "different L1 tables", which an
/// earlier version of this comment claimed, is wrong. Distinct L2/L3 tables is what the
/// test needs and what it gets.
#[cfg(target_arch = "aarch64")]
const USER_VA_A: u64 = 0x0000_0000_1000_0000;
#[cfg(target_arch = "aarch64")]
const USER_VA_B: u64 = 0x0000_0040_0000_0000;

/// Sentinels written through the user mappings. Distinct values so a stale read is
/// identifiable as *which* frame it came from rather than merely "wrong".
#[cfg(target_arch = "aarch64")]
const USER_PATTERN_1: u64 = 0x4142_4344_4546_4748;
#[cfg(target_arch = "aarch64")]
const USER_PATTERN_2: u64 = 0x5152_5354_5556_5758;
/// Third sentinel — the recycled-ASID space's frame.
#[cfg(target_arch = "aarch64")]
const USER_PATTERN_3: u64 = 0x6162_6364_6566_6768;

/// Prove that an aarch64 user address space translates, and that ASID reuse invalidates.
///
/// ## What it establishes, in order
///
/// 1. **A `TTBR0_EL1` tree translates at all.** Build a user space, map one frame at two
///    widely separated user VAs, install it, and write a sentinel through VA `A`.
/// 2. **Three views, one frame.** Read the sentinel back through VA `B` and through the
///    HHDM. All three must agree — which is only possible if both user VAs really walk
///    to the frame the mapping named, rather than one of them faulting into a
///    coincidentally-nonzero page or the write landing somewhere else entirely.
/// 3. **A second space displaces the first.** Install a different space with a
///    *different* frame at VA `A`, and the read must change. Without this, step 2 would
///    pass just as well if `TTBR0_EL1` were being ignored and some stale mapping were
///    answering.
/// 4. **ASID reuse is reached.** The test forces a rollover — allocating spaces until
///    the counter wraps onto the first space's tag — and repeats the displacement with a
///    recycled ASID.
///
/// ## Step 4 exercises the recycling path; it does NOT verify the invalidation
///
/// Say this plainly, because the temptation to claim otherwise is exactly how this
/// project's false claims get written.
///
/// 4. **The `TLBI ASIDE1IS` is present and does work.** Steps 1-3 pass with or without
///    it, because the two spaces have distinct ASIDs and the second simply misses. So the
///    test forces a *rollover* — allocating spaces until the counter wraps back onto the
///    first space's tag — and repeats the displacement under a recycled ASID. Deleting
///    the TLBI fails this arm:
///
///    ```text
///    user-as: FAIL — recycled ASID 1 read 0x4142434445464748,
///      expected 0x6162636465666768
///      (this is space one's frame: TLBI ASIDE1IS did not invalidate)
///    ```
///
/// ## What step 4 does not establish — measured, not assumed
///
/// It does **not** show the ASID scheme is correct. That also requires user leaves to
/// carry `nG`: a global entry matches every ASID and is immune to ASID-tagged
/// invalidation, so global user pages would defeat the whole mechanism. 8.4a shipped
/// exactly that bug, and **clearing `nG` again leaves this test passing** — measured.
///
/// The reason is QEMU: it implements `TLBI ASIDE1{IS}` as a full flush of the EL1&0
/// regime, ignoring both the ASID operand and the global bit. A stale global entry is
/// therefore invalidated just as a tagged one would be, and no guest-visible experiment
/// can tell the two apart. The `nG` bit rests on the architecture specification; on
/// silicon, omitting it makes step 3 return the previous space's frame. **Hardware-phase
/// check.**
///
/// Two earlier claims here were wrong and are corrected rather than deleted, because the
/// error is instructive. This comment previously said the TLBI mutation could not be made
/// to fail, and blamed TCG for flushing on `TTBR0_EL1` writes. TCG flushes on that write
/// only when the ASID *field changes*, which in the decisive step it deliberately does
/// not; what actually masked the TLBI was this kernel's own redundant `TCR_EL1` write on
/// every switch, which QEMU turns into an unconditional flush. Making that write
/// conditional — correct in its own right — restored the falsification. The lesson is
/// narrower than "emulation hides things": *this* emulator hid it because *this* code did
/// something unnecessary.
///
/// ## Why this runs at EL1
///
/// The mappings carry `USER` (`AP[1]`), which permits EL0 access; it does not *deny*
/// EL1 access. Privileged Access Never would, but PAN is Armv8.1 and the CPU this
/// targets (`cortex-a72`) is Armv8.0, so EL1 loads and stores to user pages are
/// permitted. The `PXN` that `encode_leaf` sets on user pages blocks EL1 *execution*,
/// not data access. When PAN is present this test has to move to EL0 — noted here
/// because the reason it works today is a property of the CPU, not of the design.
#[cfg(target_arch = "aarch64")]
pub fn user_selftest() -> bool {
    // The whole test runs inside a helper so that **every** exit path — including the
    // allocation-failure returns — passes through the same cleanup below.
    //
    // The first version returned early on three allocation failures without re-parking
    // `TTBR0_EL1`, leaving a live user space installed with `EPD0` clear for the rest of
    // boot; and on the success path it destroyed the address spaces *before* re-parking,
    // so the walker briefly pointed at freed roots. Both were benign only because nothing
    // in those windows touches a low virtual address. The comment claimed the low half was
    // parked on the way out, which was true on exactly one of the four paths.
    let outcome = user_selftest_inner();

    // Re-park the low half FIRST — before any teardown — so the walker is never pointed
    // at a root that is about to be freed.
    //
    // SAFETY: reactivating the kernel root restores `TTBR0_EL1 = 0` and `TCR_EL1.EPD0`,
    // which is the state every non-EL0 path expects. The kernel root is unchanged and
    // still maps all executing code, the stack and the HHDM.
    unsafe { paging::activate(KERNEL_ROOT_PHYS.load(Ordering::Relaxed)) };

    for space in outcome.spaces {
        if let Some(space) = space {
            space.destroy();
        }
    }
    for f in outcome.frames.into_iter().flatten() {
        frame::deallocate_frame(f);
    }

    outcome.ok
}

/// What [`user_selftest`] produced: the verdict plus everything that must be released.
///
/// Returning the resources rather than freeing them in place is what lets every early
/// exit share one cleanup path.
#[cfg(target_arch = "aarch64")]
struct UserSelftestOutcome {
    ok: bool,
    spaces: [Option<AddressSpace>; 3],
    frames: [Option<PhysAddr>; 3],
}

#[cfg(target_arch = "aarch64")]
fn user_selftest_inner() -> UserSelftestOutcome {
    use crate::arch::paging as apg;

    let flags = PageFlags::PRESENT
        .union(PageFlags::WRITABLE)
        .union(PageFlags::USER);

    let mut out = UserSelftestOutcome {
        ok: true,
        spaces: [None, None, None],
        frames: [None, None, None],
    };

    // --- Space one: one frame, two user VAs ---
    let space1 = AddressSpace::new_user();
    let asid1 = space1.asid();
    out.spaces[0] = Some(space1);
    let frame1 = match frame::allocate_frame() {
        Some(f) => f,
        None => {
            println!("[selftest] user-as: FAIL — no frame for space one");
            out.ok = false;
            return out;
        }
    };
    out.frames[0] = Some(frame1);
    let space1 = out.spaces[0].as_ref().unwrap();
    space1.map_page(VirtAddr::new(USER_VA_A), frame1, flags);
    space1.map_page(VirtAddr::new(USER_VA_B), frame1, flags);

    // SAFETY: installing a freshly built user tree. Nothing holds references derived
    // from the low half — it translated nothing until this instant.
    unsafe { space1.activate_user() };

    // SAFETY: just mapped writable, and the regime is live as of the ISB in
    // `activate_user`. The address is a user VA this test owns.
    unsafe { core::ptr::write_volatile(USER_VA_A as *mut u64, USER_PATTERN_1) };

    // View 2: the other user VA. View 3: the HHDM alias of the same frame.
    // SAFETY: both are live mappings of `frame1`.
    let via_b = unsafe { core::ptr::read_volatile(USER_VA_B as *const u64) };
    let via_hhdm = unsafe { core::ptr::read_volatile(frame1.as_ptr::<u64>()) };

    if via_b != USER_PATTERN_1 || via_hhdm != USER_PATTERN_1 {
        println!(
            "[selftest] user-as: FAIL — three views disagree: A wrote {:#x}, \
             B read {:#x}, HHDM read {:#x}",
            USER_PATTERN_1, via_b, via_hhdm
        );
        out.ok = false;
    }

    // --- Space two: a different frame at the same VA, distinct ASID ---
    let space2 = AddressSpace::new_user();
    let asid2 = space2.asid();
    out.spaces[1] = Some(space2);
    let frame2 = match frame::allocate_frame() {
        Some(f) => f,
        None => {
            println!("[selftest] user-as: FAIL — no frame for space two");
            out.ok = false;
            return out;
        }
    };
    out.frames[1] = Some(frame2);
    // SAFETY: fresh frame reachable through the HHDM.
    unsafe { core::ptr::write_volatile(frame2.as_mut_ptr::<u64>(), USER_PATTERN_2) };
    let space2 = out.spaces[1].as_ref().unwrap();
    space2.map_page(VirtAddr::new(USER_VA_A), frame2, flags);

    // SAFETY: switching the low half to a fully built tree.
    unsafe { space2.activate_user() };
    // SAFETY: live mapping of `frame2`.
    let after_switch = unsafe { core::ptr::read_volatile(USER_VA_A as *const u64) };
    if after_switch != USER_PATTERN_2 {
        println!(
            "[selftest] user-as: FAIL — after switching space, VA A read {:#x}, \
             expected {:#x} (TTBR0 switch had no effect)",
            after_switch, USER_PATTERN_2
        );
        out.ok = false;
    }

    // --- ASID reuse: the arm that actually tests the invalidation ---
    let mut recycled: Option<AddressSpace> = None;
    let mut attempts = 0u32;
    for _ in 0..(apg::ASID_ROLLOVER as u32 + 2) {
        attempts += 1;
        let candidate = AddressSpace::new_user();
        if candidate.asid() == asid1 {
            recycled = Some(candidate);
            break;
        }
        candidate.destroy();
    }

    match recycled {
        None => {
            println!(
                "[selftest] user-as: FAIL — ASID {} never recycled within {} \
                 allocations; the reuse path was not reached and the TLBI is untested",
                asid1, attempts
            );
            out.ok = false;
            return out;
        }
        Some(space3) => {
            out.spaces[2] = Some(space3);
            let frame3 = match frame::allocate_frame() {
                Some(f) => f,
                None => {
                    println!("[selftest] user-as: FAIL — no frame for the recycled space");
                    out.ok = false;
                    return out;
                }
            };
            out.frames[2] = Some(frame3);
            // SAFETY: fresh frame, HHDM-reachable.
            unsafe { core::ptr::write_volatile(frame3.as_mut_ptr::<u64>(), USER_PATTERN_3) };
            out.spaces[2]
                .as_ref()
                .unwrap()
                .map_page(VirtAddr::new(USER_VA_A), frame3, flags);

            // Re-install space one first, so its translations are genuinely cached under
            // `asid1` immediately before the recycled space claims the same tag.
            // SAFETY: space one is still intact.
            unsafe { out.spaces[0].as_ref().unwrap().activate_user() };
            // SAFETY: live mapping — this read is what populates the TLB under asid1.
            let warm = unsafe { core::ptr::read_volatile(USER_VA_A as *const u64) };
            if warm != USER_PATTERN_1 {
                println!(
                    "[selftest] user-as: FAIL — warm-up read got {:#x}, expected {:#x}; \
                     the reuse arm cannot be meaningful without a cached entry",
                    warm, USER_PATTERN_1
                );
                out.ok = false;
            }

            // SAFETY: installing the recycled-ASID space.
            unsafe { out.spaces[2].as_ref().unwrap().activate_user() };
            // SAFETY: live mapping of `frame3` — unless a stale entry answers.
            let recycled_read = unsafe { core::ptr::read_volatile(USER_VA_A as *const u64) };
            if recycled_read != USER_PATTERN_3 {
                println!(
                    "[selftest] user-as: FAIL — recycled ASID {} read {:#x}, expected \
                     {:#x}{}",
                    asid1,
                    recycled_read,
                    USER_PATTERN_3,
                    if recycled_read == USER_PATTERN_1 {
                        " (this is space one's frame: TLBI ASIDE1IS did not invalidate)"
                    } else {
                        ""
                    }
                );
                out.ok = false;
            }
        }
    }

    if out.ok {
        println!(
            "[selftest] user-as: PASS (three views agree; TTBR0 switch observed; ASID {} \
             recycled on allocation {}, TLBI proven by mutation; asid2={}). nG tagging \
             NOT covered — QEMU cannot observe it; hardware-phase check.",
            asid1, attempts, asid2
        );
    }
    out
}

// --- EL0 round-trip self-test (Phase 8.4b, aarch64) ---

/// User VA the EL0 payload's code is mapped at.
#[cfg(target_arch = "aarch64")]
const EL0_CODE_VA: u64 = 0x0000_0000_0040_0000;
/// User VA of the payload's stack page. Deliberately far from the code so a stack
/// overflow runs into a hole rather than into the text it is executing.
#[cfg(target_arch = "aarch64")]
const EL0_STACK_VA: u64 = 0x0000_0000_0080_0000;

/// The exit code the EL0 payload must produce: `ADD(40,2)` + `SUM6(1,2,4,8,16,32)`.
///
/// A single number carrying the result of two syscalls with eight arguments between them,
/// so a wrong positional accessor anywhere shifts it. The SUM6 operands are powers of two
/// precisely so two compensating errors cannot land back on the right total.
#[cfg(target_arch = "aarch64")]
const EL0_EXPECTED_EXIT: u64 = 42 + 63;

/// Drop to EL0 and run a payload that makes three syscalls. **Never returns.**
///
/// ## What it proves, and how each part can fail
///
/// Every row below is a check that exists in [`el0_verify`]. An earlier version of this
/// table listed four rows of which one was real: it advertised an assertion on the printed
/// message that nothing observed, and one on "control returns here at all", which is not
/// an assertion and whose premise is impossible — the last expression of this function has
/// type `!`. A review found it by breaking `copy_from_user` outright and watching the test
/// still report PASS. The instrument has since been built, so the table is now true.
///
/// | assertion | goes red when |
/// |---|---|
/// | exit status is `42` | the return value did not travel back to EL0 in `x0`, or `SP_EL0` was lost across a syscall |
/// | `printed_bytes` == the message length | `copy_from_user`, the user mapping, or the UTF-8 path is wrong |
/// | `svc_count` advances by 3 | the `0x400` slot is not dispatching, or `svc` never reaches EL1 |
///
/// The exit code is the *result of a syscall computed from its arguments* (`ADD(40, 2)`),
/// not a constant. A payload that merely reached `SYS_EXIT` would pass a constant check;
/// this one has to have received `42` back in `x0` from the previous syscall, **spilled it
/// to its user stack**, and reloaded it after a second syscall. The spill is what makes
/// the frame's `SP_EL0` slot load-bearing: without it, zeroing `SP_EL0` on every exception
/// return is undetectable, which a review measured.
///
/// The `svc_count` row is the weakest of the three — reaching `SYS_EXIT` with `42` already
/// implies three syscalls — and is kept because it names the failing stage directly when
/// the drop to EL0 works but dispatch does not.
///
/// ## Why it runs forever rather than returning
///
/// `SYS_EXIT` records its code and returns to EL0, where the payload spins. There is no
/// task teardown to invoke yet — that arrives with the scheduler integration in 8.5. So
/// this test drops to EL0 **on a dedicated stack it can abandon**: the timer interrupt
/// preempts the spinning payload, and [`el0_verify`] observes the recorded status from the
/// next scheduled kernel task rather than by returning through the `eret`.
///
/// ## What that costs, permanently, for the rest of the boot
///
/// Three consequences, none of them fixable without per-task `TTBR0_EL1`, and all of them
/// previously undocumented:
///
/// 1. **The low half stays live at EL1.** [`AddressSpace::activate_user`] clears
///    `TCR_EL1.EPD0` and this function never re-parks — it cannot, since the payload is
///    still executing from that tree. So from here until shutdown, low virtual addresses
///    translate at EL1 through a leaked user tree, and the guard-page behaviour that
///    `new_user`'s docs describe is gone. `user_selftest` re-parks on every exit path
///    precisely to preserve that property; this test regresses it, and a review confirmed
///    it by reading the payload's first instruction from EL1 after the test finished.
/// 2. **Seven frames, one ASID and one task leak.** The root, three intermediate tables
///    (the code and stack VAs fall in different 2 MiB regions), two data frames, and an
///    ASID out of the 63 available. `AddressSpace` has no `Drop`, and `destroy()` is never
///    called. The frame-accounting tests still pass because the leak happens once, before
///    the suite starts.
/// 3. **`el0-payload` spins at EL0 forever**, taking a round-robin share of every timer
///    slice for the life of the node.
///
/// The honest summary: this proves the round trip, the syscall results and the uaccess
/// path, and it cannot yet prove an orderly *exit* from EL0 — it trades a permanent
/// resource leak for that coverage. Naming it here beats discovering later that "the EL0
/// test passes" meant less than it sounded like.
#[cfg(target_arch = "aarch64")]
pub fn el0_selftest() -> bool {
    use crate::arch::aarch64::syscall;

    let space = AddressSpace::new_user();

    let code_frame = match frame::allocate_frame() {
        Some(f) => f,
        None => {
            println!("[selftest] el0: FAIL — no frame for code");
            return false;
        }
    };
    let stack_frame = match frame::allocate_frame() {
        Some(f) => f,
        None => {
            println!("[selftest] el0: FAIL — no frame for stack");
            frame::deallocate_frame(code_frame);
            return false;
        }
    };

    // Copy the payload into the code frame through the HHDM, before the frame is user-
    // mapped: writing it through the *user* VA would require the kernel to be running on
    // this address space already, which it is not yet.
    let payload = syscall::payload();
    assert!(
        payload.len() <= crate::mm::PAGE_SIZE as usize,
        "EL0 payload is {} bytes, larger than the single page it is copied into",
        payload.len()
    );
    // SAFETY: freshly allocated frame, reachable through the HHDM, exclusively ours.
    unsafe {
        core::ptr::copy_nonoverlapping(
            payload.as_ptr(),
            code_frame.as_mut_ptr::<u8>(),
            payload.len(),
        );
    }

    // Code: user-readable and executable — deliberately NOT writable, so a payload bug
    // that writes to its own text faults instead of self-modifying.
    space.map_page(
        VirtAddr::new(EL0_CODE_VA),
        code_frame,
        PageFlags::PRESENT.union(PageFlags::USER),
    );
    // Stack: user, writable, and non-executable.
    space.map_page(
        VirtAddr::new(EL0_STACK_VA),
        stack_frame,
        PageFlags::PRESENT
            .union(PageFlags::WRITABLE)
            .union(PageFlags::USER)
            .union(PageFlags::NO_EXECUTE),
    );

    syscall::clear_exit_status();
    let before = crate::arch::aarch64::exceptions::svc_count();

    // Register the space with the scheduler *as well as* installing it now.
    //
    // Installing it alone was enough while this was the only EL0 task in existence. It
    // stopped being enough the moment 8.4d added a second one: the scheduler restores
    // `TTBR0_EL1` only for tasks whose root it knows, so an out-of-band `activate_user`
    // left this task's tree un-restorable. The soak's first run caught it immediately —
    // this task resumed with the soak's tree installed and took an instruction abort at
    // its own code VA, which is precisely the failure per-task address spaces exist to
    // prevent, arriving from the one EL0 task that had opted out of them.
    crate::sched::set_task_user_space(
        crate::sched::current_task_id(),
        space.root_phys().as_u64(),
        space.asid(),
    );
    // Note for anyone adding a third caller: `user_selftest` above also calls
    // `activate_user` without registering, and is safe only because it runs on a task the
    // scheduler considers kernel-only and re-parks the low half on every exit path. That
    // is now a load-bearing property of that function, not an incidental one.
    //
    // SAFETY: the tree is fully built and this is the space the payload runs in.
    unsafe { space.activate_user() };

    // The stack grows down: start it at the top of the mapped page.
    let sp = EL0_STACK_VA + crate::mm::PAGE_SIZE;

    // Hand off to EL0. This does not return — the payload spins after SYS_EXIT and the
    // timer preempts it — so the verdict is collected by the task that runs next.
    // Five syscalls now: ADD, SUM6, DEBUG_PRINT, GETPC, EXIT.
    EL0_EXPECTED.store(before + 5, Ordering::Relaxed);
    // SAFETY: code and stack are mapped in the installed tree with the permissions the
    // payload needs.
    unsafe { syscall::enter_el0(EL0_CODE_VA, sp) }
}

/// `svc_count` the EL0 payload is expected to reach, published before the drop so the
/// checker can run from a different task.
#[cfg(target_arch = "aarch64")]
static EL0_EXPECTED: AtomicU64 = AtomicU64::new(0);

/// Verify what [`el0_selftest`] produced. Called from a kernel task after the EL0 task
/// has had time to run.
#[cfg(target_arch = "aarch64")]
pub fn el0_verify() -> bool {
    use crate::arch::aarch64::{exceptions, syscall};

    let count = exceptions::svc_count();
    let expected = EL0_EXPECTED.load(Ordering::Relaxed);
    let status = syscall::exit_status();

    let mut ok = true;
    if count < expected {
        println!(
            "[selftest] el0: FAIL — svc_count {} < expected {} (syscalls did not all \
             reach the lower-EL sync vector)",
            count, expected
        );
        ok = false;
    }

    // The uaccess assertion. `SYS_DEBUG_PRINT` counts the bytes it successfully copied out
    // of userspace and printed; the payload's message is the only thing that increments
    // it. Without this check, `copy_from_user` can fail on every call — or be replaced by
    // an unconditional error return — and the test still passes, because the payload
    // ignores that syscall's return value and nothing observes the console line. That was
    // measured, not hypothesised.
    // `user_pc` — the only check on that accessor anywhere. `SYS_GETPC` recorded whatever
    // `frame.user_pc()` returned; it must be an address inside the payload's own mapped
    // code page. A mutation returning `spsr` or `sp_el0` instead lands far outside it.
    let pc = syscall::last_user_pc();
    let code_lo = EL0_CODE_VA;
    let code_hi = EL0_CODE_VA + crate::mm::PAGE_SIZE;
    if pc < code_lo || pc >= code_hi {
        println!(
            "[selftest] el0: FAIL — SYS_GETPC saw user_pc {:#x}, outside the payload's \
             code page [{:#x}, {:#x}) (ELR_EL1 is not reaching user_pc())",
            pc, code_lo, code_hi
        );
        ok = false;
    }

    let printed = syscall::printed_bytes();
    let expected_bytes = syscall::msg_len();
    if printed != expected_bytes {
        println!(
            "[selftest] el0: FAIL — SYS_DEBUG_PRINT copied {} bytes, expected {} \
             (copy_from_user, the user mapping, or the UTF-8 path is broken)",
            printed, expected_bytes
        );
        ok = false;
    }
    match status {
        // 105 = ADD(40,2)=42 + SUM6(1,2,4,8,16,32)=63. One number carrying both results,
        // so a wrong accessor anywhere in either call changes it. The two operands are
        // recovered from the user stack, so SP_EL0 is load-bearing across four syscalls.
        Some(EL0_EXPECTED_EXIT) => {}
        Some(other) => {
            println!(
                "[selftest] el0: FAIL — SYS_EXIT code {}, expected {} (= ADD 42 + SUM6 63; \
                 a positional accessor, the x0 return path, or SP_EL0 is wrong)",
                other, EL0_EXPECTED_EXIT
            );
            ok = false;
        }
        None => {
            println!("[selftest] el0: FAIL — payload never reached SYS_EXIT");
            ok = false;
        }
    }
    if ok {
        println!(
            "[selftest] el0: PASS (dropped to EL0, {} syscalls dispatched via slot 8, \
             ADD returned 42 through x0 and survived a spill to SP_EL0, {} bytes copied \
             from userspace, SYS_EXIT observed)",
            // `saturating_sub` on both halves. Unreachable — a matching status implies
            // `el0_selftest` ran and stored `before + 3` — but this is a dev-profile build
            // with overflow checks on, so an underflow here would panic *inside the
            // success path of a passing test*, which is the worst place to learn about it.
            count.saturating_sub(expected.saturating_sub(5)),
            printed
        );
    }
    ok
}
