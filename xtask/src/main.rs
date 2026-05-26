//! # ThemeliOS xtask — build and development tooling
//!
//! This binary runs on the host machine (macOS, Linux) and handles:
//! - Cross-compiling the kernel for bare-metal targets
//! - Downloading the Limine bootloader (one-time setup)
//! - Creating bootable ISO images
//! - Launching QEMU for testing
//! - Building documentation
//!
//! ## Usage
//!
//! ```sh
//! cargo xtask build              # Build kernel for x86_64
//! cargo xtask build --arch arm64 # Build kernel for aarch64
//! cargo xtask run                # Build + create ISO + launch QEMU (x86_64)
//! cargo xtask run --arch arm64   # Build + launch QEMU (aarch64)
//! cargo xtask test               # Build + run kernel tests in QEMU
//! cargo xtask docs               # Build mdbook + rustdoc
//! ```
//!
//! The `cargo xt` alias also works (defined in `.cargo/config.toml`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

// ============================================================================
// Build constants
// ============================================================================

/// The `-Zbuild-std` flag for building the kernel. We build `core` (the no_std
/// standard library) and `alloc` (dynamic allocation: Vec, Box, String, etc.)
/// from source for the bare-metal target. This flag must be the same everywhere
/// we invoke `cargo build` or `cargo doc` for the kernel.
const BUILD_STD: &str = "-Zbuild-std=core,alloc";

/// The `-Zbuild-std-features` flag. Enables `compiler-builtins-mem` which
/// provides memory intrinsics (memcpy, memset, etc.) that the compiler may
/// generate calls to. Without this, linking fails on bare-metal targets.
const BUILD_STD_FEATURES: &str = "-Zbuild-std-features=compiler-builtins-mem";

// ============================================================================
// Architecture helpers
// ============================================================================

/// Maps user-facing architecture names to Rust target triples.
/// "x86_64" and "amd64" both map to x86_64-unknown-none, etc.
fn resolve_target(arch: &str) -> &'static str {
    match arch {
        "x86_64" | "amd64" | "x86-64" => "x86_64-unknown-none",
        "aarch64" | "arm64" => "aarch64-unknown-none",
        other => {
            eprintln!("Error: unknown architecture '{other}'");
            eprintln!("Supported: x86_64 (amd64), aarch64 (arm64)");
            process::exit(1);
        }
    }
}

/// Returns the QEMU binary name for the given architecture.
fn qemu_binary(arch: &str) -> &'static str {
    match arch {
        "x86_64" | "amd64" | "x86-64" => "qemu-system-x86_64",
        "aarch64" | "arm64" => "qemu-system-aarch64",
        _ => unreachable!(),
    }
}

// ============================================================================
// Path helpers
// ============================================================================

/// Returns the path to the workspace root (the directory containing the
/// top-level Cargo.toml). We find it by walking up from the xtask binary's
/// manifest directory.
fn workspace_root() -> PathBuf {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| ".".to_string());
    Path::new(&manifest_dir)
        .parent()
        .expect("xtask must be in a subdirectory of the workspace root")
        .to_path_buf()
}

/// Parsed command-line options shared across subcommands.
struct Options {
    /// Target architecture (default: x86_64).
    arch: String,
    /// Show the QEMU graphical display window instead of running headless.
    display: bool,
}

/// Parses --arch and --display flags from the argument list.
fn parse_options(args: &[String]) -> Options {
    let mut arch = "x86_64".to_string();
    let mut display = false;
    let mut skip_next = false;

    for (i, arg) in args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--arch" {
            if let Some(next) = args.get(i + 1) {
                arch = next.clone();
                skip_next = true;
            } else {
                eprintln!("Error: --arch requires a value (x86_64, arm64)");
                process::exit(1);
            }
        } else if let Some(value) = arg.strip_prefix("--arch=") {
            arch = value.to_string();
        } else if arg == "--display" {
            display = true;
        }
    }

    Options { arch, display }
}

// ============================================================================
// Limine bootloader management
// ============================================================================

/// The git branch for pre-built Limine bootloader binaries.
const LIMINE_BRANCH: &str = "v8.x-binary";

