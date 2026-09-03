//! # aarch64 page-table descriptor format, TTBR control, and TLB maintenance
//!
//! The aarch64 counterpart to [`crate::arch::x86_64::paging`]. It presents the same
//! contract to the arch-neutral walker in [`crate::mm::page_table`] — encode a leaf,
//! encode a table, decode validity/address, activate a root, flush a translation —
//! but the hardware underneath is different in four ways that matter, and every one
//! of them is a silent-corruption trap if it is fumbled.
//!
//! ## 1. Two roots, not one
//!
//! x86_64 has a single root (CR3) covering the whole address space, conventionally
//! split at PML4 index 256. aarch64 has **two** translation bases:
//!
//! - `TTBR0_EL1` translates the *low* half (`0x0000_...`), i.e. userspace.
//! - `TTBR1_EL1` translates the *high* half (`0xffff_...`), i.e. the kernel.
//!
//! Each is the root of its own 4-level tree covering a 2^48 region, so the index of
//! the first kernel address is **0**, not 256 — the top 16 bits select the register,
//! not a table slot. Hence [`KERNEL_ROOT_START`] `= 0`.
//!
//! Phase 7 runs at EL1 only, so the kernel's address space *is* the TTBR1 tree, and
//! `TTBR0_EL1` is parked at 0 (nothing mapped, any low-half access faults). EL0 and
//! per-process TTBR0 trees are deferred with the rest of the ring-3 port.
//!
//! ## 2. Table vs. block vs. page is encoded in bit 1, level-dependently
//!
//! x86_64 uses one layout at every level. aarch64 does not:
//!
//! ```text
//! Level      bit1=1                    bit1=0
//! ---------  ------------------------  -----------------------
//! L0/L1/L2   table (points at next)    block (maps directly)
//! L3         page (maps 4 KiB)         invalid/reserved
//! ```
//!
//! So a leaf at L3 must set bit 1, while a block at L1/L2 must clear it — the exact
//! opposite sense at different levels. [`is_block`] is therefore only meaningful at
//! L0-L2, which is precisely where the walker calls it.
//!
//! ## 3. The write permission is inverted
//!
//! `AP[2]` (bit 7) is *read-only when set*. x86's `WRITABLE` is a positive bit; the
//! ARM equivalent is the **absence** of AP[2]. [`encode_leaf`] performs that
//! inversion. Getting this backwards yields writable pages where read-only was asked
//! for — a silent protection hole rather than a fault.
//!
//! ## 4. The Access Flag is mandatory
//!
//! If `AF` (bit 10) is clear, the *first* access raises an Access Flag fault rather
//! than succeeding. There is no hardware AF-setting on this configuration, so every
//! leaf we produce sets AF up front.
//!
//! ## Memory attributes: we adopt Limine's MAIR, we do not install our own
//!
//! `MAIR_EL1` is an 8-entry table of memory-attribute bytes; a descriptor selects one
//! by index (`AttrIndx`, bits 4:2). The index means nothing on its own — it is a
//! pointer into whatever MAIR happens to be loaded.
//!
//! The kernel's high-half tree is *cloned* from Limine's, and those cloned entries
//! carry Limine's `AttrIndx` values. Installing our own MAIR layout would silently
//! reinterpret the cacheability of every mapping we inherited — the HHDM could become
//! Device memory, or device MMIO could become cached — with no fault at the point of
//! the mistake and corruption showing up arbitrarily later. That is the single
//! riskiest failure mode in this sub-phase.
//!
//! So we **read** `MAIR_EL1` and locate the indices we need ([`normal_attr_index`],
//! [`device_attr_index`]) instead of writing it. Same reasoning applies to `TCR_EL1`:
//! [`verify_tcr`] asserts the geometry we assume rather than reprogramming a
//! translation regime we are actively executing on.

use core::arch::asm;
use core::sync::atomic::{AtomicU32, Ordering};

/// Number of entries in each page table level (9 index bits → 512 descriptors).
pub const ENTRIES_PER_TABLE: usize = 512;

/// Root-table index where the kernel half begins.
///
/// **Zero on aarch64**, unlike x86_64's 256. The kernel half lives in its own tree
/// rooted at `TTBR1_EL1`, so its first address (`0xffff_0000_0000_0000` with
/// T1SZ=16) has L0 index 0 — bits 47:39 of the address, which are all zero there.
/// The `0xffff` prefix selects TTBR1; it is not part of any table index.
pub const KERNEL_ROOT_START: usize = 0;

/// Human-readable names for the four levels, used by the `pgtable` debug walk.
pub const LEVEL_NAMES: [&str; 4] = ["L0", "L1", "L2", "L3"];

