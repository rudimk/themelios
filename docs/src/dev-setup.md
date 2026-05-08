# Development Setup

This guide walks through setting up a development environment for ThemeliOS on macOS or Linux.

## Prerequisites

### 1. Rust nightly toolchain

ThemeliOS requires Rust nightly because the kernel uses unstable features (`#![no_std]`, `#![no_main]`, inline assembly, custom allocators).

The project pins the exact toolchain via `rust-toolchain.toml`, so you just need `rustup` installed — it will automatically download the correct nightly version.

**Install rustup** (if you don't have it):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

After cloning the repo, the first `cargo` command will automatically install the pinned nightly toolchain plus the bare-metal targets (`x86_64-unknown-none`, `aarch64-unknown-none`).

You can verify with:

```bash
rustup show
```

You should see a nightly toolchain with the `x86_64-unknown-none` and `aarch64-unknown-none` targets listed.

### 2. QEMU

QEMU emulates the hardware that ThemeliOS runs on. You need `qemu-system-x86_64` for the primary amd64 target and optionally `qemu-system-aarch64` for arm64.

**macOS (Homebrew):**

```bash
brew install qemu
```

This installs all QEMU system emulators.

### 3. xorriso

xorriso creates bootable ISO images. The build pipeline uses it to package the kernel with the Limine bootloader into a hybrid BIOS+UEFI ISO.

**macOS (Homebrew):**

```bash
brew install xorriso
```

**Ubuntu/Debian:**

```bash
sudo apt install xorriso
```

**Fedora:**

```bash
sudo dnf install xorriso
```

### 4. C compiler (for Limine CLI tool)

The first `cargo xtask run` downloads and builds the Limine bootloader's CLI tool, which is a small C program. This requires a C compiler.

- **macOS**: Xcode Command Line Tools (`xcode-select --install`)
- **Linux**: `gcc` or `clang` (usually pre-installed)

**Ubuntu/Debian:**

```bash
sudo apt install qemu-system-x86 qemu-system-arm
```

**Fedora:**

```bash
sudo dnf install qemu-system-x86 qemu-system-aarch64
```

**Arch Linux:**

```bash
sudo pacman -S qemu-full
```

Verify installation:

```bash
qemu-system-x86_64 --version
qemu-system-aarch64 --version
```

### 3. mdbook (optional, for building documentation)

```bash
cargo install mdbook
```

## Building and running

All build and run commands go through the `xtask` tool. You never need to invoke `cargo build` for the kernel directly.

### Build the kernel

```bash
cargo xtask build
```

This cross-compiles the kernel for `x86_64-unknown-none` (the default target).

For arm64:

```bash
cargo xtask build --arch arm64
```

### Run in QEMU

```bash
cargo xtask run
```

This builds the kernel and launches it in QEMU with serial output piped to your terminal. Press **Ctrl+A, X** to exit QEMU.

For arm64:

```bash
cargo xtask run --arch arm64
```

### Build documentation

```bash
cargo xtask docs
```

This builds both the mdbook (to `docs/book/`) and the rustdoc API docs.

### Shorthand alias

The workspace defines a `cargo xt` alias, so these also work:

```bash
cargo xt build
cargo xt run
cargo xt docs
```

## Project layout

```
themelios/
├── kernel/          # The kernel crate (#![no_std], bare-metal)
│   └── src/
│       ├── main.rs  # Kernel entry point, module declarations
│       ├── arch/    # Architecture-specific (x86_64, aarch64)
│       ├── mm/      # Memory management
│       ├── sched/   # Scheduler
│       ├── cap/     # Capability system
│       ├── ipc/     # Inter-process communication
│       ├── drivers/ # Device drivers (VirtIO, serial, etc.)
│       ├── fs/      # Filesystem
│       └── net/     # Networking
├── xtask/           # Build tooling (runs on host)
├── docs/            # mdbook documentation
├── .cargo/          # Cargo configuration
└── CLAUDE.md        # Project documentation for AI assistants
```

## IDE setup

### VS Code

Install the `rust-analyzer` extension. It should pick up the workspace configuration automatically.

If `rust-analyzer` struggles with the `#![no_std]` kernel crate, you may need to add this to `.vscode/settings.json`:

```json
{
    "rust-analyzer.cargo.target": "x86_64-unknown-none",
    "rust-analyzer.cargo.buildScripts.enable": true
}
```

### Other editors

Any editor with rust-analyzer LSP support should work. The key setting is ensuring the target is set to `x86_64-unknown-none` for the kernel crate.

## Troubleshooting

### "can't find crate for `core`"

This means the bare-metal target isn't installed. Run:

```bash
rustup target add x86_64-unknown-none aarch64-unknown-none
```

Or let `rust-toolchain.toml` handle it by running any `cargo` command in the project.

### "error: `-Zbuild-std` is unstable"

You need to be on the nightly toolchain. Check with `rustup show` — the project's `rust-toolchain.toml` should select nightly automatically.

### QEMU not found

Make sure QEMU is installed and on your `$PATH`. See the QEMU installation section above.
