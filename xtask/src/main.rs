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
use std::thread;
use std::time::{Duration, Instant};

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
        "aarch64" | "arm64" => "aarch64-unknown-none-softfloat",
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

/// Parses `--arch` and `--display` from the argument list.
///
/// **Anything else is a hard error.** Architecture is selected by exactly one flag, and
/// the failure mode of ignoring an unrecognised one is quiet in the worst way: a
/// `cargo xtask test --arm64` that silently drops the flag builds *amd64*, runs the amd64
/// suite, and prints "all tests passed" — a green result for the architecture you were
/// not testing. Rejecting the flag costs one line and makes the mistake impossible.
///
/// This is also why there is no `--amd64` / `--arm64` shorthand alongside `--arch`: one
/// selector with one spelling cannot drift out of sync with itself.
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
                eprintln!("Error: --arch requires a value (amd64, arm64)");
                process::exit(1);
            }
        } else if let Some(value) = arg.strip_prefix("--arch=") {
            arch = value.to_string();
        } else if arg == "--display" {
            display = true;
        } else {
            eprintln!("Error: unknown option {arg:?}");
            eprintln!("Architecture is selected with `--arch amd64` or `--arch arm64`.");
            process::exit(1);
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
// Userspace servers
// ============================================================================

/// The userspace server binaries to build and embed in the kernel.
///
/// Each entry is a binary crate in the `servers/` workspace. Built for
/// `x86_64-unknown-none`, linked with the server linker script as a flat binary,
/// and copied to `target/servers/<name>.bin` where the kernel embeds it via
/// `include_bytes!`.
const SERVER_BINARIES: &[&str] = &[
    "echo-server",
    "api-server",
    "squashfs-server",
    "overlay-server",
    "ext2-server",
    "fstest-client",
    "net-server",
];

/// Build the userspace server workspace and stage the flat binaries.
///
/// Must run before the kernel build: the kernel `include_bytes!`s these files.
/// Servers are linked with `--oformat binary` (LLD emits the raw memory image,
/// so the kernel needs no ELF parser) against the server linker script, with a
/// static relocation model (they load at a fixed virtual base).
fn build_servers(root: &Path) {
    let servers_dir = root.join("servers");
    let linker_script = servers_dir.join("linker.ld");
    let out_dir = root.join("target/servers");
    fs::create_dir_all(&out_dir).expect("failed to create target/servers");

    // Link flags: place sections per the server linker script, emit a flat
    // binary, and link non-relocatable (fixed load address).
    let rustflags = format!(
        "-C link-arg=-T{} -C link-arg=--oformat=binary -C relocation-model=static",
        linker_script.display()
    );

    println!("Building userspace servers...");
    let status = Command::new("cargo")
        .current_dir(&servers_dir)
        .env("RUSTFLAGS", &rustflags)
        .args([
            "build",
            "--release",
            "--target", "x86_64-unknown-none",
            BUILD_STD,
            BUILD_STD_FEATURES,
        ])
        .status()
        .expect("Failed to execute cargo build for servers");
    if !status.success() {
        eprintln!("Server build failed!");
        process::exit(1);
    }

    // Stage each server's flat binary where the kernel embeds it.
    let release_dir = servers_dir.join("target/x86_64-unknown-none/release");
    for name in SERVER_BINARIES {
        let built = release_dir.join(name);
        let staged = out_dir.join(format!("{name}.bin"));
        fs::copy(&built, &staged).unwrap_or_else(|e| {
            panic!("failed to stage server binary {}: {e}", built.display())
        });
        let size = fs::metadata(&staged).map(|m| m.len()).unwrap_or(0);
        println!("  {name}: {size} bytes -> {}", staged.display());
    }

    // Build the Phase 5.0/5.1 smoke-test binaries as real ELFs (not flat
    // binaries), staged where the kernel embeds them.
    build_detached_elf(root, &out_dir, "elf-smoke"); // 5.0 loader test (native ABI)
    build_detached_elf(root, &out_dir, "linux-smoke"); // 5.1 Linux-personality test
    build_detached_elf(root, &out_dir, "fs-smoke"); // 5.2 Linux FS-syscall test
    build_detached_elf(root, &out_dir, "threads-smoke"); // 5.3 threads/futex test
    build_detached_elf(root, &out_dir, "isolation-smoke"); // 5.7 container-isolation test
    build_detached_elf(root, &out_dir, "confine-smoke"); // 6.1b rootfs-confinement test
}

/// Build a detached smoke-test crate as a **real ELF** (not a flat binary).
///
/// Unlike the other servers, these are linked with the default object format (no
/// `--oformat=binary`) and forced to a static, non-PIE `ET_EXEC`, so the kernel's
/// ELF loader has genuine ELF headers + `PT_LOAD` segments to parse. Each is a
/// detached crate (own workspace), so `build_servers`' flat-binary flags don't
/// reach it. Staged to `target/servers/<name>.elf`.
fn build_detached_elf(root: &Path, out_dir: &Path, name: &str) {
    let crate_dir = root.join("servers").join(name);
    // Static reloc + no-PIE keeps the output ET_EXEC with fixed segment vaddrs.
    let rustflags = "-C relocation-model=static -C link-arg=-no-pie";

    println!("Building {name} (real ELF)...");
    let status = Command::new("cargo")
        .current_dir(&crate_dir)
        .env("RUSTFLAGS", rustflags)
        .args([
            "build",
            "--release",
            "--target",
            "x86_64-unknown-none",
            "-Zbuild-std=core",
        ])
        .status()
        .unwrap_or_else(|e| panic!("Failed to execute cargo build for {name}: {e}"));
    if !status.success() {
        eprintln!("{name} build failed!");
        process::exit(1);
    }

    let built = crate_dir.join(format!("target/x86_64-unknown-none/release/{name}"));
    let staged = out_dir.join(format!("{name}.elf"));
    fs::copy(&built, &staged)
        .unwrap_or_else(|e| panic!("failed to stage {name}: {e} ({})", built.display()));
    let size = fs::metadata(&staged).map(|m| m.len()).unwrap_or(0);
    println!("  {name}.elf: {size} bytes -> {}", staged.display());
}

// ============================================================================
// Virtual disks
// ============================================================================

/// Ensure a scratch VirtIO block disk exists, returning its path.
///
/// Phase 3 needs at least one VirtIO block device attached to QEMU so the
/// kernel's PCI scan (sub-phase 3.0) and VirtIO-blk driver (sub-phase 3.2) have
/// something to discover and drive. For now this is a small zero-filled raw
/// image; sub-phase 3.9 ("image creation tooling") replaces it with real
/// SquashFS and ext2 images.
///
/// The disk is created once and reused — it is only regenerated if missing, so
/// repeated `cargo xtask run`/`test` invocations don't rewrite it.
/// Magic in sector 0 of the scratch disk.
///
/// The kernel's destructive block tests probe for this before writing, so they can never
/// target the SquashFS root or the ext2 data volume regardless of what order discovery
/// returns devices in. Kept in sync with `SCRATCH_SIGNATURE` in `kernel/src/test_runner.rs`.
const SCRATCH_SIGNATURE: &[u8] = b"THEMELIOS-SCRATCH-V1";

/// Create a blank raw disk image if it is not already there, and return its path.
///
/// Used by the aarch64 test path to make up the device *count*
/// `test_virtio_discovery` asserts without pulling in `mksquashfs`/`mkfs.ext2` for
/// content no test on that architecture reads yet.
///
/// **Deliberately unsigned.** `open_scratch_disk` identifies the one disk destructive
/// tests may overwrite by the [`SCRATCH_SIGNATURE`] in sector 0; a filler that carried it
/// would make that probe ambiguous and put a destructive write on an arbitrary disk. Zeros
/// also mean it matches no filesystem probe — not SquashFS's `hsqs`, not ext2's 0xEF53 —
/// so a future mount attempt fails cleanly rather than half-recognising it.
fn ensure_filler_disk(root: &Path, name: &str) -> PathBuf {
    /// Same 4 MiB as the scratch disk: large enough for a sane reported capacity, small
    /// enough to be free to create.
    const SIZE_BYTES: usize = 4 * 1024 * 1024;

    let disk_path = root.join("target").join(name);
    if fs::metadata(&disk_path).map(|m| m.len() as usize).ok() != Some(SIZE_BYTES) {
        fs::write(&disk_path, vec![0u8; SIZE_BYTES]).expect("Failed to create filler disk image");
        println!("Created blank VirtIO filler disk: {}", disk_path.display());
    }
    disk_path
}

fn ensure_scratch_disk(root: &Path) -> PathBuf {
    let disk_path = root.join("target/themelios-scratch.img");

    // Rewrite when the image is missing **or** unsigned. Checking only for existence
    // would leave a scratch image created before the signature landed (8.1) permanently
    // unsigned, and the destructive block tests would then fail with "no scratch disk
    // found" on any tree that had run the suite before — a confusing failure whose cause
    // is an old file rather than the code under test.
    let needs_write = match fs::read(&disk_path) {
        Ok(existing) => !existing.starts_with(SCRATCH_SIGNATURE),
        Err(_) => true,
    };
    if needs_write {
        // 4 MiB of zeros. Enough for the driver to report a sane capacity and
        // for round-trip read/write sector tests in sub-phase 3.2.
        const SIZE_BYTES: usize = 4 * 1024 * 1024;
        let mut img = vec![0u8; SIZE_BYTES];

        // Signature in sector 0, so the destructive block tests can *identify* the
        // disk they are licensed to overwrite instead of trusting enumeration order.
        //
        // Before 8.1 those tests took "the first block device" and relied on this
        // image being attached first, which put it in the lowest PCI slot. That is a
        // silent contract between the harness's argument order and the kernel's
        // discovery order, and it does not survive aarch64: QEMU `virt` maps `-device`
        // arguments to virtio-mmio slots with *decreasing* base addresses, so a kernel
        // scanning slots upward finds this disk last and "the first block device"
        // becomes the ext2 data volume. The writes below would then land in ext2's
        // block/inode bitmaps, and the damage would surface several sub-phases later
        // looking exactly like an ext2 bug.
        //
        // The other two images are already identified by content — SquashFS by `hsqs`,
        // ext2 by 0xEF53 — so this simply gives the scratch disk the same property.
        img[..SCRATCH_SIGNATURE.len()].copy_from_slice(SCRATCH_SIGNATURE);

        fs::write(&disk_path, &img)
            .expect("Failed to create scratch disk image");
        println!("Created scratch VirtIO disk: {}", disk_path.display());
    }

    disk_path
}

/// Append the QEMU flags that attach `disk_path` as a VirtIO block device.
///
/// Uses the explicit two-part form rather than the `if=virtio` shorthand so the
/// device shows up as a `virtio-blk-pci` function on the Q35 PCI bus, which is
/// what the kernel's PCI enumeration expects to find:
/// - `-drive ...,if=none,id=<id>`: define the backing image without auto-
///   attaching it to any controller.
/// - `-device virtio-blk-pci,drive=<id>`: create the VirtIO block PCI device.
///
/// `disable-legacy=on` forces the modern (VirtIO 1.0+) PCI interface (device ID
/// 1af4:1042), which advertises its registers through PCI capabilities in MMIO
/// BARs — the only interface the kernel's VirtIO transport speaks. `readonly`
/// attaches the drive read-only (used for the immutable SquashFS root).
///
/// Devices are assigned PCI slots in command-line order, so callers control
/// discovery order by the order they emit these arg groups.
fn virtio_disk_args(disk_path: &Path, id: &str, readonly: bool) -> Vec<String> {
    let ro = if readonly { ",readonly=on" } else { "" };
    vec![
        "-drive".to_string(),
        format!("file={},format=raw,if=none,id={id}{ro}", disk_path.display()),
        "-device".to_string(),
        format!("virtio-blk-pci,drive={id},disable-legacy=on"),
    ]
}

/// QEMU device arguments for a VirtIO disk on the **aarch64 `virt`** machine.
///
/// `virt` has no PCI-attached VirtIO by default: devices arrive on the **virtio-mmio**
/// bus, so the device model is `virtio-blk-device` rather than `virtio-blk-pci`. There is
/// no `disable-legacy` property on the mmio device either — legacy versus modern is a
/// property of the *bus*, set once for the machine by `virtio_mmio_modern_args`.
///
/// Note the slot mapping runs **backwards**: QEMU's `qbus_realize` prepends child buses,
/// so `-device` arguments in increasing command-line order land on *decreasing* mmio base
/// addresses. Callers must therefore not assume command-line order is discovery order —
/// which is why every test that cares identifies its disk by content rather than position.
fn virtio_disk_args_mmio(disk_path: &Path, id: &str, readonly: bool) -> Vec<String> {
    let ro = if readonly { ",readonly=on" } else { "" };
    vec![
        "-drive".to_string(),
        format!("file={},format=raw,if=none,id={id}{ro}", disk_path.display()),
        "-device".to_string(),
        format!("virtio-blk-device,drive={id}"),
    ]
}

/// QEMU arguments forcing the **modern** virtio-mmio interface machine-wide.
///
/// QEMU's `virtio-mmio` device defaults `force-legacy` to **true**, and `hw/arm/virt.c`
/// never overrides it — so a `virt` machine started without this presents every VirtIO
/// device as legacy (version 1), which this kernel does not speak. The kernel reports and
/// skips such slots by name rather than silently finding no devices, but the fix belongs
/// here: without it there is nothing for it to find.
fn virtio_mmio_modern_args() -> Vec<String> {
    vec![
        "-global".to_string(),
        "virtio-mmio.force-legacy=false".to_string(),
    ]
}

/// QEMU arguments for a VirtIO NIC on `virt`, with an optional host-forward rule.
fn virtio_net_args_mmio(id: &str, hostfwd: Option<&str>) -> Vec<String> {
    let mut netdev = format!("user,id={id}");
    if let Some(rule) = hostfwd {
        netdev.push(',');
        netdev.push_str(rule);
    }
    vec![
        "-netdev".to_string(),
        netdev,
        "-device".to_string(),
        format!("virtio-net-device,netdev={id}"),
    ]
}
/// Boot self-test failure signatures, scanned by **both** aarch64 QEMU paths.
///
/// Module-level rather than local to the smoke, because the two paths had drifted and it
/// mattered. `cmd_test_aarch64` used to check only the suite's own PASS/FAIL sentinel, so
/// a kernel whose *boot* self-tests failed still printed `[test] RESULT: ALL TESTS PASSED`
/// and exited 0 — which is exactly what happened when an accessor mutation broke the EL0
/// round trip during 8.4c: the suite reported success on a kernel that had already failed
/// two boot assertions. CI happened to catch it because the smoke job scans this list, but
/// `cargo xtask test --arch arm64` on its own did not, and that is the command a
/// contributor runs.
///
/// The boot self-tests and the test suite check different things and both are gates; one
/// list, two readers.
const AARCH64_BOOT_FAILURES: &[&str] = &[
    "KERNEL PANIC",
    "Phase 7.1 MMU/paging FAILED self-test",
    "[selftest] paging: FAIL",
    "Phase 7.2 FAILED self-test",
    "[selftest] exceptions: FAIL",
    "[selftest] timer: FAIL",
    "Phase 7.3 scheduler FAILED self-test",
    "[selftest] sched: FAIL",
    "[selftest] percpu: FAIL",
    // Phase 8.4 (user address spaces on TTBR0) and 8.4b (the EL0 round trip). These
    // were missing until a review of 8.4b went looking: the list stopped at 7.3, so
    // both sub-phases' self-tests printed their verdict into a log nothing inspected,
    // and every assertion in them was decorative as far as CI was concerned. The
    // `[selftest] …: FAIL` prefixes catch a specific failed assertion; the `[boot] …`
    // lines catch the case where the self-test returns false by a path that did not
    // print one — belt and braces, because they fail for different reasons.
    "[selftest] user-as: FAIL",
    "[boot] Phase 8.4 user address space FAILED self-test",
    "[selftest] el0: FAIL",
    "[boot] Phase 8.4b EL0 round trip FAILED self-test",
    // Phase 8.4d, the EL0 preemption soak. Added at the same time as the soak itself,
    // because a mutation that corrupted every soak return produced `[soak] FAIL` twice
    // and *still* exited 0 — the identical hole the 8.4 entries above were added to close,
    // reopened by the next thing that printed its own verdict.
    "[soak] FAIL",
    "[boot] Phase 8.4d EL0 preemption soak FAILED",
    // Was "FP/SIMD does NOT trap at EL1" until 8.4e, when FP had to be enabled at EL1
    // for userspace's sake and that line stopped being a failure. The signature it is
    // replaced by covers the thing that still matters: kernel work disturbing a task's
    // vector state, which the save area exists to prevent.
    "[fp] selftest: FAIL",
    "!!! aarch64 EXCEPTION !!!",
];

/// `cargo xtask test --arch aarch64` — run the kernel suite on QEMU `virt`.
///
/// Mirrors [`cmd_test`], with one structural difference: how the verdict gets out.
///
/// x86_64 writes to QEMU's `isa-debug-exit` device and the result arrives as a process
/// exit code, which is unambiguous and needs no parsing. The `virt` machine has no such
/// device — aarch64 has no I/O ports for one to live behind — so the kernel prints a
/// sentinel line and then powers the machine off through PSCI.
///
/// That gives four outcomes rather than two, and the extra two are the useful ones:
///
/// | What happened                        | Verdict                             |
/// |--------------------------------------|-------------------------------------|
/// | PASS sentinel, QEMU exits            | pass                                |
/// | FAIL sentinel, QEMU exits            | fail — the `[FAIL]` lines say which |
/// | QEMU exits, no sentinel              | fail — the kernel died mid-suite    |
/// | no exit before the deadline          | fail — hang                         |
///
/// Without the PSCI shutdown the third and fourth rows collapse into one another, and
/// a kernel that panicked halfway through looks exactly like a kernel that hung.
fn cmd_test_aarch64(root: &Path, target: &str) {
    /// Printed by `test_runner` when every test that ran, passed. Must match
    /// `AARCH64_PASS_SENTINEL` in `kernel/src/test_runner.rs` exactly — both sides
    /// spell out the whole line rather than matching a prefix, so a drift is a hard
    /// failure here rather than a silently weaker check.
    const PASS: &str = "[test] RESULT: ALL TESTS PASSED";
    /// Printed when at least one test failed. Matches `AARCH64_FAIL_SENTINEL`.
    const FAIL: &str = "[test] RESULT: FAILURES PRESENT";

    println!("Building ThemeliOS kernel (test mode) for {target}...");

    // Clean cached artifacts so build.rs re-runs and the ULID is fresh, matching the
    // amd64 path.
    let _ = Command::new("cargo")
        .current_dir(root)
        .args(["clean", "--package", "themelios", "--target", target])
        .status();

    let status = Command::new("cargo")
        .current_dir(root)
        .args([
            "build",
            "--package", "themelios",
            "--target", target,
            "--features", "test",
            BUILD_STD,
            BUILD_STD_FEATURES,
        ])
        .status()
        .expect("Failed to execute cargo build");
    if !status.success() {
        eprintln!("aarch64 test build failed!");
        process::exit(1);
    }

    let kernel = root.join(format!("target/{target}/debug/themelios"));
    let limine_dir = ensure_limine(root);
    let (esp, code, vars) = prepare_aarch64_boot(root, &kernel, &limine_dir);

    let serial_log = root.join("target/aarch64-test-serial.log");
    let _ = fs::remove_file(&serial_log);

    // The suite drives real storage and a NIC, so the aarch64 VM needs devices. Until 8.3
    // it had none — only the firmware pflash pair and the boot ESP — which is why every
    // storage and network test was skipped there.
    //
    // The *same count* as amd64 (three disks, one NIC), because `test_virtio_discovery`
    // asserts the multiset, but deliberately **not the same images**. amd64 attaches the
    // SquashFS root and the ext2 data volume; nothing that runs on aarch64 today reads
    // either — the filesystem tests all still need `mod fs` and ring 3 — so building them
    // here would make `mksquashfs` and `mkfs.ext2` prerequisites of the arm64 CI job to
    // produce two disks no test opens. Two blank fillers give discovery the same shape at
    // no cost. 8.6 swaps them for the real images when the storage stack ports, and
    // declares the tools then, as part of the change that actually needs them.
    let scratch = ensure_scratch_disk(root);
    let filler_a = ensure_filler_disk(root, "themelios-arm64-filler-a.img");
    let filler_b = ensure_filler_disk(root, "themelios-arm64-filler-b.img");

    println!("Running the suite on QEMU virt (headless)...");
    let mut cmd = qemu_aarch64_base(&esp, &code, &vars);

    // Three disks, scratch first, mirroring the amd64 command-line order. Note that on
    // `virt` this maps to *decreasing* mmio slot addresses, so the resulting discovery
    // order is the reverse of amd64's — deliberately not something any test depends on.
    cmd.args(virtio_disk_args_mmio(&scratch, "blkscratch", false));
    cmd.args(virtio_disk_args_mmio(&filler_a, "blkfilla", true));
    cmd.args(virtio_disk_args_mmio(&filler_b, "blkfillb", false));

    // A NIC on user-mode networking, with the same host-forward rule the amd64 path uses.
    let hostfwd = format!(
        "hostfwd=tcp:127.0.0.1:{}-:{TCP_TEST_GUEST_PORT}",
        tcp_test_host_port()
    );
    cmd.args(virtio_net_args_mmio("net0", Some(&hostfwd)));
    cmd.args(["-display", "none"]);
    cmd.arg("-serial").arg(format!("file:{}", serial_log.display()));

    let mut child = cmd.spawn().expect("Failed to launch qemu-system-aarch64");

    // Poll for exit rather than blocking on it, so a hung kernel is bounded. The suite
    // itself takes a couple of seconds; the budget is generous because a CI runner
    // building nothing but still emulating every instruction is slow, and the cost of
    // being wrong here is a spurious red build.
    let deadline = Duration::from_secs(120);
    let start = Instant::now();
    let mut exited = false;
    while start.elapsed() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    if !exited {
        let _ = child.kill();
    }
    let _ = child.wait();

    let serial = fs::read_to_string(&serial_log).unwrap_or_default();
    println!("--- aarch64 serial output ---\n{serial}\n-----------------------------");

    // Order matters: check for the failure sentinel first. A run that prints FAIL has
    // reported its own verdict, and that is more specific than anything inferred.
    if serial.contains(FAIL) {
        eprintln!("aarch64 suite FAILED — see the [FAIL] lines above.");
        process::exit(1);
    }

    // Boot self-test failures, checked **before** honouring the PASS sentinel.
    //
    // The suite and the boot self-tests are independent gates: `run_tests` reports only on
    // the tests in its own tables, and a boot self-test that fails merely prints and lets
    // boot continue. So a kernel with a broken EL0 round trip reached `[test] RESULT: ALL
    // TESTS PASSED` and this function exited 0 — observed during 8.4c, where mutating a
    // syscall-frame accessor produced two `[selftest] el0: FAIL` lines and a passing suite.
    // The smoke job scanned for these already; this path did not, which meant the command
    // a contributor actually runs was the weaker of the two.
    if let Some(sig) = AARCH64_BOOT_FAILURES
        .iter()
        .find(|f| serial.contains(**f))
    {
        eprintln!(
            "aarch64 suite FAILED: kernel reported '{sig}' during boot. The test tables may \
             all have passed — a boot self-test is a separate gate and this one is red."
        );
        process::exit(1);
    }

    if serial.contains(PASS) {
        if !exited {
            // The suite passed but PSCI did not stop the machine. Worth saying out
            // loud: the tests are fine and the shutdown path is not.
            eprintln!(
                "aarch64 suite passed, but QEMU did not exit within {}s — PSCI \
                 SYSTEM_OFF appears not to have taken effect.",
                deadline.as_secs()
            );
            process::exit(1);
        }
        println!("aarch64 suite passed.");
        return;
    }
    if exited {
        eprintln!(
            "aarch64 suite FAILED: QEMU exited without printing a verdict — the kernel \
             died partway through the suite (look for a panic or an exception above)."
        );
    } else {
        eprintln!(
            "aarch64 suite FAILED: no verdict and no exit within {}s — the kernel hung.",
            deadline.as_secs()
        );
    }
    process::exit(1);
}

/// QEMU arguments attaching a VirtIO network device backed by user-mode (slirp)
/// networking (Phase 4).
///
/// - `-netdev user,id=<id>`: QEMU's built-in user-mode network — no host config,
///   provides a gateway (10.0.2.2), DHCP, and DNS (10.0.2.3). Enough to exercise
///   ARP, DHCP, and outbound UDP/TCP without touching the host network stack.
/// - `-device virtio-net-pci,netdev=<id>,disable-legacy=on`: create the modern
///   VirtIO-net PCI device the kernel driver binds to.
fn virtio_net_args(id: &str) -> Vec<String> {
    virtio_net_args_fwd(id, None)
}

/// Host TCP port forwarded to the guest's TCP listener (guest port 7) during
/// `cargo xtask test`, for the Phase 6.5 `test_api_server` HTTP round-trip.
///
/// Overridable via `THEMELIOS_TEST_PORT`, because the default is a *fixed* port and QEMU
/// refuses to start when it is already bound: `Could not set up host forwarding rule`.
/// Two suite runs on one host therefore collide, and so does a run started while a
/// previous QEMU is still shutting down — which reports as a test failure with no failing
/// test, several minutes into the run. Both a parallel CI job and a second developer
/// shell hit this.
fn tcp_test_host_port() -> u16 {
    match std::env::var("THEMELIOS_TEST_PORT") {
        Ok(v) => v.parse().unwrap_or_else(|_| {
            panic!("THEMELIOS_TEST_PORT is set to {v:?}, which is not a port number")
        }),
        Err(_) => 15007,
    }
}
/// Guest TCP port the api-server test listens on (reuses the existing hostfwd).
const TCP_TEST_GUEST_PORT: u16 = 7;

/// The HTTP request(s) the host peer sends to the ring-3 `api-server`, each on its
/// own connection. This is the **live inbound smoke** (phase 3 of `test_api_server`):
/// a single authenticated `GET /containers/json` proving the real accept → HTTP-parse
/// → authenticate → route → reply path over TCP, including that the
/// `Authorization: Bearer` header round-trips intact. The token literal must match the
/// kernel's `API_TOKEN` const in `kernel/src/process/server.rs`. A single connection
/// is used deliberately — the immature net server can deliver stale RX data across
/// *sequential* connections on one listener (a pre-existing bug; see the plan's 6.5b
/// note), so multi-connection content assertions are flaky. The *content* of the
/// routing + auth + JSON-parsing logic is proven separately and deterministically by
/// the api-server's in-process self-test (phase 2), which needs no network, so this
/// wire smoke only has to prove an authenticated request round-trips.
const API_TEST_REQUESTS: &[&[u8]] = &[
    b"GET /containers/json HTTP/1.1\r\nHost: themelios\r\nAuthorization: Bearer themelios-dev-secret-token\r\n\r\n",
];

/// Spawn a detached host thread that drives the ring-3 `api-server` over the
/// `hostfwd` rule: it opens one connection per entry in [`API_TEST_REQUESTS`],
/// sending each request in order and reading the complete framed response (looping
/// until `Content-Length` is satisfied or the peer closes). It retries the first
/// connect until the guest is listening. Failures are silent — the guest asserts the
/// response statuses via the api-server's result page; this thread only drives the
/// connections in order.
fn spawn_tcp_test_peer() {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::{Duration, Instant};

    // Read a full HTTP/1.1 response: headers, then `Content-Length` body bytes.
    // Returns true if a complete response was read.
    fn read_response(stream: &mut TcpStream) -> bool {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            // Once headers are in, read exactly Content-Length more bytes.
            if let Some(hdr_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..hdr_end]).to_ascii_lowercase();
                let need = head
                    .split("\r\n")
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= hdr_end + 4 + need {
                    return true;
                }
            }
            match stream.read(&mut chunk) {
                Ok(0) => return !buf.is_empty(), // peer closed
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => return false,
            }
        }
    }

    std::thread::spawn(|| {
        let sa: SocketAddr = match format!("127.0.0.1:{}", tcp_test_host_port()).parse() {
            Ok(a) => a,
            Err(_) => return,
        };
        let deadline = Instant::now() + Duration::from_secs(85);
        // Send the requests in order, one connection each. The guest serves them in
        // accept order, so response i corresponds to request i.
        let mut next = 0usize;
        while next < API_TEST_REQUESTS.len() && Instant::now() < deadline {
            match TcpStream::connect_timeout(&sa, Duration::from_millis(500)) {
                Ok(mut stream) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
                    if stream.write_all(API_TEST_REQUESTS[next]).is_ok() && read_response(&mut stream) {
                        next += 1;
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(150)),
            }
        }
    });
}

