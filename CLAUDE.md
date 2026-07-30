# ThemeliOS

An experimental capability-based microkernel OS written in Rust, designed as a secure foundation for running container workloads in cloud environments. "Themelio" (θεμέλιο) is Greek for "foundation."

## Project Overview

ThemeliOS is a from-scratch kernel — it does not use or build on top of Linux. It implements its own scheduler, memory manager, IPC, and isolation primitives. The long-term goal is a minimal, immutable OS that boots, runs OCI-compatible containers, and serves as a Kubernetes/K3s worker node.

## Architecture

- **Microkernel**: minimal kernel providing memory management, scheduling, IPC, and capability enforcement. Everything else (drivers, filesystems, networking) runs in userspace.
- **Capability-based security**: all resource access is mediated by unforgeable capability tokens. No ambient authority — processes can only access resources they've been explicitly granted.
- **Immutable root**: the root filesystem is read-only. Nodes are cattle, not pets. Updates are whole-image swaps.
- **No SSH, no shell**: all management is via an external API. There is no interactive login.
- **Logging**: RAM-backed ring buffer with real-time streaming to external collectors. No persistent log storage on-node.

## Target Platforms

- **Primary**: x86_64 (amd64)
- **Secondary**: aarch64 (arm64)
- **Dev/test**: QEMU/KVM
- **Future**: bare metal (headless), AWS, GCP, Azure

## Tech Stack

- **Language**: Rust (nightly, `#![no_std]`)
- **Build system**: `cargo` workspace with `xtask` pattern (no Makefile)
- **Testing**: QEMU-based boot tests via `cargo xtask run`
- **Docs**: `rustdoc` for code, `mdbook` for architecture and usage documentation
- **CI**: GitHub Actions (future)

## Project Structure

```
themelios/
├── kernel/              # The kernel crate (no_std)
│   ├── src/
│   │   ├── main.rs      # Kernel entry point
│   │   ├── arch/        # Architecture-specific code (x86_64, aarch64)
│   │   ├── mm/          # Memory management (physical, virtual, allocator)
│   │   ├── sched/       # Scheduler
│   │   ├── cap/         # Capability system
│   │   ├── ipc/         # Inter-process communication
│   │   ├── drivers/     # VirtIO and platform drivers
│   │   ├── fs/          # Filesystem (read-only root, ephemeral layers)
│   │   └── net/         # Network stack
│   └── Cargo.toml
├── xtask/               # Build/run/test tooling (runs on host)
│   ├── src/
│   │   └── main.rs
│   └── Cargo.toml
├── docs/                # mdbook documentation
│   ├── book.toml
│   └── src/
│       ├── SUMMARY.md
│       └── ...
├── Cargo.toml           # Workspace root
├── rust-toolchain.toml  # Pins nightly toolchain
├── CLAUDE.md            # This file
├── LICENSE              # MIT
└── .cargo/
    └── config.toml      # Per-target build flags, linker scripts
```

## Code Style

### Comments and Documentation

**All code must be extensively commented.** This is a learning project and a complex system — comments are mandatory, not optional.

- Every public function, struct, enum, trait, and module gets a `///` doc comment explaining what it does, why it exists, and how it fits into the larger system.
- Non-obvious logic, Rust idioms, and OS/hardware concepts get inline `//` comments.
- Each module (`mod.rs` or top-level file) gets a `//!` module-level doc comment explaining the module's purpose and design.
- When implementing OS concepts (page tables, scheduling algorithms, capability systems), explain the concept, not just the code.

### Rust Style

- Follow standard Rust conventions (`rustfmt` defaults).
- Use `clippy` with default lints.
- Prefer safe Rust. Use `unsafe` only where hardware interaction or performance demands it, and always document why it's safe with a `// SAFETY:` comment.
- All `unsafe` blocks must have a corresponding safety comment.

## Build Commands

```bash
# Build the kernel (defaults to x86_64)
cargo xtask build

# Build for a specific architecture
cargo xtask build --arch aarch64

# Build kernel and create bootable ISO (without launching QEMU)
cargo xtask iso

# Build, create ISO, and run in QEMU (headless, serial output to terminal)
cargo xtask run

# Same, but with a QEMU graphical window
cargo xtask run --display

# Build and run on arm64 QEMU
cargo xtask run --arch aarch64

# Run tests
cargo xtask test

# Build documentation
cargo xtask docs
```

