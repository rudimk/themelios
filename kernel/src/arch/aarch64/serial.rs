//! # aarch64 PL011 UART driver + `_print`
//!
//! The aarch64 side of the [`arch::serial`](crate::arch::serial) facade, backing the
//! `print!`/`println!` macros. QEMU `virt` (and most ARM SoCs) expose an ARM PL011
//! UART; on `virt` its MMIO block is at physical `0x0900_0000`.
//!
//! ## Why the UART is installed late
//!
//! Unlike x86 (where the 16550 is reached through *port I/O* that needs no page
//! mapping), the PL011 is **memory-mapped**, and Limine's higher-half handoff does
//! **not** map device MMIO — only RAM (confirmed empirically during the Phase 7 boot
//! spike: a bare `HHDM + 0x0900_0000` access data-aborts). So early boot must first
//! map the UART page into the kernel address space (see [`crate::arch::aarch64::boot`])
//! and then hand the resulting **virtual** address to [`init`]. Until then, `_print`
//! silently drops output rather than faulting.

use core::fmt;

use crate::sync::InterruptMutex;

/// PL011 register offsets (from the MMIO base).
const UART_DR: usize = 0x00; // data register
const UART_FR: usize = 0x18; // flag register
/// Interrupt mask set/clear — a *set* bit here enables that interrupt.
const UART_IMSC: usize = 0x38;
/// Masked interrupt status: which enabled interrupts are currently asserting.
const UART_MIS: usize = 0x40;
/// Interrupt clear — write 1s to acknowledge.
const UART_ICR: usize = 0x44;

/// `UART_FR` bit 5: transmit FIFO full.
const FR_TXFF: u32 = 1 << 5;
/// `UART_FR` bit 4: receive FIFO empty.
const FR_RXFE: u32 = 1 << 4;

/// `IMSC`/`MIS`/`ICR` bit 4: receive interrupt — the RX FIFO reached its trigger level.
const INT_RX: u32 = 1 << 4;
/// `IMSC`/`MIS`/`ICR` bit 6: receive *timeout* — characters are sitting in the FIFO but
/// it never filled to the trigger level.
///
/// Both are needed, and forgetting the timeout is the classic PL011 bug. Interactive
/// typing almost never reaches the FIFO trigger level (default: 1/2 full, eight
/// characters), so with only `INT_RX` enabled the first seven keystrokes sit in the
/// FIFO with no interrupt raised, and the shell appears dead until the eighth arrives.
const INT_RX_TIMEOUT: u32 = 1 << 6;

/// A PL011 instance addressed by a **mapped virtual** base address.
struct Pl011 {
    base: usize,
}

// SAFETY: `base` is an MMIO address; the writer is only ever touched under the
// `InterruptMutex`, so shared mutation is serialized. `Send` is required to store it
// in the static mutex.
unsafe impl Send for Pl011 {}

impl Pl011 {
    /// Read one byte if the receive FIFO has anything, else `None`.
    fn getc(&self) -> Option<u8> {
        // SAFETY: `base` is a device-mapped PL011; FR/DR are inside the mapped page.
        unsafe {
            let fr = core::ptr::read_volatile((self.base + UART_FR) as *const u32);
            if fr & FR_RXFE != 0 {
                return None;
            }
            Some(core::ptr::read_volatile((self.base + UART_DR) as *const u32) as u8)
        }
    }

    /// Enable receive interrupts (data available, and data sitting idle in the FIFO).
    fn enable_rx_interrupt(&self) {
        // SAFETY: `base` is a device-mapped PL011; IMSC/ICR are inside the mapped page.
        unsafe {
            // Clear anything already latched, so enabling does not immediately deliver
            // an interrupt for a character received before the handler existed.
            core::ptr::write_volatile(
                (self.base + UART_ICR) as *mut u32,
                INT_RX | INT_RX_TIMEOUT,
            );
            let imsc = core::ptr::read_volatile((self.base + UART_IMSC) as *const u32);
            core::ptr::write_volatile(
                (self.base + UART_IMSC) as *mut u32,
                imsc | INT_RX | INT_RX_TIMEOUT,
            );
        }
    }

