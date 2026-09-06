//! # Embedded server binaries
//!
//! The userspace servers are compiled to flat binaries (see the `servers/`
//! workspace and the server linker script) and embedded directly in the kernel
//! image with `include_bytes!`. The kernel never reads them from disk and never
//! parses an executable format — at spawn time it simply copies these bytes into
//! ring-3 pages and jumps in (see `process::server::spawn_server`).
//!
//! ## Build dependency
//!
//! These files are produced by `cargo xtask build`/`test`, which builds the
//! `servers/` workspace and copies each flat binary to
//! `target/servers/<arch>/` **before** compiling the kernel. Building the kernel
//! directly with `cargo` (bypassing xtask) requires those files to already exist.
//!
//! ## The staging directory is architecture-partitioned, and must stay that way
//!
//! Until Phase 8.5a these paths were `target/servers/<name>.bin`, unqualified. That was
//! safe only because one architecture existed. The moment the server target became a
//! parameter it became a trap: `cargo xtask build --arch arm64` after an amd64 build would
//! have embedded **x86 blobs** in an arm64 kernel, which fails as an undefined-instruction
//! abort at EL0 — arbitrarily far from the cause, with nothing pointing back here.
//!
//! This whole module is `#[cfg(target_arch = "x86_64")]` today (it lives under `mod
//! process`, which is), so the paths below name `amd64` directly rather than through a
//! `cfg`. When 8.5b ports `libthemelios` and the six `_start` routines, this file grows an
//! aarch64 arm and the two sets of paths must stay in step with `xtask`'s `stage_arch`.
//!
//! The `.elf` blobs additionally carry a compile-time check that they are for the right
//! architecture — see below. The flat `.bin` servers cannot: `--oformat=binary` leaves no
//! ELF header to read, so their only protection is the partitioned directory.

/// The echo server: a minimal IPC echo used to validate the server framework
/// (sub-phase 3.4). Replaced/joined by the real filesystem servers in 3.5+.
pub static ECHO_SERVER: &[u8] = include_bytes!("../../../target/servers/amd64/echo-server.bin");

/// The api-server (Phase 6.5): the ring-3 Docker Engine API control plane. Holds a
/// spawn-granted Management cap, listens via the management ABI, and serves the read
/// (GET) endpoint pipeline. Spawned in `kmain` normal mode and by `test_api_server`.
pub static API_SERVER: &[u8] = include_bytes!("../../../target/servers/amd64/api-server.bin");

/// The SquashFS server: reads the read-only SquashFS root image (sub-phase 3.5).
pub static SQUASHFS_SERVER: &[u8] = include_bytes!("../../../target/servers/amd64/squashfs-server.bin");

/// The overlay server: RAM upper layer merged over a read-only lower (3.6).
pub static OVERLAY_SERVER: &[u8] = include_bytes!("../../../target/servers/amd64/overlay-server.bin");

/// The ext2 server: read-write ext2 for persistent data volumes (3.7).
pub static EXT2_SERVER: &[u8] = include_bytes!("../../../target/servers/amd64/ext2-server.bin");

/// The filesystem syscall test client: exercises the VFS syscalls from ring 3 (3.8).
pub static FSTEST_CLIENT: &[u8] = include_bytes!("../../../target/servers/amd64/fstest-client.bin");

/// The net server: the ring-3 TCP/IP stack (smoltcp) driven over the kernel net
/// service's frame bridge (Phase 4.2).
pub static NET_SERVER: &[u8] = include_bytes!("../../../target/servers/amd64/net-server.bin");

/// Phase 5.0 ELF loader smoke-test binary — a **real ELF** (not a flat binary),
/// loaded by `crate::linux::elf` in `test_elf_exec`. Native ThemeliOS ABI.
#[cfg_attr(not(feature = "test"), allow(dead_code))]
pub static ELF_SMOKE: &[u8] = include_bytes!("../../../target/servers/amd64/elf-smoke.elf");

