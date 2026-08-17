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


/// Switch to `SP_EL1` for ordinary kernel execution.
///
/// Limine hands off with **`SPSel = 0`**, meaning EL1 code runs on `SP_EL0` while
/// `SP_EL1` holds whatever the bootloader left there. That is not a stylistic detail —
/// it decides both *where exceptions are delivered* and *what stack they land on*:
///
/// - With `SPSel = 0`, a synchronous exception at EL1 goes to the **`0x000`** vector
///   group ("current EL with SP_EL0"), not the `0x200` group.
/// - On entry the CPU switches to `SP_EL1` regardless. If nothing has initialised it,
///   the entry stub's register save writes through a garbage pointer and takes a data
///   abort *inside the handler* — which then nests, lands in the `0x200` group, and
///   reports the nested syndrome. The original exception is never seen.
///
/// That failure is genuinely confusing from the outside: a `brk` presents as a data
/// abort at a fixed address, in the wrong vector slot, with an `SPSR` describing the
/// nested state rather than the interrupted one.
///
/// Copying the live stack pointer across and setting `SPSel = 1` makes the kernel run
/// on `SP_EL1` from here on, so exceptions are delivered to the `0x200` group with a
/// valid stack already loaded. SP is numerically unchanged, so this is invisible to
/// the surrounding Rust.
///
/// Note that handler and interrupted code then share one stack — there is no IST/TSS
/// analog on aarch64, so a kernel-stack overflow re-faults on the same stack. Accepted
/// for bring-up; a dedicated exception stack is future work.
#[inline(always)]
fn use_sp_el1() {
    // SAFETY: copies the current stack pointer into SP_EL1 and selects it, so the
    // numeric value of `sp` is unchanged across the sequence. The ISB ensures the
    // SPSel write has taken effect before the next instruction.
    unsafe {
        asm!(
            "mov x9, sp",
            "msr SPSel, #1",
            "mov sp, x9",
            "isb",
            out("x9") _,
            options(preserves_flags),
        );
    }
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
    // Use the shared lookup rather than a local copy: it panics instead of guessing.
    // A "fall back to index 0" here would map the UART *cacheable* on the MAIR Limine
    // actually provides (index 0 is Normal write-back), which is a silent failure —
    // speculative reads of device registers and coalesced writes, with no fault.
    let attr_idx = crate::arch::aarch64::paging::device_attr_index();
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
            // A valid descriptor with bit 1 clear at L0-L2 is a *block*, not a table
            // pointer. Descending into one would treat the mapped data frame as a page
            // table and write a descriptor into it — the same hazard `ensure_table`
            // guards against in the shared walker. Unreachable while the bootloader
            // leaves the device-MMIO hole unmapped, but silent if that ever changes.
            assert!(
                entry & DESC_TABLE != 0,
                "map_device_page: descriptor maps a block — cannot install a 4 KiB \
                 mapping beneath it"
            );
            table_phys = entry & ADDR_MASK;
        }
    }
    // L3: set the page entry.
    let l3_idx = ((virt >> 12) & 0x1ff) as usize;
    let l3 = (hhdm + table_phys) as *mut u64;
    core::ptr::write_volatile(l3.add(l3_idx), page_desc);

    // Publish the new entry: ensure the stores land, invalidate the stale TLB entry
    // for this VA, and synchronize.
    //
    // The TLBI operand carries VA[55:12] in bits 43:0, a TTL hint in 47:44 and an ASID
    // in 63:48, so the page number must be masked or the upper VA bits corrupt both
    // fields. See `arch::aarch64::paging::TLBI_VA_MASK` — benign on QEMU's ARMv8.0
    // `cortex-a72`, not on FEAT_TTL hardware.
    asm!(
        "dsb ishst",
        "tlbi vae1is, {va}",
        "dsb ish",
        "isb",
        va = in(reg) ((virt >> 12) & crate::arch::aarch64::paging::TLBI_VA_MASK),
        options(nostack, preserves_flags),
    );
}

