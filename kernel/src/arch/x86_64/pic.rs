//! # 8259 Programmable Interrupt Controller (PIC) Driver
//!
//! The 8259 PIC is the original IBM PC interrupt controller. Modern x86 systems
//! have an APIC (Advanced PIC) but still include 8259 emulation for compatibility.
//! We use the legacy 8259 in Phase 1 because it's simpler; LAPIC/IO-APIC comes
//! later with SMP support.
//!
//! ## Architecture
//!
//! The PC has two cascaded 8259 PICs:
//! - **Master PIC**: handles IRQ0-7, command port 0x20, data port 0x21
//! - **Slave PIC**: handles IRQ8-15, command port 0xA0, data port 0xA1
//!
//! The slave is wired to the master's IRQ2 line (cascade). When a slave IRQ
//! fires, the slave signals the master via IRQ2, and the master forwards it
//! to the CPU.
//!
//! ## IRQ to Vector Mapping
//!
//! The BIOS maps IRQs to vectors 8-15 (master) and 0x70-0x77 (slave), which
//! collides with CPU exception vectors 8-15. We remap them to vectors 32-47
//! to avoid this conflict:
//!
//! | IRQ | Device (typical)      | Vector |
//! |-----|-----------------------|--------|
//! | 0   | PIT timer             | 32     |
//! | 1   | Keyboard              | 33     |
//! | 2   | Cascade (slave PIC)   | 34     |
//! | 3   | COM2                  | 35     |
//! | 4   | COM1 (serial)         | 36     |
//! | 5   | LPT2                  | 37     |
//! | 6   | Floppy                | 38     |
//! | 7   | LPT1 / spurious       | 39     |
//! | 8   | RTC                   | 40     |
//! | 9   | ACPI / available      | 41     |
//! | 10  | Available             | 42     |
//! | 11  | Available             | 43     |
//! | 12  | PS/2 mouse            | 44     |
//! | 13  | FPU / coprocessor     | 45     |
//! | 14  | Primary ATA           | 46     |
//! | 15  | Secondary ATA / spur. | 47     |

use super::cpu;
use crate::println;

// --- PIC I/O ports ---

/// Master PIC command port. Write ICW1/OCW2/OCW3 here; read ISR/IRR.
const MASTER_COMMAND: u16 = 0x20;

/// Master PIC data port. Write ICW2-4 and the interrupt mask (OCW1) here;
/// read the current mask.
const MASTER_DATA: u16 = 0x21;

/// Slave PIC command port.
const SLAVE_COMMAND: u16 = 0xA0;

/// Slave PIC data port.
const SLAVE_DATA: u16 = 0xA1;

// --- PIC command constants ---

/// ICW1 bit 4: must be set to begin an initialization sequence.
const ICW1_INIT: u8 = 0x10;

/// ICW1 bit 0: ICW4 will be sent (required on x86 for 8086 mode).
const ICW1_ICW4: u8 = 0x01;

/// ICW4 bit 0: 8086/88 mode (as opposed to MCS-80/85 mode).
const ICW4_8086: u8 = 0x01;

/// OCW2: non-specific End Of Interrupt. Tells the PIC we've finished handling
/// the highest-priority interrupt currently in service.
const EOI: u8 = 0x20;

/// OCW3: Read the In-Service Register (ISR). Writing this to the command port
/// makes the next read from the command port return the ISR contents.
/// The ISR tells us which IRQs are currently being handled (handler running,
/// EOI not yet sent). Used to distinguish real IRQs from spurious ones.
const OCW3_READ_ISR: u8 = 0x0B;

// --- Vector mapping ---

/// Base vector for master PIC IRQs. IRQ0 maps to vector 32, IRQ1 to 33, etc.
/// Must be a multiple of 8 (hardware requirement for the 8259).
pub const MASTER_OFFSET: u8 = 32;

/// Base vector for slave PIC IRQs. IRQ8 maps to vector 40, IRQ9 to 41, etc.
pub const SLAVE_OFFSET: u8 = 40;

// --- I/O delay ---

/// Write to the POST diagnostic port (0x80) to introduce a short I/O delay.
///
/// Old 8259 chips can't handle back-to-back I/O writes at modern CPU speeds.
/// Writing a zero to port 0x80 wastes ~1-4 microseconds — enough for the PIC
/// to latch each command before the next one arrives.
///
/// Port 0x80 is the BIOS POST code port. Writing to it is harmless (just
/// updates a diagnostic LED on ancient hardware) and is the standard Linux
/// technique for PIC I/O delays.
#[inline(always)]
fn io_wait() {
    unsafe {
        cpu::outb(0x80, 0);
    }
}

