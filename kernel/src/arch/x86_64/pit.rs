//! # 8254 Programmable Interval Timer (PIT) Driver
//!
//! The PIT is the classic PC timer chip. It has three independent countdown
//! channels, each driven by a 1.193182 MHz oscillator:
//!
//! - **Channel 0**: Connected to IRQ0 via the 8259 PIC — our system timer
//! - **Channel 1**: Originally for DRAM refresh (legacy, unused)
//! - **Channel 2**: Connected to the PC speaker (unused)
//!
//! We configure channel 0 to generate periodic interrupts at ~100 Hz (one
//! tick every ~10 ms). This gives us a timer for preemptive scheduling.
//!
//! ## How it works
//!
//! The PIT counts down from a 16-bit reload value at 1.193182 MHz. When the
//! counter reaches zero, it fires an interrupt on IRQ0 and automatically
//! reloads the counter. The interrupt frequency is:
//!
//! ```text
//!   frequency = 1_193_182 / divisor
//! ```
//!
//! For 100 Hz: divisor = 1_193_182 / 100 = 11_932 (actual: ~99.998 Hz).
//!
//! ## Operating modes
//!
//! We use **Mode 3 (Square Wave Generator)** because it produces the most
//! regular interrupt timing. In this mode, the counter decrements by 2 on
//! each clock cycle and generates a symmetric square wave output signal.
//! The output toggles high/low, and IRQ0 fires on each rising edge.

use super::cpu;
use crate::println;

// --- PIT I/O ports ---

/// Channel 0 data port. The 16-bit reload value is written here as two
/// successive bytes (low byte first, then high byte — as configured by
/// the mode/command register's access mode field).
const CHANNEL_0_DATA: u16 = 0x40;

/// Mode/command register (write-only). Controls which channel to configure,
/// the access mode (lobyte/hibyte), the operating mode, and BCD/binary format.
///
/// Bits 7-6: Channel select (00 = channel 0)
/// Bits 5-4: Access mode (11 = lobyte/hibyte — send both bytes)
/// Bits 3-1: Operating mode (000-101)
/// Bit 0:    BCD/Binary (0 = 16-bit binary, 1 = 4-digit BCD)
const MODE_COMMAND: u16 = 0x43;

// --- PIT configuration ---

/// The PIT's base oscillator frequency: 1,193,182 Hz.
///
/// This is derived from the original IBM PC's 14.31818 MHz master crystal
/// divided by 12. The frequency is a hardware constant — it's the same on
/// all x86 PCs regardless of CPU speed.
const PIT_FREQUENCY: u32 = 1_193_182;

/// Target timer interrupt frequency in Hz.
///
/// 100 Hz = one interrupt every 10 ms. This is a good balance between
/// scheduling granularity (responsive context switches) and overhead
/// (not spending too much time in timer interrupt handlers).
const TARGET_HZ: u32 = 100;

/// Countdown divisor: PIT_FREQUENCY / TARGET_HZ.
///
/// The PIT counts down from this value at 1.193182 MHz, firing IRQ0 each
/// time it reaches zero. 1_193_182 / 100 = 11_932 (truncated).
/// Actual frequency: 1_193_182 / 11_932 ≈ 100.001 Hz.
const DIVISOR: u16 = (PIT_FREQUENCY / TARGET_HZ) as u16;

/// Command byte to configure channel 0 in mode 3 (square wave generator).
///
/// ```text
/// Bits 7-6 = 00  → channel 0
/// Bits 5-4 = 11  → access mode: lobyte/hibyte (send both bytes of divisor)
/// Bits 3-1 = 011 → operating mode 3 (square wave generator)
/// Bit 0    = 0   → 16-bit binary countdown (not BCD)
///
/// Binary: 00_11_011_0 = 0x36
/// ```
const CHANNEL_0_MODE_3: u8 = 0x36;

/// Initialize the PIT to generate periodic interrupts at ~100 Hz on IRQ0.
///
/// Configures channel 0 in mode 3 (square wave generator) with a divisor
/// that produces approximately 100 interrupts per second.
///
/// The PIC must be initialized (`pic::init()`) and IRQ0 unmasked
/// (`pic::unmask(0)`) for the timer interrupts to actually reach the CPU.
/// Interrupts must also be globally enabled (`sti`).
pub fn init() {
    let actual_hz = PIT_FREQUENCY / DIVISOR as u32;

    unsafe {
        // Send the mode/command byte first. This tells the PIT which channel
        // we're configuring, what access mode to use, and the operating mode.
        cpu::outb(MODE_COMMAND, CHANNEL_0_MODE_3);

        // Send the 16-bit divisor as two bytes. The PIT latches the low byte
        // internally and waits for the high byte before loading the counter.
        cpu::outb(CHANNEL_0_DATA, (DIVISOR & 0xFF) as u8);
        cpu::outb(CHANNEL_0_DATA, ((DIVISOR >> 8) & 0xFF) as u8);
    }

    println!("PIT configured: channel 0, mode 3, divisor {} (~{} Hz)", DIVISOR, actual_hz);
}