/// Like [`virtio_net_args`] but optionally adds a slirp `hostfwd` rule so the
/// host can reach a guest TCP listener (used by the server-socket test).
fn virtio_net_args_fwd(id: &str, hostfwd: Option<&str>) -> Vec<String> {
    let netdev = match hostfwd {
        Some(fw) => format!("user,id={id},{fw}"),
        None => format!("user,id={id}"),
    };
    vec![
        "-netdev".to_string(),
        netdev,
        "-device".to_string(),
        format!("virtio-net-pci,netdev={id},disable-legacy=on"),
    ]
}

/// Locate a host tool: try each candidate (a bare name resolved via `PATH`, or
/// an absolute path) and return the first that exists/resolves.
///
/// Used to find `mkfs.ext2`, which Homebrew installs *keg-only* on macOS (Apple
/// ships a conflicting version) and therefore does NOT symlink onto `PATH`. We
/// fall back to the known keg locations so the developer never has to edit
/// their `PATH`. See `docs/src/dev-setup.md` and the repo `Brewfile`.
fn find_tool(candidates: &[&str]) -> Option<PathBuf> {
    for cand in candidates {
        let path = Path::new(cand);
        if path.is_absolute() {
            if path.exists() {
                return Some(path.to_path_buf());
            }
        } else {
            // Bare name: resolve via `which`.
            if let Ok(out) = Command::new("which").arg(cand).output() {
                if out.status.success() {
                    let resolved = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !resolved.is_empty() {
                        return Some(PathBuf::from(resolved));
                    }
                }
            }
        }
    }
    None
}