/// Ensure the Limine bootloader binaries are available locally.
///
/// On first run, this clones the Limine binary distribution from GitHub
/// into `target/limine/`. On subsequent runs, it reuses the cached copy.
/// After cloning, it builds the `limine` CLI tool (a small C program used
/// to install BIOS boot sectors on the ISO).
fn ensure_limine(root: &Path) -> PathBuf {
    let limine_dir = root.join("target/limine");

    if limine_dir.join("limine").exists() || limine_dir.join("limine.exe").exists() {
        // Already set up — the CLI tool exists
        return limine_dir;
    }

    if !limine_dir.exists() {
        println!("Downloading Limine bootloader (one-time setup)...");

        // Clone only the binary distribution branch (pre-compiled bootloader files).
        // --depth=1 avoids downloading the full history.
        let status = Command::new("git")
            .args([
                "clone",
                "https://github.com/limine-bootloader/limine.git",
                &format!("--branch={LIMINE_BRANCH}"),
                "--depth=1",
            ])
            .arg(limine_dir.to_str().unwrap())
            .status()
            .expect("Failed to run git. Is git installed?");

        if !status.success() {
            eprintln!("Failed to clone Limine bootloader.");
            eprintln!("Check your internet connection and try again.");
            process::exit(1);
        }
    }

    // Build the Limine CLI tool. This compiles a single C file (limine.c)
    // into the `limine` binary, which is used to install BIOS boot sectors
    // onto the ISO image. Requires a C compiler (cc/gcc/clang).
    println!("Building Limine CLI tool...");
    let status = Command::new("make")
        .current_dir(&limine_dir)
        .arg("-C")
        .arg(limine_dir.to_str().unwrap())
        .status()
        .expect("Failed to run make. Are build tools (Xcode CLI tools) installed?");

    if !status.success() {
        eprintln!("Failed to build Limine CLI tool.");
        eprintln!("Ensure you have a C compiler installed (Xcode Command Line Tools on macOS).");
        process::exit(1);
    }

    limine_dir
}

// ============================================================================
// ISO image creation
// ============================================================================

/// Create a bootable ISO image containing the kernel and Limine bootloader.
///
/// The ISO is a hybrid BIOS + UEFI image:
/// - BIOS boot: uses Limine's El Torito boot sector (limine-bios-cd.bin)
/// - UEFI boot: uses Limine's UEFI CD image (limine-uefi-cd.bin)
///
/// This means the same ISO works on both legacy BIOS and modern UEFI systems.
///
/// Directory structure inside the ISO:
/// ```
/// /boot/limine/limine-bios.sys    — Limine BIOS second-stage loader
/// /boot/limine/limine-bios-cd.bin — BIOS El Torito boot image
/// /boot/limine/limine-uefi-cd.bin — UEFI El Torito boot image
/// /boot/limine/limine.conf        — Bootloader configuration
/// /boot/themelios                  — The kernel ELF binary
/// /EFI/BOOT/BOOTX64.EFI          — UEFI fallback bootloader
/// ```
fn create_iso(root: &Path, kernel_path: &Path, limine_dir: &Path) -> PathBuf {
    let iso_root = root.join("target/iso_root");
    let iso_path = root.join("target/themelios.iso");

    println!("Creating bootable ISO...");

    // Clean and create the ISO directory structure
    let _ = fs::remove_dir_all(&iso_root);
    fs::create_dir_all(iso_root.join("boot/limine"))
        .expect("Failed to create ISO directory structure");
    fs::create_dir_all(iso_root.join("EFI/BOOT"))
        .expect("Failed to create EFI directory");

    // Copy the kernel binary
    fs::copy(kernel_path, iso_root.join("boot/themelios"))
        .expect("Failed to copy kernel to ISO root");

    // Copy Limine bootloader files
    let limine_files = [
        ("limine-bios.sys", "boot/limine/limine-bios.sys"),
        ("limine-bios-cd.bin", "boot/limine/limine-bios-cd.bin"),
        ("limine-uefi-cd.bin", "boot/limine/limine-uefi-cd.bin"),
        ("BOOTX64.EFI", "EFI/BOOT/BOOTX64.EFI"),
    ];

    for (src_name, dst_path) in &limine_files {
        let src = limine_dir.join(src_name);
        if !src.exists() {
            eprintln!("Missing Limine file: {}", src.display());
            eprintln!("Try deleting target/limine/ and running again.");
            process::exit(1);
        }
        fs::copy(&src, iso_root.join(dst_path))
            .unwrap_or_else(|e| panic!("Failed to copy {src_name}: {e}"));
    }

    // Copy the Limine boot configuration
    let limine_conf = root.join("limine.conf");
    if !limine_conf.exists() {
        eprintln!("Missing limine.conf in project root!");
        process::exit(1);
    }
    fs::copy(&limine_conf, iso_root.join("boot/limine/limine.conf"))
        .expect("Failed to copy limine.conf");

    // Create the ISO image using xorriso.
    //
    // xorriso -as mkisofs: run xorriso in mkisofs compatibility mode
    // -b: BIOS El Torito boot image (relative to ISO root)
    // -no-emul-boot: boot image is not a floppy emulation
    // -boot-load-size 4: load 4 sectors of the boot image
    // -boot-info-table: patch the boot image with ISO info
    // --efi-boot: UEFI El Torito boot image
    // -efi-boot-part: create an EFI partition in the ISO
    // --efi-boot-image: the EFI boot image is also accessible as a file
    // --protective-msdos-label: add a protective MBR for hybrid boot
    let status = Command::new("xorriso")
        .args([
            "-as", "mkisofs",
            "-b", "boot/limine/limine-bios-cd.bin",
            "-no-emul-boot",
            "-boot-load-size", "4",
            "-boot-info-table",
            "--efi-boot", "boot/limine/limine-uefi-cd.bin",
            "-efi-boot-part",
            "--efi-boot-image",
            "--protective-msdos-label",
        ])
        .arg(iso_root.to_str().unwrap())
        .arg("-o")
        .arg(iso_path.to_str().unwrap())
        .status()
        .expect("Failed to run xorriso. Install with: brew install xorriso");

    if !status.success() {
        eprintln!("ISO creation failed!");
        process::exit(1);
    }

    // Install Limine's BIOS boot sectors onto the ISO.
    // This patches the ISO's MBR so that BIOS firmware can boot from it
    // without relying on El Torito (which some BIOSes don't support for
    // hard drive-style boot).
    let limine_cli = limine_dir.join("limine");
    let status = Command::new(limine_cli.to_str().unwrap())
        .args(["bios-install"])
        .arg(iso_path.to_str().unwrap())
        .status()
        .expect("Failed to run limine bios-install");

    if !status.success() {
        eprintln!("Warning: BIOS boot sector installation failed.");
        eprintln!("UEFI boot will still work.");
    }

    println!("ISO created: {}", iso_path.display());
    iso_path
}

