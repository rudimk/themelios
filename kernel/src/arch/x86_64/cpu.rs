//! # x86_64 CPU operations
//!
//! Low-level CPU instructions for the x86_64 architecture. These are thin
//! wrappers around inline assembly for common operations like I/O port
//! access and CPU control.
//!
//! ## I/O Ports
//!
//! x86 has a separate I/O address space (distinct from memory) used to
//! communicate with legacy hardware devices. Devices are accessed by reading
//! from or writing to specific port numbers using the `in` and `out` CPU
//! instructions.
//!
//! Common port assignments:
//! - 0x3F8: COM1 serial port (16550 UART)
//! - 0x60/0x64: PS/2 keyboard/mouse controller
//! - 0xCF8/0xCFC: PCI configuration space

use core::arch::asm;

/// Write a single byte to an x86 I/O port.
///
/// This executes the `out dx, al` instruction, which sends the byte in `al`
/// to the I/O port number in `dx`.
///
/// # Safety
///
/// Writing to an I/O port can have arbitrary side effects on hardware.
/// The caller must ensure the port number is valid and the write is
/// appropriate for the device at that port.
#[inline(always)]
pub unsafe fn outb(port: u16, value: u8) {
    // The `out` instruction sends a byte to an I/O port.
    // - "dx" register holds the port number
    // - "al" register holds the byte to send
    // - options(nomem, nostack, preserves_flags): tells the compiler this
    //   instruction doesn't touch memory, the stack, or CPU flags, allowing
    //   better optimization.
    unsafe {
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Read a single byte from an x86 I/O port.
///
/// This executes the `in al, dx` instruction, which reads a byte from the
/// I/O port number in `dx` into `al`.
///
/// # Safety
///
/// Reading from an I/O port can have side effects on hardware (e.g., clearing
/// an interrupt flag or advancing a FIFO). The caller must ensure the port
/// number is valid and the read is appropriate.
#[inline(always)]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // The `in` instruction reads a byte from an I/O port.
    // - "dx" register holds the port number
    // - "al" register receives the byte
    unsafe {
        asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// Halt the CPU until the next interrupt arrives.
///
/// This executes the `hlt` instruction, which puts the CPU into a low-power
/// state until an interrupt fires. Used in idle loops to avoid busy-waiting.
#[inline(always)]
pub fn halt() {
    // hlt is safe in the sense that it doesn't corrupt state — it just
    // pauses the CPU. But it does require being in ring 0 (kernel mode).
    unsafe {
        asm!("hlt", options(nomem, nostack, preserves_flags));
    }
}