/// Phase 5.1 Linux-personality smoke-test binary — a real ELF that speaks the
/// **Linux** syscall ABI, run by `test_linux_exec`.
#[cfg_attr(not(feature = "test"), allow(dead_code))]
pub static LINUX_SMOKE: &[u8] = include_bytes!("../../../target/servers/amd64/linux-smoke.elf");

/// Phase 5.2 Linux FS-syscall smoke-test binary — opens/reads a file from its
/// container rootfs and checks path clamping, run by `test_linux_fs`.
#[cfg_attr(not(feature = "test"), allow(dead_code))]
pub static FS_SMOKE: &[u8] = include_bytes!("../../../target/servers/amd64/fs-smoke.elf");

/// Phase 5.3 Linux threads/futex smoke-test binary — clones a thread and joins it
/// via futex, run by `test_linux_threads`.
#[cfg_attr(not(feature = "test"), allow(dead_code))]
pub static THREADS_SMOKE: &[u8] = include_bytes!("../../../target/servers/amd64/threads-smoke.elf");

/// Phase 5.7 container-isolation smoke-test binary — as a container `/init`,
/// proves the rootfs `..` clamp is live (byte-matched escape read) and that
/// `socket()` is denied with `-EPERM`, run by `test_container_isolation`.
#[cfg_attr(not(feature = "test"), allow(dead_code))]
pub static ISOLATION_SMOKE: &[u8] =
    include_bytes!("../../../target/servers/amd64/isolation-smoke.elf");

/// Phase 6.1b container rootfs-confinement smoke-test binary — as a *confined*
/// container `/init`, proves it can read its own `/only` but cannot open a
/// `/host_secret` that exists at the shared mount root. Run by
/// `test_container_confinement`.
#[cfg_attr(not(feature = "test"), allow(dead_code))]
pub static CONFINE_SMOKE: &[u8] = include_bytes!("../../../target/servers/amd64/confine-smoke.elf");

// --- Compile-time architecture check on the embedded ELFs ---
//
// `e_machine` lives at offset 18 of the ELF header, little-endian. Asserting it here means
// a blob built for the wrong architecture fails the *kernel build*, rather than being
// loaded and faulting at its first instruction in userspace with no trace of why.
//
// Only the detached `.elf` smoke binaries can be checked. The flat `.bin` servers are
// linked with `--oformat=binary`, so they are raw memory images with no header — nothing
// to assert on, and the reason the staging directory is partitioned by architecture
// instead of relying on a check like this one.
//
// `xtask` performs the same check on the built artifact before staging it, which catches a
// stale file from a previous build for the other architecture. Two checks, different
// moments: that one guards the copy, this one guards the embed.

/// ELF `e_machine` for x86-64.
const EM_X86_64: u16 = 62;

/// Read `e_machine` from an embedded ELF image.
const fn elf_machine(image: &[u8]) -> u16 {
    // `include_bytes!` always yields at least a full header for these; a short file would
    // have failed the build already.
    assert!(image.len() > 19, "embedded ELF is too short to have a header");
    assert!(
        image[0] == 0x7f && image[1] == b'E' && image[2] == b'L' && image[3] == b'F',
        "embedded blob is not an ELF"
    );
    (image[18] as u16) | ((image[19] as u16) << 8)
}

const _: () = assert!(elf_machine(ELF_SMOKE) == EM_X86_64);
const _: () = assert!(elf_machine(LINUX_SMOKE) == EM_X86_64);
const _: () = assert!(elf_machine(FS_SMOKE) == EM_X86_64);
const _: () = assert!(elf_machine(THREADS_SMOKE) == EM_X86_64);
const _: () = assert!(elf_machine(ISOLATION_SMOKE) == EM_X86_64);
const _: () = assert!(elf_machine(CONFINE_SMOKE) == EM_X86_64);