/// Locate `mksquashfs` (from the `squashfs`/`squashfs-tools` package).
fn find_mksquashfs() -> PathBuf {
    find_tool(&["mksquashfs", "/opt/homebrew/bin/mksquashfs", "/usr/local/bin/mksquashfs"])
        .unwrap_or_else(|| {
            eprintln!("Error: mksquashfs not found.");
            eprintln!("Install it:  macOS: brew install squashfs   Linux: apt install squashfs-tools");
            eprintln!("(or run `brew bundle` from the repo root on macOS)");
            process::exit(1);
        })
}

/// Locate `mkfs.ext2` (from `e2fsprogs`). Keg-only on macOS, so we check the
/// keg `sbin` directories in addition to `PATH`.
fn find_mkfs_ext2() -> PathBuf {
    find_tool(&[
        "mkfs.ext2",
        "/opt/homebrew/opt/e2fsprogs/sbin/mkfs.ext2", // Homebrew on Apple Silicon
        "/usr/local/opt/e2fsprogs/sbin/mkfs.ext2",    // Homebrew on Intel macOS
        "/sbin/mkfs.ext2",
        "/usr/sbin/mkfs.ext2",
    ])
    .unwrap_or_else(|| {
        eprintln!("Error: mkfs.ext2 not found.");
        eprintln!("Install it:  macOS: brew install e2fsprogs   Linux: apt install e2fsprogs");
        eprintln!("(or run `brew bundle` from the repo root on macOS)");
        process::exit(1);
    })
}