## Development Setup

### Prerequisites

Host tools the build/test pipeline shells out to (Rust itself is managed by
rustup via `rust-toolchain.toml`, not the OS package manager):

- Rust nightly toolchain (pinned via `rust-toolchain.toml`)
- QEMU: `qemu-system-x86_64` and `qemu-system-aarch64`
- `xorriso` (bootable ISO creation)
- `mksquashfs` from `squashfs` / `squashfs-tools` (SquashFS root image, Phase 3)
- `mkfs.ext2` from `e2fsprogs` (ext2 data volume image, Phase 3; keg-only on
  macOS — `xtask` resolves the keg path automatically)
- mdbook (for documentation)

On macOS these are declared in the repo's `Brewfile` — run `brew bundle` to
install them all. See the [development setup guide](docs/src/dev-setup.md) in
the mdbook for per-OS instructions.

## Milestone Roadmap

**Important**: Milestone status is tracked in two places that must be kept in sync:
1. This table below
2. `docs/src/milestones.md` — both the summary table at the top and the inline status label on each phase heading

When starting or completing a phase, update all three locations (this table, the docs summary table, and the docs phase heading).

| Phase | Goal | Status |
|-------|------|--------|
| **0** | Boot on QEMU, serial output | Complete |
| **1** | Memory allocator, scheduler, interrupts (x86_64) | Complete |
| **2** | Capability system, process isolation, IPC, audit logging | Complete |
| **3** | VirtIO block driver, read-only FS, ephemeral layers | Complete |
| **4** | VirtIO net driver, TCP/IP stack | Complete |
| **5** | OCI containers, Linux syscall compat, exec, registries | Complete (core; real-image busybox, live registry transport, ring-3 oci-server deferred) |
| **6** | Docker-compatible management API | Not started |
| **7** | aarch64 port (boot, memory, scheduler, shell) | Not started |
| **8** | Hyperscaler support (AWS, GCP, Azure), secure boot | Not started |
| **9** | Testing and benchmarks | Not started |
| **10** | Kubernetes worker node (full parity) | Not started |
| **11** | GPU support across clouds | Not started |
| **12** | Production operations (observability, updates) | Not started |

## Current Status — resume here

_Single source of truth for "where are we / what's next". Update the relevant
line when finishing a sub-phase. Detailed per-sub-phase checklists live in
`.sisyphus/plans/` (local, gitignored); the git commit history has the full
narrative per commit._