// ============================================================================
// Commands
// ============================================================================

/// Build the kernel for the specified target architecture.
fn cmd_build(args: &[String]) {
    let opts = parse_options(args);
    let target = resolve_target(&opts.arch);
    let root = workspace_root();

    println!("Building ThemeliOS kernel for {target}...");

    // Clean the kernel crate's cached artifacts before building. This forces
    // a full recompile every time, which ensures build.rs re-runs and generates
    // a fresh ULID. Without this, cargo's caching would skip recompilation
    // (and ULID regeneration) when no source files have changed.
    let _ = Command::new("cargo")
        .current_dir(&root)
        .args(["clean", "--package", "themelios", "--target", target])
        .status();

    // Build the kernel crate with the bare-metal target.
    // -Zbuild-std=core: build the core library from source for our target,
    //   since bare-metal targets don't have a pre-built standard library.
    // -Zbuild-std-features=compiler-builtins-mem: include memory intrinsics
    //   (memcpy, memset, etc.) that the compiler may generate calls to.
    let status = Command::new("cargo")
        .current_dir(&root)
        .args([
            "build",
            "--package", "themelios",
            "--target", target,
            BUILD_STD,
            BUILD_STD_FEATURES,
        ])
        .status()
        .expect("Failed to execute cargo build");

    if !status.success() {
        eprintln!("Build failed!");
        process::exit(1);
    }

    println!("Build complete: target/{target}/debug/themelios");
}

/// Build the kernel and create a bootable ISO (without launching QEMU).
///
/// This is useful when you want to run QEMU manually, e.g. with a display
/// window:
///
/// ```sh
/// cargo xtask iso
/// qemu-system-x86_64 -M q35 -cdrom target/themelios.iso -serial stdio -no-reboot
/// ```
fn cmd_iso(args: &[String]) {
    let opts = parse_options(args);
    let target = resolve_target(&opts.arch);
    let root = workspace_root();

    cmd_build(args);

    let kernel_binary = root.join(format!("target/{target}/debug/themelios"));
    let limine_dir = ensure_limine(&root);
    let iso_path = create_iso(&root, &kernel_binary, &limine_dir);

    println!();
    println!("To boot headless:  cargo xtask run");
    println!("To boot with GUI:  qemu-system-x86_64 -M q35 -cdrom {} -serial stdio -no-reboot", iso_path.display());
}