/// Locate `debugfs` (from `e2fsprogs`), used to pre-populate the ext2 image
/// with test files so the ext2 server's read path can be validated.
fn find_debugfs() -> PathBuf {
    find_tool(&[
        "debugfs",
        "/opt/homebrew/opt/e2fsprogs/sbin/debugfs",
        "/usr/local/opt/e2fsprogs/sbin/debugfs",
        "/sbin/debugfs",
        "/usr/sbin/debugfs",
    ])
    .unwrap_or_else(|| {
        eprintln!("Error: debugfs not found (part of e2fsprogs).");
        eprintln!("Install it:  macOS: brew install e2fsprogs   Linux: apt install e2fsprogs");
        process::exit(1);
    })
}

// ============================================================================
// Filesystem image creation (cargo xtask image)
// ============================================================================

/// Path to the SquashFS root image.
fn root_image_path(root: &Path) -> PathBuf {
    root.join("target/themelios-root.squashfs")
}

/// Path to the ext2 data volume image.
fn data_image_path(root: &Path) -> PathBuf {
    root.join("target/themelios-data.ext2")
}

/// Build the staging directory tree that becomes the SquashFS root, returning
/// its path. Contents are deterministic so tests can assert exact bytes.
fn build_rootfs_staging(root: &Path) -> PathBuf {
    let staging = root.join("target/rootfs");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(staging.join("etc")).expect("create rootfs/etc");
    fs::create_dir_all(staging.join("docs")).expect("create rootfs/docs");
    fs::create_dir_all(staging.join("data")).expect("create rootfs/data");

    // Small files (these end up packed into a SquashFS fragment block).
    fs::write(staging.join("version"), b"THEMELIOS_ROOT\n").unwrap();
    fs::write(staging.join("etc/hostname"), b"themelios\n").unwrap();
    fs::write(staging.join("hello.txt"), b"Hello from SquashFS!\n").unwrap();
    fs::write(staging.join("docs/readme.txt"), b"nested file in /docs\n").unwrap();

    // A large file spanning multiple data blocks, with a deterministic pattern
    // the SquashFS server test reads back and verifies byte-for-byte.
    let big: Vec<u8> = (0..300 * 1024u32).map(|i| (i.wrapping_mul(31).wrapping_add(7) & 0xFF) as u8).collect();
    fs::write(staging.join("big.bin"), &big).unwrap();

    staging
}

/// Create the SquashFS root image (gzip-compressed) from the staging tree.
fn create_root_image(root: &Path) -> PathBuf {
    let staging = build_rootfs_staging(root);
    let out = root_image_path(root);
    let _ = fs::remove_file(&out);
    let mksquashfs = find_mksquashfs();

    println!("Creating SquashFS root image...");
    // -comp gzip: SquashFS default; matches the server's miniz_oxide inflate.
    // -noappend: overwrite rather than append. -all-root: deterministic uid/gid.
    // -no-xattrs: keep the on-disk format minimal for the server parser.
    let status = Command::new(&mksquashfs)
        .arg(&staging)
        .arg(&out)
        .args(["-comp", "gzip", "-noappend", "-all-root", "-no-xattrs"])
        .status()
        .expect("failed to run mksquashfs");
    if !status.success() {
        eprintln!("mksquashfs failed!");
        process::exit(1);
    }
    println!("  {}", out.display());
    out
}

/// Create the ext2 data volume image (16 MiB, 1 KiB blocks).
fn create_data_image(root: &Path) -> PathBuf {
    let out = data_image_path(root);
    let mkfs = find_mkfs_ext2();

    println!("Creating ext2 data image...");
    // Zero-fill 16 MiB, then format ext2 with 1 KiB blocks (simple, predictable
    // layout for the ext2 server). No journal (-O ^has_journal): ext2, not ext4.
    let zeros = vec![0u8; 16 * 1024 * 1024];
    fs::write(&out, &zeros).expect("failed to create ext2 backing file");
    // Feature notes: the ext2 server is a *linear* directory parser and reads a
    // classic on-disk layout. We disable features it does not implement so the
    // image is deterministic across e2fsprogs versions/hosts:
    //   ^has_journal   — ext2, not ext4 (no journal).
    //   ^resize_inode  — no reserved GDT blocks.
    //   ^dir_index     — no hashed (htree) directories; keep dirs linear. Some
    //                    e2fsprogs/debugfs versions lay out even small indexed
    //                    directories as htrees, which the linear parser misreads.
    //   ^metadata_csum,^64bit — no checksums / 64-bit fields the server ignores.
    let status = Command::new(&mkfs)
        .args([
            "-F", "-q", "-b", "1024", "-I", "256",
            "-O", "^has_journal,^resize_inode,^dir_index,^metadata_csum,^64bit",
        ])
        .arg(&out)
        .status()
        .expect("failed to run mkfs.ext2");
    if !status.success() {
        eprintln!("mkfs.ext2 failed!");
        process::exit(1);
    }

    // Pre-populate the (otherwise empty) image with deterministic test files so
    // the ext2 server's read path has real data to verify. The server's write
    // path (sub-phase 3.7 step 2) then creates files from inside the OS.
    populate_data_image(root, &out);

    println!("  {}", out.display());
    out
}

/// Write test files into an ext2 image using `debugfs`.
///
/// Creates `/hello.txt`, a `/sub/nested.txt`, and a 20 000-byte `/data.bin`
/// whose size forces use of single-indirect block pointers (>12 KiB with 1 KiB
/// blocks), all with deterministic contents the ext2 server test asserts on.
fn populate_data_image(root: &Path, image: &Path) {
    let src = root.join("target/ext2src");
    let _ = fs::remove_dir_all(&src);
    fs::create_dir_all(&src).expect("create ext2src");

    fs::write(src.join("hello.txt"), b"Hello from ext2!\n").unwrap();
    fs::write(src.join("nested.txt"), b"nested ext2 file\n").unwrap();
    // 20 000 bytes: 12 direct blocks (12 KiB) + the rest via single indirect.
    let data: Vec<u8> = (0..20_000u32)
        .map(|i| (i.wrapping_mul(37).wrapping_add(11) & 0xFF) as u8)
        .collect();
    fs::write(src.join("data.bin"), &data).unwrap();

    // A debugfs script: write files and make a subdirectory. Paths to host
    // source files are absolute; destinations are inside the image.
    let script = format!(
        "write {hello} hello.txt\nmkdir /sub\nwrite {nested} /sub/nested.txt\nwrite {data} data.bin\nquit\n",
        hello = src.join("hello.txt").display(),
        nested = src.join("nested.txt").display(),
        data = src.join("data.bin").display(),
    );
    let script_path = root.join("target/ext2-populate.debugfs");
    fs::write(&script_path, script).expect("write debugfs script");

    let debugfs = find_debugfs();
    let status = Command::new(&debugfs)
        .args(["-w", "-f"])
        .arg(&script_path)
        .arg(image)
        .status()
        .expect("failed to run debugfs");
    if !status.success() {
        eprintln!("debugfs population failed!");
        process::exit(1);
    }
}

