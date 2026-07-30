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
fn ensure_scratch_disk(root: &Path) -> PathBuf {
    let disk_path = root.join("target/themelios-scratch.img");

    if !disk_path.exists() {
        // 4 MiB of zeros. Enough for the driver to report a sane capacity and
        // for round-trip read/write sector tests in sub-phase 3.2.
        const SIZE_BYTES: usize = 4 * 1024 * 1024;
        let zeros = vec![0u8; SIZE_BYTES];
        fs::write(&disk_path, &zeros)
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
/// `cargo xtask test`, for the Phase 4.6 `test_tcp_server` round-trip.
const TCP_TEST_HOST_PORT: u16 = 15007;
/// Guest TCP port the server test listens on.
const TCP_TEST_GUEST_PORT: u16 = 7;

/// The payload the host peer sends and `test_tcp_server` echoes back.
const TCP_TEST_PAYLOAD: &[u8] = b"THEMELIOS_TCP_PING";

/// Spawn a detached host thread that connects to the guest TCP listener (via the
/// `hostfwd` rule), sends [`TCP_TEST_PAYLOAD`], and reads the echo. It retries
/// until the guest is listening (early connects are refused), then stops after a
/// successful exchange. Failures are silent: the guest side asserts the
/// round-trip; this thread only drives the connection.
fn spawn_tcp_test_peer() {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::{Duration, Instant};

    std::thread::spawn(|| {
        let sa: SocketAddr = match format!("127.0.0.1:{TCP_TEST_HOST_PORT}").parse() {
            Ok(a) => a,
            Err(_) => return,
        };
        let deadline = Instant::now() + Duration::from_secs(85);
        while Instant::now() < deadline {
            match TcpStream::connect_timeout(&sa, Duration::from_millis(500)) {
                Ok(mut stream) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
                    if stream.write_all(TCP_TEST_PAYLOAD).is_ok() {
                        // Read the echo back (keeps the connection open long enough
                        // for the guest to recv + echo), then we're done.
                        let mut buf = [0u8; 64];
                        let _ = stream.read(&mut buf);
                        return;
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

/// Build the kernel with test feature, boot in QEMU, and check exit code.
///
/// Builds the kernel with `--features test`, which makes `kmain` run the
/// test harness instead of the interactive shell. QEMU is launched with
/// the `isa-debug-exit` device so the kernel can signal pass/fail:
///
/// - QEMU exit code `3` (kernel wrote `0x01`) → all tests passed
/// - QEMU exit code `1` (kernel wrote `0x00`) → some test failed
/// - Timeout (30 seconds) → kernel hung or panicked
///
/// Serial output is captured and printed on failure or timeout.
fn cmd_test(args: &[String]) {
    let opts = parse_options(args);
    let target = resolve_target(&opts.arch);
    let root = workspace_root();

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
    let iso_path = create_iso(&root, &kernel_binary, &limine_dir);

    // Launch QEMU with isa-debug-exit device and a timeout.
    let qemu = qemu_binary(&opts.arch);
    println!("Running tests in QEMU...\n");

    // 90s: the suite now boots several userspace filesystem servers, each doing
    // IPC + block I/O, so give QEMU comfortable headroom over the ~few seconds a
    // healthy run takes (a real hang still trips well before this).
    let timeout_secs = 90;

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
        "hostfwd=tcp:127.0.0.1:{TCP_TEST_HOST_PORT}-:{TCP_TEST_GUEST_PORT}"
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

/// `cargo xtask arm64-gate` — compile smoltcp alone for `aarch64-unknown-none`.
///
/// Phase 4 runs and tests only on amd64, but the TCP/IP stack (smoltcp) is meant
/// to be architecture-independent so the Phase 7 arm64 port inherits it unchanged.
/// This builds the `servers/smoltcp-gate` crate — a minimal `no_std` lib pinned to
/// the net server's exact smoltcp feature set — for aarch64. A failure here means
/// the stack has taken an amd64-only (or `std`) dependency. Compile-only; no QEMU.
fn cmd_arm64_gate(_args: &[String]) {
    let root = workspace_root();
    let gate_dir = root.join("servers/smoltcp-gate");

    println!("Compiling smoltcp for aarch64-unknown-none (arm64 dependency gate)...");
    let status = Command::new("cargo")
        .current_dir(&gate_dir)
        .args([
            "build",
            "--target", "aarch64-unknown-none",
            BUILD_STD,
        ])
        .status()
        .expect("Failed to execute cargo build for the arm64 gate");

    if status.success() {
        println!("arm64 gate passed: smoltcp builds for aarch64-unknown-none.");
    } else {
        eprintln!("arm64 gate FAILED: smoltcp did not build for aarch64-unknown-none.");
        process::exit(1);
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
    image    Create the SquashFS root and ext2 data disk images
    docs     Build mdbook and rustdoc
    arm64-gate  Compile smoltcp for aarch64-unknown-none (dependency gate)

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
        "image" => cmd_image(rest),
        "docs" => cmd_docs(rest),
        "arm64-gate" => cmd_arm64_gate(rest),
        "help" | "--help" | "-h" => print_usage(),
        unknown => {
            eprintln!("Unknown command: {unknown}\n");
            print_usage();
            process::exit(1);
        }
    }
}
