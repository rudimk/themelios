//! # `arch::syscall` — architecture-neutral syscall frame and user-memory access
//!
//! The seam that lets the Linux personality ([`crate::linux`]) and anything else handling
//! a syscall compile without naming an architecture. Like [`crate::arch::irq`] and the
//! other facades, this is a `pub use` re-export of the active arch's implementation — no
//! runtime dispatch, identical codegen to calling through directly.
//!
//! ## What this facade is for, concretely
//!
//! Before it existed, `linux/fs.rs`, `linux/syscall.rs` and `linux/thread.rs` opened with
//! `use crate::arch::x86_64::syscall::{...}` and then named x86 registers at **76 call
//! sites** (`frame.rdi`, `frame.rax`, `frame.r10`). That is why `mod linux` was
//! `#[cfg(target_arch = "x86_64")]` in its entirety, and why a test as portable as
//! `test_path_resolve` — ten cases of pure string logic over `resolve_path` — was skipped
//! on aarch64.
//!
//! ## The abstraction is *positional*, not register-named
//!
//! The important design point. The Linux syscall ABI is a syscall number plus arguments
//! at positions 0-5; which register carries each position is per-architecture and of no
//! interest to a caller implementing `openat`. So [`SyscallFrame`] exposes `nr()`,
//! `arg0()`-`arg5()`, `set_ret()`/`ret_mut()` and `user_pc()`, and each architecture maps
//! those onto its own registers:
//!
//! | position | x86_64 | aarch64 |
//! |---|---|---|
//! | number | `rax` | `x8` |
//! | 0-5 | `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9` | `x0`-`x5` |
//! | return | `rax` | `x0` |
//! | user PC | `rcx` | `ELR_EL1` |
//!
//! Two asymmetries the table hides, both of which the positional API exists to contain:
//! x86 uses **`r10` for position 3, not `rcx`**, because the `syscall` instruction clobbers
//! `rcx` with the return address; and on x86 the number and the return value **share a
//! register**, so a caller that writes the return before reading the number loses the
//! number — on aarch64 they are separate and the same code would work. Portable callers
//! must therefore read the number first, and the facade is where that rule is written down.
//!
//! ## User-memory access
//!
//! [`copy_from_user`], [`copy_to_user`] and [`user_range_ok`] are the chokepoint for
//! pointers that arrive from userspace. Both arches bound the range against their own user
//! address regime — x86 against the canonical-hole boundary, aarch64 against
//! `2^(64 - T0SZ)` read from `TCR_EL1`. **Both copy functions are `unsafe`**: a validated
//! range is not a mapped range, and neither architecture has an exception table yet, so an
//! in-range but unmapped pointer faults fatally. See the arch modules for the details and
//! the current limitations.

#[cfg(target_arch = "x86_64")]
pub use crate::arch::x86_64::syscall::SyscallFrame;

// `allow(unused_imports)` on the aarch64 side only, and it is temporary rather than
// cosmetic: the sole consumer of this facade today is `mod linux`, still x86-gated on its
// VFS and process dependencies. So on aarch64 the re-exports are correct, compiled, and
// nothing calls them until the ring-3 servers arrive in 8.5. Silencing it beats leaving
// four warnings in every arm64 build for a module that is deliberately ahead of its
// callers — but it does mean a *wrong* aarch64 mapping here would not be caught by the
// build. The positional accessors are exercised by the EL0 self-test's dispatch path.
#[cfg(target_arch = "aarch64")]
#[allow(unused_imports)]
pub use crate::arch::aarch64::syscall::SyscallFrame;

// The user-copy trio is `pub(crate)` on x86, so the re-export has to be too — a `pub use`
// of a `pub(crate)` item is an error, not a widening. That is the right visibility anyway:
// this is a binary crate, and the one thing a user-memory chokepoint should never become
// is part of a public API surface.
#[cfg(target_arch = "x86_64")]
pub(crate) use crate::arch::x86_64::syscall::{copy_from_user, copy_to_user, user_range_ok};

// aarch64 keeps the user-copy primitives in their own module rather than beside the
// dispatch code, because they are the hostile-input surface and warrant being read as a
// unit. The facade flattens that difference away, which is the point of a facade.
#[cfg(target_arch = "aarch64")]
#[allow(unused_imports)] // see the note above; the consumer is `mod linux`, still x86-gated
pub(crate) use crate::arch::aarch64::uaccess::{copy_from_user, copy_to_user, user_range_ok};