/// Whether every kernel-half root slot must be pre-populated with an empty
/// next-level table when the kernel address space is built.
///
/// **False on aarch64**, and the reason is the split TTBR design. On x86_64 a user
/// address space copies the kernel's root entries by value, so a kernel mapping
/// added later into a previously-empty slot would be invisible to processes created
/// before it — pre-population avoids that by sharing next-level frames by pointer.
///
/// Here the kernel tree is rooted in `TTBR1_EL1` and is never copied into a user
/// address space at all: userspace gets its own independent `TTBR0_EL1` tree. A
/// kernel mapping added at any time is therefore visible to every context that has
/// our TTBR1 loaded, which is all of them. Pre-populating would burn 512 frames
/// (2 MiB) to solve a problem this architecture does not have.
pub const PREPOPULATE_KERNEL_ROOT: bool = false;

/// `TCR_EL1.EPD0` (bit 7): disable translation-table walks through `TTBR0_EL1`.
///
/// Set by [`activate`] so a stray low address faults instead of walking a table at
/// PA 0; cleared by [`activate_user`] when the first user space is installed.
const TCR_EPD0: u64 = 1 << 7;

/// Mask selecting the output-address bits [47:12] of a descriptor.
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

// --- Raw descriptor bits ---

/// Bit 0: descriptor is valid. Clear ⇒ translation fault on access.
const DESC_VALID: u64 = 1 << 0;
/// Bit 1: "table" at L0-L2, "page" at L3. Clear at L0-L2 ⇒ block descriptor.
const DESC_TABLE: u64 = 1 << 1;
/// Bit 10: Access Flag. Clear ⇒ Access Flag fault on first touch.
const DESC_AF: u64 = 1 << 10;
/// Bits 9:8 = 0b11: Inner Shareable. Required on Normal memory for the broadcast
/// (`...IS`) TLB maintenance we issue to be coherent.
const DESC_SH_INNER: u64 = 0b11 << 8;
/// Bit 7: `AP[2]` — **read-only when set**. Writable = this bit clear.
const DESC_AP_RO: u64 = 1 << 7;
/// Bit 6: `AP[1]` — EL0 (unprivileged) access permitted when set.
const DESC_AP_EL0: u64 = 1 << 6;
/// Bit 11: not-Global. **Set on user mappings, clear on kernel mappings.**
///
/// The polarity is the trap: clear means *global*, and a global TLB entry matches
/// **every ASID**. So a user mapping left global is not merely untagged — it stays live
/// across an address-space switch and answers for whichever process runs next, and
/// `TLBI ASIDE1IS` cannot remove it, because ASID-tagged invalidation by definition
/// selects only non-global entries.
///
/// 8.4 shipped ASID allocation, `TTBR0_EL1` switching and an `ASIDE1IS` invalidation
/// while `encode_leaf` still produced global user pages — making the whole scheme inert
/// on hardware. QEMU hid it: TCG's softmmu TLB is neither ASID- nor global-aware, so the
/// self-test passed. Review caught it against this file's own Phase 7 note, which had
/// already written down that the ASID bits were "benign only because every mapping we
/// produce is global … That stops being true the moment EL0 mappings with `nG = 1` land."
const DESC_NG: u64 = 1 << 11;

/// Bit 53: Privileged eXecute Never (EL1).
const DESC_PXN: u64 = 1 << 53;
/// Bit 54: Unprivileged eXecute Never (EL0).
const DESC_UXN: u64 = 1 << 54;

// --- PageFlags ---

/// Bitflags for page mappings, in the *same semantic vocabulary* as the x86_64
/// implementation so that arch-neutral callers (`mm::mmio`, the walker, and later the
/// ring-3 paths) compile unchanged on both architectures.
///
/// These are **not** hardware bits on aarch64. They are an intent description that
/// [`encode_leaf`] translates into an ARM descriptor — including inverting
/// `WRITABLE` into `AP[2]` and turning `CACHE_DISABLE` into a Device `AttrIndx`.
/// The internal representation is deliberately kept identical to the x86 flag values
/// so that shared constants and `Debug` output read the same across arches.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PageFlags(u64);

impl PageFlags {
    /// Mapping is valid. Maps to `DESC_VALID` (+ the level-appropriate bit 1).
    pub const PRESENT: Self = Self(1 << 0);

    /// Mapping is writable. Encoded as the **absence** of `AP[2]`.
    pub const WRITABLE: Self = Self(1 << 1);

    /// Mapping is reachable from EL0 (userspace). Encoded as `AP[1]`.
    pub const USER: Self = Self(1 << 2);

    /// Write-through caching. aarch64 expresses cacheability through MAIR rather
    /// than per-descriptor bits; we have no distinct write-through attribute in
    /// Limine's MAIR, so this is accepted and treated as Normal memory. Present for
    /// source compatibility with the x86 flag set.
    pub const WRITE_THROUGH: Self = Self(1 << 3);

    /// Uncached — for MMIO. Selects the Device-`nGnRnE` `AttrIndx` from MAIR.
    pub const CACHE_DISABLE: Self = Self(1 << 4);

    /// Accessed. On x86 the CPU sets this; on aarch64 the Access Flag is set
    /// unconditionally by [`encode_leaf`], so this flag carries no extra meaning.
    pub const ACCESSED: Self = Self(1 << 5);

    /// Dirty. No equivalent in our configuration (hardware DBM is not enabled);
    /// accepted for source compatibility.
    pub const DIRTY: Self = Self(1 << 6);

