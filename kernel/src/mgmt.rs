//! # Container management ABI (Phase 6.3)
//!
//! The **capability-guarded ring-0 surface** that the node's control plane (the
//! ring-3 `api-server`, landing in Phase 6.5) drives to answer Docker Engine API
//! requests: `list`, `inspect`, `create`, `start`, `stop`, `logs`, `node_info`,
//! and `listen` (open an inbound-TCP listener). It is the trusted counterpart to
//! the untrusted HTTP/JSON parsing that stays in ring 3 — every container
//! lifecycle mutation happens here, in the kernel, behind a single capability
//! check.
//!
//! ## Why a dedicated authority
//!
//! Managing containers is *ambient authority* if left ungated: any process could
//! start, stop, or enumerate every container on the node. The capability model
//! forbids that. So management is minted as one coarse sentinel capability —
//! [`CapType::Management`](crate::cap::CapType::Management) — exactly analogous to
//! the network authority ([`SOCKET_FACTORY`](crate::cap::SOCKET_FACTORY)): holding
//! it grants *all* management operations; not holding it denies *every* one.
//!
//! **Invariant.** The `Management` cap is minted only to the `api-server`, never
//! into a container's CSpace (a container is created with an empty CSpace, so it
//! can never reach this ABI). This is the confused-deputy guard: management ops
//! take a **container id** (resolved through [`registry::lookup`]), never an
//! arbitrary pid, so even the api-server cannot be tricked into acting on a
//! kernel service — kernel services have no registry row, and pids never recycle.
//!
//! ## Shape
//!
//! Each op is a plain kernel function: it (1) resolves the caller's `Management`
//! cap, (2) emits an [`AuditOp::ApiAccess`] audit entry, then (3) does its work
//! and returns **owned data** (a `Vec<u8>` of compact Engine-API JSON, or, for
//! `listen`, a freshly minted `Socket` capability handle). Returning owned bytes —
//! rather than borrowing kernel tables across the ring boundary — is what lets the
//! ring-3 api-server consume the result with no shared-lifetime hazard.
//!
//! Because these are ordinary cap-checked functions returning owned data (no
//! shared IPC region, no server round-trip), they are tested directly, the same
//! way `net::socket::sys_socket` is: grant the cap, call the op, assert.
//!
//! ## Scope (Phase 6.3)
//!
//! This sub-phase lands the ABI and its capability gate. The **syscall/IPC
//! wrappers** that expose it to ring 3, and the `ServerConfig`/`spawn_server`
//! change that *grants* the sentinel cap to the api-server, are deferred to Phase
//! 6.4 (which de-risks the OS's first ring-3 inbound TCP and first sentinel-cap
//! grant together). `listen` mints a `Socket` cap here; the api-server will
//! `accept` on it through the ordinary socket ABI (`sys_accept`) — there is no
//! separate management `accept`.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use crate::audit::{self, AuditOp};
use crate::cap::{CapHandle, CapRights, CapType, Capability};
use crate::container::{self, registry};
use crate::container::registry::{ContainerMeta, ContainerState};
use crate::net::socket;
use crate::oci::json::Value;
use crate::process::{self, ProcessId};

// --- Management op numbers (audit `detail`) ---
//
// These identify which management verb was invoked in the audit trail. They are
// internal to the audit record, not an ABI number space (the syscall/IPC layer,
// Phase 6.4, assigns its own).
const OP_LIST: u64 = 1;
const OP_INSPECT: u64 = 2;
const OP_CREATE: u64 = 3;
const OP_START: u64 = 4;
const OP_STOP: u64 = 5;
const OP_LOGS: u64 = 6;
const OP_NODE_INFO: u64 = 7;
const OP_LISTEN: u64 = 8;

/// Errors from a management operation.
///
/// Distinct from the raw socket/container error types so the (future, 6.4)
/// syscall layer can map each to a stable errno without leaking kernel internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MgmtError {
    /// The caller does not hold the `Management` capability. Every op begins with
    /// this check, so a process without the cap can do *nothing* here.
    PermissionDenied,
    /// No container matched the given id / id-prefix / name.
    NotFound,
    /// The operation is invalid for the container's current state (e.g. `start`
    /// on a container that is not `Created`, or `stop` on one already `Exited`).
    InvalidState,
    /// A required argument was empty or malformed (e.g. `create` with no image).
    InvalidArgument,
    /// The container could not be created from the image (unpack/rootfs failure).
    CreateFailed,
    /// A backing kernel service (the net server, for `listen`) was unavailable.
    ServerUnavailable,
    /// The caller's CSpace is full — a minted capability could not be inserted.
    NoResources,
}

// --- Capability resolution ---

