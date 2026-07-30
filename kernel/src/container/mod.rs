//! # Container runtime (Phase 5.5)
//!
//! The payoff of Phases 5.0–5.4: take an OCI/docker-save image, assemble its
//! rootfs on a writable mount, and launch its entrypoint as a **capability-
//! isolated Linux process** whose only filesystem authority is that rootfs.
//!
//! ```text
//!   image bundle ──oci::unpack──▶ files+config
//!                     │
//!                     ├─ write files to a rootfs mount (kmkdir/kcreate/kwrite)
//!                     └─ load /entrypoint ELF *from the rootfs* (VfsByteSource)
//!                            │
//!                            ▼
//!             a Linux process: rootfs_mount = the assembled rootfs,
//!             argv = entrypoint++cmd, env, cwd — run in ring 3
//! ```
//!
//! Isolation is structural (the thesis): the container process holds only its
//! rootfs; Linux path syscalls resolve against that one mount and cannot escape
//! (Phase 5.2). Its exit status is captured (Phase 5.5) so a waiter can report it.
//!
//! **Containment note:** `oci::unpack` parses untrusted image bytes and currently
//! runs in the kernel. It is safe `alloc`-only Rust that returns `Result` (no
//! `unsafe`, no panic on bad input), so a bug is a contained error, not memory
//! corruption. Relocating the parser into a dedicated ring-3 `oci-server` is a
//! documented hardening step (the parser lifts unchanged) — it does not change
//! this runtime's shape.

use crate::fs;
use crate::linux::elf::{self, ByteSource, ElfError};
use crate::oci;
use crate::process::{self, Personality, ProcessId};
use alloc::string::String;
use alloc::vec::Vec;

extern crate alloc;

/// Errors from launching a container.
#[derive(Debug)]
pub enum RunError {
    /// The image bundle could not be unpacked.
    Unpack(oci::OciError),
    /// Writing the rootfs to the mount failed.
    Rootfs,
    /// The image has no entrypoint (empty Entrypoint and Cmd).
    NoEntrypoint,
    /// The entrypoint ELF could not be loaded from the rootfs.
    Load(ElfError),
}

/// An [`elf::ByteSource`] that reads an ELF from a file on a VFS mount, so the
/// loader can load a container's entrypoint straight out of its rootfs.
pub struct VfsByteSource {
    mount: u64,
    fd: u64,
    size: usize,
}

impl VfsByteSource {
    /// Open `path` on `mount` for reading. Fails if it isn't a regular file.
    pub fn open(mount: u64, path: &[u8]) -> Result<Self, ()> {
        let (size, is_dir) = fs::kstat(mount, path).map_err(|_| ())?;
        if is_dir {
            return Err(());
        }
        let fd = fs::kopen(mount, path).map_err(|_| ())?;
        Ok(Self { mount, fd, size: size as usize })
    }
    /// Close the underlying VFS fd (call once loading is done).
    pub fn close(&self) {
        let _ = fs::kclose(self.mount, self.fd);
    }
}

impl ByteSource for VfsByteSource {
    fn len(&self) -> usize {
        self.size
    }
    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<(), ElfError> {
        // The VFS may return short reads; loop until `buf` is filled.
        let mut done = 0usize;
        while done < buf.len() {
            match fs::kread(self.mount, self.fd, (off + done) as u64, &mut buf[done..]) {
                Ok(0) => return Err(ElfError::TooSmall), // EOF before the range end
                Ok(n) => done += n,
                Err(_) => return Err(ElfError::TooSmall),
            }
        }
        Ok(())
    }
}

/// Create every ancestor directory of `path` on `mount` (best-effort; existing
/// dirs are fine). `path` is absolute, e.g. `/bin/hello` creates `/bin`.
fn ensure_parent_dirs(mount: u64, path: &str) {
    let mut acc = String::new();
    // Split on '/', creating each prefix directory except the final component.
    let comps: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    for comp in &comps[..comps.len().saturating_sub(1)] {
        if comp.is_empty() {
            continue;
        }
        acc.push('/');
        acc.push_str(comp);
        let _ = fs::kmkdir(mount, acc.as_bytes()); // ignore AlreadyExists
    }
}

