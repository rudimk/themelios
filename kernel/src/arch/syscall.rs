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
//! Three asymmetries the table hides, all of which the positional API exists to contain:
//!
//! 1. x86 uses **`r10` for position 3, not `rcx`**, because the `syscall` instruction
//!    clobbers `rcx` with the return address.
//! 2. On x86 the number and the return value **share `rax`**, so writing the return before
//!    reading the number loses the number. On aarch64 they are separate registers.
//! 3. **And the mirror image, which matters just as much:** on aarch64 the return value and
//!    *argument 0* share `x0`, so writing the return before reading `arg0()` loses the
//!    argument. On x86 they are separate.
//!
//! An earlier version of this note gave only the first two and described aarch64 as the
//! safe case, which is exactly half a rule — each architecture has an aliasing pair, just a
//! different one. The portable discipline that satisfies both: **read every input you need
//! (`nr` and all arguments) before writing the return.** Both aliases are then unreachable.
//!
//! ## `clone` is the counter-example worth knowing about
//!
//! The claim above is that which register carries a position is per-architecture and of no
//! interest to the caller. That holds for the *registers* and not for every syscall's
//! *positions*: Linux's `clone` swaps positions 3 and 4 between architectures (arm64
//! selects `CLONE_BACKWARDS`, giving `(flags, newsp, parent_tid, tls, child_tid)`, where
//! x86_64 has `(flags, newsp, parent_tid, child_tid, tls)`). A positional facade cannot
//! fix that and does not try to; see the note in [`crate::linux::thread`].
//!
//! ## User-memory access is NOT yet portable, and this module does not pretend it is
//!
//! [`copy_from_user`] and [`copy_to_user`] are re-exported **on x86_64 only**. An earlier
//! version of this file re-exported an aarch64 pair under the same names and claimed the
//! facade "flattens that difference away". It does not: the two are unrelated functions
//! that happen to share a spelling.
//!
//! | | x86_64 | aarch64 |
//! |---|---|---|
//! | `copy_from_user` | `(uptr: u64, len: usize) -> Option<Vec<u8>>`, safe | `(dst: &mut [u8], src: u64) -> Result<(), UserCopyError>`, **`unsafe`** |
//! | `copy_to_user` | `(uptr, &[u8]) -> bool`, safe | `(dst, &[u8]) -> Result<…>`, **`unsafe`** |
//! | `user_range_ok` | bounds check **plus a page-table walk proving every page is mapped** | bounds check only |
//!
//! Different arity, argument order, return type, and safety — and, worst of the four, a
//! different *security guarantee*: x86's `user_range_ok` proves the range is mapped, while
//! aarch64's own docs say "a validated range is not a mapped range". None of the 21 call
//! sites in [`crate::linux`] would compile against the aarch64 shape, so the collision was
//! never going to be discovered by building.
//!
//! Unifying them is real work — aarch64 needs the exception table it does not have before
//! it can offer x86's mapped-range guarantee — and it wants a live consumer to design
//! against. That arrives with the ring-3 servers in 8.5. Until then a portable user-copy
//! **does not exist**, and a module named `arch::syscall` claiming otherwise is worse than
//! its absence: aarch64 code that needs these reaches for
//! [`crate::arch::aarch64::uaccess`] by name and is thereby reminded that it is using the
//! architecture-specific one.

#[cfg(target_arch = "x86_64")]
pub use crate::arch::x86_64::syscall::SyscallFrame;

// Unused on aarch64 today, and the `allow` is load-bearing rather than cosmetic — but not
// for the reason a first draft of this comment gave, which claimed `dispatch` consumes it
// through here. It does not: `dispatch` names the type from its own module. Nothing on
// this architecture goes through the facade yet, because its only consumer (`mod linux`)
// is x86-gated on the syscall-number table and its VFS dependencies, not merely on
// register names.
//
// So this re-export is a declaration of intent that 8.5 will use. What keeps the *aarch64
// mapping* honest in the meantime is not this line but `dispatch` routing the live syscall
// path through all ten accessors, each of which the EL0 self-test now fails on if mutated.
#[cfg(target_arch = "aarch64")]
#[allow(unused_imports)]
pub use crate::arch::aarch64::syscall::SyscallFrame;

// x86_64 only — see the table above. `pub(crate)` because that is the visibility of the
// underlying items, and because the last thing a user-memory chokepoint should become is
// part of a public API surface.
#[cfg(target_arch = "x86_64")]
pub(crate) use crate::arch::x86_64::syscall::{copy_from_user, copy_to_user, user_range_ok};