/// Verify `pid` holds the management authority at `handle`.
///
/// Mirrors `net::socket::resolve_factory`: presence of the [`CapType::Management`]
/// variant is the entire check — rights are **not** consulted (it is a coarse
/// all-or-nothing sentinel). Any lookup failure or wrong cap type is a denial.
fn resolve_management(pid: ProcessId, handle: CapHandle) -> Result<(), MgmtError> {
    process::with_cspace_mut(pid, |cspace| match cspace.lookup(handle) {
        Ok(cap) => match cap.cap_type {
            CapType::Management => Ok(()),
            _ => Err(MgmtError::PermissionDenied),
        },
        Err(_) => Err(MgmtError::PermissionDenied),
    })
    .unwrap_or(Err(MgmtError::PermissionDenied))
}

/// Audit-log a management op (detail = the management op number).
fn audit(pid: ProcessId, op: u64) {
    audit::log_event(pid, AuditOp::ApiAccess, CapType::Management, op);
}

// --- JSON shaping (Docker Engine API subset) ---

/// Join a container's argv into a single command string (Docker `Command` field).
fn command_string(command: &[String]) -> String {
    command.join(" ")
}

/// A Docker `/containers/json` summary object for one container.
fn summary_value(c: &ContainerMeta) -> Value {
    let mut names = Vec::new();
    // Docker prefixes container names with `/`.
    let mut slash_name = String::from("/");
    slash_name.push_str(&c.name);
    names.push(Value::Str(slash_name));

    Value::Object(alloc::vec![
        (String::from("Id"), Value::Str(c.id.clone())),
        (String::from("Names"), Value::Array(names)),
        (String::from("Image"), Value::Str(c.image.clone())),
        (String::from("Command"), Value::Str(command_string(&c.command))),
        (String::from("Created"), Value::Num(c.created_ms as f64)),
        (String::from("State"), Value::Str(String::from(c.state.as_str()))),
    ])
}

/// A Docker `/containers/{id}/json` detail object for one container.
fn detail_value(c: &ContainerMeta) -> Value {
    let exit_code = match c.state {
        ContainerState::Exited(code) => code,
        _ => 0,
    };
    let cmd = c
        .command
        .iter()
        .map(|s| Value::Str(s.clone()))
        .collect::<Vec<_>>();

    let mut slash_name = String::from("/");
    slash_name.push_str(&c.name);

    let state = Value::Object(alloc::vec![
        (String::from("Status"), Value::Str(String::from(c.state.as_str()))),
        (String::from("ExitCode"), Value::Num(exit_code as f64)),
    ]);
    let config = Value::Object(alloc::vec![
        (String::from("Cmd"), Value::Array(cmd)),
        (String::from("Image"), Value::Str(c.image.clone())),
    ]);

    Value::Object(alloc::vec![
        (String::from("Id"), Value::Str(c.id.clone())),
        (String::from("Name"), Value::Str(slash_name)),
        (String::from("Image"), Value::Str(c.image.clone())),
        (String::from("Created"), Value::Num(c.created_ms as f64)),
        (String::from("State"), state),
        (String::from("Config"), config),
    ])
}

// --- Capability-checked operations ---

/// List all containers (`docker ps -a`). Returns a JSON array of summaries.
pub fn list(pid: ProcessId, handle: CapHandle) -> Result<Vec<u8>, MgmtError> {
    resolve_management(pid, handle)?;
    audit(pid, OP_LIST);
    let items = registry::list()
        .iter()
        .map(summary_value)
        .collect::<Vec<_>>();
    Ok(Value::Array(items).to_bytes())
}

/// Inspect one container by id / id-prefix / name (`docker inspect`). Returns a
/// JSON detail object, or [`MgmtError::NotFound`].
pub fn inspect(pid: ProcessId, handle: CapHandle, id_or_name: &str) -> Result<Vec<u8>, MgmtError> {
    resolve_management(pid, handle)?;
    audit(pid, OP_INSPECT);
    let c = registry::lookup(id_or_name).ok_or(MgmtError::NotFound)?;
    Ok(detail_value(&c).to_bytes())
}

/// Create a container from `image` (`docker create`). `name` may be empty to
/// auto-generate. An empty `image` is rejected ([`MgmtError::InvalidArgument`]) —
/// the API requires an explicit image, unlike the internal registry helper which
/// treats `""` as the demo bundle. Returns `{"Id":…,"Warnings":[]}`.
pub fn create(
    pid: ProcessId,
    handle: CapHandle,
    image: &str,
    name: &str,
) -> Result<Vec<u8>, MgmtError> {
    resolve_management(pid, handle)?;
    audit(pid, OP_CREATE);
    if image.is_empty() {
        return Err(MgmtError::InvalidArgument);
    }
    let id = registry::create_from_image(image, name).map_err(|_| MgmtError::CreateFailed)?;
    let obj = Value::Object(alloc::vec![
        (String::from("Id"), Value::Str(id)),
        (String::from("Warnings"), Value::Array(Vec::new())),
    ]);
    Ok(obj.to_bytes())
}

