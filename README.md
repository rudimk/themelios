# ThemeliOS

An experimental capability-based microkernel OS written in Rust, designed as a secure foundation for running container workloads in cloud environments.

*Themelio* (θεμέλιο) is Greek for "foundation."

## What is this?

ThemeliOS is a **from-scratch kernel** — it does not use or build on top of Linux. It implements its own memory management, process scheduling, IPC, and a capability-based security model where processes have zero access to anything unless explicitly granted.

The long-term goal is a minimal, immutable OS that boots, runs OCI-compatible containers, and serves as a Kubernetes/K3s worker node.

## Status

Early development — Phase 0 (boot on QEMU) complete. Phase 1 (memory, scheduler, interrupts) up next.

## Quick start

**Prerequisites:** Rust (installed via [rustup](https://rustup.rs/)), QEMU, xorriso

```bash
# Build the kernel (x86_64)
cargo xtask build

# Build and run in QEMU (headless, serial output in terminal)
cargo xtask run

# Same, but with a QEMU graphical window
cargo xtask run --display

# Build a bootable ISO without launching QEMU
cargo xtask iso

# Build for arm64
cargo xtask build --arch arm64
```

The project pins its Rust nightly toolchain via `rust-toolchain.toml` — the first `cargo` command will install it automatically.

## Documentation

Full documentation is built with [mdbook](https://rust-lang.github.io/mdBook/):

- **[Introduction](docs/src/introduction.md)** — project overview and motivation
- **[Development Setup](docs/src/dev-setup.md)** — getting your environment ready
- **[Architecture Overview](docs/src/architecture.md)** — microkernel design, why capabilities
- **[Capability System](docs/src/capabilities.md)** — the core security model
- **[Memory Management](docs/src/memory.md)** — physical/virtual memory design
- **[Milestones](docs/src/milestones.md)** — phased roadmap from boot to containers

To build the docs locally:

```bash
cargo install mdbook
cargo xtask docs
```

## License

MIT — Copyright (c) 2026 Rudi MK
