//! # `arch::serial` — architecture-neutral serial console facade
//!
//! Backs the crate-wide `print!`/`println!` macros. Each architecture provides a
//! `_print(fmt::Arguments)` that writes to its debug UART (x86_64: 16550 via port
//! I/O; aarch64: PL011 via MMIO); this facade re-exports the active one so the macros
//! compile on both. Defining the macros here (rather than inside `arch::x86_64`) is
//! what let the port stop the ~35 `println!`-not-found errors on aarch64.

#[cfg(target_arch = "x86_64")]
pub use crate::arch::x86_64::serial::_print;

#[cfg(target_arch = "aarch64")]
pub use crate::arch::aarch64::serial::_print;

/// Print formatted text to the serial console.
///
/// Works like `std::print!` but outputs to the debug UART. The arch serial writer
/// must be initialized first (x86: `serial::init`; aarch64: boot maps + installs the
/// PL011); otherwise output is silently dropped. Uses an `InterruptMutex` internally,
/// so it is safe to call from interrupt handlers.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        $crate::arch::serial::_print(format_args!($($arg)*));
    }};
}

/// Print formatted text to the serial console, followed by a newline.
#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}