/// Ensure both filesystem images exist, creating any that are missing.
///
/// Called from run/test so disks are present without rebuilding every time
/// (images are only created when absent). `cargo xtask image` forces a rebuild.
fn ensure_images(root: &Path) -> (PathBuf, PathBuf) {
    let squashfs = root_image_path(root);
    let ext2 = data_image_path(root);
    let squashfs = if squashfs.exists() { squashfs } else { create_root_image(root) };
    let ext2 = if ext2.exists() { ext2 } else { create_data_image(root) };
    (squashfs, ext2)
}

/// `cargo xtask image` — (re)create the SquashFS root and ext2 data images.
fn cmd_image(_args: &[String]) {
    let root = workspace_root();
    let squashfs = create_root_image(&root);
    let ext2 = create_data_image(&root);
    println!();
    println!("Images ready:");
    println!("  root (SquashFS): {}", squashfs.display());
    println!("  data (ext2):     {}", ext2.display());
}

// ============================================================================
// ISO image creation
// ============================================================================

/// Boot the aarch64 kernel on QEMU `virt` via Limine/UEFI (Phase 7).
///
/// `virt` has no BIOS, so this is UEFI-only: assemble an EFI System Partition (ESP)
/// tree containing Limine's `BOOTAA64.EFI`, the kernel, and `limine.conf`, hand it to
/// QEMU as a virtual-FAT disk, and boot it behind AAVMF/edk2 UEFI firmware. GICv2 is
/// pinned (`gic-version=2`) to match the Phase 7 bring-up.
fn run_aarch64(root: &Path, kernel: &Path, limine_dir: &Path, display: bool) {
    let (esp, code, vars) = prepare_aarch64_boot(root, kernel, limine_dir);

    let mode = if display { "with display" } else { "headless" };
    println!("Launching QEMU (aarch64 virt, {mode})...");
    println!("Press Ctrl+A, X to exit QEMU.\n");

    let mut cmd = qemu_aarch64_base(&esp, &code, &vars);

    // Attach the same devices the test path does. Without them an interactive arm64 boot
    // reports `0 device(s), 32 empty` and the shell has nothing to inspect — which made
    // 8.3's "VirtIO works on arm64" true only of `cargo xtask test`, a distinction the
    // docs did not draw. Cheap, and it means the shell and the suite see the same machine.
    let scratch = ensure_scratch_disk(root);
    let filler_a = ensure_filler_disk(root, "themelios-arm64-filler-a.img");
    let filler_b = ensure_filler_disk(root, "themelios-arm64-filler-b.img");
    cmd.args(virtio_disk_args_mmio(&scratch, "blkscratch", false));
    cmd.args(virtio_disk_args_mmio(&filler_a, "blkfilla", true));
    cmd.args(virtio_disk_args_mmio(&filler_b, "blkfillb", false));
    cmd.args(virtio_net_args_mmio("net0", None));
    cmd.args(virtio_mmio_modern_args());

    cmd.args(["-serial", "stdio"]);
    if !display {
        cmd.args(["-display", "none"]);
    }

    match cmd.status() {
        Ok(s) if s.success() => println!("\nQEMU exited cleanly."),
        Ok(_) => println!("\nQEMU exited."),
        Err(e) => {
            eprintln!("Failed to launch qemu-system-aarch64: {e}");
            process::exit(1);
        }
    }
}

/// Assemble the aarch64 UEFI boot inputs shared by `run --arch aarch64` and the CI
/// boot smoke: an ESP tree (`BOOTAA64.EFI` + kernel + `limine.conf`), the read-only
/// firmware CODE flash, and a fresh writable copy of the VARS flash. Returns
/// `(esp_dir, code_fd, vars_fd)`.
fn prepare_aarch64_boot(root: &Path, kernel: &Path, limine_dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let esp = root.join("target/aarch64-esp");
    let _ = fs::remove_dir_all(&esp);
    fs::create_dir_all(esp.join("EFI/BOOT")).expect("create ESP/EFI/BOOT");
    fs::create_dir_all(esp.join("boot/limine")).expect("create ESP/boot/limine");

    let bootaa64 = limine_dir.join("BOOTAA64.EFI");
    if !bootaa64.exists() {
        eprintln!(
            "Limine's BOOTAA64.EFI not found at {} — is the Limine binary branch complete?",
            bootaa64.display()
        );
        process::exit(1);
    }
    fs::copy(&bootaa64, esp.join("EFI/BOOT/BOOTAA64.EFI")).expect("copy BOOTAA64.EFI");
    fs::copy(kernel, esp.join("boot/themelios")).expect("copy kernel to ESP");
    fs::copy(root.join("limine.conf"), esp.join("boot/limine/limine.conf"))
        .expect("copy limine.conf to ESP");

    let (code, vars_src) = match find_aavmf() {
        Some(pair) => pair,
        None => {
            eprintln!(
                "aarch64 UEFI firmware (AAVMF/edk2) not found. Install it — e.g.\n  \
                 Debian/Ubuntu: apt-get install qemu-efi-aarch64\n  \
                 macOS (brew):  brew install qemu (bundles edk2 firmware)"
            );
            process::exit(1);
        }
    };
    let vars = root.join("target/aarch64-vars.fd");
    fs::copy(&vars_src, &vars).expect("copy AAVMF VARS flash");
    (esp, code, vars)
}

/// A `qemu-system-aarch64 -M virt` command with the firmware pflash pair and the ESP
/// virtual-FAT disk attached (no serial/display — the caller adds those).
fn qemu_aarch64_base(esp: &Path, code: &Path, vars: &Path) -> Command {
    let mut cmd = Command::new("qemu-system-aarch64");
    cmd.args(["-M", "virt,gic-version=2", "-cpu", "cortex-a72", "-m", "512M", "-no-reboot"]);
    // Without this every virtio-mmio device is presented as legacy (v1) — see
    // `virtio_mmio_modern_args`.
    cmd.args(virtio_mmio_modern_args());
    cmd.arg("-drive").arg(format!("if=pflash,format=raw,readonly=on,file={}", code.display()));
    cmd.arg("-drive").arg(format!("if=pflash,format=raw,file={}", vars.display()));
    cmd.arg("-drive").arg(format!("file=fat:rw:{},format=raw,if=virtio", esp.display()));
    cmd
}

/// `cargo xtask arm64-smoke` — build the aarch64 kernel, boot it headless on QEMU
/// `virt`, and assert it reaches the boot banner. This is the CI proof that the
/// Limine → EL1 → PL011 path works on aarch64, not just that the kernel compiles.
///
/// The 7.0b kernel idles after the banner (no self-shutdown until the timer/exit
/// mechanism lands), so we capture serial to a file, poll for the banner marker, then
/// terminate QEMU. Absence of the marker within the window is a failure.
fn cmd_arm64_smoke(_args: &[String]) {
    let root = workspace_root();

    println!("Building the aarch64 kernel for the boot smoke...");
    let status = Command::new("cargo")
        .current_dir(&root)
        .args([
            "build", "--package", "themelios", "--target", "aarch64-unknown-none-softfloat",
            BUILD_STD, BUILD_STD_FEATURES,
        ])
        .status()
        .expect("Failed to build the aarch64 kernel");
    if !status.success() {
        eprintln!("arm64 boot smoke FAILED: kernel did not build for aarch64.");
        process::exit(1);
    }

    let kernel = root.join("target/aarch64-unknown-none-softfloat/debug/themelios");
    let limine_dir = ensure_limine(&root);
    let (esp, code, vars) = prepare_aarch64_boot(&root, &kernel, &limine_dir);

    let serial_log = root.join("target/aarch64-smoke-serial.log");
    let _ = fs::remove_file(&serial_log);

    println!("Booting aarch64 kernel on QEMU virt (headless)...");
    let sock = root.join("target/aarch64-smoke.sock");
    let mut cmd = qemu_aarch64_base(&esp, &code, &vars);
    cmd.args(["-display", "none", "-monitor", "none"]);
    cmd.arg("-chardev")
        .arg(format!("socket,id=s0,path={},server=on,wait=off", sock.display()));
    cmd.args(["-serial", "chardev:s0"]);

    await_aarch64_banner(cmd, &sock, &serial_log, "ESP");
}

