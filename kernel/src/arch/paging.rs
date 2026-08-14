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
//! Only x86_64 implements this today; the aarch64 side arrives with that port.

#[cfg(target_arch = "x86_64")]
pub use crate::arch::x86_64::paging::*;

// The aarch64 implementation lands with the rest of that port; until then this facade
// is empty there, which is harmless because `mm` is not yet compiled on aarch64.