/// aarch64 kernel entry (called from the arch-neutral `kmain` prologue). Diverges.
///
/// `hhdm` is Limine's higher-half direct-map offset; `k` captures the kernel image's
/// physical/virtual bases (both from Limine). Enables FP, maps + installs the UART,
/// prints a banner + a few sysregs, then idles.
pub fn kmain_aarch64(
    hhdm: u64,
    k: KernelAddr,
    entries: &[&limine::memory_map::Entry],
) -> ! {
    enable_fp();
    // Must precede anything that can take an exception — see the function docs.
    use_sp_el1();

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
    {
        let spsel: u64;
        // SAFETY: reading SPSel has no side effects.
        unsafe { asm!("mrs {}, SPSel", out(reg) spsel, options(nomem, nostack)) };
        crate::println!(
            "[boot] SPSel={} (running on SP_EL{}) — exceptions use the SP_ELx vectors",
            spsel & 1,
            spsel & 1
        );
    }
    crate::println!("[boot] PL011 mapped + serial online");
    crate::println!("[boot] Phase 7.0b boot-to-banner reached; idling.");

    // Install exception vectors before anything that can fault. Until this runs, a
    // data abort branches to whatever VBAR_EL1 held at handoff and the kernel dies
    // silently; afterwards it prints a decoded syndrome, faulting address and PC.
    // Deliberately ahead of the memory bring-up so a paging mistake is diagnosable.
    crate::arch::aarch64::exceptions::init();

    // --- Phase 7.1: memory management on our own page tables ---
    bring_up_memory(hhdm, k, entries);

    // Prove the synchronous-exception path with a real trap.
    let exc_ok = crate::arch::aarch64::exceptions::selftest();
    if exc_ok {
        crate::println!("[boot] Phase 7.2 exception vectors reached; self-test passed.");
    } else {
        crate::println!("[boot] Phase 7.2 exception vectors FAILED self-test.");
    }

    crate::println!("[boot] (GIC+timer=7.2 cont., sched=7.3, shell=7.4)");

    loop {
        crate::arch::irq::halt();
    }
}

/// Bring up the memory subsystem: frame allocator, kernel heap, and the kernel's own
/// page tables — then prove the MMU work with an end-to-end self-test.
///
/// Mirrors the x86_64 bring-up order in `kmain_x86_64`, with one deliberate omission
/// noted below.
///
/// ## Why bootloader memory is not reclaimed here
///
/// x86_64 reclaims `BOOTLOADER_RECLAIMABLE` regions after switching page tables. We do
/// not, on purpose. Our kernel root clones Limine's L0 descriptors, which point at
/// Limine's *lower-level* tables — and those live in bootloader-reclaimable memory. On
/// aarch64 this sub-phase has no exception handlers yet (7.2), so if a reclaimed table
/// frame were handed out and overwritten, the resulting translation fault would be an
/// unrecoverable silent hang rather than a diagnosable abort. Reclaiming is an
/// optimization; correctness first. Revisit once 7.2 gives us fault reporting.
fn bring_up_memory(hhdm: u64, k: KernelAddr, entries: &[&limine::memory_map::Entry]) {
    use crate::arch::aarch64::paging;

    // Physical↔virtual conversion must work before anything touches a PhysAddr.
    crate::mm::init_hhdm(hhdm);

    // Confirm the translation geometry we are about to assume. Verify rather than
    // program: we are already executing on tables built for the current TCR, so
    // rewriting it would fault instantly and undiagnosably.
    let (t1sz, tg1) = paging::verify_tcr();
    let mair = paging::read_mair();
    crate::println!(
        "[mm] TCR_EL1: T1SZ={} TG1={:#b} (48-bit kernel VA, 4 KiB granule) — verified",
        t1sz,
        tg1
    );
    crate::println!(
        "[mm] MAIR_EL1: {:#018x} (normal idx {}, device idx {}) — adopted from Limine",
        mair,
        paging::normal_attr_index(),
        paging::device_attr_index()
    );

    // Physical frame allocator, from Limine's memory map.
    crate::mm::frame::init(entries, hhdm, k.phys_base);
    let free = crate::mm::frame::free_frame_count();
    let total = crate::mm::frame::total_frame_count();
    crate::println!(
        "[mm] Frame allocator: {} free / {} total ({} MiB usable)",
        free,
        total,
        (free as u64 * crate::mm::PAGE_SIZE) / (1024 * 1024)
    );

    // The critical moment: build our own L0 tree from Limine's kernel-half mappings
    // and load it into TTBR1_EL1. If anything is missing — the running code, the
    // stack, the HHDM, or the UART page mapped in 7.0b — the CPU faults before the
    // next line prints.
    crate::mm::page_table::init();
    crate::println!("[mm] Running on kernel-owned page tables (TTBR1_EL1, TTBR0_EL1=0)");

    // Kernel heap. Not strictly required by the paging self-test, but bringing it up
    // here proves `alloc` works on aarch64 and is what the 7.4 test suite will need.
    crate::mm::heap::init();
    {
        use alloc::vec::Vec;
        let mut v: Vec<u64> = Vec::new();
        for i in 0..64 {
            v.push(i * i);
        }
        assert!(v.len() == 64 && v[63] == 63 * 63, "aarch64 heap smoke failed");
        crate::println!("[mm] Kernel heap online (Vec smoke OK)");
    }


    // Prove the descriptor encoding, MAIR attributes, and TLB discipline actually
    // work, rather than merely compiling.
    let ok = crate::mm::page_table::selftest();
    if ok {
        crate::println!("[boot] Phase 7.1 MMU/paging reached; self-test passed.");
    } else {
        crate::println!("[boot] Phase 7.1 MMU/paging FAILED self-test.");
    }
}
