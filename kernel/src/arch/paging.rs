//! # Paging facade
//!
//! Re-exports the active architecture's page-table primitives so that the 4-level
//! walker in [`crate::mm::page_table`] — and everything above it — is written once.
//!
//! Like the other `arch::*` facades this is a compile-time `pub use`, not a trait
//! object: the chosen implementation is monomorphized in with no indirection.
//!
//! ## The contract
//!
//! Each architecture module supplies:
//!
//! | Item | Meaning |
//! |------|---------|
//! | `ENTRIES_PER_TABLE` | descriptors per table (512 on both) |
//! | `KERNEL_ROOT_START` | first root index belonging to the kernel half |
//! | `LEVEL_NAMES` | level labels for debug walks |
//! | `PageFlags` | portable mapping intent (`PRESENT`/`WRITABLE`/`USER`/…) |
//! | `encode_leaf` / `encode_table` | intent → hardware descriptor |
//! | `is_valid` / `is_block` / `addr_of` / `flags_of` | hardware descriptor → intent |
//! | `activate` / `current_root` | address-space switch |
//! | `flush_page` / `flush_all` | TLB maintenance |
//!
//! The two implementations differ far more than the shared vocabulary suggests —
//! aarch64 has split TTBR0/TTBR1 roots, level-dependent descriptor meanings, inverted
//! write permission, a mandatory access flag, and MAIR-indirected cacheability. Those
//! differences are documented where they live, in
//! [`crate::arch::aarch64::paging`].

#[cfg(target_arch = "x86_64")]
pub use crate::arch::x86_64::paging::*;

#[cfg(target_arch = "aarch64")]
pub use crate::arch::aarch64::paging::*;
