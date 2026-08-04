//! # aarch64 early boot (Phase 7.0b)
//!
//! Runs after Limine's higher-half handoff — the CPU is already at **EL1 with the MMU
//! enabled, caches on, stack set, and BSS zeroed** (confirmed by the Phase 7 boot
//! spike). So this is *not* a bare-metal reset: we inherit Limine's state and do the
//! minimum to get an interactive-quality console up, then idle. The scheduler, the
//! kernel's own page tables, exceptions, and the timer land in 7.1–7.3.
//!
//! Two things must happen before the first `println!`:
//! 1. **Enable FP/SIMD.** `aarch64-unknown-none` is a hardfloat target and the
//!    compiler lowers ordinary struct-moves / formatting to SIMD, which **traps** at
//!    EL1 unless `CPACR_EL1.FPEN = 0b11` (spike-confirmed: a bare `f64` op faults with
//!    `ESR_EL1.EC = 0x07`). Limine does not guarantee it.
//! 2. **Map the PL011 UART.** Limine's HHDM maps RAM but **not** device MMIO, so the
//!    UART at physical `0x0900_0000` is unmapped after handoff (spike-confirmed: both
//!    raw-phys and `HHDM + 0x0900_0000` data-abort). We add a single Device-`nGnRnE`
//!    page to the kernel tables (`TTBR1_EL1`) pointing at the UART, then install the
//!    serial writer at that virtual address.

use core::arch::asm;

/// PL011 MMIO base on QEMU `virt`.
const PL011_PHYS: u64 = 0x0900_0000;

// --- Page-table constants (4 KiB granule, 48-bit VA, 4-level) ---
const PAGE_SIZE: u64 = 4096;
/// Mask selecting the output-address bits [47:12] of a descriptor.
const ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;
/// Table/valid descriptor bits (valid + table/page).
const DESC_VALID: u64 = 1 << 0;
const DESC_TABLE: u64 = 1 << 1; // "table" at L0–L2, "page" at L3
const DESC_AF: u64 = 1 << 10; // access flag (else access faults)
const DESC_PXN: u64 = 1 << 53; // privileged execute-never
const DESC_UXN: u64 = 1 << 54; // unprivileged execute-never

/// A 4 KiB page table (512 × 8-byte descriptors), page-aligned.
#[repr(C, align(4096))]
struct Table([u64; 512]);

/// A tiny bump pool of page-table frames in `.bss`, used to populate any missing
/// intermediate levels for the one UART mapping (avoids depending on the frame
/// allocator this early). Four frames is plenty for a single 4 KiB mapping (≤3 new
/// tables). `.bss` is zeroed by Limine, so fresh tables start empty.
struct Pool {
    tables: [Table; 4],
    next: usize,
}
static mut POOL: Pool = Pool {
    tables: [const { Table([0; 512]) }; 4],
    next: 0,
};

/// Kernel virtual→physical translation, captured from Limine's executable-address
/// response so we can compute the physical address of a `.bss` page table.
#[derive(Clone, Copy)]
pub struct KernelAddr {
    pub phys_base: u64,
    pub virt_base: u64,
}

impl KernelAddr {
    /// Physical address of a kernel virtual address in the loaded image (linear map).
    fn phys_of(&self, virt: u64) -> u64 {
        virt - self.virt_base + self.phys_base
    }
}

#[inline(always)]
fn read_sysreg_ttbr1() -> u64 {
    let v: u64;
    // SAFETY: reading TTBR1_EL1 has no side effects.
    unsafe { asm!("mrs {}, TTBR1_EL1", out(reg) v, options(nomem, nostack)) };
    v
}

#[inline(always)]
fn read_mair() -> u64 {
    let v: u64;
    // SAFETY: reading MAIR_EL1 has no side effects.
    unsafe { asm!("mrs {}, MAIR_EL1", out(reg) v, options(nomem, nostack)) };
    v
}

/// Enable FP/SIMD at EL1 (`CPACR_EL1.FPEN = 0b11`) so compiler-emitted SIMD does not
/// trap. Must run before any non-trivial Rust (formatting, struct moves).
#[inline(always)]
fn enable_fp() {
    // SAFETY: CPACR_EL1 write is a PSTATE/feature-enable with an ISB to sequence it.
    unsafe {
        asm!(
            "msr CPACR_EL1, {}",
            "isb",
            in(reg) (0b11u64 << 20),
            options(nostack),
        );
    }
}

/// Find a MAIR index whose attribute byte is `0x00` (Device-`nGnRnE`), which Limine
/// sets up. Falls back to index 0 if none is found (QEMU tolerates it for the UART).
fn device_attr_index(mair: u64) -> u64 {
    for i in 0..8u64 {
        if (mair >> (i * 8)) & 0xff == 0x00 {
            return i;
        }
    }
    0
}

