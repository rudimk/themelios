//! # Container metadata registry (Phase 6.1)
//!
//! A first-class notion of "a container" — the metadata the Docker Engine API
//! needs (`docker ps`, `docker inspect`) that the raw process table doesn't hold:
//! a stable container **id**, a **name**, the **image** it came from, when it was
//! **created**, its **command** (argv), and its **state**, all keyed to the
//! backing process pid.
//!
//! The table is owned here (not in the process table) so a container's metadata
//! **survives its process**: `docker ps -a` and `docker logs` must work on a
//! container that has already exited, but a pid is torn down on exit and pids
//! recycle. A row lives from `create` until an explicit [`remove`] (`docker rm`).
//!
//! ## Rootfs scope (documented limitation)
//!
//! Every container is currently assembled onto the single shared `/data` ext2
//! mount. Per-container **rootfs isolation** (so two *simultaneously running*
//! containers don't collide on absolute paths) needs either a mount-per-container
//! mechanism — which the fs layer can't provide (mounts require a physical
//! ext2 disk and are append-only, never freed) — or confining each container to a
//! subdirectory, a change to the security-critical Phase 5.2 path resolver. That
//! confinement is deferred to its own sub-phase; this registry stores the mount id
//! each container was assembled onto so it lifts cleanly when confinement lands.

use crate::arch::x86_64::idt;
use crate::oci::sha256;
use crate::process::ProcessId;
use crate::sync::InterruptMutex;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

extern crate alloc;

use super::RunError;

/// A container's lifecycle state (a subset of Docker's, mapped to what we track).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContainerState {
    /// Created (metadata + process exist) but not yet started.
    Created,
    /// Running (its ring-3 task has been spawned and hasn't exited).
    Running,
    /// Exited with the given status code.
    Exited(u64),
}

impl ContainerState {
    /// The Docker-style lowercase status word (`created`/`running`/`exited`).
    pub fn as_str(&self) -> &'static str {
        match self {
            ContainerState::Created => "created",
            ContainerState::Running => "running",
            ContainerState::Exited(_) => "exited",
        }
    }
}

/// One container's metadata row.
#[derive(Clone)]
pub struct ContainerMeta {
    /// The 64-hex-char container id (Docker-style; id-prefix lookup is supported).
    pub id: String,
    /// The container name (user-supplied or auto-generated), without a leading `/`.
    pub name: String,
    /// The image reference the container was created from.
    pub image: String,
    /// Creation time as **milliseconds since boot** (no wall clock exists yet, so
    /// this is monotonic-since-boot, not a Unix timestamp — documented).
    pub created_ms: u64,
    /// The resolved command (entrypoint++cmd argv).
    pub command: Vec<String>,
    /// Current lifecycle state.
    pub state: ContainerState,
    /// The backing process pid (valid while `state != Exited` for a live process;
    /// retained for reference after exit).
    pub pid: ProcessId,
    /// The mount the rootfs was assembled onto (the shared `/data` mount for now).
    pub rootfs_mount: u64,
}

/// The global container table.
static CONTAINERS: InterruptMutex<Vec<ContainerMeta>> = InterruptMutex::new(Vec::new());

/// A monotonic counter mixed into generated ids so two containers created in the
/// same tick still get distinct ids.
static ID_SEQ: AtomicU64 = AtomicU64::new(1);

/// Auto-name counter for containers created without an explicit name.
static NAME_SEQ: AtomicU64 = AtomicU64::new(1);

/// Milliseconds since boot (10 ms per PIT tick, matching `SYS_UPTIME_MS`).
fn now_ms() -> u64 {
    idt::tick_count().wrapping_mul(10)
}

/// Generate a fresh Docker-style 64-hex-char container id from the sequence
/// counter + the current tick (SHA-256 of the pair, so ids are opaque and
/// collision-resistant, and id-prefix lookup behaves like Docker's).
fn generate_id() -> String {
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut seed = seq.to_string();
    seed.push('-');
    seed.push_str(&idt::tick_count().to_string());
    sha256::hex(&sha256::sha256(seed.as_bytes()))
}