**Phase 5 — Containers: COMPLETE (core).** A container is an ordinary process
made to believe it's on Linux, in its own rootfs, holding no capabilities:
(1) a **Linux syscall personality** (a per-process Linux-ABI table, routed by a
personality flag so it doesn't collide with the native ABI); (2) a **single
rootfs mount** with a `..`-clamping path resolver; (3) an **empty capability
space** — so every privileged op is denied by construction. The pipeline is
`image → unpack (oci) → assemble rootfs (VFS) → load entrypoint ELF → ring-3
Linux process`. Isolation is the **capability system**, not namespaces — it
falls out of the microkernel model. `main` is green (46 tests, 3× soak clean);
`cargo xtask test` before pushing. Full design in mdbook `containers.md`.
**Next up: Phase 6 (Docker-compatible management API).**

- ✅ **5.0** ELF64 loader + `exec` — parse ET_EXEC, map PT_LOAD (`W^X`), build the
  SysV initial stack (argc/argv/envp/auxv), enter ring 3 at `e_entry`. `elf-smoke`
  (native ABI) + `test_elf_exec`.
- ✅ **5.1** Linux syscall personality — `Personality::{Native,Linux}` routes
  dispatch to a Linux table (write/writev/brk/mmap/arch_prctl/ioctl/clock_gettime/
  getrandom/exit_group/…). Per-task `%fs` base restored on context switch (TLS).
  `linux-smoke` + `test_linux_exec`.
- ✅ **5.2** Linux filesystem syscalls over the VFS (openat/read/write/close/lseek/
  fstat/newfstatat/getdents64/getcwd/chdir/readlinkat), rooted at one mount; the
  path resolver **clamps `..` at rootfs** (container-escape prevention). `fs-smoke`
  + `test_path_resolve` + `test_linux_fs`.
- ✅ **5.3** Linux threads — `clone(CLONE_THREAD)` (sibling task, child resumes at
  parent RIP with rax=0), `futex` WAIT/WAKE (address-keyed wait queue),
  `set_tid_address`, `exit` vs `exit_group`. `threads-smoke` + `test_linux_threads`.
- ✅ **5.4** OCI image unpacking — `docker save` bundles (outer tar → manifest +
  config + uncompressed layer tars), layers applied in order with whiteouts
  resolved, into a flat rootfs + runtime config. Hand-rolled tar + JSON readers
  (`alloc`-only). `test_oci_unpack`.
- ✅ **5.5** Container runtime — unpack → assemble rootfs on the ext2 mount → load
  the entrypoint **from that rootfs** (`VfsByteSource`) → run as a Linux process;
  `exit_group` captures the exit status. `run` shell cmd; `test_container_run`
  (linux-smoke as `/init`, end-to-end).
- ✅ **5.6** Registry pull — Docker Registry HTTP API v2 over a `Connection` trait:
  manifest v2 → config + **gzip** layer blobs by `sha256:` digest, each
  **digest-verified before use**; `oci/{sha256,gzip,registry}.rs` (+ `miniz_oxide`).
  Fail-closed parsers: bounded gzip inflate (bomb cap), bounded JSON depth,
  `checked_add` lengths (Momus-hardened). `test_sha256`, `test_registry_pull`,
  `test_registry_hardening`. Live TCP transport (slirp `guestfwd` + host
  `registry:2`) deferred; the pull pipeline is fully tested offline.
- ✅ **5.7** Enforced isolation + teardown + finalize — Linux `socket()` (nr 41) →
  **`-EPERM`** (no `SOCKET_FACTORY` cap; checked errno, not `-ENOSYS`); `kill`
  (self-only, else `-EPERM`) / `wait4` (`-ECHILD`); `container::terminate` +
  `stop <pid>` (container-type-guarded), plus a **`destroy_process` UAF fix**
  (tasks marked Dead *before* the address space is freed). `isolation-smoke` +
  `test_container_isolation` prove the boundary on the live syscall path:
  positive read OK, `../../../../only` **succeeds and byte-matches** `/only`
  (live `..` clamp), absent path misses, `socket()` → `-EPERM`. mdbook
  `containers.md` written; milestone trackers reconciled. **46 tests, 3× soak.**
  Deferred (documented): real static-musl busybox over a live registry, container
  `exec`, real `wait4`/signal-handler delivery, ring-3 `oci-server` relocation.

Per-milestone branches: each Phase 5 sub-phase was a fresh branch + PR off the
latest `main` (5.0 #4 … 5.6 #10); 5.7 lives on `claude/phase-5.7-finalize`.
**Phase 4 (Networking) is complete** — design + sub-phase detail in mdbook
`networking.md`.

Notable — three long-standing concurrency bugs in the syscall/IPC core were
root-caused and fixed during Phase 4.5 (they grew from rare to majority-of-runs
as the suite added tasks/syscalls; `b1b7cea` had only partially masked the
first):
- **Syscall-exit double-fault.** `PerCpu.user_rsp_scratch` (`gs:0x8`) is a single
  shared slot; the syscall exit re-stashed the user RSP there and read it back
  with interrupts *enabled*, so a preempting syscall clobbered it and the task
  `sysretq`'d onto another task's stack. Fixed by making the exit tail atomic
  (`cli`; `sysretq` restores IF).
- **Stale GS base.** GS base is a global register not saved per task, and the
  interrupt stubs never `swapgs`, so an interrupt in ring 3 could context-switch
  with the user GS base. Fixed by writing `IA32_GS_BASE = &PER_CPU` on every
  context switch (not just `KERNEL_GS_BASE`).
- **Spurious IPC "call failed".** `ipc_receive` woke a popped caller-sender
  before its reply was delivered (when `ipc_call` raced ahead of the server
  parking). Fixed by only waking plain sends (`reply_token == 0`); callers wait
  for `ipc_reply`. Same guard as `ipc_try_receive`.

## License

MIT — Copyright (c) 2026 Rudi MK