/// `cargo xtask arm64-iso-smoke` — build the **arm64 ISO** and boot *it* on QEMU
/// `virt`, asserting the kernel reaches its banner.
///
/// `arm64-smoke` boots from a directory-backed UEFI ESP, which proves the kernel
/// works but says nothing about the released image. This boots the actual
/// `themelios-arm64.iso` artifact, so the UEFI-only ISO layout — EFI El Torito with
/// `BOOTAA64.EFI` and no BIOS scaffolding — is verified rather than assumed.
fn cmd_arm64_iso_smoke(args: &[String]) {
    let root = workspace_root();

    // Build the kernel and wrap it in the real arm64 ISO.
    let mut iso_args: Vec<String> = args.to_vec();
    iso_args.push("--arch".to_string());
    iso_args.push("aarch64".to_string());
    cmd_build(&iso_args);

    let kernel = root.join("target/aarch64-unknown-none-softfloat/debug/themelios");
    let limine_dir = ensure_limine(&root);
    let iso = create_iso(&root, &kernel, &limine_dir, "aarch64-unknown-none-softfloat");

    let (code, vars) = match find_aavmf() {
        Some(pair) => pair,
        None => {
            eprintln!(
                "aarch64 UEFI firmware (AAVMF/edk2) not found. Install it — e.g.\n  \
                 Debian/Ubuntu: apt-get install qemu-efi-aarch64"
            );
            process::exit(1);
        }
    };
    // The VARS flash must be writable, so work on a copy rather than the system one.
    let vars_copy = root.join("target/aarch64-iso-vars.fd");
    fs::copy(&vars, &vars_copy).expect("copy AAVMF VARS flash");

    let serial_log = root.join("target/aarch64-iso-smoke-serial.log");
    let _ = fs::remove_file(&serial_log);

    println!("Booting the aarch64 ISO on QEMU virt (headless)...");
    let mut cmd = Command::new("qemu-system-aarch64");
    cmd.args(["-M", "virt,gic-version=2", "-cpu", "cortex-a72", "-m", "512M", "-no-reboot"]);
    // Without this every virtio-mmio device is presented as legacy (v1) — see
    // `virtio_mmio_modern_args`.
    cmd.args(virtio_mmio_modern_args());
    cmd.arg("-drive")
        .arg(format!("if=pflash,format=raw,readonly=on,file={}", code.display()));
    cmd.arg("-drive")
        .arg(format!("if=pflash,format=raw,file={}", vars_copy.display()));
    // Attach the ISO as a read-only VirtIO disk. `virt` has no IDE/ATAPI, so
    // `-cdrom` is not the right shape here; UEFI boots the EFI System Partition
    // that xorriso's `-efi-boot-part` embedded in the image.
    cmd.arg("-drive")
        .arg(format!("file={},format=raw,if=virtio,readonly=on", iso.display()));
    let sock = root.join("target/aarch64-iso-smoke.sock");
    cmd.args(["-display", "none", "-monitor", "none"]);
    cmd.arg("-chardev")
        .arg(format!("socket,id=s0,path={},server=on,wait=off", sock.display()));
    cmd.args(["-serial", "chardev:s0"]);

    await_aarch64_banner(cmd, &sock, &serial_log, "ISO");
}

/// Run `cmd` (a configured headless `qemu-system-aarch64`) and wait for the kernel's
/// boot banner to appear in `serial_log`, then terminate QEMU.
///
/// The 7.0b kernel idles after the banner (there is no self-shutdown until the
/// timer/exit mechanism lands), so we cannot wait for the process to exit: we poll
/// the captured serial for the marker and kill QEMU once we see it. Absence of the
/// marker within the window is a failure. `what` names the boot medium in the
/// pass/fail message so the two smokes are distinguishable in CI logs.
fn await_aarch64_banner(mut cmd: Command, sock: &Path, serial_log: &Path, what: &str) {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};

    let _ = fs::remove_file(sock);
    let mut child = cmd.spawn().expect("Failed to launch qemu-system-aarch64");

    // The success sentinel, which advances with each sub-phase to the *last* thing the
    // boot path proves — currently the shell's own banner, printed by the shell task
    // rather than by `shell::init`, so reaching it proves the task was actually
    // scheduled and not merely spawned.
    //
    // It is deliberately never left pointing at an earlier milestone. Every marker so
    // far (the 7.0b banner, the 7.1 paging sentinel, the 7.2 timer sentinel, the 7.3
    // scheduler sentinel) prints before the work the next sub-phase adds, so leaving it
    // behind would let that work break while the smoke saw its marker and stopped
    // looking. 7.4 nearly shipped having broken exactly that rule.
    const MARKER: &str = "ThemeliOS debug shell";

    // Failure signatures, checked before the marker on every poll. Shared with
    // `cmd_test_aarch64` — see `AARCH64_BOOT_FAILURES`.
    const FAILURES: &[&str] = AARCH64_BOOT_FAILURES;

    // Connect to QEMU's serial socket. It is created with `server=on,wait=off`, so the
    // listener may not exist for a moment after spawn.
    let mut stream = None;
    for _ in 0..100 {
        thread::sleep(Duration::from_millis(100));
        if let Ok(s) = UnixStream::connect(sock) {
            stream = Some(s);
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
    }
    let Some(stream) = stream else {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("arm64 {what} smoke FAILED: QEMU never opened its serial socket.");
        process::exit(1);
    };

    // Drain the socket on a background thread. A reader thread rather than polling a
    // file because the socket is bidirectional — being able to type back is the point.
    // Accumulate *bytes*, decode once at the end. Decoding each 4 KiB read separately
    // destroys any multi-byte sequence that straddles a read boundary — the kernel's
    // help text is full of em-dashes, and an earlier version of this turned them into
    // replacement characters in the log it printed.
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let reader = {
        let captured = Arc::clone(&captured);
        let mut rx = stream.try_clone().expect("clone serial socket");
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = rx.read(&mut buf) {
                if n == 0 {
                    break;
                }
                captured.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        })
    };

    // Matching is done on a lossy view; every marker and failure signature is ASCII, so
    // this cannot change a verdict, and it keeps the byte log faithful for humans.
    let seen = |needle: &str| {
        String::from_utf8_lossy(&captured.lock().unwrap()).contains(needle)
    };
    let hit_failure = || {
        let bytes = captured.lock().unwrap();
        let s = String::from_utf8_lossy(&bytes);
        FAILURES.iter().find(|f| s.contains(**f)).map(|f| (*f).to_string())
    };

    // Kept so the write half can be dropped before draining the reader.
    let stream_tx = stream.try_clone().expect("clone serial socket");

    let mut found = false;
    let mut failure: Option<String> = None;
    let mut qemu_exited = false;
    // ~40 s budget. CI runners are slower than a dev box, and the boot runs four
    // self-tests before the shell starts.
    for _ in 0..80 {
        thread::sleep(Duration::from_millis(500));
        if let Some(f) = hit_failure() {
            failure = Some(f);
            break;
        }
        if seen(MARKER) {
            found = true;
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            // QEMU exited on its own — the kernel should be idling at a shell prompt,
            // so this is a death, not a completion. Recorded so the message below can
            // say which, the way `cmd_test_aarch64` already does.
            qemu_exited = true;
            break;
        }
    }

    // If the shell came up, type at it. This is what exercises the receive path — the
    // PL011 RX interrupt, the GIC SPI route, the ring buffer, the wake, and the line
    // editor — none of which booting alone touches.
    //
    // It is here because that gap let two real defects ship: a handler that buffered
    // bytes without ever waking the shell, and an acknowledge ordering that could wedge
    // the console permanently. Both were found by hand; neither survives this.
    let mut answered = false;
    if found && failure.is_none() {
        let mut tx = stream;
        let _ = tx.write_all(b"help\r");
        let _ = tx.flush();
        for _ in 0..40 {
            thread::sleep(Duration::from_millis(250));
            // Match a line unique to the command's *output*, not the echoed input, so
            // this proves the shell dispatched the command rather than merely echoing.
            if seen("show memory statistics") {
                answered = true;
                break;
            }
            if let Some(f) = hit_failure() {
                failure = Some(f);
                break;
            }
        }
    }

    // Close the write half, then let the reader drain what QEMU already sent before
    // killing it. Snapshotting straight after `kill()` loses whatever was still sitting
    // in the socket buffer — worst precisely when it matters, since a panic emitted
    // just before the deadline is the tail of the log.
    drop(stream_tx);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();

    let serial = String::from_utf8_lossy(&captured.lock().unwrap()).into_owned();
    // Keep writing the log file: CI and humans both go looking for it.
    let _ = fs::write(serial_log, &serial);
    println!("--- aarch64 serial output ({what}) ---\n{serial}\n-----------------------------");

    // Re-check the failure signatures against the *complete* log. The polling above
    // only sampled every 500 ms and stopped at the deadline, so a panic landing in the
    // final interval — or in the bytes drained after the kill — would otherwise be
    // reported as "sentinel not seen", while the panic sits in the log just printed.
    let failure = failure.or_else(|| {
        FAILURES.iter().find(|f| serial.contains(**f)).map(|f| (*f).to_string())
    });

    if let Some(f) = failure {
        eprintln!("arm64 {what} smoke FAILED: kernel reported '{f}' (see serial above).");
        process::exit(1);
    }
    // The platform description is what every driver now takes its device bases and
    // interrupt numbers from, so a wrong provider is a wrong machine. `platform.rs` calls
    // this "the line that makes a wrong provider obvious" — which is only true if
    // something looks at it.
    const PLATFORM_LINE: &str = "[platform] QEMU virt (aarch64, GICv2)";
    if !serial.contains(PLATFORM_LINE) {
        eprintln!(
            "arm64 {what} smoke FAILED: expected '{PLATFORM_LINE}' in the boot log — the \
             platform provider is wrong or missing."
        );
        process::exit(1);
    }

    if !found {
        if qemu_exited {
            eprintln!(
                "arm64 {what} smoke FAILED: QEMU exited before the shell came up, with \
                 no failure signature in the log — the kernel died early."
            );
        } else {
            eprintln!(
                "arm64 {what} smoke FAILED: sentinel '{MARKER}' not seen within the \
                 window, and QEMU was still running — the kernel hung."
            );
        }
        process::exit(1);
    }
    if !answered {
        eprintln!(
            "arm64 {what} smoke FAILED: the shell started but did not answer a typed \
             command — serial input is not reaching it (check the PL011 receive \
             interrupt, the GIC SPI route, and that the handler wakes the shell task)."
        );
        process::exit(1);
    }
    println!(
        "arm64 {what} smoke passed: booted, switched to kernel page tables, passed the \
         paging, exception, timer and scheduler self-tests, started the shell, and \
         answered a typed command on QEMU virt."
    );
}