/// Create a container from a staged image and record its metadata (state
/// `Created`). Returns the new container id. `name` may be empty to auto-generate.
///
/// The image is resolved by [`resolve_bundle`]; live registry pull is deferred
/// (Phase 5.6's transport is offline-only), so only staged/embedded images resolve.
pub fn create_from_image(image: &str, name: &str) -> Result<String, RunError> {
    let mount = crate::fs::data_mount().ok_or(RunError::Rootfs)?;
    create_on_mount(image, name, mount)
}

/// Like [`create_from_image`] but onto an explicit `mount` rather than the default
/// `/data` mount. Used where the caller manages the mount directly (tests).
///
/// The container is **confined** (Phase 6.1b) to a `/c/<id>` subdirectory of the
/// mount, so multiple containers on the shared mount cannot see each other's (or
/// the mount root's) files. The id is generated first so it can name the base.
pub fn create_on_mount(image: &str, name: &str, mount: u64) -> Result<String, RunError> {
    let bundle = resolve_bundle(image)?;
    let id = generate_id();
    let mut base = String::from("/c/");
    base.push_str(&id);
    let (pid, command) = super::create_confined(&bundle, mount, Some(&base))?;
    let name = if name.is_empty() {
        let n = NAME_SEQ.fetch_add(1, Ordering::Relaxed);
        let mut s = String::from("container-");
        s.push_str(&n.to_string());
        s
    } else {
        String::from(name)
    };

    CONTAINERS.lock().push(ContainerMeta {
        id: id.clone(),
        name,
        image: String::from(image),
        created_ms: now_ms(),
        command,
        state: ContainerState::Created,
        pid,
        rootfs_mount: mount,
    });
    Ok(id)
}

/// Resolve an image reference to an image bundle. Only staged/embedded images are
/// available (live registry pull is deferred): `"demo"` (and the empty string)
/// map to the built-in demo bundle. Anything else is an error.
fn resolve_bundle(image: &str) -> Result<Vec<u8>, RunError> {
    match image {
        "" | "demo" | "demo:latest" => Ok(super::demo_bundle()),
        _ => Err(RunError::Unpack(crate::oci::OciError::NoManifest)),
    }
}

/// Update a container's state by id (exact match).
pub fn set_state(id: &str, state: ContainerState) {
    if let Some(c) = CONTAINERS.lock().iter_mut().find(|c| c.id == id) {
        c.state = state;
    }
}

/// Mark the container backed by `pid` as `Exited(code)` (called when a container
/// process exits). No-op if no container tracks that pid.
pub fn note_exit(pid: ProcessId, code: u64) {
    if let Some(c) = CONTAINERS.lock().iter_mut().find(|c| c.pid == pid) {
        c.state = ContainerState::Exited(code);
    }
}

/// Look up a container by exact id, id-prefix, or exact name (Docker resolution
/// order: id/prefix first, then name). Returns a clone of the row.
pub fn lookup(id_or_name: &str) -> Option<ContainerMeta> {
    let table = CONTAINERS.lock();
    // Exact id, then unambiguous id-prefix, then exact name.
    if let Some(c) = table.iter().find(|c| c.id == id_or_name) {
        return Some(c.clone());
    }
    if !id_or_name.is_empty() {
        let mut it = table.iter().filter(|c| c.id.starts_with(id_or_name));
        if let Some(c) = it.next() {
            if it.next().is_none() {
                return Some(c.clone());
            }
            // Ambiguous prefix — fall through to a name match.
        }
    }
    table.iter().find(|c| c.name == id_or_name).cloned()
}

/// Snapshot the whole table (for `docker ps`).
pub fn list() -> Vec<ContainerMeta> {
    CONTAINERS.lock().clone()
}

/// Remove a container's metadata row by exact id (`docker rm`). Returns whether a
/// row was removed. (Freeing the backing process/rootfs is the caller's job.)
pub fn remove(id: &str) -> bool {
    let mut table = CONTAINERS.lock();
    let before = table.len();
    table.retain(|c| c.id != id);
    table.len() != before
}
