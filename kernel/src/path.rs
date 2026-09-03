//! # Path resolution and the root clamp
//!
//! One function: [`resolve_path`], which turns a userspace-supplied path into a
//! normalized absolute rootfs path that **cannot escape above the root**.
//!
//! ## Why it lives here and not in `linux::fs`
//!
//! It is pure string logic over `alloc` — no syscall frame, no VFS, no architecture. But
//! until 8.4c it sat in `linux/fs.rs`, and that file opens by importing a syscall frame
//! and the user-copy primitives, which named x86 registers. So the whole of `mod linux`
//! was `#[cfg(target_arch = "x86_64")]`, and `test_path_resolve` — ten cases of string
//! comparison — was skipped on aarch64 with the reason *"path clamping is portable, but
//! `mod linux` is still x86_64-gated"*. The skip reason was correct and named its own fix.
//!
//! Landing [`crate::arch::syscall`] removed the register names, but not the rest of the
//! module's dependencies (`process`, `LinuxFd`, the VFS), so `mod linux` stays gated. The
//! function itself has no such dependencies, so it moves out rather than waiting for
//! them.
//!
//! ## Why that is worth doing rather than leaving the test skipped
//!
//! This is the **container-escape boundary**. Every Linux path syscall funnels through it,
//! and the clamp is the only thing stopping `../../..` in a container from naming a host
//! path. A boundary that is tested on one architecture and merely *assumed* on the other
//! is exactly the asymmetry the aarch64 port exists to remove — and unlike most of the
//! deferred surface, nothing about it is architecture-dependent, so the asymmetry bought
//! nothing.

use alloc::string::String;
use alloc::vec::Vec;

/// Resolve a Linux `path` against `cwd` into a normalized, absolute rootfs path
/// that **cannot escape above the root**.
///
/// Components are processed left to right: `""`/`"."` are skipped, `".."` pops the
/// last component (or stays at `/` if already at the root — the clamp that
/// prevents container escape), everything else is pushed. The result is always
/// absolute (`/`-prefixed) and free of `.`/`..`. This is the security boundary
/// for Linux path syscalls, so it is factored out and unit-tested directly.
pub fn resolve_path(cwd: &str, path: &str) -> String {
    let mut comps: Vec<&str> = Vec::new();
    // Absolute paths start from the root; relative paths start from the cwd.
    let base = if path.starts_with('/') { "" } else { cwd };
    for part in base.split('/').chain(path.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                comps.pop(); // pop() on empty is a no-op → clamped at root
            }
            other => comps.push(other),
        }
    }
    let mut out = String::from("/");
    for (i, c) in comps.iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(c);
    }
    out
}
