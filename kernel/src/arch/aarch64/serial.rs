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
/// `UART_FR` bit 5: transmit FIFO full.
const FR_TXFF: u32 = 1 << 5;

/// A PL011 instance addressed by a **mapped virtual** base address.
struct Pl011 {
    base: usize,
}

// SAFETY: `base` is an MMIO address; the writer is only ever touched under the
// `InterruptMutex`, so shared mutation is serialized. `Send` is required to store it
// in the static mutex.
unsafe impl Send for Pl011 {}

impl Pl011 {
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