/// Locate the aarch64 UEFI firmware pair (CODE, VARS) across common distro/macOS
/// paths. Returns `None` if not found (resolved at runtime like the `mkfs.ext2`
/// lookup, rather than hardcoded).
fn find_aavmf() -> Option<(PathBuf, PathBuf)> {
    let pairs = [
        ("/usr/share/AAVMF/AAVMF_CODE.fd", "/usr/share/AAVMF/AAVMF_VARS.fd"),
        ("/usr/share/qemu-efi-aarch64/QEMU_EFI.fd", "/usr/share/qemu-efi-aarch64/QEMU_VARS.fd"),
        ("/usr/share/edk2/aarch64/QEMU_EFI.fd", "/usr/share/edk2/aarch64/QEMU_VARS.fd"),
        ("/opt/homebrew/share/qemu/edk2-aarch64-code.fd", "/opt/homebrew/share/qemu/edk2-arm-vars.fd"),
        ("/usr/local/share/qemu/edk2-aarch64-code.fd", "/usr/local/share/qemu/edk2-arm-vars.fd"),
    ];
    for (code, vars) in pairs {
        if Path::new(code).exists() && Path::new(vars).exists() {
            return Some((PathBuf::from(code), PathBuf::from(vars)));
        }
    }
    None
}

/// Create a bootable ISO image containing the kernel and Limine bootloader.
///
/// The layout differs by architecture, because the firmware does:
///
/// - **amd64** — a hybrid BIOS + UEFI image. A BIOS El Torito boot sector
///   (`limine-bios-cd.bin`) plus `limine-bios.sys` and a `limine bios-install` pass, *and*
///   a UEFI El Torito image (`limine-uefi-cd.bin`) carrying `BOOTX64.EFI`. The same ISO
///   boots on legacy BIOS and on UEFI.
/// - **arm64** — UEFI only, carrying `BOOTAA64.EFI`. QEMU `virt` and arm64 platforms
///   generally have no BIOS, so there is no BIOS scaffolding to add.
///
/// Directory structure inside the ISO (amd64; arm64 omits the three BIOS entries and
/// swaps `BOOTX64.EFI` for `BOOTAA64.EFI`):
/// ```text
/// /boot/limine/limine-bios.sys    — Limine BIOS second-stage loader
/// /boot/limine/limine-bios-cd.bin — BIOS El Torito boot image
/// /boot/limine/limine-uefi-cd.bin — UEFI El Torito boot image
/// /boot/limine/limine.conf        — Bootloader configuration
/// /boot/themelios                 — The kernel ELF binary
/// /EFI/BOOT/BOOTX64.EFI           — UEFI fallback bootloader
/// ```
fn create_iso(root: &Path, kernel_path: &Path, limine_dir: &Path, target: &str) -> PathBuf {
    // aarch64 `virt` (and arm64 platforms generally) are UEFI-only: there is no BIOS,
    // so no BIOS El Torito image, no `limine-bios.sys`, and no `bios-install` pass.
    // The arm64 ISO is therefore a pure EFI El Torito image carrying BOOTAA64.EFI.
    let arm64 = target == "aarch64-unknown-none-softfloat";
    let arch_tag = if arm64 { "arm64" } else { "amd64" };

    // Separate staging dirs and outputs so building one arch never clobbers the
    // other — the release job builds both from the same checkout.
    let iso_root = root.join(format!("target/iso_root_{arch_tag}"));
    let iso_path = root.join(format!("target/themelios-{arch_tag}.iso"));

    println!("Creating bootable {arch_tag} ISO...");

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
    let limine_files: &[(&str, &str)] = if arm64 {
        &[
            ("limine-uefi-cd.bin", "boot/limine/limine-uefi-cd.bin"),
            ("BOOTAA64.EFI", "EFI/BOOT/BOOTAA64.EFI"),
        ]
    } else {
        &[
            ("limine-bios.sys", "boot/limine/limine-bios.sys"),
            ("limine-bios-cd.bin", "boot/limine/limine-bios-cd.bin"),
            ("limine-uefi-cd.bin", "boot/limine/limine-uefi-cd.bin"),
            ("BOOTX64.EFI", "EFI/BOOT/BOOTX64.EFI"),
        ]
    };

    for (src_name, dst_path) in limine_files {
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
    let mut cmd = Command::new("xorriso");
    cmd.args(["-as", "mkisofs"]);
    if !arm64 {
        // BIOS El Torito image — x86 only.
        cmd.args([
            "-b", "boot/limine/limine-bios-cd.bin",
            "-no-emul-boot",
            "-boot-load-size", "4",
            "-boot-info-table",
        ]);
    }
    cmd.args([
        "--efi-boot", "boot/limine/limine-uefi-cd.bin",
        "-efi-boot-part",
        "--efi-boot-image",
        "--protective-msdos-label",
    ]);
    let status = cmd
        .arg(iso_root.to_str().unwrap())
        .arg("-o")
        .arg(iso_path.to_str().unwrap())
        .status()
        .expect("Failed to run xorriso. Install with: brew install xorriso");

    if !status.success() {
        eprintln!("ISO creation failed!");
        process::exit(1);
    }

    if !arm64 {
        // Install Limine's BIOS boot sectors onto the ISO.
        // This patches the ISO's MBR so that BIOS firmware can boot from it
        // without relying on El Torito (which some BIOSes don't support for
        // hard drive-style boot). Meaningless on arm64, which has no BIOS.
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

    // Build the userspace servers first — the kernel embeds their flat binaries
    // via include_bytes!, so they must exist before the kernel compiles.
    build_servers(&root);

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
/// cargo xtask iso                  # target/themelios-amd64.iso
/// cargo xtask iso --arch aarch64   # target/themelios-arm64.iso
/// qemu-system-x86_64 -M q35 -cdrom target/themelios-amd64.iso -serial stdio -no-reboot
/// ```
///
/// The two images differ in more than the kernel inside them: amd64 is a hybrid
/// BIOS+UEFI ISO, arm64 is UEFI-only (see [`create_iso`]).
fn cmd_iso(args: &[String]) {
    let opts = parse_options(args);
    let target = resolve_target(&opts.arch);
    let root = workspace_root();

    cmd_build(args);

    let kernel_binary = root.join(format!("target/{target}/debug/themelios"));
    let limine_dir = ensure_limine(&root);
    let iso_path = create_iso(&root, &kernel_binary, &limine_dir, target);

    println!();
    if target == "aarch64-unknown-none-softfloat" {
        println!("To boot headless:  cargo xtask run --arch aarch64");
        println!("To smoke the ISO:  cargo xtask arm64-iso-smoke");
    } else {
        println!("To boot headless:  cargo xtask run");
        println!(
            "To boot with GUI:  qemu-system-x86_64 -M q35 -cdrom {} -serial stdio -no-reboot",
            iso_path.display()
        );
    }
}

/// Build the kernel, create a bootable ISO, and launch it in QEMU.
fn cmd_run(args: &[String]) {
    let opts = parse_options(args);
    let target = resolve_target(&opts.arch);
    let root = workspace_root();

    // Step 1: Build the kernel
    cmd_build(args);

    let kernel_binary = root.join(format!("target/{target}/debug/themelios"));

    // Step 2: Download Limine if needed. Both architectures boot via Limine; aarch64
    // is UEFI-only (no BIOS), so it takes a separate ESP path.
    let limine_dir = ensure_limine(&root);

    if matches!(opts.arch.as_str(), "aarch64" | "arm64") {
        run_aarch64(&root, &kernel_binary, &limine_dir, opts.display);
        return;
    }

    let iso_path = create_iso(&root, &kernel_binary, &limine_dir, target);

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
            // -no-reboot: turn a guest reset request into a shutdown rather than a
            //   restart, so a triple-faulting kernel stops once instead of looping.
            //
            // -no-shutdown is deliberately NOT passed. It used to be, to "keep QEMU alive
            //   after guest shutdown" -- which made sense when a guest halt only ever
            //   meant a crash worth inspecting. Now that the shell has a `shutdown`
            //   command it means the opposite: QEMU pauses instead of exiting, the
            //   terminal never comes back, and the command someone typed in order to
            //   leave appears to do nothing. Dropping it also makes amd64 behave like
            //   aarch64, which never passed it.
            let mut cmd = Command::new(qemu);
            cmd.current_dir(&root)
                .args([
                    "-M", "q35",
                    "-m", "256M",
                    "-cdrom", iso_path.to_str().unwrap(),
                    "-serial", "stdio",
                    "-no-reboot",
                ]);

            // Attach VirtIO block disks. Order fixes PCI slot assignment:
            // scratch first (writable, lowest slot — used by block R/W tests),
            // then the read-only SquashFS root, then the ext2 data volume.
            let scratch = ensure_scratch_disk(&root);
            cmd.args(virtio_disk_args(&scratch, "blkscratch", false));
            let (squashfs, ext2) = ensure_images(&root);
            cmd.args(virtio_disk_args(&squashfs, "blkroot", true));
            cmd.args(virtio_disk_args(&ext2, "blkdata", false));

            // Attach a VirtIO NIC on user-mode networking (Phase 4).
            cmd.args(virtio_net_args("net0"));

            // In headless mode, suppress the QEMU graphical window.
            // With --display, let QEMU use its default backend (Cocoa/GTK/SDL).
            if !opts.display {
                cmd.args(["-display", "none"]);
            }

            cmd.status()
        }
        // aarch64 is handled earlier (UEFI path) and returns before this match.
        "aarch64" | "arm64" => unreachable!("aarch64 handled by run_aarch64"),
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

/// Build the kernel with test feature, boot in QEMU, and check exit code.
///
/// Builds the kernel with `--features test`, which makes `kmain` run the
/// test harness instead of the interactive shell. QEMU is launched with
/// the `isa-debug-exit` device so the kernel can signal pass/fail:
///
/// - QEMU exit code `3` (kernel wrote `0x01`) → all tests passed
/// - QEMU exit code `1` (kernel wrote `0x00`) → some test failed
/// - Timeout (see `timeout_secs` below) → kernel hung or panicked
///
/// Serial output is captured and printed on failure or timeout.
fn cmd_test(args: &[String]) {
    let opts = parse_options(args);
    let target = resolve_target(&opts.arch);
    let root = workspace_root();

    // aarch64 runs the same suite but cannot report its verdict the same way: the
    // `virt` machine has no `isa-debug-exit`, so the result comes off the serial
    // console. Different enough in its mechanics to warrant its own function.
    if target == "aarch64-unknown-none-softfloat" {
        cmd_test_aarch64(&root, target);
        return;
    }

    println!("Building ThemeliOS kernel (test mode) for {target}...");

    // Build the userspace servers first (the kernel embeds their binaries).
    build_servers(&root);

    // Clean cached artifacts to ensure build.rs re-runs (fresh ULID).
    let _ = Command::new("cargo")
        .current_dir(&root)
        .args(["clean", "--package", "themelios", "--target", target])
        .status();

    // Build the kernel with the test feature enabled.
    let status = Command::new("cargo")
        .current_dir(&root)
        .args([
            "build",
            "--package", "themelios",
            "--target", target,
            "--features", "test",
            BUILD_STD,
            BUILD_STD_FEATURES,
        ])
        .status()
        .expect("Failed to execute cargo build");

    if !status.success() {
        eprintln!("Test build failed!");
        process::exit(1);
    }

    let kernel_binary = root.join(format!("target/{target}/debug/themelios"));

    // Create bootable ISO
    let limine_dir = ensure_limine(&root);
    let iso_path = create_iso(&root, &kernel_binary, &limine_dir, target);

    // Launch QEMU with isa-debug-exit device and a timeout.
    let qemu = qemu_binary(&opts.arch);
    println!("Running tests in QEMU...\n");

    // The suite boots several userspace filesystem and network servers, each doing
    // IPC + block I/O, and a healthy run is now tens of seconds — not the "~few
    // seconds" this budget was originally sized against. At 90s a slow or loaded CI
    // runner could exhaust the budget mid-suite and report a *timeout* for what was
    // only slowness, which is indistinguishable from a real kernel hang (this
    // happened on the Phase 7.0b PR). 180s keeps a genuine hang bounded while leaving
    // headroom as the suite grows. Note the workflow's `timeout-minutes` must stay
    // comfortably above this plus a full from-scratch build, or GitHub kills the job
    // first — which is a worse diagnostic than the harness's own timeout.
    let timeout_secs = 180;

    let mut cmd = Command::new(qemu);
    cmd.current_dir(&root)
        .args([
            "-M", "q35",
            "-m", "256M",
            "-cdrom", iso_path.to_str().unwrap(),
            "-serial", "stdio",
            "-display", "none",
            "-no-reboot",
            // isa-debug-exit: writing to port 0xf4 causes QEMU to exit
            // with code (value << 1) | 1
            "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04",
        ]);

    // Regenerate the ext2 data image fresh for every test run: the ext2 write
    // tests mutate it, so a stale image from a previous run would make them
    // non-deterministic. (The SquashFS root is read-only and is left cached.)
    let _ = fs::remove_file(data_image_path(&root));

    // Attach VirtIO block disks (same order as `run`): scratch (writable),
    // SquashFS root (read-only), ext2 data volume. The kernel identifies each
    // by probing its on-disk magic, so order only fixes slot assignment.
    let scratch = ensure_scratch_disk(&root);
    cmd.args(virtio_disk_args(&scratch, "blkscratch", false));
    let (squashfs, ext2) = ensure_images(&root);
    cmd.args(virtio_disk_args(&squashfs, "blkroot", true));
    cmd.args(virtio_disk_args(&ext2, "blkdata", false));

    // Attach a VirtIO NIC on user-mode networking (Phase 4) so net tests have a
    // device to bind. slirp answers ARP for the gateway (10.0.2.2). A `hostfwd`
    // rule maps host 127.0.0.1:TCP_TEST_HOST_PORT → guest :TCP_TEST_GUEST_PORT so
    // the host-side peer below can reach `test_tcp_server`'s listener.
    let hostfwd = format!(
        "hostfwd=tcp:127.0.0.1:{}-:{TCP_TEST_GUEST_PORT}", tcp_test_host_port()
    );
    cmd.args(virtio_net_args_fwd("net0", Some(&hostfwd)));

    // Host-side TCP peer for `test_tcp_server`: repeatedly connect to the guest's
    // listener (via the hostfwd above), send a known payload, and read the echo.
    // The guest asserts it received the payload; this thread just drives the
    // connection. It retries until the guest is listening, then stops.
    spawn_tcp_test_peer();

    let child = cmd
        .stdout(process::Stdio::inherit())
        .stderr(process::Stdio::inherit())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to launch {qemu}: {e}");
            eprintln!("Is QEMU installed? See docs/src/dev-setup.md for instructions.");
            process::exit(1);
        }
    };

    // Wait for QEMU with a timeout
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // QEMU exited. Check the exit code.
                // isa-debug-exit: writing 0x01 → exit code 3 (success)
                //                 writing 0x00 → exit code 1 (failure)
                let code = status.code().unwrap_or(-1);
                println!();
                if code == 3 {
                    println!("All tests passed.");
                    process::exit(0);
                } else if code == 1 {
                    eprintln!("Some tests FAILED (QEMU exit code {code}).");
                    process::exit(1);
                } else {
                    eprintln!("QEMU exited with unexpected code {code}.");
                    eprintln!("The kernel may have panicked or triple-faulted.");
                    process::exit(1);
                }
            }
            Ok(None) => {
                // Still running — check timeout
                if start.elapsed().as_secs() >= timeout_secs {
                    eprintln!("\nTest TIMEOUT after {timeout_secs} seconds.");
                    eprintln!("The kernel may be hung or in an infinite loop.");
                    let _ = child.kill();
                    let _ = child.wait();
                    process::exit(1);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("Error waiting for QEMU: {e}");
                process::exit(1);
            }
        }
    }
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