/// Initialize both 8259 PICs by sending the 4-step ICW sequence.
///
/// This remaps hardware IRQs from their BIOS-default vectors (which collide
/// with CPU exceptions 0-31) to vectors 32-47, then masks all IRQ lines.
/// Individual IRQs are unmasked later as their handlers are registered.
///
/// ## Initialization Command Word (ICW) sequence
///
/// Each PIC requires four ICWs sent in strict order:
/// 1. **ICW1** → command port: begin initialization, announce ICW4 is coming
/// 2. **ICW2** → data port: set the base interrupt vector offset
/// 3. **ICW3** → data port: configure cascade wiring between master and slave
/// 4. **ICW4** → data port: set operating mode (8086 mode)
///
/// After all four ICWs, the data port becomes the interrupt mask register
/// (OCW1). Writing to it controls which IRQ lines are enabled (0 = enabled,
/// 1 = masked/disabled).
pub fn init() {
    unsafe {
        // --- ICW1: start initialization sequence on both PICs ---
        // Bit 4 (INIT) tells the PIC to expect ICW2-4.
        // Bit 0 (ICW4) says ICW4 will be sent (required for 8086 mode).
        cpu::outb(MASTER_COMMAND, ICW1_INIT | ICW1_ICW4);
        io_wait();
        cpu::outb(SLAVE_COMMAND, ICW1_INIT | ICW1_ICW4);
        io_wait();

        // --- ICW2: set vector offsets ---
        // This is the critical remapping step. The offset tells the PIC what
        // vector number to use for its IRQ0 (master) or IRQ8 (slave). The
        // remaining 7 IRQs on each PIC are consecutive vectors from the offset.
        cpu::outb(MASTER_DATA, MASTER_OFFSET);
        io_wait();
        cpu::outb(SLAVE_DATA, SLAVE_OFFSET);
        io_wait();

        // --- ICW3: configure cascade ---
        // Master: bitmask of which IRQ line has a slave connected.
        // Bit 2 set = slave on IRQ2 (binary 00000100 = 0x04).
        cpu::outb(MASTER_DATA, 0x04);
        io_wait();
        // Slave: its cascade identity — the master IRQ line it's connected to.
        // Value 2 = "I am the slave on the master's IRQ2".
        cpu::outb(SLAVE_DATA, 0x02);
        io_wait();

        // --- ICW4: set 8086 mode ---
        // The only mode that makes sense on modern x86 hardware. Without this,
        // the PIC would use the ancient 8080 interrupt protocol.
        cpu::outb(MASTER_DATA, ICW4_8086);
        io_wait();
        cpu::outb(SLAVE_DATA, ICW4_8086);
        io_wait();

        // --- Mask all IRQs initially ---
        // 0xFF = all 8 lines masked on each PIC. Individual IRQs are unmasked
        // as their handlers are installed (e.g., pic::unmask(0) for the timer).
        cpu::outb(MASTER_DATA, 0xFF);
        cpu::outb(SLAVE_DATA, 0xFF);
    }

    println!("8259 PIC initialized (IRQs remapped to vectors {}-{})",
        MASTER_OFFSET, SLAVE_OFFSET + 7);
}

/// Send End Of Interrupt (EOI) to the appropriate PIC(s).
///
/// Must be called at the end of every hardware IRQ handler. Failing to send
/// EOI causes the PIC to stop delivering that IRQ line (and all lower-priority
/// lines on the same PIC).
///
/// For master IRQs (0-7): send EOI to master only.
/// For slave IRQs (8-15): send EOI to both slave AND master — because the
/// slave signals the master via the IRQ2 cascade line, and both PICs need
/// to know the interrupt has been handled.
pub fn send_eoi(irq: u8) {
    unsafe {
        if irq >= 8 {
            cpu::outb(SLAVE_COMMAND, EOI);
        }
        cpu::outb(MASTER_COMMAND, EOI);
    }
}

/// Unmask (enable) a specific IRQ line.
///
/// Clears the corresponding bit in the PIC's mask register. The mask register
/// uses inverted logic: 0 = enabled, 1 = masked/disabled.
///
/// For slave IRQs (8-15), this also ensures IRQ2 (the cascade line) is
/// unmasked on the master PIC, since slave interrupts can't reach the CPU
/// if the cascade is blocked.
pub fn unmask(irq: u8) {
    unsafe {
        if irq < 8 {
            let mask = cpu::inb(MASTER_DATA) & !(1 << irq);
            cpu::outb(MASTER_DATA, mask);
        } else {
            let mask = cpu::inb(SLAVE_DATA) & !(1 << (irq - 8));
            cpu::outb(SLAVE_DATA, mask);
            // Ensure IRQ2 (cascade) is unmasked on the master so slave
            // interrupts can actually reach the CPU.
            let master_mask = cpu::inb(MASTER_DATA) & !(1 << 2);
            cpu::outb(MASTER_DATA, master_mask);
        }
    }
}

