//! # ThemeliOS xtask — build and development tooling
//!
//! This binary runs on the host machine (macOS, Linux) and handles:
//! - Cross-compiling the kernel for bare-metal targets
//! - Launching QEMU for testing
//! - Building documentation
//!
//! ## Usage
//!
//! ```sh
//! cargo xtask build              # Build kernel for x86_64
//! cargo xtask build --arch arm64 # Build kernel for aarch64
//! cargo xtask run                # Build + launch QEMU (x86_64)
//! cargo xtask run --arch arm64   # Build + launch QEMU (aarch64)
//! cargo xtask test               # Build + run kernel tests in QEMU
//! cargo xtask docs               # Build mdbook + rustdoc
//! ```
//!
//! The `cargo xt` alias also works (defined in `.cargo/config.toml`).

use std::env;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

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
        _ => unreachable!(), // resolve_target would have exited already
    }
}

/// Returns the path to the workspace root (the directory containing the
/// top-level Cargo.toml). We find it by walking up from the xtask binary's
/// manifest directory.
fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points to xtask/, so the workspace root is one level up.
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| ".".to_string());
    Path::new(&manifest_dir)
        .parent()
        .expect("xtask must be in a subdirectory of the workspace root")
        .to_path_buf()
}

/// Parses the --arch flag from the argument list. Returns the architecture
/// string and the remaining arguments with --arch removed.
fn parse_arch(args: &[String]) -> (String, Vec<String>) {
    let mut arch = "x86_64".to_string();
    let mut remaining = Vec::new();
    let mut skip_next = false;

    for (i, arg) in args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--arch" {
            // Next argument is the architecture name
            if let Some(next) = args.get(i + 1) {
                arch = next.clone();
                skip_next = true;
            } else {
                eprintln!("Error: --arch requires a value (x86_64, arm64)");
                process::exit(1);
            }
        } else if let Some(value) = arg.strip_prefix("--arch=") {
            arch = value.to_string();
        } else {
            remaining.push(arg.clone());
        }
    }

    (arch, remaining)
}

/// Build the kernel for the specified target architecture.
fn cmd_build(args: &[String]) {
    let (arch, _remaining) = parse_arch(args);
    let target = resolve_target(&arch);
    let root = workspace_root();

    println!("Building ThemeliOS kernel for {target}...");

    // Build the kernel crate with the bare-metal target.
    // -Zbuild-std=core builds the core library from source for our target,
    // since bare-metal targets don't have a pre-built standard library.
    let status = Command::new("cargo")
        .current_dir(&root)
        .args([
            "build",
            "--package", "themelios",
            "--target", target,
            "-Zbuild-std=core",
            "-Zbuild-std-features=compiler-builtins-mem",
        ])
        .status()
        .expect("Failed to execute cargo build");

    if !status.success() {
        eprintln!("Build failed!");
        process::exit(1);
    }

    println!("Build complete: target/{target}/debug/themelios");
}

/// Build the kernel and launch it in QEMU.
fn cmd_run(args: &[String]) {
    // First, build the kernel.
    cmd_build(args);

    let (arch, _remaining) = parse_arch(args);
    let target = resolve_target(&arch);
    let root = workspace_root();
    let kernel_binary = root.join(format!("target/{target}/debug/themelios"));

    println!("Launching QEMU ({arch})...");
    println!("Press Ctrl+A, X to exit QEMU.\n");

    let qemu = qemu_binary(&arch);

    // Build the QEMU command. The exact arguments depend on the architecture
    // and will be refined as we implement the boot sequence.
    let status = match arch.as_str() {
        "x86_64" | "amd64" | "x86-64" => {
            Command::new(qemu)
                .current_dir(&root)
                .args([
                    "-kernel", kernel_binary.to_str().unwrap(),
                    "-serial", "stdio",     // Serial output to terminal
                    "-display", "none",     // No graphical display (headless)
                    "-no-reboot",           // Don't reboot on triple fault
                    "-no-shutdown",         // Don't exit on shutdown (helps debugging)
                ])
                .status()
        }
        "aarch64" | "arm64" => {
            Command::new(qemu)
                .current_dir(&root)
                .args([
                    "-machine", "virt",             // ARM virtual machine
                    "-cpu", "cortex-a72",            // A common 64-bit ARM CPU
                    "-kernel", kernel_binary.to_str().unwrap(),
                    "-serial", "stdio",
                    "-display", "none",
                    "-no-reboot",
                    "-no-shutdown",
                ])
                .status()
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
    // TODO(phase-1): Implement QEMU-based test runner.
    // This will boot the kernel with a special test flag, run in-kernel tests,
    // and exit QEMU with a success/failure exit code.
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
    // Note: rustdoc for no_std targets requires the same -Zbuild-std flags.
    println!("Building rustdoc...");
    let doc_status = Command::new("cargo")
        .current_dir(&root)
        .args([
            "doc",
            "--package", "themelios",
            "--target", "x86_64-unknown-none",
            "-Zbuild-std=core",
            "-Zbuild-std-features=compiler-builtins-mem",
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
    run      Build and launch in QEMU
    test     Build and run tests in QEMU
    docs     Build mdbook and rustdoc

Options:
    --arch <ARCH>  Target architecture: x86_64 (default), arm64"
    );
}

fn main() {
    // Skip the first argument (binary name). When invoked via `cargo xtask`,
    // the arguments are: ["xtask", "<command>", ...].
    let args: Vec<String> = env::args().collect();

    // Find the subcommand. It's either args[1] (direct invocation) or we need
    // to skip "xtask" if cargo passes it.
    let subcommand_args = if args.len() > 1 { &args[1..] } else { &[] as &[String] };

    if subcommand_args.is_empty() {
        print_usage();
        process::exit(1);
    }

    let command = &subcommand_args[0];
    let rest = &subcommand_args[1..];

    match command.as_str() {
        "build" => cmd_build(rest),
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
