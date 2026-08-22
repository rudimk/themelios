//! # GICv2 — ARM Generic Interrupt Controller
//!
//! The aarch64 analog of the x86 PIC/APIC: the thing that decides *which* device
//! caused an IRQ. The CPU's IRQ vector says only "an interrupt happened"; the
//! controller supplies the interrupt ID and expects to be told when servicing is done.
//!
//! GICv2 (rather than v3) is a deliberate choice for bring-up: it is two MMIO register
//! blocks with no system-register interface, no redistributors, and no `ICC_*_EL1`
//! plumbing. QEMU `virt` provides it with `-machine virt,gic-version=2`. GICv3 is real
//! work for Graviton and belongs in Phase 8.
//!
//! ## Two blocks, two jobs
//!
//! - **Distributor (`GICD`)** — global. Decides which interrupts are enabled, their
//!   priorities, and which CPU they target. One per system.
//! - **CPU interface (`GICC`)** — per-core. Acknowledging (`IAR`) and completing
//!   (`EOIR`) interrupts, and the priority mask (`PMR`) below which they are ignored.
//!
//! Both must be enabled, *and* `PMR` must be permissive, before anything is delivered.
//! Forgetting `PMR` is the classic silent failure: everything looks configured and no
//! interrupt ever arrives, because the default mask of 0 blocks every priority.
//!
//! ## Interrupt ID ranges
//!
//! | INTID   | Kind | Notes |
//! |---------|------|-------|
//! | 0-15    | SGI  | software-generated, inter-processor |
//! | 16-31   | PPI  | private peripheral, per-core — **the timer lives here (27)** |
//! | 32-1019 | SPI  | shared peripheral, device interrupts |
//!
//! PPIs are *banked per core*: each core sees its own copy of the enable and priority
//! registers for 0-31. That is why enabling the timer needs no CPU targeting, and why
//! it will need doing again on every secondary core when SMP arrives.
//!
//! ## Acknowledge / EOI discipline
//!
//! Every interrupt taken must be acknowledged by reading `GICC_IAR` and completed by
//! writing the *same* value to `GICC_EOIR`. Skipping the EOI leaves the interrupt
//! active and the controller delivers nothing further — the system appears to take
//! exactly one timer tick and then hang. Writing a different value than was read is
//! equally wrong. The read value is therefore threaded straight through
//! [`dispatch_irq`] rather than recomputed.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::mm::addr::PhysAddr;
use crate::println;

/// Size of each register block.
///
/// The distributor and CPU-interface *bases* now come from `crate::platform`; only the
/// window size is still a property of the GICv2 architecture rather than the board.
const GIC_BLOCK_SIZE: usize = 0x1_0000;

// --- Distributor register offsets ---
/// Distributor control: bit 0 enables forwarding to the CPU interfaces.
const GICD_CTLR: usize = 0x000;
/// Interrupt controller type: bits 4:0 give (number of INTIDs / 32) - 1.
const GICD_TYPER: usize = 0x004;
/// Set-enable, one bit per INTID.
const GICD_ISENABLER: usize = 0x100;
/// Priority, one *byte* per INTID. Lower value = higher priority.
const GICD_IPRIORITYR: usize = 0x400;
/// CPU targets, one *byte* per INTID. Resets to 0 on GICv2 — meaning *no CPU* — so an
/// SPI left untouched here is enabled, prioritised, and never delivered.
const GICD_ITARGETSR: usize = 0x800;

// --- CPU interface register offsets ---
/// CPU interface control: bit 0 enables signalling to this core.
const GICC_CTLR: usize = 0x000;
/// Priority mask: interrupts of lower priority than this are not signalled.
const GICC_PMR: usize = 0x004;
/// Interrupt acknowledge — reading claims the highest-priority pending interrupt.
const GICC_IAR: usize = 0x00C;
/// End of interrupt — write back the value read from `IAR`.
const GICC_EOIR: usize = 0x010;

/// A spurious acknowledge. The controller returns this from `IAR` when there is
/// nothing to claim, and it must **not** be EOI'd.
const SPURIOUS_INTID: u32 = 1023;

/// Mask extracting the INTID from an `IAR` read (bits 9:0). The upper bits carry the
/// source CPU for SGIs and must be masked off before comparing against an INTID.
const IAR_INTID_MASK: u32 = 0x3ff;

/// Mapped virtual base of the distributor, or 0 before [`init`].
static GICD_VIRT: AtomicU64 = AtomicU64::new(0);
/// Mapped virtual base of the CPU interface, or 0 before [`init`].
static GICC_VIRT: AtomicU64 = AtomicU64::new(0);

