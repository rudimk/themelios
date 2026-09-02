//! # `arch::power` — architecture-neutral machine power control
//!
//! A facade over "stop the machine" and "restart the machine", so the shell can offer
//! `shutdown` and `reboot` without naming an architecture. Same shape as
//! [`irq`](crate::arch::irq) and [`time`](crate::arch::time): a `pub use` re-export of the
//! active architecture's implementation, no runtime dispatch.
//!
//! ## The two sides are not equally real, and that matters
//!
//! It would be easy to read this facade as claiming parity. It does not.
//!
//! - **aarch64** has PSCI (`arch::aarch64::psci`), an ARM-standard firmware interface.
//!   `SYSTEM_OFF` and `SYSTEM_RESET` are the same calls Linux makes, and they are
//!   discovered rather than guessed — a fixed function ID, not an address the OS has to
//!   find. Verified on QEMU `virt`. It should carry to other ARM platforms far better
//!   than the x86 side carries anywhere, but note that `psci.rs` hardcodes the `HVC`
//!   conduit first and a platform that needs `SMC` would need its conduit read from the
//!   DT or FADT; that has not been tested against real hardware, and this project's
//!   roadmap deliberately does not plan for it yet.
//! - **x86_64** has no equivalent without ACPI. Soft-off requires reading `\_S5` out of
//!   AML, which needs an interpreter this kernel does not have, so
//!   `power_off` writes to the fixed `PM1a_CNT` addresses that *emulators* are known to
//!   use, and says so in the boot log when none of them answers. Reset is better off —
//!   the `0xCF9` reset control register is real chipset hardware — but shutdown on
//!   physical x86 is genuinely not implemented.
//!
//! Both functions diverge, and both end in a parked CPU rather than a return if the
//! machine does not oblige. A caller can rely on never getting control back; it cannot
//! rely on the machine actually having stopped.

/// Power the machine off. Never returns.
#[cfg(target_arch = "x86_64")]
pub use crate::arch::x86_64::power::power_off;

/// Reset the machine. Never returns.
#[cfg(target_arch = "x86_64")]
pub use crate::arch::x86_64::power::reset;

#[cfg(target_arch = "aarch64")]
pub use crate::arch::aarch64::psci::{reset_or_hang as reset, shutdown_or_hang as power_off};
