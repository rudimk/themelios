//! # 16550 UART Serial Driver
//!
//! Driver for the 16550 UART (Universal Asynchronous Receiver/Transmitter),
//! the standard serial port on x86 PCs. This is our primary debug output
//! channel — all kernel messages go through here.
//!
//! ## How it works
//!
//! The UART is accessed through x86 I/O ports. COM1 lives at base port 0x3F8
//! with 8 registers at consecutive port addresses. We configure it for:
//! - 115200 baud (fastest standard rate)
//! - 8 data bits, no parity, 1 stop bit (8N1) — the universal default
//! - FIFO enabled with 14-byte threshold
//!
//! When running in QEMU with `-serial stdio`, anything we write to the UART
//! appears on the host terminal. This is how we see kernel output.
//!
//! ## Register map (relative to base port 0x3F8)
//!
//! | Offset | DLAB=0 Read     | DLAB=0 Write    | DLAB=1        |
//! |--------|-----------------|-----------------|---------------|
//! | +0     | Receive Buffer  | Transmit Hold   | Divisor (low) |
//! | +1     | Interrupt Enable| Interrupt Enable| Divisor (high)|
//! | +2     | Interrupt ID    | FIFO Control    | —             |
//! | +3     | Line Control    | Line Control    | —             |
//! | +4     | Modem Control   | Modem Control   | —             |
//! | +5     | Line Status     | —               | —             |
//! | +6     | Modem Status    | —               | —             |
//! | +7     | Scratch         | Scratch         | —             |
//!
//! DLAB (Divisor Latch Access Bit) is bit 7 of the Line Control Register.
//! When set, ports +0 and +1 access the baud rate divisor instead of
//! the data/interrupt registers.

use core::fmt;

use super::cpu;
use crate::sync::InterruptMutex;

/// Base I/O port address for COM1 (the first serial port).
/// This is a standard x86 PC convention — COM1 is always at 0x3F8.
const COM1_BASE: u16 = 0x3F8;

/// Register offsets from the base port address.
/// Each register is one byte, accessed via I/O port reads/writes.
mod regs {
    /// Data register: write bytes here to transmit, read to receive.
    /// When DLAB=1, this becomes the low byte of the baud rate divisor.
    pub const DATA: u16 = 0;

    /// Interrupt Enable Register. We disable all UART interrupts since
    /// we poll for transmit readiness instead.
    /// When DLAB=1, this becomes the high byte of the baud rate divisor.
    pub const INTERRUPT_ENABLE: u16 = 1;

    /// FIFO Control Register (write-only). Controls the built-in FIFO
    /// buffers that smooth out byte-at-a-time I/O.
    pub const FIFO_CONTROL: u16 = 2;

    /// Line Control Register. Sets data format (bits, parity, stop bits)
    /// and controls DLAB access to the baud rate divisor.
    pub const LINE_CONTROL: u16 = 3;

    /// Modem Control Register. Controls hardware flow control signals
    /// (DTR, RTS) and loopback mode for testing.
    pub const MODEM_CONTROL: u16 = 4;

    /// Line Status Register (read-only). Reports transmitter/receiver
    /// status. Bit 5 (THRE) tells us when we can send the next byte.
    pub const LINE_STATUS: u16 = 5;
}

/// Bit 0 of the Line Status Register: Data Ready.
/// When set, a byte has been received and is available in the receive buffer.
const LINE_STATUS_DR: u8 = 1 << 0;

/// Bit 5 of the Line Status Register: Transmitter Holding Register Empty.
/// When this bit is set, the UART is ready to accept another byte for
/// transmission. We poll this before each write.
const LINE_STATUS_THRE: u8 = 1 << 5;

/// A handle to a 16550 UART serial port at a given I/O base address.
///
/// This struct doesn't hold any state beyond the port address — all state
/// lives in the hardware registers. Multiple instances pointing to the same
/// port are fine (they're just different handles to the same hardware).
pub struct SerialPort {
    /// The base I/O port address (e.g., 0x3F8 for COM1).
    base: u16,
}

impl SerialPort {
    /// Create a new serial port handle for COM1 (0x3F8).
    pub const fn com1() -> Self {
        Self { base: COM1_BASE }
    }