    /// Acknowledge the receive interrupts at the UART.
    ///
    /// Distinct from the GIC's EOI, and both are required: the GIC stops tracking the
    /// interrupt as active, while this clears the *source*. Skip this and the PL011
    /// keeps asserting its line, so the GIC re-pends immediately after every EOI and
    /// the machine makes no further progress.
    fn ack_rx(&self) {
        // SAFETY: `base` is a device-mapped PL011; ICR is inside the mapped page.
        unsafe {
            core::ptr::write_volatile(
                (self.base + UART_ICR) as *mut u32,
                INT_RX | INT_RX_TIMEOUT,
            );
        }
    }

    fn putc(&self, c: u8) {
        // SAFETY: `base` is a device-mapped PL011 (installed by `init`); DR/FR are
        // within the mapped 4 KiB page.
        unsafe {
            let fr = (self.base + UART_FR) as *const u32;
            while core::ptr::read_volatile(fr) & FR_TXFF != 0 {}
            core::ptr::write_volatile((self.base + UART_DR) as *mut u8, c);
        }
    }
}

impl fmt::Write for Pl011 {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                self.putc(b'\r'); // CRLF for terminal sanity
            }
            self.putc(b);
        }
        Ok(())
    }
}

/// The global serial writer. `None` until [`init`] installs the mapped UART. Behind an
/// `InterruptMutex` so `print!` is safe from interrupt context (matches x86).
static SERIAL: InterruptMutex<Option<Pl011>> = InterruptMutex::new(None);

/// Install the PL011 at the given **virtual** base address (already device-mapped by
/// early boot). After this, `print!`/`println!` reach the console.
pub fn init(base_vaddr: usize) {
    *SERIAL.lock() = Some(Pl011 { base: base_vaddr });
}

/// Internal print entry used by the `print!`/`println!` macros (via the
/// [`arch::serial`](crate::arch::serial) facade). Drops output if the UART is not yet
/// installed.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    let mut guard = SERIAL.lock();
    if let Some(ref mut uart) = *guard {
        let _ = uart.write_fmt(args);
    }
}

/// Enable PL011 receive interrupts. Called by the shell once it is ready to consume
/// input; until then the UART is transmit-only.
pub fn enable_receive_interrupt() {
    if let Some(ref uart) = *SERIAL.lock() {
        uart.enable_rx_interrupt();
    }
}

/// Drain the receive FIFO into the shell's input buffer, from the IRQ handler.
///
/// Reads until the FIFO is empty rather than taking a single byte. The receive-timeout
/// interrupt fires once for a burst that is sitting in the FIFO, so a handler that
/// consumed one character per interrupt would leave the rest stranded until the *next*
/// keystroke — input would lag one character behind forever.
///
/// Returns the number of bytes taken, for the caller's diagnostics.
pub fn handle_receive_interrupt() -> usize {
    let guard = SERIAL.lock();
    let Some(ref uart) = *guard else {
        return 0;
    };

    let mut taken = 0;
    while let Some(byte) = uart.getc() {
        crate::shell::input::push_byte(byte);
        taken += 1;
    }

    // Acknowledge at the source *after* draining. Clearing first would leave a window
    // where a character arriving between the clear and the last read is consumed here
    // but leaves no pending interrupt behind.
    uart.ack_rx();
    drop(guard);

    // Waking is a separate step from buffering, and forgetting it is silent: the bytes
    // land in the ring buffer, the interrupt is acknowledged, and the shell — blocked
    // in `read_line` — is never made runnable again, so a perfectly working receive
    // path looks like a dead console. Only wake when something actually arrived; a
    // timeout interrupt with an empty FIFO is not a reason to schedule anyone.
    if taken > 0 {
        crate::shell::input::wake_shell();
    }

    taken
}

