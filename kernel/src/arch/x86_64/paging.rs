//! # x86_64 page-table descriptor format and TLB/CR3 primitives
//!
//! This module owns everything about paging that is *specific to x86_64*: the bit
//! layout of a page-table entry, the flag encoding, and the instructions that
//! activate an address space (`mov cr3`) or invalidate a translation (`invlpg`).
//!
//! The arch-neutral 4-level walker in [`crate::mm::page_table`] drives this through
//! the [`crate::arch::paging`] facade, so the walk itself — index extraction,
//! intermediate-table allocation, kernel/user half handling — is written once and
//! shared with aarch64.
//!
//! ## Entry layout
//!
//! Every level (PML4, PDP, PD, PT) uses the same 8-byte descriptor:
//!
//! ```text
//! Bit(s)  | Field
//! --------|------------------------------------------
//! 0       | Present (P)
//! 1       | Read/Write (R/W)
//! 2       | User/Supervisor (U/S)
//! 3       | Page-level write-through (PWT)
//! 4       | Page-level cache disable (PCD)
//! 5       | Accessed (A) — set by CPU
//! 6       | Dirty (D) — set by CPU (PT level only)
//! 7       | Page size (PS) / PAT (level-dependent)
//! 8       | Global (G) (PT level only)
//! 9-11    | Available to OS
//! 12-51   | Physical address of next table / page frame
//! 52-62   | Available to OS / reserved
//! 63      | No Execute (NX) — requires IA32_EFER.NXE
//! ```
//!
//! A pleasant property of x86_64 (which aarch64 does *not* share) is that the same
//! encoding works at every level: a "table" entry and a "leaf" entry differ only in
//! how the CPU interprets the address they hold. That is why [`encode_table`] and
//! [`encode_leaf`] here are near-identical, while their aarch64 counterparts are not.

use crate::arch::x86_64::cpu;

/// Number of entries in each page table level. x86_64 uses 9 bits per level → 512.
pub const ENTRIES_PER_TABLE: usize = 512;

/// Root-table index where the kernel half begins.
///
/// x86_64 has a *single* root table (the PML4, addressed by CR3) covering the whole
/// 48-bit virtual address space, split by convention: indices 0-255 are the user
/// half (`0x0000_0000_0000_0000`-`0x0000_7FFF_FFFF_FFFF`) and 256-511 are the kernel
/// half (`0xFFFF_8000_0000_0000`+). Entries 256-511 are shared across all address
/// spaces so kernel code is mapped no matter which process is active.
///
/// aarch64 differs fundamentally here — it has *two* root registers (TTBR0/TTBR1),
/// so its kernel root starts at index 0. The walker reads this constant rather than
/// hardcoding 256.
pub const KERNEL_ROOT_START: usize = 256;

/// Human-readable names for the four levels, used by the `pgtable` debug walk.
pub const LEVEL_NAMES: [&str; 4] = ["PML4", "PDP", "PD", "PT"];

/// Whether every kernel-half root slot must be pre-populated with an empty
/// next-level table when the kernel address space is built.
///
/// **True on x86_64.** User address spaces are created by copying the kernel's
/// root entries *by value*. If a kernel mapping is added at runtime (a device MMIO
/// window, say) into a root slot that was empty when a process was created, that
/// process would never see it — and since kernel tasks run on whichever address
/// space is currently active, the kernel could fault on its own mapping. Making
/// every kernel slot present up front means all address spaces share the same
/// next-level frames by pointer, so anything added deeper is visible everywhere.
///
/// Costs at most 256 frames (1 MiB), allocated once.
pub const PREPOPULATE_KERNEL_ROOT: bool = true;

/// Mask selecting the physical address bits (12-51) of a descriptor.
///
/// Physical addresses in page-table entries are always page-aligned, so the low 12
/// bits carry flags and the address occupies bits 12 through 51.
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

// --- PageFlags ---

/// Bitflags for page table entry attributes.
///
/// These control how the CPU treats the mapped page: whether it is present, writable,
/// reachable from ring 3, cached, executable. On x86_64 they *are* the hardware bits,
/// so `bits()` can be ORed straight into a descriptor. (On aarch64 the same semantic
/// names map onto a very different — and partly inverted — encoding.)
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PageFlags(u64);

impl PageFlags {
    /// Page is present in physical memory. If clear, any access triggers a page
    /// fault (#PF). This is the fundamental "is this entry valid" bit.
    pub const PRESENT: Self = Self(1 << 0);

    /// Page is writable. If clear, writes trigger a page fault. Note: in ring 0 the
    /// CPU ignores this unless CR0.WP is set (which it is, for real protection).
    pub const WRITABLE: Self = Self(1 << 1);

    /// Page is accessible from ring 3 (userspace). If clear, only ring 0 can touch
    /// it. This is how user processes are isolated from kernel memory.
    pub const USER: Self = Self(1 << 2);

    /// Write-through caching: writes go to cache *and* memory immediately.
    pub const WRITE_THROUGH: Self = Self(1 << 3);

    /// Disable caching entirely. Used for MMIO, where reads must reach the device
    /// rather than a stale cache line.
    pub const CACHE_DISABLE: Self = Self(1 << 4);

    /// Set by the CPU when the page is accessed (read or written).
    pub const ACCESSED: Self = Self(1 << 5);