/// `cargo xtask arm64-gate` — compile smoltcp alone for `aarch64-unknown-none-softfloat`.
///
/// Phase 4 runs and tests only on amd64, but the TCP/IP stack (smoltcp) is meant
/// to be architecture-independent so the Phase 7 arm64 port inherits it unchanged.
/// This builds the `servers/smoltcp-gate` crate — a minimal `no_std` lib pinned to
/// the net server's exact smoltcp feature set — for aarch64. A failure here means
/// the stack has taken an amd64-only (or `std`) dependency. Compile-only; no QEMU.
fn cmd_arm64_gate(_args: &[String]) {
    let root = workspace_root();
    let gate_dir = root.join("servers/smoltcp-gate");

    println!("Compiling smoltcp for aarch64-unknown-none-softfloat (arm64 dependency gate)...");
    let status = Command::new("cargo")
        .current_dir(&gate_dir)
        .args([
            "build",
            "--target", "aarch64-unknown-none-softfloat",
            BUILD_STD,
        ])
        .status()
        .expect("Failed to execute cargo build for the arm64 gate");

    if !status.success() {
        eprintln!("arm64 gate FAILED: smoltcp did not build for aarch64-unknown-none-softfloat.");
        process::exit(1);
    }
    println!("  smoltcp: OK");

    // Phase 7: also build the kernel itself for aarch64. From 7.0b the kernel boots on
    // aarch64 (QEMU virt), so this gate must catch any change that breaks the aarch64
    // build — "amd64 stays green" alone is insufficient once the arch seam is shared.
    // The kernel embeds no userspace servers on aarch64 (they are cfg-gated out), so a
    // direct `cargo build` needs no prior server staging.
    println!("Compiling the kernel for aarch64-unknown-none-softfloat (arm64 kernel gate)...");
    let kstatus = Command::new("cargo")
        .current_dir(&root)
        .args([
            "build",
            "--package", "themelios",
            "--target", "aarch64-unknown-none-softfloat",
            BUILD_STD,
            BUILD_STD_FEATURES,
        ])
        .status()
        .expect("Failed to execute cargo build for the aarch64 kernel");
    if !kstatus.success() {
        eprintln!("arm64 gate FAILED: the kernel did not build for aarch64-unknown-none-softfloat.");
        process::exit(1);
    }
    println!("  kernel: OK");
    println!("arm64 gate passed: smoltcp + kernel build for aarch64-unknown-none-softfloat.");
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
    image    Create the SquashFS root and ext2 data disk images
    docs     Build mdbook and rustdoc
    arm64-gate       Compile smoltcp + kernel for aarch64-unknown-none-softfloat (dependency gate)
    arm64-smoke      Boot the aarch64 kernel on QEMU virt from a UEFI ESP (banner smoke)
    arm64-iso-smoke  Boot the aarch64 ISO on QEMU virt (banner smoke)

Options:
    --arch <ARCH>  Target architecture: amd64 (default) or arm64.
                   Accepts amd64 | x86_64 | x86-64, and arm64 | aarch64.
                   Prefer naming it explicitly on both sides:
                       cargo xtask test --arch amd64
                       cargo xtask test --arch arm64
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
        "image" => cmd_image(rest),
        "docs" => cmd_docs(rest),
        "arm64-gate" => cmd_arm64_gate(rest),
        "arm64-smoke" => cmd_arm64_smoke(rest),
        "arm64-iso-smoke" => cmd_arm64_iso_smoke(rest),
        "help" | "--help" | "-h" => print_usage(),
        unknown => {
            eprintln!("Unknown command: {unknown}\n");
            print_usage();
            process::exit(1);
        }
    }
}