/// Build the kernel, create a bootable ISO, and launch it in QEMU.
fn cmd_run(args: &[String]) {
    let opts = parse_options(args);
    let target = resolve_target(&opts.arch);
    let root = workspace_root();

    // Step 1: Build the kernel
    cmd_build(args);

    let kernel_binary = root.join(format!("target/{target}/debug/themelios"));

    // Step 2: Download Limine if needed and create bootable ISO
    let limine_dir = ensure_limine(&root);
    let iso_path = create_iso(&root, &kernel_binary, &limine_dir);

    // Step 3: Launch QEMU
    let qemu = qemu_binary(&opts.arch);
    let mode = if opts.display { "with display" } else { "headless" };
    println!("Launching QEMU ({}, {mode})...", opts.arch);
    println!("Press Ctrl+A, X to exit QEMU.\n");

    let status = match opts.arch.as_str() {
        "x86_64" | "amd64" | "x86-64" => {
            // Boot the ISO in QEMU with BIOS firmware (the default).
            // -M q35: use the Q35 chipset (more modern than the default i440fx)
            // -cdrom: attach the ISO as a CD-ROM drive
            // -serial stdio: route the virtual COM1 port to our terminal
            // -no-reboot: don't reboot on triple fault (helps debugging crashes)
            // -no-shutdown: keep QEMU alive after guest shutdown
            let mut cmd = Command::new(qemu);
            cmd.current_dir(&root)
                .args([
                    "-M", "q35",
                    "-m", "256M",
                    "-cdrom", iso_path.to_str().unwrap(),
                    "-serial", "stdio",
                    "-no-reboot",
                    "-no-shutdown",
                ]);

            // In headless mode, suppress the QEMU graphical window.
            // With --display, let QEMU use its default backend (Cocoa/GTK/SDL).
            if !opts.display {
                cmd.args(["-display", "none"]);
            }

            cmd.status()
        }
        "aarch64" | "arm64" => {
            eprintln!("aarch64 boot is not yet implemented (Phase 0 is x86_64 only).");
            process::exit(1);
        }
        _ => unreachable!(),
    };

    match status {
        Ok(s) if s.success() => println!("\nQEMU exited cleanly."),
        Ok(s) => {
            eprintln!("\nQEMU exited with status: {s}");
            process::exit(1);
        }
        Err(e) => {
            eprintln!("Failed to launch {qemu}: {e}");
            eprintln!("Is QEMU installed? See docs/src/dev-setup.md for instructions.");
            process::exit(1);
        }
    }
}

/// Build the kernel and run tests in QEMU.
fn cmd_test(args: &[String]) {
    let _ = args;
    eprintln!("Testing is not yet implemented. Coming in Phase 1.");
    process::exit(1);
}

/// Build mdbook documentation and rustdoc.
fn cmd_docs(_args: &[String]) {
    let root = workspace_root();

    // Build mdbook (architecture and usage docs)
    println!("Building mdbook documentation...");
    let mdbook_status = Command::new("mdbook")
        .current_dir(root.join("docs"))
        .arg("build")
        .status();

    match mdbook_status {
        Ok(s) if s.success() => println!("mdbook built to docs/book/"),
        Ok(_) => eprintln!("mdbook build failed!"),
        Err(e) => eprintln!("Failed to run mdbook: {e}\nInstall with: cargo install mdbook"),
    }

    // Build rustdoc (API docs for the kernel crate)
    println!("Building rustdoc...");
    let doc_status = Command::new("cargo")
        .current_dir(&root)
        .args([
            "doc",
            "--package", "themelios",
            "--target", "x86_64-unknown-none",
            BUILD_STD,
            BUILD_STD_FEATURES,
            "--no-deps",
        ])
        .status();

    match doc_status {
        Ok(s) if s.success() => {
            println!("rustdoc built to target/x86_64-unknown-none/doc/");
        }
        Ok(_) => eprintln!("rustdoc build failed!"),
        Err(e) => eprintln!("Failed to run cargo doc: {e}"),
    }
}

/// Print usage information.
fn print_usage() {
    eprintln!(
        "ThemeliOS development toolkit

Usage: cargo xtask <COMMAND> [OPTIONS]

Commands:
    build    Build the kernel
    iso      Build the kernel and create a bootable ISO
    run      Build, create ISO, and launch in QEMU (headless)
    test     Build and run tests in QEMU
    docs     Build mdbook and rustdoc

Options:
    --arch <ARCH>  Target architecture: x86_64 (default), arm64
    --display      Open QEMU with a graphical window (for run command)

Prerequisites:
    QEMU:     brew install qemu
    xorriso:  brew install xorriso"
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let subcommand_args = if args.len() > 1 { &args[1..] } else { &[] as &[String] };

    if subcommand_args.is_empty() {
        print_usage();
        process::exit(1);
    }

    let command = &subcommand_args[0];
    let rest = &subcommand_args[1..];

    match command.as_str() {
        "build" => cmd_build(rest),
        "iso" => cmd_iso(rest),
        "run" => cmd_run(rest),
        "test" => cmd_test(rest),
        "docs" => cmd_docs(rest),
        "help" | "--help" | "-h" => print_usage(),
        unknown => {
            eprintln!("Unknown command: {unknown}\n");
            print_usage();
            process::exit(1);
        }
    }
}
