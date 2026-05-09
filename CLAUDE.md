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

- Rust nightly toolchain (pinned via `rust-toolchain.toml`)
- QEMU: `qemu-system-x86_64` and `qemu-system-aarch64`
- mdbook (for documentation)

See the [development setup guide](docs/src/dev-setup.md) in the mdbook for detailed instructions.

## Milestone Roadmap

**Important**: Milestone status is tracked in two places that must be kept in sync:
1. This table below
2. `docs/src/milestones.md` — both the summary table at the top and the inline status label on each phase heading

When starting or completing a phase, update all three locations (this table, the docs summary table, and the docs phase heading).

| Phase | Goal | Status |
|-------|------|--------|
| **0** | Boot on QEMU, serial output | Complete |
| **1** | Memory allocator, scheduler, interrupts | Not started |
| **2** | Capability system, process isolation, IPC, audit logging | Not started |
| **3** | VirtIO block driver, read-only FS, ephemeral layers | Not started |
| **4** | VirtIO net driver, TCP/IP stack | Not started |
| **5** | OCI containers, Linux syscall compat, exec, registries | Not started |
| **6** | Docker-compatible management API | Not started |
| **7** | Hyperscaler support (AWS, GCP, Azure), secure boot | Not started |
| **8** | Testing and benchmarks | Not started |
| **9** | Kubernetes worker node (full parity) | Not started |
| **10** | GPU support across clouds | Not started |
| **11** | Production operations (observability, updates) | Not started |

## License

MIT — Copyright (c) 2026 Rudi MK