    /// Initialize the UART hardware.
    ///
    /// This configures the serial port for 115200 baud, 8N1, with FIFO enabled.
    /// Must be called before any read/write operations.
    ///
    /// The initialization sequence follows the standard 16550 UART setup:
    /// 1. Disable interrupts (we use polling, not interrupt-driven I/O)
    /// 2. Set baud rate via the divisor latch
    /// 3. Configure data format (8 data bits, no parity, 1 stop bit)
    /// 4. Enable and clear FIFOs
    /// 5. Assert DTR and RTS modem control signals
    pub fn init(&self) {
        unsafe {
            // Step 1: Disable all UART interrupts.
            // We'll poll the Line Status Register instead of using interrupts.
            cpu::outb(self.base + regs::INTERRUPT_ENABLE, 0x00);

            // Step 2: Enable DLAB (Divisor Latch Access Bit) to set baud rate.
            // Setting bit 7 of Line Control makes ports +0/+1 access the
            // 16-bit baud rate divisor instead of the data/interrupt registers.
            cpu::outb(self.base + regs::LINE_CONTROL, 0x80);

            // Step 3: Set baud rate divisor.
            // Divisor = 115200 / desired_baud. For 115200 baud: divisor = 1.
            // Low byte goes to port +0, high byte to port +1.
            cpu::outb(self.base + regs::DATA, 0x01);             // Divisor low byte
            cpu::outb(self.base + regs::INTERRUPT_ENABLE, 0x00); // Divisor high byte

            // Step 4: Set data format and disable DLAB.
            // 0x03 = 8 data bits, no parity, 1 stop bit (8N1).
            // This also clears the DLAB bit, returning ports +0/+1 to normal.
            cpu::outb(self.base + regs::LINE_CONTROL, 0x03);

            // Step 5: Enable FIFOs with 14-byte threshold.
            // 0xC7 = enable FIFOs, clear both buffers, 14-byte trigger level.
            // The FIFO buffers incoming/outgoing bytes so we don't lose data
            // if we don't read/write fast enough.
            cpu::outb(self.base + regs::FIFO_CONTROL, 0xC7);

            // Step 6: Assert DTR (Data Terminal Ready) and RTS (Request To Send).
            // 0x03 = DTR + RTS. These modem control signals tell the other end
            // that we're ready to communicate.
            cpu::outb(self.base + regs::MODEM_CONTROL, 0x03);
        }
    }

    /// Enable the "Received Data Available" interrupt on this UART.
    ///
    /// After this call, the UART will assert IRQ4 (for COM1) whenever a byte
    /// is received and ready to read. The IRQ4 handler should call
    /// `receive_byte()` to drain the receive buffer.
    ///
    /// Bit 0 of the Interrupt Enable Register (IER) enables the receive
    /// data available interrupt. We leave other interrupt sources disabled.
    pub fn enable_receive_interrupt(&self) {
        unsafe {
            cpu::outb(self.base + regs::INTERRUPT_ENABLE, 0x01);
        }
    }

    /// Check if the receive buffer has data ready to read.
    pub fn data_ready(&self) -> bool {
        unsafe { cpu::inb(self.base + regs::LINE_STATUS) & LINE_STATUS_DR != 0 }
    }

    /// Read a byte from the receive buffer.
    ///
    /// Returns `Some(byte)` if data is available, `None` otherwise.
    /// Call this from the IRQ4 handler to drain all available bytes.
    pub fn receive_byte(&self) -> Option<u8> {
        if self.data_ready() {
            Some(unsafe { cpu::inb(self.base + regs::DATA) })
        } else {
            None
        }
    }

    /// Check if the transmit holding register is empty (ready for next byte).
    fn is_transmit_ready(&self) -> bool {
        // Read the Line Status Register and check the THRE bit.
        // When THRE is set, the UART has finished sending the previous byte
        // and is ready for the next one.
        unsafe { cpu::inb(self.base + regs::LINE_STATUS) & LINE_STATUS_THRE != 0 }
    }