/// Allocate a zeroed page table from the pool; returns its kernel virtual address.
///
/// # Safety
/// Single-threaded early boot (UP, interrupts masked), so the non-atomic bump is
/// sound; panics via `None`-less indexing if the pool is exhausted (≤3 needed).
unsafe fn alloc_table() -> u64 {
    let pool = &mut *core::ptr::addr_of_mut!(POOL);
    let idx = pool.next;
    pool.next += 1;
    let t = &mut pool.tables[idx];
    // `.bss` is already zeroed, but be explicit.
    for e in t.0.iter_mut() {
        *e = 0;
    }
    t as *mut Table as u64
}

/// Map a single 4 KiB Device page at `virt` → `phys` into the current `TTBR1_EL1`
/// tables, allocating any missing intermediate tables from the pool.
///
/// # Safety
/// Edits live page tables; caller guarantees `virt` is a TTBR1 (upper-half) address
/// not already mapped, and that `hhdm` correctly direct-maps physical RAM.
unsafe fn map_device_page(k: KernelAddr, hhdm: u64, virt: u64, phys: u64) {
    let attr_idx = device_attr_index(read_mair());
    // L3 page descriptor for Device memory: valid+page, AF, AttrIndx, XN, AP=00 (RW EL1),
    // SH=00 (device is outer-shareable implicitly), NS=0.
    let page_desc = (phys & ADDR_MASK)
        | DESC_VALID
        | DESC_TABLE
        | DESC_AF
        | (attr_idx << 2)
        | DESC_PXN
        | DESC_UXN;

    // Start at the L0 table (TTBR1). Walk L0→L1→L2, creating tables as needed.
    let mut table_phys = read_sysreg_ttbr1() & ADDR_MASK;
    for level in 0..3 {
        let shift = 39 - level * 9; // L0=39, L1=30, L2=21
        let idx = ((virt >> shift) & 0x1ff) as usize;
        let table = (hhdm + table_phys) as *mut u64;
        let entry = core::ptr::read_volatile(table.add(idx));
        if entry & DESC_VALID == 0 {
            let new_virt = alloc_table();
            let new_phys = k.phys_of(new_virt);
            core::ptr::write_volatile(table.add(idx), (new_phys & ADDR_MASK) | DESC_VALID | DESC_TABLE);
            table_phys = new_phys;
        } else {
            table_phys = entry & ADDR_MASK;
        }
    }
    // L3: set the page entry.
    let l3_idx = ((virt >> 12) & 0x1ff) as usize;
    let l3 = (hhdm + table_phys) as *mut u64;
    core::ptr::write_volatile(l3.add(l3_idx), page_desc);

    // Publish the new entry: ensure the stores land, invalidate the stale TLB entry
    // for this VA, and synchronize.
    asm!(
        "dsb ishst",
        "tlbi vae1is, {va}",
        "dsb ish",
        "isb",
        va = in(reg) (virt >> 12),
        options(nostack, preserves_flags),
    );
    let _ = PAGE_SIZE;
}

/// aarch64 kernel entry (called from the arch-neutral `kmain` prologue). Diverges.
///
/// `hhdm` is Limine's higher-half direct-map offset; `k` captures the kernel image's
/// physical/virtual bases (both from Limine). Enables FP, maps + installs the UART,
/// prints a banner + a few sysregs, then idles.
pub fn kmain_aarch64(hhdm: u64, k: KernelAddr) -> ! {
    enable_fp();

    // Map the PL011 at its HHDM address (upper-half, currently unmapped) and install
    // the serial writer there.
    let uart_va = hhdm + PL011_PHYS;
    // SAFETY: `uart_va` is a TTBR1 address Limine left unmapped (device MMIO hole);
    // hhdm direct-maps RAM incl. the page tables we walk.
    unsafe { map_device_page(k, hhdm, uart_va, PL011_PHYS) };
    crate::arch::aarch64::serial::init(uart_va as usize);

    let current_el = {
        let v: u64;
        // SAFETY: no side effects.
        unsafe { asm!("mrs {}, CurrentEL", out(reg) v, options(nomem, nostack)) };
        (v >> 2) & 0x3
    };

    crate::println!();
    crate::println!("========================================");
    crate::println!("  ThemeliOS — booting on aarch64 (EL{})", current_el);
    crate::println!("========================================");
    crate::println!("[boot] Limine higher-half handoff OK (MMU on)");
    crate::println!("[boot] HHDM offset: {:#018x}", hhdm);
    crate::println!("[boot] kernel phys/virt base: {:#x} / {:#018x}", k.phys_base, k.virt_base);
    crate::println!("[boot] FP/SIMD enabled (CPACR_EL1.FPEN)");
    crate::println!("[boot] PL011 mapped + serial online");
    crate::println!("[boot] Phase 7.0b boot-to-banner reached; idling.");
    crate::println!("[boot] (MMU/paging=7.1, exceptions+GIC+timer=7.2, sched=7.3, shell=7.4)");

    loop {
        crate::arch::irq::halt();
    }
}