/// Count of IRQs dispatched, so a self-test can prove delivery rather than infer it.
static IRQ_COUNT: AtomicUsize = AtomicUsize::new(0);
/// Count of spurious acknowledges seen (diagnostic only).
static SPURIOUS_COUNT: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn gicd_read(off: usize) -> u32 {
    let base = GICD_VIRT.load(Ordering::Relaxed);
    debug_assert!(base != 0, "GIC distributor used before init");
    // SAFETY: `base` is the mapped, uncached distributor window; `off` is a register
    // offset within it. Volatile because these reads have device side effects.
    unsafe { core::ptr::read_volatile((base as usize + off) as *const u32) }
}

#[inline]
fn gicd_write(off: usize, val: u32) {
    let base = GICD_VIRT.load(Ordering::Relaxed);
    debug_assert!(base != 0, "GIC distributor used before init");
    // SAFETY: as above; writes to device registers must not be elided or reordered.
    unsafe { core::ptr::write_volatile((base as usize + off) as *mut u32, val) }
}

#[inline]
fn gicc_read(off: usize) -> u32 {
    let base = GICC_VIRT.load(Ordering::Relaxed);
    debug_assert!(base != 0, "GIC CPU interface used before init");
    // SAFETY: mapped uncached CPU-interface window; reading IAR *claims* an interrupt,
    // so this must be volatile and must happen exactly once per dispatch.
    unsafe { core::ptr::read_volatile((base as usize + off) as *const u32) }
}

#[inline]
fn gicc_write(off: usize, val: u32) {
    let base = GICC_VIRT.load(Ordering::Relaxed);
    debug_assert!(base != 0, "GIC CPU interface used before init");
    // SAFETY: as above; writing EOIR completes an interrupt.
    unsafe { core::ptr::write_volatile((base as usize + off) as *mut u32, val) }
}

/// Map both GIC register blocks and bring the controller up.
///
/// Must run after the MMU and the frame allocator (the mapping goes through
/// [`crate::mm::mmio`], which allocates page tables) and before interrupts are
/// unmasked.
///
/// This is also the first thing on aarch64 to map real device memory through
/// `mm::mmio`, which means it is the first real exercise of the `CACHE_DISABLE` →
/// Device-`nGnRnE` `AttrIndx` path that Phase 7.1 could only check by inspecting a
/// descriptor. If that attribute selection were wrong, GIC register accesses would be
/// cached and reordered, and the symptom would be exactly the "configured correctly,
/// no interrupts ever arrive" failure described above.
pub fn init() {
    // Bases come from the platform description, not from constants beside this driver:
    // the point of `platform` is that a discovery phase can supply different ones
    // without editing the GIC code. Panics loudly on a platform whose controller is not
    // a GICv2, rather than silently mapping whatever happened to be in a const.
    let (gicd_phys, gicc_phys) = match crate::platform::info().intc {
        crate::platform::InterruptController::GicV2 { gicd, gicc } => (gicd, gicc),
        other => panic!("aarch64 GIC driver on a non-GICv2 platform: {:?}", other),
    };
    let gicd = crate::mm::mmio::map(PhysAddr::new(gicd_phys), GIC_BLOCK_SIZE);
    let gicc = crate::mm::mmio::map(PhysAddr::new(gicc_phys), GIC_BLOCK_SIZE);
    GICD_VIRT.store(gicd.as_u64(), Ordering::Relaxed);
    GICC_VIRT.store(gicc.as_u64(), Ordering::Relaxed);

    // Quiesce the distributor while reconfiguring.
    gicd_write(GICD_CTLR, 0);

    // TYPER tells us how many INTIDs this implementation supports: bits 4:0 hold
    // (lines / 32) - 1. Reported for diagnostics, and it doubles as proof the mapping
    // works — a wrong or uncached mapping typically reads back as 0 or garbage.
    let typer = gicd_read(GICD_TYPER);
    let num_intids = ((typer & 0x1f) + 1) * 32;

    // Enable the CPU interface and open the priority mask. PMR defaults to 0, which
    // blocks *everything*; without this the rest of the configuration is inert.
    gicc_write(GICC_PMR, 0xff);
    gicc_write(GICC_CTLR, 1);

    // Enable the distributor.
    gicd_write(GICD_CTLR, 1);

    println!(
        "[gic] GICv2 up: GICD@{:#x} GICC@{:#x} ({} INTIDs, TYPER={:#010x})",
        gicd.as_u64(),
        gicc.as_u64(),
        num_intids,
        typer
    );
}