    /// Block/huge mapping. We never create these; the walk detects Limine's.
    pub const HUGE_PAGE: Self = Self(1 << 7);

    /// Global mapping — valid in every address space.
    ///
    /// aarch64 expresses the opposite (`nG`, not-global, bit 11), and globality is the
    /// *default*: a descriptor with `nG` clear is global. So this flag still needs no
    /// encoding — but the reason is no longer "we never set `nG`", which stopped being
    /// true when user mappings landed. Globality is now decided by [`PageFlags::USER`]:
    /// user pages are non-global, everything else is global.
    pub const GLOBAL: Self = Self(1 << 8);

    /// Execution from this mapping faults. Encoded as `PXN | UXN`.
    pub const NO_EXECUTE: Self = Self(1 << 63);

    /// Empty flags (no bits set).
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Raw bit value of these flags (the portable representation, not ARM bits).
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

// --- System register access ---

/// Read `MAIR_EL1` (the memory-attribute indirection table).
#[inline]
pub fn read_mair() -> u64 {
    let v: u64;
    // SAFETY: reading MAIR_EL1 has no side effects.
    unsafe { asm!("mrs {}, MAIR_EL1", out(reg) v, options(nomem, nostack)) };
    v
}

/// Read `TCR_EL1` (translation control: address-space sizes and granules).
#[inline]
pub fn read_tcr() -> u64 {
    let v: u64;
    // SAFETY: reading TCR_EL1 has no side effects.
    unsafe { asm!("mrs {}, TCR_EL1", out(reg) v, options(nomem, nostack)) };
    v
}

/// Read `TTBR1_EL1` (kernel-half translation base).
#[inline]
pub fn read_ttbr1() -> u64 {
    let v: u64;
    // SAFETY: reading TTBR1_EL1 has no side effects.
    unsafe { asm!("mrs {}, TTBR1_EL1", out(reg) v, options(nomem, nostack)) };
    v
}

// --- MAIR attribute discovery ---
//
// A MAIR byte of 0x00 is Device-nGnRnE (strongly ordered, no gathering/reordering/
// early-ack) — what MMIO needs. Normal Inner/Outer Write-Back Read/Write-Allocate is
// 0xFF. Limine sets up both; we locate them rather than assuming a layout.

/// Index of a Device-`nGnRnE` attribute (MAIR byte `0x00`) in the live `MAIR_EL1`.
///
/// # Panics
///
/// Panics if no such attribute exists. There is deliberately **no fallback**. An
/// earlier version returned a "conventional" index 0, which is precisely backwards for
/// the register Limine actually hands us: on the observed `MAIR_EL1 = 0x…00ff`, index
/// 0 is Normal write-back cacheable. Falling back to it would map device MMIO as
/// cacheable — speculative reads of device registers, coalesced and reordered writes,
/// and no fault anywhere near the mistake. Guessing is strictly worse than refusing to
/// proceed, and this module already made that choice for [`verify_tcr`].
pub fn device_attr_index() -> u64 {
    let mair = read_mair();
    for i in 0..8u64 {
        if (mair >> (i * 8)) & 0xff == 0x00 {
            return i;
        }
    }
    panic!(
        "aarch64 paging: MAIR_EL1 = {:#018x} has no Device-nGnRnE (0x00) attribute; \
         refusing to guess an index for device memory",
        mair
    );
}

/// Index of a Normal write-back cacheable attribute (MAIR byte `0xFF`) in the live
/// `MAIR_EL1`.
///
/// # Panics
///
/// Panics if no such attribute exists — same reasoning as [`device_attr_index`], with
/// the error inverted: a wrong index here maps ordinary RAM as Device memory, where
/// unaligned access faults and instruction fetch is CONSTRAINED UNPREDICTABLE.
pub fn normal_attr_index() -> u64 {
    let mair = read_mair();
    for i in 0..8u64 {
        if (mair >> (i * 8)) & 0xff == 0xff {
            return i;
        }
    }
    panic!(
        "aarch64 paging: MAIR_EL1 = {:#018x} has no Normal write-back (0xFF) attribute; \
         refusing to guess an index for normal memory",
        mair
    );
}

/// Verify that the live `TCR_EL1` matches the translation geometry this module
/// assumes: 48-bit kernel VAs (`T1SZ = 16`) on a 4 KiB granule (`TG1 = 0b10`).
///
/// We *verify* rather than program TCR because the kernel is already executing on
/// tables built for the current setting — rewriting the geometry underneath a live
/// translation regime would fault immediately and undiagnosably. If Limine ever hands
/// off a different geometry, failing loudly here is far better than mis-walking
/// tables and corrupting memory.
///
/// Returns `(t1sz, tg1)` on success.
///
/// # Panics
///
/// Panics if T1SZ is not 16 or the TTBR1 granule is not 4 KiB.
pub fn verify_tcr() -> (u64, u64) {
    let tcr = read_tcr();
    let t1sz = (tcr >> 16) & 0x3f;
    // TG1 (bits 31:30) uses a different encoding from TG0: 0b10 = 4 KiB.
    let tg1 = (tcr >> 30) & 0x3;

    // T0SZ (bits 5:0) and TG0 (bits 15:14) describe the *user* regime. Phase 7 had no
    // reason to look at them; 8.4 does, for two reasons that are easy to conflate:
    //
    //  1. `user_va_bits()` derives the `copy_from_user`/`copy_to_user` bound from T0SZ
    //     rather than hard-coding 48 bits. A hard-coded bound that disagreed with the
    //     hardware would be wrong in one of two directions — too small merely rejects
    //     valid pointers, but too large lets a user pointer past the end of the regime
    //     through the check and into a walk.
    //  2. TG0 must be the 4 KiB granule, or the L0/L1/L2/L3 index arithmetic the shared
    //     walker performs does not describe this regime at all.
    //
    // Note TG0 and TG1 use *different* encodings for the same granule — TG0: 0b00 =
    // 4 KiB, TG1: 0b10 = 4 KiB — which is exactly the sort of asymmetry that gets
    // copy-pasted wrong, so both are checked explicitly against their own encoding.
    let t0sz = tcr & 0x3f;
    let tg0 = (tcr >> 14) & 0x3;
    assert!(
        t0sz == 16,
        "aarch64 paging: TCR_EL1.T0SZ = {}, expected 16 (48-bit user VA)",
        t0sz
    );
    assert!(
        tg0 == 0b00,
        "aarch64 paging: TCR_EL1.TG0 = {:#b}, expected 0b00 (4 KiB granule)",
        tg0
    );
    assert!(
        t1sz == 16,
        "aarch64 paging: TCR_EL1.T1SZ = {}, expected 16 (48-bit kernel VA)",
        t1sz
    );
    assert!(
        tg1 == 0b10,
        "aarch64 paging: TCR_EL1.TG1 = {:#b}, expected 0b10 (4 KiB granule)",
        tg1
    );

    // The walk attributes for the TTBR1 regime. Not cosmetic: the entire map/unmap
    // contract here is `DSB ISHST` → `TLBI` with **no cache maintenance**. That is
    // sufficient because the only observer that matters today is this PE's own
    // table-walk unit, which sits inside the local Inner Shareable domain, so a
    // `DSB ISH` orders our descriptor stores against it whatever SH1 says — provided
    // the walk is *cacheable*, so the walker reads through the same caches we wrote
    // into. Against a non-cacheable walk our stores would sit in the D-cache while
    // the walker read stale RAM, and every mapping would need a `DC CVAC` first.
    // RGN encoding: 0b00 Non-cacheable, 0b01 WB RA/WA, 0b10 WT RA/nWA, 0b11 WB RA/nWA.
    // Three of the four are cacheable, so the check is "not Non-cacheable" rather than
    // equality with the one value Limine happens to use. Asserting a single encoding
    // would panic the kernel at boot on perfectly good firmware — the same mistake
    // that an over-tight SH1 check made below.
    let sh1 = (tcr >> 28) & 0x3;
    let orgn1 = (tcr >> 26) & 0x3;
    let irgn1 = (tcr >> 24) & 0x3;
    assert!(
        irgn1 != 0b00 && orgn1 != 0b00,
        "aarch64 paging: TCR_EL1 table walks are not cacheable \
         (IRGN1={:#b}, ORGN1={:#b}); the DSB/TLBI-only map contract would need \
         explicit cache maintenance (DC CVAC on each descriptor)",
        irgn1,
        orgn1
    );
    // SH1 encodes the shareability of the walker's own accesses:
    //   0b00 Non-shareable   0b01 reserved   0b10 Outer Shareable   0b11 Inner Shareable
    //
    // What must be rejected is **Non-shareable**, where the walker's accesses are not
    // coherent with other observers at all. Either shareable setting is acceptable:
    // the Outer Shareable domain is a superset of the Inner Shareable one, so Outer is
    // at least as coherent as Inner, not less. Limine hands off Outer Shareable
    // (0b10) on QEMU `virt`, which is why this is not an equality check.
    //
    // Caveats for when SMP arrives — all three, not just the barrier:
    //   1. `DSB ISH` would have to become `DSB OSH` for observers outside the inner
    //      domain.
    //   2. `TLBI ...IS` broadcasts to the inner-shareable domain only; outer-shareable
    //      TLBI variants need FEAT_TLBIOS (ARMv8.4).
    //   3. Our own leaves hardcode Inner Shareable (`DESC_SH_INNER`), so a frame
    //      reachable both through a leaf we encode and through an Outer Shareable
    //      table walk would be a shareability-mismatched alias. No such alias exists
    //      today only because table frames are reached solely through Limine's HHDM
    //      leaves — an accident of layout, not an invariant we enforce.
    // The kernel is UP for bring-up and secondary CPUs are deferred, so the
    // inner-shareable barriers are sufficient today.
    assert!(
        sh1 == 0b10 || sh1 == 0b11,
        "aarch64 paging: TCR_EL1.SH1 = {:#b} (Non-shareable or reserved); table walks \
         must be shareable for broadcast TLBI to be coherent with the walker",
        sh1
    );

    (t1sz, tg1)
}

/// Size of the user (`TTBR0_EL1`) virtual-address regime, in bits, read from
/// `TCR_EL1.T0SZ`.
///
/// The regime spans `[0, 2^user_va_bits)`; anything at or above that translates through
/// nothing and is the bound `copy_from_user`/`copy_to_user` reject against.
///
/// **Derived, not assumed.** Hard-coding 48 would be right on every machine this kernel
/// currently boots and wrong on one configured with a larger T0SZ — and wrong in the
/// dangerous direction, since a bound *above* the real top of the regime lets a user
/// pointer past the check. [`verify_tcr`] asserts the value it is derived from, so the
/// two cannot drift apart silently.
#[allow(dead_code)] // consumed by copy_from_user/copy_to_user in 8.4b
pub fn user_va_bits() -> u32 {
    let t0sz = read_tcr() & 0x3f;
    64 - t0sz as u32
}

// --- Descriptor encode / decode ---

/// Encode an L3 leaf descriptor mapping `phys` with `flags`.
///
/// Fixed bits: `VALID | DESC_TABLE` (bit 1 means *page* at L3) and `AF` (else the
/// first access faults). Then, from the portable flags:
///
/// - `WRITABLE` **absent** ⇒ set `AP[2]` (read-only). Note the inversion.
/// - `USER` ⇒ set `AP[1]` (EL0 permitted).
/// - `NO_EXECUTE` ⇒ set `PXN | UXN`.
/// - `CACHE_DISABLE` ⇒ Device `AttrIndx`, no shareability bits (Device memory is
///   implicitly outer-shareable). Otherwise Normal `AttrIndx` + Inner Shareable.
///
/// Kernel mappings additionally get `UXN` unconditionally: a page not marked `USER`
/// must never be executable at EL0, regardless of what the caller asked for.
pub fn encode_leaf(phys: u64, flags: PageFlags) -> u64 {
    let mut desc = (phys & ADDR_MASK) | DESC_VALID | DESC_TABLE | DESC_AF;

    // Cacheability + shareability.
    if flags.contains(PageFlags::CACHE_DISABLE) {
        desc |= device_attr_index() << 2;
        // Device memory: SH is ignored by the architecture; leave it zero.
    } else {
        desc |= normal_attr_index() << 2;
        // Normal memory must be Inner Shareable for broadcast TLBI to be coherent.
        desc |= DESC_SH_INNER;
    }

    // Access permissions. AP[2] is read-only-when-set, so writability is inverted.
    if !flags.contains(PageFlags::WRITABLE) {
        desc |= DESC_AP_RO;
    }
    if flags.contains(PageFlags::USER) {
        desc |= DESC_AP_EL0;
        // Non-global: this translation belongs to one address space, identified by the
        // ASID in TTBR0_EL1. Without this the entry matches every ASID and survives an
        // address-space switch — see `DESC_NG`. Kernel mappings deliberately stay global:
        // they are identical in every context and re-walking them on each switch is pure
        // cost.
        desc |= DESC_NG;
        // EL0-accessible ⇒ never executable at EL1. This is the SMEP analog: without
        // it a user text page is executable by the kernel, so any corrupted branch
        // target lands in attacker-controlled code at EL1. Set while the encoder is
        // being written rather than discovered during the EL0 port.
        desc |= DESC_PXN;
    } else {
        // Not a user page ⇒ never executable at EL0.
        desc |= DESC_UXN;
    }

    // Execute permission.
    if flags.contains(PageFlags::NO_EXECUTE) {
        desc |= DESC_PXN | DESC_UXN;
    }

    desc
}

/// Encode an intermediate (table-pointing) descriptor for `phys`.
///
/// Table descriptors at L0-L2 carry only `VALID | TABLE` plus the next-level address.
/// The optional table-level permission fields (`APTable`, `UXNTable`, `PXNTable`) are
/// deliberately left zero: zero means "impose no additional restriction", so the leaf
/// descriptor alone decides the effective permission. That mirrors the x86 choice of
/// permissive intermediates and keeps permission reasoning in exactly one place.
#[inline]
pub const fn encode_table(phys: u64) -> u64 {
    (phys & ADDR_MASK) | DESC_VALID | DESC_TABLE
}

/// Encode an intermediate descriptor for a table in the kernel half.
///
/// Identical to [`encode_table`] here: aarch64 table descriptors carry no `USER` bit to
/// withhold — permission comes from the leaf, and the optional `APTable`/`UXNTable`/
/// `PXNTable` restriction fields are left zero either way. The separate entry point
/// exists so the arch-neutral walker can express the distinction that x86_64 needs.
#[inline]
pub const fn encode_kernel_table(phys: u64) -> u64 {
    encode_table(phys)
}

/// Whether a raw descriptor is valid.
#[inline]
pub const fn is_valid(raw: u64) -> bool {
    raw & DESC_VALID != 0
}

/// Whether a raw descriptor at L0-L2 is a *block* (maps memory directly) rather than
/// a table pointer.
///
/// Only meaningful at L0-L2: at L3 a clear bit 1 is invalid, not a block. The walker
/// calls this only at intermediate levels, matching that constraint.
#[inline]
pub const fn is_block(raw: u64) -> bool {
    is_valid(raw) && (raw & DESC_TABLE == 0)
}

/// Extract the output physical address from a raw descriptor.
#[inline]
pub const fn addr_of(raw: u64) -> u64 {
    raw & ADDR_MASK
}

/// Decode a raw descriptor back into portable flags.
///
/// This is a best-effort inverse of [`encode_leaf`] for debugging and introspection
/// (the `pgtable` walk). The ARM encoding is not bijective with the portable flag set
/// — `WRITE_THROUGH`, `DIRTY` and `ACCESSED` have no distinct representation — so it
/// recovers the flags that carry real meaning: presence, writability, EL0 access,
/// execute-never, block-ness, and cacheability.
pub fn flags_of(raw: u64) -> PageFlags {
    let mut flags = PageFlags::empty();
    if is_valid(raw) {
        flags = flags | PageFlags::PRESENT;
    }
    // AP[2] set ⇒ read-only, so writable is its absence.
    if raw & DESC_AP_RO == 0 {
        flags = flags | PageFlags::WRITABLE;
    }
    if raw & DESC_AP_EL0 != 0 {
        flags = flags | PageFlags::USER;
    }
    if raw & (DESC_PXN | DESC_UXN) != 0 {
        flags = flags | PageFlags::NO_EXECUTE;
    }
    // A valid descriptor with bit 1 clear is a block — only at L0-L2, but callers of
    // this function are debug paths that already know the level.
    if is_valid(raw) && (raw & DESC_TABLE == 0) {
        flags = flags | PageFlags::HUGE_PAGE;
    }
    // Device AttrIndx ⇒ report as uncached.
    if (raw >> 2) & 0x7 == device_attr_index() {
        flags = flags | PageFlags::CACHE_DISABLE;
    }
    // nG (bit 11) clear means global.
    if raw & (1 << 11) == 0 {
        flags = flags | PageFlags::GLOBAL;
    }
    flags
}

// --- Address-space activation and TLB maintenance ---

/// Activate the kernel address space whose L0 root is at `root_phys`.
///
/// This is the aarch64 analog of writing CR3, with two differences worth stating:
///
/// 1. It writes **`TTBR1_EL1`** (the kernel half), not TTBR0. At EL1 with no
///    userspace, TTBR0 translates nothing, so switching it would leave the kernel
///    still running on the bootloader's tables — it would prove nothing.
/// 2. It needs explicit barriers. x86 serializes on the `mov cr3`; ARM does not.
///    We `DSB ISH` so every prior page-table store is observable, write the register,
///    invalidate all EL1 translations, then `DSB ISH` + `ISB` so the new regime is in
///    force before the next instruction is fetched.
///
/// The low half is disabled at the same time via **`TCR_EL1.EPD0`**, not merely by
/// zeroing `TTBR0_EL1`. Zeroing the register is not enough: `TTBR0_EL1 = 0` means *the
/// level-0 table sits at physical address 0*, and the walker will happily fetch a
/// descriptor from there. On QEMU `virt` that faults only because PA 0 is unbacked; on
/// a platform with ROM or RAM at PA 0 a stray EL1 null dereference would walk whatever
/// is there and could land on a valid mapping. `EPD0 = 1` disables TTBR0 walks
/// outright.
///
/// Writing `EPD0` does not violate the "never reprogram a live translation regime" rule
/// that [`verify_tcr`] enforces — it touches only the TTBR0 regime, which translates
/// nothing and which we are not executing from.
///
/// # Safety
///
/// `root_phys` must be a page-aligned L0 table that maps the currently executing
/// code, stack, and the HHDM identically to the outgoing tables. Everything the CPU
/// touches between the register write and the `ISB` must remain valid.
pub unsafe fn activate(root_phys: u64) {
    // SAFETY: the caller guarantees `root_phys` maps current code/stack/HHDM. The
    // barrier sequence is the architecturally required one for a TTBR change:
    // publish prior table stores, switch, invalidate stale translations, synchronize.
    unsafe {
        asm!(
            // Make sure every page-table store we made is visible before the switch.
            "dsb ish",
            "msr TTBR1_EL1, {root}",
            // No userspace in Phase 7: park the low half and disable its walks
            // entirely (TCR_EL1.EPD0, bit 7) so low addresses fault rather than
            // walking a table the hardware would otherwise look for at PA 0.
            "msr TTBR0_EL1, xzr",
            "mrs {tmp}, TCR_EL1",
            "orr {tmp}, {tmp}, #(1 << 7)",
            "msr TCR_EL1, {tmp}",
            "isb",
            // Drop every cached EL1 translation, broadcast to the inner-shareable
            // domain, then synchronize so the next fetch uses the new tables.
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            root = in(reg) root_phys,
            tmp = out(reg) _,
            options(nostack, preserves_flags),
        );
    }
}

/// Read the physical address of the currently active kernel root (`TTBR1_EL1`).
///
/// The low bits carry the ASID/CnP fields, so they are masked off.
#[inline]
pub fn current_root() -> u64 {
    read_ttbr1() & ADDR_MASK
}

/// Install a user address space in `TTBR0_EL1` under `asid`, and enable TTBR0 walks.
///
/// ## What the ASID buys, and the one case where it costs
///
/// TLB entries for the TTBR0 regime are *tagged* with the ASID in `TTBR0_EL1[63:48]` —
/// **provided the descriptor is non-global**. That proviso is the whole design: a global
/// entry (`nG` clear) matches every ASID, is unaffected by ASID-tagged invalidation, and
/// therefore survives an address-space switch and answers for the next process. User
/// leaves must set `nG`, and [`encode_leaf`] does; see [`DESC_NG`] for the round this
/// took to get right.
///
/// Given non-global user entries, switching to a space with a **different** ASID needs no
/// invalidation: the old entries stay cached but no longer match. That is the point of
/// tagging.
///
/// The invalidation is load-bearing in exactly one situation: **ASID reuse.** When the
/// allocator wraps and hands a recycled number to a different tree, entries cached under
/// that tag *do* match, and they describe the previous occupant's mappings. Hence
/// `TLBI ASIDE1IS` here, keyed on the ASID being installed. [`ASID_ROLLOVER`] and the
/// self-test that drives it exist to reach that case deliberately rather than waiting
/// for it to appear under load.
///
/// **This `TLBI` is unverified, and that is measured rather than suspected.** Deleting
/// it leaves `mm::page_table::user_selftest` — which forces a genuine ASID rollover —
/// passing, because QEMU's TCG flushes its softmmu TLB whenever `TTBR0_EL1` is written,
/// so nothing survives for a recycled tag to match. No guest-visible test can tell the
/// two apart on a machine that discards the state. It is kept because the architecture
/// requires it and its absence on real silicon is silent cross-address-space corruption;
/// confirming it needs hardware.
///
/// ## `EPD0`
///
/// [`activate`] parks `TTBR0_EL1` and sets `TCR_EL1.EPD0` so low addresses fault instead
/// of walking a table at PA 0. Installing a user space is where that gets undone —
/// clearing `EPD0` is what makes the low half translate at all, and forgetting it would
/// give a fully-built user tree that faults on every access.
///
/// # Safety
///
/// `root_phys` must be a page-aligned L0 table for the TTBR0 regime. This changes what
/// EL0 (and EL1 accesses to low addresses) translate through; the caller is responsible
/// for the space being the one the current task should be running on.
pub unsafe fn activate_user(root_phys: u64, asid: u16) {
    debug_assert_eq!(root_phys & 0xfff, 0, "user root must be page-aligned");
    debug_assert_ne!(asid, 0, "ASID 0 is reserved; allocate a nonzero one");

    let ttbr0 = (root_phys & ADDR_MASK) | ((asid as u64) << 48);

    // Enable TTBR0 walks — **once**, not on every switch.
    //
    // `activate` parks the low half by setting `TCR_EL1.EPD0`, and the first user space
    // installed has to clear it. Doing that unconditionally on every switch looks
    // harmless and is not: QEMU's `TCR_EL1` write handler performs an *unconditional*
    // full TLB flush, so a redundant write turns every address-space switch into a
    // complete invalidation. That masked the `TLBI ASIDE1IS` below entirely — deleting
    // the TLBI left the self-test passing, and the wrong conclusion drawn from that was
    // that the ASID path could not be falsified under emulation at all. With the write
    // made conditional, deleting the TLBI fails the test as intended.
    //
    // It is also simply correct: `EPD0` is a property of the regime, not of a particular
    // address space, so re-clearing it per switch is work with no meaning.
    //
    // SAFETY: reads `TCR_EL1`, and writes it back only to clear `EPD0` — the field
    // governing the TTBR0 regime, which translates nothing at the moment it is cleared.
    unsafe {
        let tcr: u64;
        asm!("mrs {0}, TCR_EL1", out(reg) tcr, options(nomem, nostack, preserves_flags));
        if tcr & TCR_EPD0 != 0 {
            asm!(
                "msr TCR_EL1, {0}",
                "isb",
                in(reg) tcr & !TCR_EPD0,
                options(nostack, preserves_flags),
            );
        }
    }

    // SAFETY: the architecturally required order for a TTBR0 change that may be reusing
    // an ASID. `DSB ISHST` publishes our descriptor stores before the walker can see the
    // new root; the `TLBI` then drops any translation still cached under this tag, and
    // `DSB ISH` + `ISB` complete it before the next instruction is fetched.
    //
    // The invalidation follows the install rather than preceding it. ARM's recommended
    // pattern for ASID reuse is to invalidate while the regime is parked, since between
    // these two instructions the tag is live and the CPU may speculatively populate under
    // it. Doing it properly means parking TTBR0 at a reserved root first — deferred, and
    // recorded here rather than left as an unstated assumption.
    unsafe {
        asm!(
            "dsb ishst",
            "msr TTBR0_EL1, {ttbr0}",
            "tlbi aside1is, {asid_op}",
            "dsb ish",
            "isb",
            ttbr0 = in(reg) ttbr0,
            asid_op = in(reg) (asid as u64) << 48,
            options(nostack, preserves_flags),
        );
    }
}

/// Modulus of the ASID counter. The allocator hands out **`ASID_ROLLOVER - 1` = 63**
/// distinct values (`1..=63`), recycling the first on the 64th allocation.
///
/// Deliberately far below the architectural space (256 or 65536 depending on
/// `TCR_EL1.AS`) so that **rollover is reachable in a test rather than theoretical**.
/// The recycling path is the only one where `TLBI ASIDE1IS` does any work, and a
/// wrap-around that needs 65535 address spaces to reach is a path that gets exercised
/// for the first time in production.
///
/// ASID 0 is skipped: it is conventionally reserved, and `activate_user` asserts nonzero
/// so that "I forgot to allocate one" is a panic and not a silently shared tag.
pub const ASID_ROLLOVER: u16 = 64;

/// Next ASID to hand out. Wraps at [`ASID_ROLLOVER`], skipping 0.
static NEXT_ASID: AtomicU32 = AtomicU32::new(1);

/// Allocate an ASID for a new user address space.
///
/// A monotonic counter modulo [`ASID_ROLLOVER`], skipping 0. Deliberately *not* a
/// free-list that recycles the ASID of a destroyed space: a counter reaches the reuse
/// case predictably after a known number of allocations, which is what makes the
/// rollover self-test able to reach it. A free-list would make reuse depend on
/// destruction order and turn the one path where invalidation matters into something
/// that happens rarely and unrepeatably.
///
/// Two live address spaces sharing an ASID after a wrap is *sound* precisely because
/// [`activate_user`] invalidates the tag on every install. That is the contract the two
/// halves of this design make with each other, and the reason neither may be changed
/// without the other.
pub fn allocate_asid() -> u16 {
    let n = NEXT_ASID.fetch_add(1, Ordering::Relaxed);
    // Map onto 1..ASID_ROLLOVER — never 0.
    let span = (ASID_ROLLOVER - 1) as u32;
    (1 + (n - 1) % span) as u16
}


/// Mask selecting the VA field of a `TLBI VAE1*` register operand.
///
/// The operand is **not** a bare page number. Its layout is:
///
/// ```text
/// bits 63:48  ASID
/// bits 47:44  TTL (translation-table level hint, FEAT_TTL)
/// bits 43:0   VA[55:12]
/// ```
///
/// Passing an unmasked `VA >> 12` spills the upper virtual-address bits into both the
/// TTL and ASID fields. For a kernel address like `0xFFFF_FD00_0000_0000` that yields
/// `TTL = 0b1111` — which decodes as *64 KiB granule, level 3* — while we run a 4 KiB
/// granule. When the TTL hint disagrees with the entry's actual granule/level, it is
/// CONSTRAINED UNPREDICTABLE whether the entry is invalidated at all.
///
/// The resulting failure is silent and severe: `unmap_page` clears the descriptor, the
/// TLB entry survives, the frame returns to the allocator and is handed to something
/// else, and the stale translation keeps writing to it. It does not reproduce on
/// QEMU's `cortex-a72` (ARMv8.0 — bits 47:44 are RES0 there), only on FEAT_TTL parts
/// from ARMv8.4 onward, which is exactly the Neoverse/Graviton hardware Phase 8
/// targets.
///
/// The stray ASID bits are currently benign only because every mapping we produce is
/// global (`nG` clear), and global entries are invalidated regardless of ASID. That
/// stops being true the moment EL0 mappings with `nG = 1` land.
///
/// Same masking Linux performs in `__TLBI_VADDR`.
pub(crate) const TLBI_VA_MASK: u64 = (1 << 44) - 1;

/// Invalidate the TLB entry for a single virtual address.
///
/// Broadcast to the inner-shareable domain. The surrounding barriers are the map/unmap
/// contract: `DSB ISHST` guarantees the descriptor store has landed before the
/// invalidate, and `DSB ISH` + `ISB` guarantee the invalidate has completed before
/// execution continues.
///
/// # Safety
///
/// Must be issued after the page-table store that changed this translation.
#[inline]
pub unsafe fn flush_page(virt: u64) {
    // SAFETY: TLB invalidation only drops cached translations; the next access
    // re-walks the tables. The barriers order it against the descriptor store.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vae1is, {page}",
            "dsb ish",
            "isb",
            page = in(reg) ((virt >> 12) & TLBI_VA_MASK),
            options(nostack, preserves_flags),
        );
    }
}

/// Invalidate all EL1 translations on all cores in the inner-shareable domain.
///
/// # Safety
///
/// Always architecturally safe (translations are re-walked); `unsafe` to keep TLB
/// maintenance explicit at call sites and to match the x86 signature.
#[inline]
pub unsafe fn flush_all() {
    // SAFETY: invalidating all EL1 translations forces a re-walk; the tables
    // themselves are unchanged.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );
    }
}
