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

### 5. Filesystem image tools (squashfs, e2fsprogs)

Phase 3 (storage) builds the disk images ThemeliOS boots from using two host
tools, invoked by `cargo xtask image`:

- **`mksquashfs`** (from `squashfs-tools`) — builds the compressed, read-only
  SquashFS root image.
- **`mkfs.ext2`** (from `e2fsprogs`) — formats the read-write ext2 data volume.

**macOS (Homebrew):**

```bash
brew install squashfs e2fsprogs
```

> **Note:** `e2fsprogs` is *keg-only* on macOS (Apple ships conflicting
> versions), so Homebrew does not symlink `mkfs.ext2` onto your `PATH`. `xtask`
> handles this automatically — it looks for `mkfs.ext2` on `PATH` and falls back
> to `/opt/homebrew/opt/e2fsprogs/sbin/mkfs.ext2` (and the Intel
> `/usr/local/opt/...` location). You do **not** need to edit your `PATH`.

**Ubuntu/Debian:**

```bash
sudo apt install squashfs-tools e2fsprogs
```

**Fedora:**

```bash
sudo dnf install squashfs-tools e2fsprogs
```

**Arch Linux:**

```bash
sudo pacman -S squashfs-tools e2fsprogs
```

### Installing everything at once (macOS)

The repo ships a [`Brewfile`](https://github.com/Homebrew/homebrew-bundle) that
declares every macOS host dependency (QEMU, xorriso, squashfs, e2fsprogs). From
the repo root:

```bash
brew bundle
```

## Building and running

All build and run commands go through the `xtask` tool. You never need to invoke `cargo build` for the kernel directly.

### Choosing the architecture: `--arch`

Every command that produces or runs a kernel takes **one** architecture flag, spelled the
same way everywhere:

```bash
--arch amd64      # or x86_64, or x86-64
--arch arm64      # or aarch64
```

There is deliberately no `--amd64` / `--arm64` shorthand. Two spellings of one selector
is two things to keep in sync, and the failure mode is quiet: a command that silently
ignores an unrecognised flag builds the *default* architecture and reports success. One
spelling, and an unknown flag is an error.

Omitting `--arch` selects **amd64**, because that is the primary target. It is not the
only gated one — CI runs the full suite on both architectures, plus three arm64 boot
smokes, and the release job requires all of them. The default is a convenience, not a
recommendation: when you are working across both architectures, name it on both sides:

```bash
cargo xtask test --arch amd64
cargo xtask test --arch arm64
```

so that the two commands read as a pair and neither is "the one where the flag is
missing".

The rest of this section shows both forms for each command.

### Build the kernel

```bash
cargo xtask build --arch amd64    # x86_64-unknown-none
cargo xtask build --arch arm64    # aarch64-unknown-none-softfloat
```

### Run in QEMU

```bash
cargo xtask run --arch amd64
cargo xtask run --arch arm64
```

This builds the kernel, creates a bootable ISO, and launches it in QEMU in **headless
mode** — serial output is piped to your terminal, but no graphical window opens. You land
at the ThemeliOS debug shell; type `help` for commands. Press **Ctrl+A, X** to exit QEMU.

The two are not equivalent in what they can reach: amd64 boots the full stack (storage,
networking, containers, the management API), while arm64 is a ring-0 kernel-core port —
memory, scheduling, interrupts, VirtIO device discovery and the shell. Commands whose
subsystem is not yet ported are absent from the arm64 shell rather than present and
broken; the filesystem, socket and container commands all still need ring 3 (EL0), which
lands in 8.4/8.5.

Both architectures are launched with three VirtIO disks and a NIC attached, so `discovery`
finds the same device set the test suite does.

### Run the test suite

```bash
cargo xtask test --arch amd64
cargo xtask test --arch arm64
```

Both boot the kernel under QEMU with the test harness compiled in, run the same 55-test
suite, and exit with a non-zero status if anything fails. Tests whose subsystem is not
ported to an architecture report `[SKIP]` with the reason, so the totals line reads
`N passed, 0 failed, M skipped, 55 total` on both — the suite size is asserted, so a test
cannot go missing silently.

Both suites bind a fixed host TCP port for the networking tests, so **two suites cannot
run at once**. If you need to (or a stale QEMU is holding the port), override it:

```bash
THEMELIOS_TEST_PORT=15107 cargo xtask test --arch amd64
```

### Build ISO only (without launching QEMU)

```bash
cargo xtask iso --arch amd64   # target/themelios-amd64.iso
cargo xtask iso --arch arm64   # target/themelios-arm64.iso
```

This builds the kernel and creates a bootable ISO without launching QEMU. Useful when
you want to run QEMU manually with custom flags.

The two images are **not** interchangeable, and differ by more than the kernel inside
them:

| Image | Platform | Firmware | Boot structure |
|-------|----------|----------|----------------|
| `themelios-amd64.iso` | x86_64 | BIOS or UEFI | Hybrid: BIOS El Torito + `limine-bios.sys` + a `limine bios-install` pass, plus an EFI El Torito image with `BOOTX64.EFI` |
| `themelios-arm64.iso` | aarch64 | UEFI only | EFI El Torito only, with `BOOTAA64.EFI` — QEMU `virt` and arm64 platforms generally have no BIOS |

To check that the arm64 image boots (needs `qemu-system-aarch64` and the AAVMF
firmware):

```bash
cargo xtask arm64-iso-smoke
```

### Run with QEMU display window

To see the QEMU graphical window (shows the Limine bootloader screen and any framebuffer output):

```bash
cargo xtask run --display
```

This does everything `cargo xtask run` does but opens a QEMU window instead of running headless. Serial output still goes to your terminal.

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