/// Mask (disable) a specific IRQ line.
///
/// Sets the corresponding bit in the PIC's mask register, preventing that
/// IRQ from reaching the CPU. Other IRQ lines are unaffected.
pub fn mask(irq: u8) {
    unsafe {
        if irq < 8 {
            let mask = cpu::inb(MASTER_DATA) | (1 << irq);
            cpu::outb(MASTER_DATA, mask);
        } else {
            let mask = cpu::inb(SLAVE_DATA) | (1 << (irq - 8));
            cpu::outb(SLAVE_DATA, mask);
        }
    }
}

// --- Spurious IRQ detection ---
//
// The 8259 PIC can generate "spurious" interrupts — phantom IRQs that don't
// correspond to any real hardware event. This happens when:
// - An IRQ line is asserted and then de-asserted before the CPU acknowledges it
// - Electrical noise on the IRQ line
// - The PIC's internal priority logic resolves an ambiguity
//
// Spurious IRQs only appear on the lowest-priority line of each PIC:
// **IRQ7** (master) and **IRQ15** (slave). No other IRQ lines produce spurious
// interrupts because the 8259 resolves them before signaling the CPU.
//
// ## Detection method
//
// Read the In-Service Register (ISR) to check whether the PIC actually has
// this IRQ in service. If the ISR bit for the IRQ is NOT set, the interrupt
// is spurious — the PIC signaled the CPU but then realized no real IRQ was
// pending.
//
// ## EOI rules for spurious IRQs
//
// - **Spurious IRQ7**: Don't send any EOI. The master never started servicing
//   this IRQ, so there's nothing to acknowledge.
// - **Spurious IRQ15**: Send EOI to the master only. The master IS servicing
//   the IRQ2 cascade (it saw the slave assert IRQ2), but the slave never
//   started servicing IRQ15. So the master needs its cascade cleared, but the
//   slave must not receive an EOI it didn't earn.

/// Read the In-Service Register (ISR) from the master PIC.
///
/// Sends OCW3 (read ISR command) to the command port, then reads the result
/// back from the same port. Each bit in the returned byte corresponds to one
/// IRQ line (bit 0 = IRQ0, bit 7 = IRQ7). A set bit means that IRQ's handler
/// is currently executing (EOI not yet sent).
fn read_master_isr() -> u8 {
    unsafe {
        cpu::outb(MASTER_COMMAND, OCW3_READ_ISR);
        cpu::inb(MASTER_COMMAND)
    }
}

/// Read the In-Service Register (ISR) from the slave PIC.
///
/// Same as `read_master_isr` but for the slave. Bit positions correspond to
/// IRQ8-15 (bit 0 = IRQ8, bit 7 = IRQ15).
fn read_slave_isr() -> u8 {
    unsafe {
        cpu::outb(SLAVE_COMMAND, OCW3_READ_ISR);
        cpu::inb(SLAVE_COMMAND)
    }
}

/// Check whether an IRQ is spurious and handle it appropriately.
///
/// Only IRQ7 and IRQ15 can be spurious. For all other IRQs, this function
/// returns `false` immediately.
///
/// Returns `true` if the IRQ is spurious — in that case, the caller must
/// **skip the normal handler** and **not send EOI** (this function handles
/// any necessary EOI internally for spurious IRQ15).
pub fn is_spurious(irq: u8) -> bool {
    match irq {
        7 => {
            // Master PIC spurious: check if IRQ7 (bit 7) is actually in service.
            // If not set → spurious. Don't send any EOI.
            read_master_isr() & (1 << 7) == 0
        }
        15 => {
            // Slave PIC spurious: check if IRQ15 (bit 7 of slave ISR) is in service.
            if read_slave_isr() & (1 << 7) == 0 {
                // Spurious IRQ15: the master IS servicing IRQ2 (cascade), so we
                // must send EOI to the master to clear the cascade. But the slave
                // never started servicing, so no EOI to the slave.
                unsafe { cpu::outb(MASTER_COMMAND, EOI); }
                return true;
            }
            false
        }
        _ => false,
    }
}
