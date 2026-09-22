//! # The ThemeliOS native syscall numbers — one list, every consumer
//!
//! This file is **single-sourced into three places**: the kernel's `arch::syscall`, the
//! aarch64 dispatcher through it, and `servers/libthemelios`, which is a separate cargo
//! workspace and so cannot `use` the kernel at all. Both sides include it by `#[path]`, the
//! same mechanism the ring-3 `http` and `json` modules already use.
//!
//! It exists as its own file precisely so that inclusion is possible: it must name nothing
//! from either crate — no `crate::`, no imports, no `cfg` — or `#[path]` inclusion on the
//! userspace side stops compiling.
//!
//! ## Why one list is not a tidiness preference
//!
//! These numbers are the contract between a server blob and whichever kernel loads it. They
//! are also the only part of the syscall ABI the two architectures share exactly: x86 and
//! aarch64 differ in which register carries the number and the arguments (`rax`/`rdi`… vs
//! `x8`/`x0`…), never in the numbers.
//!
//! Phase 8.5b was written because that invariant had already been broken once, inside the
//! kernel: aarch64's 8.4 bring-up dispatcher used 1-7 for its own test calls — the same
//! seven numbers the ABI assigns to `SYS_SEND` through `SYS_DEBUG_PRINT`, with entirely
//! different meanings. Nothing detected it, because the two sets had no common caller.
//!
//! Its first fix put the canonical list in `arch::syscall` and pinned x86's copies to it by
//! `const` assertion — and a review then showed that guard was kernel-internal, while
//! `libthemelios` went on declaring its own 25 numbers that nothing checked. Setting
//! `libthemelios`' `SYS_SEND` to 99 built the entire project green; the only symptom was an
//! amd64 test timing out after 180 seconds, and on aarch64 there would have been no symptom
//! at all until a server ran. That is the same defect the sub-phase exists to remove, one
//! boundary further out. Hence this file: not two lists with a checker between them, one
//! list.

pub mod abi {
    pub const SYS_NULL: u64 = 0;
    pub const SYS_SEND: u64 = 1;
    pub const SYS_RECEIVE: u64 = 2;
    pub const SYS_CALL: u64 = 3;
    pub const SYS_REPLY: u64 = 4;
    pub const SYS_YIELD: u64 = 5;
    pub const SYS_EXIT: u64 = 6;
    pub const SYS_DEBUG_PRINT: u64 = 7;
    pub const SYS_OPEN: u64 = 8;
    pub const SYS_READ_FILE: u64 = 9;
    pub const SYS_WRITE_FILE: u64 = 10;
    pub const SYS_CLOSE: u64 = 11;
    pub const SYS_STAT: u64 = 12;
    pub const SYS_READDIR: u64 = 13;
    pub const SYS_UPTIME_MS: u64 = 14;
    pub const SYS_SOCKET: u64 = 15;
    pub const SYS_BIND: u64 = 16;
    pub const SYS_SENDTO: u64 = 17;
    pub const SYS_RECVFROM: u64 = 18;
    pub const SYS_SOCKET_CLOSE: u64 = 19;
    pub const SYS_TRY_RECEIVE: u64 = 20;
    pub const SYS_CONNECT: u64 = 21;
    pub const SYS_LISTEN: u64 = 22;
    pub const SYS_ACCEPT: u64 = 23;
    pub const SYS_TCP_SEND: u64 = 24;
    pub const SYS_TCP_RECV: u64 = 25;
    pub const SYS_MGMT: u64 = 26;
}