/// Write an unpacked image's files onto `mount`, creating parent directories.
fn assemble_rootfs(mount: u64, image: &oci::Image) -> Result<(), RunError> {
    for f in &image.files {
        if f.is_dir {
            ensure_parent_dirs(mount, &f.path);
            let _ = fs::kmkdir(mount, f.path.as_bytes());
            continue;
        }
        ensure_parent_dirs(mount, &f.path);
        let fd = match fs::kcreate(mount, f.path.as_bytes()) {
            Ok(fd) => fd,
            // If it already exists, open it and overwrite from the start.
            Err(_) => fs::kopen(mount, f.path.as_bytes()).map_err(|_| RunError::Rootfs)?,
        };
        if !f.data.is_empty() {
            fs::kwrite(mount, fd, 0, &f.data).map_err(|_| RunError::Rootfs)?;
        }
        let _ = fs::kclose(mount, fd);
    }
    Ok(())
}

/// Unpack `bundle`, assemble its rootfs on `mount`, and create (but do **not**
/// yet start) the container process: a Linux process rooted at `mount`, with argv
/// = entrypoint++cmd, the image env, and cwd. Returns the new pid so the caller
/// can do any final setup (e.g. mapping a region) before [`start`].
pub fn create(bundle: &[u8], mount: u64) -> Result<ProcessId, RunError> {
    let image = oci::unpack(bundle).map_err(RunError::Unpack)?;
    assemble_rootfs(mount, &image)?;

    // Entrypoint binary path + argv (entrypoint then cmd); fall back to cmd alone.
    let mut argv: Vec<String> = Vec::new();
    argv.extend(image.config.entrypoint.iter().cloned());
    argv.extend(image.config.cmd.iter().cloned());
    let entry_path = argv.first().cloned().ok_or(RunError::NoEntrypoint)?;

    // Load the entrypoint ELF straight out of the assembled rootfs.
    let src = VfsByteSource::open(mount, entry_path.as_bytes()).map_err(|_| RunError::Rootfs)?;
    let (pid, _) = process::create_process("container", None);
    // Set the rootfs first so path syscalls resolve inside it from the start.
    process::set_rootfs_mount(pid, mount);
    let load = elf::load_into(pid, &src);
    src.close();
    let img = load.map_err(RunError::Load)?;

    // Build argv/env as &str slices for the initial stack.
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let env_refs: Vec<&str> = image.config.env.iter().map(|s| s.as_str()).collect();
    let rsp =
        elf::build_initial_stack(pid, &img, &argv_refs, &env_refs).map_err(RunError::Load)?;

    process::set_user_entry(pid, img.entry, rsp);
    process::set_personality(pid, Personality::Linux);
    process::set_cwd(pid, image.config.cwd.clone());
    Ok(pid)
}

/// Start a container process created by [`create`] (spawns its ring-3 task).
pub fn start(pid: ProcessId) {
    crate::linux::spawn_loaded("container", pid);
}

// --- Demo image (for the `run` shell command) ---

/// Build one 512-byte USTAR header (see `oci::tar` for the reader side).
fn tar_header(name: &str, size: usize, typeflag: u8) -> [u8; 512] {
    let mut h = [0u8; 512];
    let nb = name.as_bytes();
    let n = nb.len().min(100);
    h[..n].copy_from_slice(&nb[..n]);
    h[100..108].copy_from_slice(b"0000644\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    let s = alloc::format!("{:011o}\0", size);
    h[124..136].copy_from_slice(s.as_bytes());
    h[136..148].copy_from_slice(b"00000000000\0");
    h[156] = typeflag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    let cs = alloc::format!("{:06o}\0 ", sum);
    h[148..156].copy_from_slice(cs.as_bytes());
    h
}

/// Assemble a tar archive from `(name, data, typeflag)` entries.
fn make_tar(entries: &[(&str, &[u8], u8)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data, tf) in entries {
        out.extend_from_slice(&tar_header(name, data.len(), *tf));
        out.extend_from_slice(data);
        let pad = (512 - (data.len() % 512)) % 512;
        out.extend(core::iter::repeat(0u8).take(pad));
    }
    out.extend(core::iter::repeat(0u8).take(1024));
    out
}

/// Build a self-contained demo image: a single-layer docker-save bundle whose
/// entrypoint `/init` is the embedded `linux-smoke` binary. Used by the `run`
/// shell command to demonstrate the container runtime without a staged image
/// (real-image staging arrives with the registry in 5.6).
pub fn demo_bundle() -> Vec<u8> {
    let init = crate::process::embedded::LINUX_SMOKE;
    let layer = make_tar(&[("init", init, b'0')]);
    let config = br#"{"config":{"Entrypoint":["/init"],"Env":["PATH=/bin"],"WorkingDir":"/"}}"#;
    let manifest = br#"[{"Config":"config.json","Layers":["layer.tar"]}]"#;
    make_tar(&[
        ("manifest.json", manifest, b'0'),
        ("config.json", config, b'0'),
        ("layer.tar", &layer, b'0'),
    ])
}
