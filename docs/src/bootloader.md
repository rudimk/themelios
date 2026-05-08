# Bootloader

ThemeliOS uses the [Limine](https://github.com/limine-bootloader/limine) bootloader. This page explains why, how it works, and how it fits into the build pipeline.

## Why Limine?

We evaluated several options for booting ThemeliOS:

| Option | Pros | Cons |
|--------|------|------|
| Custom UEFI app | Full control | Massive effort, x86_64 UEFI only initially |
| Multiboot2 | Simple, QEMU `-kernel` flag | BIOS only, no arm64, no UEFI |
| `bootloader` crate | Very easy Rust integration | x86_64 only, no arm64 |
| **Limine** | **BIOS + UEFI, x86_64 + arm64, well-maintained** | External dependency |

Limine was chosen because:

1. **Multi-architecture**: Supports x86_64 and aarch64 (and RISC-V, LoongArch). We need both for our cloud targets.
2. **Multi-firmware**: Works on both BIOS (legacy) and UEFI (modern). Cloud platforms use UEFI; QEMU defaults to BIOS.
3. **Higher-half kernel**: Limine sets up page tables that map our kernel at `0xffffffff80000000`, which is the standard layout for 64-bit kernels.
4. **Clean protocol**: The Limine boot protocol gives us a memory map, framebuffer, and other boot info without writing any assembly.
5. **Active maintenance**: Regular releases, good documentation.

## Cloud compatibility

Limine's UEFI support means ThemeliOS can boot on:

- **AWS EC2** (Nitro): UEFI supported on most instance types
- **GCP Compute Engine**: UEFI supported
- **Azure Gen2 VMs**: UEFI
- **Bare metal**: UEFI is standard on modern server hardware
- **QEMU/KVM**: Both BIOS (default) and UEFI (via OVMF)

The same kernel binary works on all platforms — only the bootloader firmware interface differs, and Limine handles that.

## How it works

### Boot sequence

1. **Firmware** (BIOS or UEFI) loads the Limine bootloader from the boot media
2. **Limine** reads `limine.conf` to find the kernel path and boot protocol
3. **Limine** loads the kernel ELF into memory at the addresses specified in the linker script
4. **Limine** sets up:
   - 64-bit long mode (x86_64) or EL1 (aarch64)
   - 4-level page tables with identity + higher-half mappings
   - A valid stack
5. **Limine** scans the kernel's `.requests` ELF section for boot protocol requests
6. **Limine** fills in the requests (memory map, framebuffer, etc.)
7. **Limine** jumps to the kernel entry point (`kmain`)

### Boot protocol requests

The kernel communicates with Limine through static data structures placed in a special ELF section. These are "requests" — the kernel declares what boot information it needs, and Limine fills in the responses.

```rust
// Placed in the .requests ELF section via the linker script
#[used]
#[link_section = ".requests"]
static BASE_REVISION: BaseRevision = BaseRevision::new();
```

The linker script places these between start/end markers so Limine knows where to scan:

```
.data : {
    ...
    KEEP(*(.requests_start_marker))
    KEEP(*(.requests))
    KEEP(*(.requests_end_marker))
}
```

### Configuration file

`limine.conf` (in the project root) uses the v8 format:

```
timeout: 0

/ThemeliOS
    protocol: limine
    kernel_path: boot():/boot/themelios
```

- `timeout: 0` — boot immediately without showing a menu
- `/ThemeliOS` — defines a boot entry
- `protocol: limine` — use the Limine protocol (not Linux or Multiboot)
- `kernel_path: boot():/boot/themelios` — load the kernel from the boot volume

### Linker script

The linker script (`kernel/linker-x86_64.ld`) controls the kernel's memory layout:

- **Entry point**: `ENTRY(kmain)` — tells the ELF where execution begins
- **Load address**: `0xffffffff80000000` — the higher-half virtual address
- **Sections**: `.text` (code), `.rodata` (constants), `.data` (mutable data + Limine requests), `.bss` (zeroed data)

The kernel must be compiled with `-Crelocation-model=static` to produce a non-PIE executable with fixed addresses that match the linker script.

## Build pipeline

The `cargo xtask run` command handles the full pipeline:

1. **Cross-compile** the kernel for `x86_64-unknown-none`
2. **Download Limine** (one-time: `git clone` of the `v8.x-binary` branch to `target/limine/`)
3. **Build Limine CLI** (one-time: `make` compiles `limine.c`)
4. **Create ISO** via `xorriso`:
   - Copies kernel, Limine files, and `limine.conf` into an ISO directory structure
   - Creates a hybrid BIOS+UEFI bootable ISO
   - Installs BIOS boot sectors via `limine bios-install`
5. **Launch QEMU** with the ISO attached as a CD-ROM

## Limine version

- **Bootloader**: v8.x (binary distribution from `v8.x-binary` branch)
- **Rust crate**: `limine = "0.5"` (boot protocol structures)

The bootloader binaries are cached in `target/limine/` and not committed to git.