/// Start a created container (`docker start`). Rejects with
/// [`MgmtError::InvalidState`] unless the container is `Created` (starting a
/// running or exited container is invalid). The registry state is flipped to
/// `Running` **only after** the ring-3 task is spawned, so a failed spawn never
/// leaves a phantom "running" row. Returns an empty body (Docker 204).
pub fn start(pid: ProcessId, handle: CapHandle, id_or_name: &str) -> Result<Vec<u8>, MgmtError> {
    resolve_management(pid, handle)?;
    audit(pid, OP_START);
    let c = registry::lookup(id_or_name).ok_or(MgmtError::NotFound)?;
    // Only a Created container may be started — guard against double-start and
    // restart-after-exit (neither is supported here).
    if c.state != ContainerState::Created {
        return Err(MgmtError::InvalidState);
    }
    container::start(c.pid);
    // Flip to Running only after the spawn succeeds.
    registry::set_state(&c.id, ContainerState::Running);
    Ok(Vec::new())
}

/// Stop (force-terminate) a container (`docker stop`). Rejects an already-`Exited`
/// container with [`MgmtError::InvalidState`]. On success the backing process is
/// torn down and the row is marked `Exited` by [`container::terminate`]. Returns
/// an empty body (Docker 204).
pub fn stop(pid: ProcessId, handle: CapHandle, id_or_name: &str) -> Result<Vec<u8>, MgmtError> {
    resolve_management(pid, handle)?;
    audit(pid, OP_STOP);
    let c = registry::lookup(id_or_name).ok_or(MgmtError::NotFound)?;
    if let ContainerState::Exited(_) = c.state {
        return Err(MgmtError::InvalidState);
    }
    // `terminate` is container-type-guarded and tears down the process; a `false`
    // return means it was not a live container to stop.
    if !container::terminate(c.pid) {
        return Err(MgmtError::InvalidState);
    }
    Ok(Vec::new())
}

/// Read back a container's captured stdout/stderr (`docker logs`). `tail` bounds
/// the number of most-recent bytes (`None` = all). Returns the raw log bytes (not
/// JSON — Docker streams the raw output), or [`MgmtError::NotFound`].
pub fn logs(
    pid: ProcessId,
    handle: CapHandle,
    id_or_name: &str,
    tail: Option<usize>,
) -> Result<Vec<u8>, MgmtError> {
    resolve_management(pid, handle)?;
    audit(pid, OP_LOGS);
    registry::logs(id_or_name, tail).ok_or(MgmtError::NotFound)
}

/// Node-level summary (`docker info` subset): container counts by state. Returns a
/// JSON object.
pub fn node_info(pid: ProcessId, handle: CapHandle) -> Result<Vec<u8>, MgmtError> {
    resolve_management(pid, handle)?;
    audit(pid, OP_NODE_INFO);
    let all = registry::list();
    let total = all.len();
    let running = all
        .iter()
        .filter(|c| c.state == ContainerState::Running)
        .count();
    let stopped = all
        .iter()
        .filter(|c| matches!(c.state, ContainerState::Exited(_)))
        .count();

    let obj = Value::Object(alloc::vec![
        (String::from("Name"), Value::Str(String::from("themelios"))),
        (String::from("Containers"), Value::Num(total as f64)),
        (String::from("ContainersRunning"), Value::Num(running as f64)),
        (String::from("ContainersStopped"), Value::Num(stopped as f64)),
    ]);
    Ok(obj.to_bytes())
}

/// Open an inbound-TCP listener on `port` (the api-server's HTTP socket).
///
/// Runs over the **trusted** kernel socket path (`ksocket_open_tcp` → `bind` →
/// `listen`, the same green functions `test_tcp_server` exercises), then mints a
/// per-listener [`CapType::Socket`] capability **parented to the management
/// handle** and returns its raw handle. The api-server then `accept`s on it via
/// the ordinary socket ABI (`sys_accept`) — there is no separate management
/// `accept`. Not exercised end-to-end from ring 3 until Phase 6.4.
pub fn listen(pid: ProcessId, handle: CapHandle, port: u16) -> Result<u32, MgmtError> {
    resolve_management(pid, handle)?;
    audit(pid, OP_LISTEN);
    let socket_id = socket::ksocket_open_tcp().map_err(|_| MgmtError::ServerUnavailable)?;
    socket::ksocket_bind_port(socket_id, port).map_err(|_| MgmtError::ServerUnavailable)?;
    socket::ksocket_listen(socket_id).map_err(|_| MgmtError::ServerUnavailable)?;
    // Mint the per-listener socket capability under the management authority, so
    // it is revoked with the management grant.
    process::with_cspace_mut(pid, |cspace| {
        cspace.insert(Capability {
            cap_type: CapType::Socket { socket_id },
            rights: CapRights::READ | CapRights::WRITE,
            parent: Some(handle),
        })
    })
    .ok_or(MgmtError::ServerUnavailable)?
    .map(|h| h.as_raw())
    .map_err(|_| MgmtError::NoResources)
}
