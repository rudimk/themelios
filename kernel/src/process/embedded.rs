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
//! As of 8.5d this module is built for **both** architectures, and the directory is
//! selected by the `server_blob!` macro below — the `#[cfg]` that 8.5a introduced the
//! partitioned staging directory *for*, and that three sub-phases of comments promised.
//! Its two arms must stay in step with `xtask`'s `stage_arch`.
//!
//! Until 8.5d the paths named `amd64` literally, because `mod process` was x86-only and
//! the kernel side had no choice to get wrong. Now it has one, and getting it wrong is not
//! subtle: an arm64 kernel embedding x86 blobs faults as an undefined instruction at EL0,
//! arbitrarily far from here.
//!
//! The `.elf` blobs additionally carry a compile-time check that they are for the right
//! architecture — see below. The flat `.bin` servers cannot carry one: `--oformat=binary`
//! leaves no ELF header to read. They are covered instead by the partitioned directory and
//! by the committed hash manifest (`xtask/servers-amd64.sha256`, checked by `cargo xtask
//! verify-servers`), which is what proves across the Phase 8 userspace port that the amd64
//! blobs did not move.

/// Embed a staged server blob from **this kernel's** architecture directory.
///
/// Two `#[cfg]`'d arms rather than a `const` spliced with `concat!`: `concat!` operates on
/// literals, so a `const SERVER_ARCH: &str` does not compose with it — `include_bytes!`
/// reports "expected a literal" at all thirteen call sites. The macro keeps the choice in
/// one place while handing `concat!` the two literals it needs.
#[cfg(target_arch = "x86_64")]
macro_rules! server_blob {
    ($name:literal) => {
        include_bytes!(concat!("../../../target/servers/amd64/", $name))
    };
}

#[cfg(target_arch = "aarch64")]
macro_rules! server_blob {
    ($name:literal) => {
        include_bytes!(concat!("../../../target/servers/arm64/", $name))
    };
}

/// The echo server: a minimal IPC echo used to validate the server framework
/// (sub-phase 3.4). Replaced/joined by the real filesystem servers in 3.5+.
pub static ECHO_SERVER: &[u8] = server_blob!("echo-server.bin");

/// The api-server (Phase 6.5): the ring-3 Docker Engine API control plane. Holds a
/// spawn-granted Management cap, listens via the management ABI, and serves the read
/// (GET) endpoint pipeline. Spawned in `kmain` normal mode and by `test_api_server`.
pub static API_SERVER: &[u8] = server_blob!("api-server.bin");

/// The SquashFS server: reads the read-only SquashFS root image (sub-phase 3.5).
pub static SQUASHFS_SERVER: &[u8] = server_blob!("squashfs-server.bin");

/// The overlay server: RAM upper layer merged over a read-only lower (3.6).
pub static OVERLAY_SERVER: &[u8] = server_blob!("overlay-server.bin");

/// The ext2 server: read-write ext2 for persistent data volumes (3.7).
pub static EXT2_SERVER: &[u8] = server_blob!("ext2-server.bin");

/// The filesystem syscall test client: exercises the VFS syscalls from ring 3 (3.8).
pub static FSTEST_CLIENT: &[u8] = server_blob!("fstest-client.bin");

/// The net server: the ring-3 TCP/IP stack (smoltcp) driven over the kernel net
/// service's frame bridge (Phase 4.2).
pub static NET_SERVER: &[u8] = server_blob!("net-server.bin");

/// Phase 5.0 ELF loader smoke-test binary — a **real ELF** (not a flat binary),
/// loaded by `crate::linux::elf` in `test_elf_exec`. Native ThemeliOS ABI.
#[cfg_attr(not(feature = "test"), allow(dead_code))]
pub static ELF_SMOKE: &[u8] = server_blob!("elf-smoke.elf");

// --- The Linux-personality smoke ELFs: x86_64 only ---
//
// `xtask`'s `SERVER_ELFS_ARM64` stages exactly one detached ELF for aarch64 (`elf-smoke`,
// which speaks the native ABI). The five below issue Linux syscalls by their x86_64
// numbers, and `mod linux` — which holds both the personality and the ELF loader — is
// x86-gated until Phase 8.9. Embedding them here on aarch64 would fail the build outright:
// the files do not exist in `target/servers/arm64/`.
//
// So the `#[cfg]` is not a policy choice, it is the same list as `SERVER_ELFS_ARM64`,
// expressed where the blobs are consumed. The two must move together in 8.9.

