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
| **4** | VirtIO net driver, TCP/IP stack | In progress |
| **5** | OCI containers, Linux syscall compat, exec, registries | Not started |
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

**Active: Phase 4 — Networking (in progress).** Design: the TCP/IP stack runs in
a ring-3 **net server** on **smoltcp**; a thin VirtIO-net driver stays in the
kernel; frames cross via a **pull-based** IPC bridge (net server always initiates,
kernel replies). amd64 is the run/test target; arm64-ready by design (smoltcp
compiles for both). `main` is green — keep it that way. The suite is now
reliably green (10/10 soak runs); the long-standing intermittent double-fault /
IPC-race flakiness was root-caused and fixed (see the notes at the end of this
section). Still, `cargo xtask test` before pushing.

- ✅ **4.0** VirtIO-net driver + `NetDevice` trait
- ✅ **4.1** Kernel net service (pull-based frame bridge, `SYS_UPTIME_MS`)
- ✅ **4.2** Ring-3 net server + smoltcp (Device-over-IPC, boot spawn, round-trip test)
- ✅ **4.3** Ethernet/ARP/IPv4/ICMP bring-up — stack answers ping; default route added.
  (Interactive `ping` shell cmd + outbound round-trip → moved to 4.5, they need the
  client request path.)
- ✅ **4.4** DHCPv4 client — replaces the placeholder static IP. Ring-3 smoltcp
  `dhcpv4::Socket` acquires addr/gateway/DNS from slirp; the server reports it to
  the kernel over a new `MSG_CONFIG` opcode; `ifconfig` shell cmd + `test_dhcp`
  (real end-to-end acquisition vs slirp) added. DHCP is gated behind an `arg1`
  flag (`NET_ARG_DHCP`) so the static-IP round-trip tests stay deterministic.
  **Off-ramp milestone reached: link up + DHCP address ("it's on the network").**
- ✅ **4.5** UDP sockets + `CapType::Socket` + syscalls (15–19). Non-blocking
  `ipc_try_receive` (`SYS_TRY_RECEIVE` 20) lets the net server serve client
  requests while polling smoltcp; `CapType::Socket` with a `SOCKET_FACTORY`
  authority sentinel (create-sockets right) + per-socket caps (send/recv); kernel
  socket router (`net/socket.rs`) checks caps → routes to the net server → moves
  payloads via a shared region → audits `NetAccess`. Net server keeps a UDP
  socket table served over `try_receive`. `udpsend` shell cmd; `test_socket_capability`
  + `test_udp_echo` (live DNS round-trip vs slirp) added. **32 tests pass.**
- 🟡 **4.6** TCP sockets — **client path done** (`SOCK_TYPE_TCP`; `connect`/stream
  `send`/`recv`/`close`; syscalls `SYS_CONNECT` 21, `SYS_TCP_SEND` 24, `SYS_TCP_RECV`
  25; non-blocking with a `TcpPhase` state machine → WouldBlock while connecting,
  ConnectionRefused on reset). `tcpconnect` shell cmd + `test_tcp_client` (33 tests,
  reliably green). smoltcp `tcp::Socket`s in the net server keyed by kind.
  **← server path NEXT:** `listen`/`accept` (syscalls 22/23) with a listening-socket
  pool + cap-mint-on-accept, and a deterministic round-trip test via a host-side
  listener over QEMU `hostfwd` (slirp has no reachable TCP endpoint, so the client
  test only validates plumbing; the real data round-trip needs the server path).
- ⬜ **4.7** shell (`sockets`/`ping`), boot integration, remaining tests, mdbook network doc.

Per-milestone branches now: 4.6 lives on `claude/phase-4.6-tcp-sockets` (fresh PR),
cut from `main` after the 4.4/4.5 PR merged.

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