/// Enable a single interrupt ID and give it a middle priority.
///
/// For PPIs (16-31) the registers are banked per core, so this affects only the
/// calling core — which is what the timer wants, and what will need repeating per core
/// when SMP arrives.
pub fn enable_intid(intid: u32) {
    // The priority/target updates below are read-modify-write. That is safe only
    // because this runs single-core with interrupts masked; it becomes a live race the
    // moment a second core or an interrupt-context caller exists.
    debug_assert!(
        !crate::arch::irq::are_enabled(),
        "enable_intid performs unsynchronised read-modify-write on GIC registers; \
         call it with interrupts masked"
    );

    // Priority is a byte per INTID. 0x80 is mid-range: below the PMR of 0xff we set in
    // `init`, so it is not masked, and leaves room either side for future prioritising.
    let prio_reg = GICD_IPRIORITYR + (intid as usize & !3);
    let shift = (intid as usize & 3) * 8;
    let mut prio = gicd_read(prio_reg);
    prio &= !(0xffu32 << shift);
    prio |= 0x80u32 << shift;
    gicd_write(prio_reg, prio);

    // Shared peripheral interrupts must additionally be *targeted* at a CPU. The
    // registers for INTIDs 0-31 are banked per core, so PPIs and SGIs need no
    // targeting — which is why the timer works without this — but ITARGETSR resets to
    // 0 for SPIs, and 0 means no CPU. Enabling an SPI without setting it produces
    // exactly the "configured correctly, nothing ever arrives" failure this module
    // warns about, and it is the first thing a device interrupt in 7.4 would hit.
    if intid >= 32 {
        let tgt_reg = GICD_ITARGETSR + (intid as usize & !3);
        let shift = (intid as usize & 3) * 8;
        let mut tgt = gicd_read(tgt_reg);
        tgt &= !(0xffu32 << shift);
        tgt |= 1u32 << shift; // CPU interface 0 — the only core for now
        gicd_write(tgt_reg, tgt);
    }

    // Set-enable: one bit per INTID, write-1-to-set (writing 0 does nothing, so this
    // needs no read-modify-write and cannot disturb neighbouring interrupts).
    let en_reg = GICD_ISENABLER + (intid as usize / 32) * 4;
    gicd_write(en_reg, 1u32 << (intid % 32));

    println!("[gic] INTID {} enabled (priority 0x80)", intid);
}

/// Interrupt ID of the PL011 UART on QEMU `virt`: **SPI 1**, so INTID 33.
///
/// SPIs are numbered from 32, and the `virt` device tree describes the UART's
/// interrupt as `<GIC_SPI 1 ...>` — the "1" is the SPI index, not the INTID. Getting
/// that off by 32 is the standard way to enable the wrong line and see no input at all.
///
/// Unlike the timer's PPI, an SPI is shared and must be *targeted* at a CPU, which
/// `enable_intid` handles.
#[inline]
pub fn uart_intid() -> u32 {
    crate::platform::info().uart.irq
}

/// Handle one hardware interrupt: claim it, route it, complete it.
///
/// Called from the IRQ vector slot. The value read from `IAR` is passed straight to
/// `EOIR` — see the acknowledge/EOI discipline in the module docs; completing with a
/// different value, or not at all, wedges the controller.
///
/// Returns `true` when the interrupt is one the caller should consider rescheduling
/// after — the scheduling timer, or serial input that has just woken the shell. The
/// decision is *reported* rather than acted on here so the caller can schedule strictly
/// after the EOI has been issued: a GICv2 CPU interface delivers nothing while an
/// interrupt is active.
pub fn dispatch_irq() -> bool {
    let iar = gicc_read(GICC_IAR);
    let intid = iar & IAR_INTID_MASK;

    // A spurious read means there was nothing to claim. It must not be EOI'd.
    if intid == SPURIOUS_INTID {
        SPURIOUS_COUNT.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    IRQ_COUNT.fetch_add(1, Ordering::Relaxed);

    let mut reschedule = false;
    if intid == uart_intid() {
        // Serial input. The shell task is woken from inside the drain, so a keystroke
        // is a scheduling event as much as a timer tick is — but the reschedule still
        // happens after the EOI below, for the same reason.
        let taken = super::serial::handle_receive_interrupt();
        reschedule = taken > 0;
    } else if intid == super::timer::timer_intid() {
        super::timer::handle_tick();
        // The timer is the preemption source: tell the caller a time slice expired.
        // Reported rather than acted on here so the scheduling happens *after* the
        // EOI below — see the caller for why that ordering matters.
        reschedule = true;
    }
    // Unrecognised INTIDs are still acknowledged and completed below: leaving one
    // active would stop all further delivery, which is a far worse failure than
    // silently ignoring an interrupt we did not ask for.

    gicc_write(GICC_EOIR, iar);
    reschedule
}

/// Number of hardware interrupts dispatched since boot.
pub fn irq_count() -> usize {
    IRQ_COUNT.load(Ordering::Relaxed)
}

/// Number of spurious acknowledges seen since boot.
pub fn spurious_count() -> usize {
    SPURIOUS_COUNT.load(Ordering::Relaxed)
}