    /// Set by the CPU when the page is written to.
    pub const DIRTY: Self = Self(1 << 6);

    /// Huge-page flag. At PD level this maps a 2 MiB page instead of pointing at a
    /// PT; at PDP level, 1 GiB. We never create these, but Limine does, so the walk
    /// must detect them.
    pub const HUGE_PAGE: Self = Self(1 << 7);

    /// Global page — the TLB entry survives a CR3 write. Requires CR4.PGE.
    pub const GLOBAL: Self = Self(1 << 8);

    /// No-execute (bit 63). Execution from this page faults. Requires EFER.NXE.
    pub const NO_EXECUTE: Self = Self(1 << 63);

    /// Empty flags (no bits set).
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Raw bit value of these flags.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Combine two sets of flags (bitwise OR).
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether all bits in `other` are set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

/// Allow `PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER`.
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

// --- Descriptor encode / decode ---

/// Encode a leaf descriptor mapping `phys` with `flags`.
///
/// PRESENT is forced on: a leaf that is not present is not a mapping. Everything
/// else the caller asked for is passed through verbatim, which is why the x86 flag
/// set doubles as the hardware encoding.
#[inline]
pub const fn encode_leaf(phys: u64, flags: PageFlags) -> u64 {
    (phys & ADDR_MASK) | flags.bits() | PageFlags::PRESENT.bits()
}

/// Encode an intermediate (table-pointing) descriptor for `phys`.
///
/// PRESENT | WRITABLE | USER on intermediates: on x86_64 the effective permission of
/// a mapping is the *AND* of every level's bits, so intermediates must be at least as
/// permissive as the most permissive leaf beneath them. The leaf entry is what
/// actually decides whether a page is user-accessible or writable.
#[inline]
pub const fn encode_table(phys: u64) -> u64 {
    (phys & ADDR_MASK)
        | PageFlags::PRESENT.bits()
        | PageFlags::WRITABLE.bits()
        | PageFlags::USER.bits()
}

/// Encode an intermediate descriptor for a table that lives in the **kernel half**.
///
/// Identical to [`encode_table`] minus `USER`. The distinction matters for the
/// kernel-half root slots that `AddressSpace::new_kernel` pre-populates: every user
/// address space copies those entries by value, so leaving U/S set on them would make
/// 256 root entries nominally ring-3-reachable. The leaves beneath are kernel-only and
/// x86 ANDs U/S across levels, so it is not an escalation by itself — but there is no
/// reason to hand out the permission, and the original code deliberately did not.
#[inline]
pub const fn encode_kernel_table(phys: u64) -> u64 {
    (phys & ADDR_MASK) | PageFlags::PRESENT.bits() | PageFlags::WRITABLE.bits()
}

/// Whether a raw descriptor is valid/present.
#[inline]
pub const fn is_valid(raw: u64) -> bool {
    raw & PageFlags::PRESENT.bits() != 0
}

/// Whether a raw descriptor at an intermediate level maps a huge page directly
/// (2 MiB at PD, 1 GiB at PDP) rather than pointing at a next-level table.
#[inline]
pub const fn is_block(raw: u64) -> bool {
    raw & PageFlags::HUGE_PAGE.bits() != 0
}

/// Extract the output physical address from a raw descriptor.
#[inline]
pub const fn addr_of(raw: u64) -> u64 {
    raw & ADDR_MASK
}

/// Extract the attribute bits from a raw descriptor.
#[inline]
pub const fn flags_of(raw: u64) -> PageFlags {
    PageFlags(raw & !ADDR_MASK)
}

// --- Address-space activation and TLB maintenance ---

/// Activate the address space whose root table is at `root_phys`.
///
/// # Safety
///
/// The root table must map the currently executing code and stack identically to the
/// outgoing tables, or the CPU triple-faults on the very next instruction fetch.
#[inline]
pub unsafe fn activate(root_phys: u64) {
    // SAFETY: forwarded to the caller — `root_phys` must be a valid PML4 that maps
    // current code/stack. Writing CR3 also flushes non-global TLB entries.
    unsafe { cpu::write_cr3(root_phys) }
}

/// Read the physical address of the currently active root table (CR3, flags masked).
#[inline]
pub fn current_root() -> u64 {
    cpu::read_cr3() & !0xFFF
}

/// Invalidate the TLB entry for a single virtual address.
///
/// # Safety
///
/// Must be called after the page-table store that changed this translation, so the
/// CPU re-walks rather than reusing a stale entry.
#[inline]
pub unsafe fn flush_page(virt: u64) {
    // SAFETY: `invlpg` only drops a cached translation; the next access re-walks.
    unsafe { cpu::invlpg(virt) }
}

/// Flush the entire non-global TLB by rewriting CR3 with its current value.
///
/// # Safety
///
/// Safe in the sense that reloading the *current* root cannot change the mapping;
/// marked `unsafe` to match the aarch64 signature and to keep TLB maintenance
/// visible at call sites.
#[inline]
pub unsafe fn flush_all() {
    // SAFETY: re-loading the currently active root is a no-op for translation and
    // flushes non-global entries, which is exactly the intent.
    unsafe {
        let root = cpu::read_cr3();
        cpu::write_cr3(root);
    }
}
