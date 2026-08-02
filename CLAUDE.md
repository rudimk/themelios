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
| **6** | Docker-compatible management API | Complete (core; TLS/mTLS, exec/streaming, live docker CLI, networks/images deferred) |
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

**Phase 6 — Management API: COMPLETE (core).** The node is driven entirely
through an external HTTP API (no SSH, no shell). A ring-3 **`api-server`** holds a
`Management` **sentinel capability**, opens an inbound-TCP listener via a
**kernel-accept shim**, and serves a Docker Engine API subset behind **two
authorization layers**: the kernel cap (which *process* may drive the ABI) + an
app-layer **bearer token** (which *client* may call the API). Untrusted HTTP/JSON
parsing stays in ring 3, **fail-closed against a node-halting fault**; every
container mutation crosses into the kernel through the capability-checked, audited
**`SYS_MGMT`** ABI. Full design in mdbook `management-api.md`. `cargo xtask test`
before pushing. **Next up: Phase 7 (aarch64 port).**

- ✅ **6.0** HTTP/1.1 request parser + minimal JSON serializer (`http`, `oci::json`),
  `alloc`-only, fail-closed (bounded sizes, `checked_add`, `None`-on-malformed), lifted
  unchanged into ring 3. `test_http_*`.
- ✅ **6.1b** Container **rootfs confinement** — each container confined to a `/c/<id>`
  subtree via a single `host_path` choke point; `create_confined`. Isolation probe +
  `test_container_confinement`.
- ✅ **6.3** Kernel **management ABI** (`mgmt`) + `CapType::Management` sentinel cap —
  cap-checked, audited (`ApiAccess`) list/inspect/create/start/stop/logs/node_info/
  listen ops returning owned JSON. `test_management_capability`.
- ✅ **6.4** Ring-3 **inbound TCP** + sentinel-cap grant — `ServerConfig::grant_management`
  mints the cap into a kernel-spawned server; the kernel-accept shim (`mgmt::listen`
  mints a listener `Socket` cap) + fail-closed control. Host-coordinated echo test.
- ✅ **6.5** api-server **read pipeline** — `SYS_MGMT` JSON read verbs + wrappers;
  `http`+`json` single-sourced into ring 3 via `#[path]`; GET routing (`_ping`/`version`/
  `info`/`containers/json`/`{id}/json`).
- ✅ **6.5b** **Write verbs** — `SYS_MGMT` create/start/stop/logs; `POST /containers/create`
  (untrusted body JSON → `Image` extraction → NUL-join), start/stop → 204/404/409.
  Deterministic in-process **self-test** (routing + JSON content, no flaky wire path) +
  a single live inbound smoke.
- ✅ **6.6** **Bearer-token auth** — token provisioned via boot-info to the api-server
  only; all routes except `/_ping`/`/version` require `Authorization: Bearer`; missing/
  wrong → **401 before any op** (incl. unknown paths); auth outcomes audited
  (`ApiAccess` / `ApiAuthReject`). Self-test asserts `[200,401,401,200,400,500,409]`.
- ✅ **6.7** Finalize — mdbook `management-api.md`; trackers reconciled; **Momus
  hardening audit** of the untrusted-input surface: **no reachable kernel panic, no
  auth bypass** (fail-closed confirmed end-to-end); landed the F1 empty-token guard;
  F2 (dev secret→Phase 8), F3 (bounded slowloris), F4 (container-count cap), F5 (json
  permissiveness — NUL guard load-bearing) tracked.

Per-milestone branches: each Phase 6 sub-phase was a fresh branch + PR off the
latest `main` (6.3 #17 … 6.6 #21); 6.7 lives on `claude/phase-6.7-finalize`.
**Deferred (documented):** TLS/mTLS, interactive `exec`/websocket streaming, a live
`docker` CLI / multi-request `curl` mutation sequence (net-server RX recycling +
`/data`-at-boot), and broader Engine API breadth (networks, volumes, images).
**Phases 4 (Networking) and 5 (Containers) are complete** — design + sub-phase detail
in mdbook `networking.md` / `containers.md`.

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