    /// Write a single byte to the serial port, blocking until ready.
    ///
    /// This polls the Line Status Register until the transmitter is ready,
    /// then writes the byte. In practice, at 115200 baud each byte takes
    /// ~87 microseconds, so the busy-wait is very short.
    pub fn write_byte(&self, byte: u8) {
        // Wait for the UART to finish transmitting the previous byte
        while !self.is_transmit_ready() {
            core::hint::spin_loop();
        }

        // Write the byte to the data register for transmission
        unsafe {
            cpu::outb(self.base + regs::DATA, byte);
        }
    }
}

/// Implement `core::fmt::Write` so we can use Rust's formatting macros
/// (`write!`, `writeln!`) with the serial port.
///
/// This lets us do things like:
/// ```
/// writeln!(serial, "Value: {}", 42).unwrap();
/// ```
///
/// Each character in the string is sent as a single byte to the UART.
impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // Convert \n to \r\n for proper terminal line endings.
            // Serial terminals expect carriage return + line feed (CRLF).
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

// --- Global serial writer facility ---
//
// The global SERIAL_WRITER provides a single point of access for all kernel
// serial output. It's wrapped in an InterruptMutex (not a plain spin::Mutex)
// because interrupt handlers may need to print — e.g., the timer interrupt
// handler printing scheduler diagnostics, or an exception handler printing
// a register dump. If we used a plain spinlock and an interrupt fired while
// the lock was held, the handler would spin forever → deadlock.

/// Global serial writer, accessible from anywhere in the kernel via the
/// `print!` and `println!` macros.
///
/// Starts as `None` and is initialized to `Some(SerialPort)` by `init()`.
/// The `Option` avoids requiring a way to construct `SerialPort` at compile
/// time with initialization already done (hardware init must happen at runtime).
static SERIAL_WRITER: InterruptMutex<Option<SerialPort>> = InterruptMutex::new(None);

/// Initialize the global serial writer.
///
/// Creates a COM1 serial port, configures the hardware (115200 baud, 8N1,
/// FIFO enabled), and installs it as the global writer. After this call,
/// `print!` and `println!` macros will produce output on the serial console.
///
/// Must be called exactly once, early in the boot sequence (before any
/// `println!` calls). Calling it again is harmless (just re-initializes
/// the same hardware) but unnecessary.
pub fn init() {
    let serial = SerialPort::com1();
    serial.init();
    *SERIAL_WRITER.lock() = Some(serial);
}

/// Internal print function used by the `print!` and `println!` macros.
///
/// Acquires the global serial writer lock (with interrupts disabled) and
/// writes the formatted arguments to the serial port. If the serial port
/// hasn't been initialized yet, the output is silently dropped.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    let mut guard = SERIAL_WRITER.lock();
    if let Some(ref mut serial) = *guard {
        // write_fmt returns Err only if the Write impl returns Err,
        // and our Write impl always returns Ok(()). Unwrap is safe.
        serial.write_fmt(args).unwrap();
    }
}

/// Enable receive interrupts on the global serial port.
///
/// After this call, IRQ4 fires when a byte is received on COM1.
/// Must be called after `init()`.
pub fn enable_receive_interrupt() {
    let guard = SERIAL_WRITER.lock();
    if let Some(ref serial) = *guard {
        serial.enable_receive_interrupt();
    }
}

/// Read a byte from the global serial port's receive buffer.
///
/// Returns `Some(byte)` if data is available, `None` otherwise.
/// Called from the IRQ4 handler to drain received bytes.
pub fn receive_byte() -> Option<u8> {
    let guard = SERIAL_WRITER.lock();
    if let Some(ref serial) = *guard {
        serial.receive_byte()
    } else {
        None
    }
}

/// Print formatted text to the serial console.
///
/// Works exactly like `std::print!` but outputs to the UART serial port.
/// The global serial writer must be initialized via `serial::init()` first,
/// otherwise output is silently dropped.
///
/// Uses `InterruptMutex` internally, so it's safe to call from interrupt
/// handlers — interrupts are disabled while the serial lock is held.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        $crate::arch::x86_64::serial::_print(format_args!($($arg)*));
    }};
}

/// Print formatted text to the serial console, followed by a newline.
///
/// Works exactly like `std::println!` but outputs to the UART serial port.
/// See `print!` for details on initialization and interrupt safety.
#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}