/// Phase 5.1 Linux-personality smoke-test binary — a real ELF that speaks the
/// **Linux** syscall ABI, run by `test_linux_exec`.
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(feature = "test"), allow(dead_code))]
pub static LINUX_SMOKE: &[u8] = server_blob!("linux-smoke.elf");

/// Phase 5.2 Linux FS-syscall smoke-test binary — opens/reads a file from its
/// container rootfs and checks path clamping, run by `test_linux_fs`.
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(feature = "test"), allow(dead_code))]
pub static FS_SMOKE: &[u8] = server_blob!("fs-smoke.elf");

/// Phase 5.3 Linux threads/futex smoke-test binary — clones a thread and joins it
/// via futex, run by `test_linux_threads`.
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(feature = "test"), allow(dead_code))]
pub static THREADS_SMOKE: &[u8] = server_blob!("threads-smoke.elf");

/// Phase 5.7 container-isolation smoke-test binary — as a container `/init`,
/// proves the rootfs `..` clamp is live (byte-matched escape read) and that
/// `socket()` is denied with `-EPERM`, run by `test_container_isolation`.
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(feature = "test"), allow(dead_code))]
pub static ISOLATION_SMOKE: &[u8] =
    server_blob!("isolation-smoke.elf");

/// Phase 6.1b container rootfs-confinement smoke-test binary — as a *confined*
/// container `/init`, proves it can read its own `/only` but cannot open a
/// `/host_secret` that exists at the shared mount root. Run by
/// `test_container_confinement`.
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(feature = "test"), allow(dead_code))]
pub static CONFINE_SMOKE: &[u8] = server_blob!("confine-smoke.elf");

// --- Compile-time architecture check on the embedded ELFs ---
//
// `e_machine` lives at offset 18 of the ELF header, little-endian. Asserting it here means
// a blob built for the wrong architecture fails the *kernel build*, rather than being
// loaded and faulting at its first instruction in userspace with no trace of why.
//
// Only the detached `.elf` smoke binaries can be checked *here*. The flat `.bin` servers
// are linked with `--oformat=binary`, so they are raw memory images with no header —
// nothing to assert on, which is why the staging directory is partitioned by architecture
// rather than relying on a check like this one, and why their contents are pinned by
// `xtask`'s hash manifest instead.
//
// `xtask` performs the same check on the built artifact before staging it, which catches a
// stale file from a previous build for the other architecture. Two checks, different
// moments: that one guards the copy, this one guards the embed.

/// The `e_machine` this kernel's embedded ELFs must carry.
///
/// 62 is `EM_X86_64`, 183 is `EM_AARCH64`. Mirrors `xtask`'s `expected_e_machine`, which
/// checks the same thing on the build side before staging — two checks at different
/// moments, as 8.5a set up: that one guards the copy, this one guards the embed.
#[cfg(target_arch = "x86_64")]
const EXPECTED_E_MACHINE: u16 = 62;
#[cfg(target_arch = "aarch64")]
const EXPECTED_E_MACHINE: u16 = 183;

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

// `elf-smoke` is embedded on both architectures, so this one assertion is the only part of
// the check that is live on aarch64 — and it is the part that matters there, since it is
// the only ELF an arm64 kernel carries.
const _: () = assert!(elf_machine(ELF_SMOKE) == EXPECTED_E_MACHINE);

#[cfg(target_arch = "x86_64")]
const _: () = assert!(elf_machine(LINUX_SMOKE) == EXPECTED_E_MACHINE);
#[cfg(target_arch = "x86_64")]
const _: () = assert!(elf_machine(FS_SMOKE) == EXPECTED_E_MACHINE);
#[cfg(target_arch = "x86_64")]
const _: () = assert!(elf_machine(THREADS_SMOKE) == EXPECTED_E_MACHINE);
#[cfg(target_arch = "x86_64")]
const _: () = assert!(elf_machine(ISOLATION_SMOKE) == EXPECTED_E_MACHINE);
#[cfg(target_arch = "x86_64")]
const _: () = assert!(elf_machine(CONFINE_SMOKE) == EXPECTED_E_MACHINE);
