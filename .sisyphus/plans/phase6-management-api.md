# Phase 6 — Docker-compatible Management API

**Status**: PLANNING (drafted 2026-07-31; Momus-reviewed → REVISE fixes applied
2026-07-31)
**Created**: 2026-07-31
**Phase**: 6
**Depends on**: Phase 2 (capabilities, processes, IPC), Phase 4 (TCP listen/accept
+ per-connection socket caps), Phase 5 (container lifecycle: create/start/
terminate, process introspection) — all complete.

## Goal

Manage the node **entirely over a network API** — no SSH, no interactive shell in
production. The API speaks a **Docker Engine API subset** so standard tooling
works. The off-ramp deliverable:

```
$ docker -H tcp://<node>:2375 ps                  # lists containers
$ docker -H tcp://<node>:2375 run <staged-image>  # create + start a STAGED image
$ docker -H tcp://<node>:2375 logs <id>           # streams that container's stdout
```

**Off-ramp caveat (per Momus M1):** `run` launches a **pre-staged / embedded**
image bundle, **not** an arbitrary `docker.io` pull. Live registry transport was
deferred in 5.6 (the puller is offline-tested only; `container::create` takes a
`bundle: &[u8]` whose only current source is `demo_bundle()`). Pulling an
arbitrary image over a live registry rides with that deferred transport.

We prove the JSON/route layer first with `curl` against exact Engine API shapes,
then validate the real `docker` CLI end-to-end over a QEMU `hostfwd` as a
**stretch** acceptance. **Plain HTTP first; TLS/mTLS is deferred** (rustls-in-ring-3,
the same large dependency Phase 5 deferred).

## The core thesis for this phase

**Untrusted parsing runs in ring 3; all container lifecycle stays in ring 0; a
capability-gated ABI is the only bridge.**

An HTTP/1.1 request parser, a JSON serializer, and a Docker route table consume
**untrusted bytes off a socket** — the class Phases 3–5 keep out of the kernel. So
they live in a new ring-3 **`api-server`**, symmetric with the `net-server`.

But container lifecycle is **not** moving to ring 3. `container::create` does OCI
unpack + VFS writes + ELF `load_into` + `create_process` — all ring-0 operations a
ring-3 process cannot and must not perform (Momus L1). The api-server is a **thin
driver**: it parses the request in ring 3, then invokes kernel-side lifecycle
through a **minimal, capability-guarded management ABI** (§6.3). The split is the
whole design:

- **Ring 3 (api-server):** accept a connection, parse HTTP + JSON, route, enforce
  token policy, serialize the response. Risky parsing only.
- **Ring 0 (kernel):** every lifecycle/introspection operation
  (create/start/stop/list/inspect/logs), each checked against a
  **management-authority capability** and audited. A malformed request can crash
  the api-server; it can never touch kernel memory or act without the cap.

## Grounding (from a codebase survey — Momus-verified)

**Reusable as-is:**
- Capability mint/check + the **`SOCKET_FACTORY` sentinel** *pattern*
  (`cap/mod.rs`: `Socket{socket_id:u64::MAX}` = "authority to create sockets";
  `resolve_factory` in `net/socket.rs:154` is a pure has-it-or-not check). The
  management-authority cap copies this **pattern** (but is a new `CapType`, see
  net-new #6).
- Container **create/start/terminate** (`container/mod.rs`); the container-identity
  predicate `personality==Linux && rootfs_mount.is_some()`.
- Process introspection: `process_list() -> Vec<ProcessInfo{pid,name,task_count,
  state,cap_count}>`, `exit_status`, `personality`, `rootfs_mount`.
- The **trusted, working** kernel-internal TCP accept path
  (`ksocket_listen`/`ksocket_accept`, `net/socket.rs:519-543`) — the basis of the
  proven `test_tcp_server`. `sys_send`/`sys_recv` are cap-checked and
  **type-agnostic** (`socket.rs:334,356`), so they already work on a TCP
  connection cap.
- HTTP **response** helpers (`oci/registry.rs`: `find_sub`, `\r\n\r\n` split,
  `find_content_length`) and the JSON **reader** (`oci/json.rs`, depth-bounded).
- `ServerBootInfo`/`spawn_server` config channel (`process/server.rs`), the audit
  log, SHA-256, and both test harnesses (`hostfwd` host-thread + MockConn).

**Net-new (the real work of Phase 6):**
1. An HTTP **request** parser (method/path/query/version/header-map/body) + a route
   table that tolerates a Docker `/v1.NN/` version prefix — the client only parses
   *responses*.
2. A JSON **serializer** — `json.rs` only *reads*.
3. A **container-metadata table** keyed by a generated `ContainerId` (id ↔ pid ↔
   name/image/created/command/state), **owning the container's rootfs mount and
   its log buffer** so both outlive the pid.
4. **Per-container stdout/stderr capture** — today `console_write`/`sys_writev`
   route fd 1/2 straight to the shared serial console.
5. A **guest ring-3 TCP server path** — **this does not exist today** (Momus C1):
   `sys_socket` hard-rejects non-UDP (`socket.rs:210`), no production process holds
   `SOCKET_FACTORY`, and the only inbound-TCP proof (`test_tcp_server`) is
   kernel-internal. This is a first-class sub-phase (§6.4), not a footnote.
6. A **management ABI** (capability-guarded) exposing lifecycle + introspection +
   a **connection-accept shim** to the ring-3 api-server; a **new
   `CapType::Management` sentinel** (not `SOCKET_FACTORY` reuse — that piggybacked
   on `CapType::Socket`).
7. `ServerConfig`/`spawn_server` **extension to grant sentinel caps** — today it
   only grants `filesystem_mount`; there is no field for an arbitrary/sentinel cap.
8. An **app-layer bearer-token → allowed-operations policy** *inside* the
   api-server (not a kernel capability — see Decisions/Auth).
9. **All TLS** (deferred).

## Key Architectural Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Lifecycle location | **All container lifecycle stays in ring 0** (`container::*`, `process::*`); the ring-3 api-server never runs it | `create` does OCI unpack + VFS writes + ELF load + `create_process` — ring-0 only. "Ring-3 api-server" means the *parsing* is ring-3, **not** that the runtime moves. |
| API server location | **Ring-3 `api-server`** (symmetric with net-server); reaches ring-0 lifecycle only via the management ABI | Untrusted HTTP/JSON parsing must not run in ring 0 — same rule as the VFS/net/OCI parsers. |
| Inbound TCP for the API | **Kernel-accept shim** (Momus C1): the kernel runs the *proven trusted* `ksocket_listen`/`ksocket_accept`, mints a per-connection `Socket` cap, and hands it to the api-server via the management ABI. The api-server uses cap-checked, type-agnostic `sys_send`/`sys_recv`. | The guest ring-3 TCP `socket()` path has **never run** (`sys_socket` rejects non-UDP). Reusing the working kernel accept path avoids relaxing the `sys_socket` TCP gate in the critical path. Relaxing that gate for general guest TCP servers is a **separate later hardening**, not a Phase-6 dependency. |
| Management authority | **A new `CapType::Management` sentinel cap**, minted to the api-server at spawn; every ABI op is has-it-or-not checked and audited | Copies the `SOCKET_FACTORY` *pattern* but needs a new variant (no natural existing one). Binary authority: strip the cap and the api-server is inert. |
| Auth (two layers, Momus H1) | **(a) Kernel:** the `Management` cap = "may call the ABI at all" (per-process, binary). **(b) Application:** a bearer-token → allowed-operations table is **policy inside the ring-3 api-server**, enforced before it invokes the ABI. | Caps are per-**process** CSpaces (`cap/mod.rs`), not per-**request**; the api-server is one process with one cap set for its lifetime. Per-request principals are app policy, not a kernel capability. |
| HTTP + JSON | **Hand-rolled request parser + JSON serializer, `alloc`-only, fail-closed** | Reuse `find_sub`/CRLF-split/`Content-Length`; add request-line/header parsing + a `Value`→bytes serializer. Bounded sizes/depth (like the 5.6 registry hardening). No new deps. Router tolerates the `/v1.NN/` Docker prefix from day one. |
| Container identity + ownership | **A `ContainerId`-keyed metadata table** owning name/image/created/command/state **plus the rootfs mount and the log buffer** | Docker semantics need metadata the process table lacks; and the mount + logs must **outlive the pid** (a container can be Exited but still `inspect`/`logs`-able until removed). |
| `docker logs` | **Per-container ring buffer owned by the `ContainerId` row** (Momus H2), not the pid; `console_write`/`sys_writev` map `current pid → ContainerId → buffer`; freed on container removal | Pids are torn down on exit and recycle, so a pid-keyed buffer loses post-exit logs / leaks. Non-container Linux processes allocate no buffer. |
| Per-container mounts (Momus M2) | **`create_from_image` allocates a writable rootfs mount per container; `terminate`/removal reclaims it** | `create(bundle, mount)` needs a writable ext2 mount; N API containers need N mounts with explicit lifecycle, not the single one `run` hands over today. |
| Concurrency (Momus M3) | **Strictly sequential accept/serve loop** in the api-server | The kernel↔net-server socket payload region is a single ~64 KiB slot serving one request at a time; concurrent connection servicing is not available. A throughput ceiling, documented. |
| Timestamps | **Monotonic "seconds since boot" (PIT tick)** for `Created`/`StartedAt`; a wall clock is deferred | No RTC today; Docker CLI tolerates the field's presence. |
| Transport security | **Plain HTTP first; TLS (rustls-in-ring-3) + mTLS deferred** | Large ring-3 dependency (deferred in Phase 5). Provable over plain HTTP on a trusted net / `hostfwd`; TLS is a scoped follow-up with its own bare-metal-build gate. |
| Shell in production | **Keep the serial shell for dev/test; gate it out of production builds** | "No shell" is a production posture; making the API self-sufficient is the deliverable, not removing the dev shell. |

## Sub-phase breakdown

Each sub-phase is a fresh branch + PR off the latest `main`, reviewed with
sisyphus + Momus, deterministic tests + a soak before merge — the Phase 5 cadence.
The order is chosen so the **unproven transport (6.4) lands before the api-server
(6.5)** and the HTTP core is provable independent of the socket path.

### 6.0 — HTTP request parser + JSON serializer (parsing primitives)

**Goal**: The two net-new, purely-functional building blocks, unit-tested offline.

**What to build**:
- `http::request` — parse an HTTP/1.1 request `{method, path, query, headers,
  body}`: request line, header map, `Content-Length` body. Factor the shared
  `find_sub`/CRLF-split/`find_content_length` out of `registry.rs` into a shared
  `http` module. **Fail-closed**: bounded request/header sizes, `checked_add` on
  `Content-Length`, chunked bodies rejected (documented, matching the client).
- `json::to_bytes` / `JsonBuilder` — serialize objects/arrays/strings/numbers/
  bools/null with correct escaping; round-trips with the reader.

**Acceptance**:
- [ ] Parse `GET /v1.43/containers/json?all=1` and `POST /containers/create` with a
      JSON body; assert method/path (version-prefix stripped)/query/headers/body.
- [ ] Malformed inputs (no CRLF, absurd `Content-Length`, oversized header) error,
      never panic/hang.
- [ ] Serializer round-trips through `json::parse`; `"`,`\`,control chars escape.

### 6.1 — Container metadata table + image glue — ✅ DONE (mount-isolation split out)

**Goal**: A first-class "container" owning the metadata Docker needs, surviving
the pid.

**Built**:
- `container::registry` — a `ContainerId`-keyed table: id (64-hex, SHA-256 of a
  seq counter + tick; id-prefix lookup like Docker), name (user or auto), image
  ref, `created_ms` (monotonic since-boot — no wall clock; documented), command
  (argv, captured via a new `create_with_argv`), state (Created/Running/
  Exited(code)), pid, rootfs mount. Insert on create, update on start/exit/
  terminate; id-prefix + name lookup, list, remove. The row **outlives the pid**
  (removed only by `remove`/`docker rm`).
- `create_from_image(image, name)` / `create_on_mount(image, name, mount)` —
  resolves a staged/embedded image (`demo`; live pull deferred), calls
  `create_with_argv`, records the row. `terminate` marks the row Exited(137).
- `run [image]` records a row + drives Created→Running→Exited; `ps` lists the
  table (incl. exited, like `docker ps -a`).
- `test_container_registry`: two containers, id-prefix + name lookup + miss,
  metadata, the state machine, one **end-to-end run → Exited(0)**, and removal.

**Deviation from the plan (Momus M2), surfaced and split out**: per-container
**rootfs mount isolation** is **not** built here, because the plan's model —
"allocate/free a writable mount per container" — is **infeasible on this fs
layer**: a mount requires a physical ext2-formatted virtio-blk disk (QEMU has a
fixed few), spawns a block+ext2 server pair, and `fs::register_mount` is
append-only (mounts are never freed). Every container therefore currently
assembles onto the single shared `/data` mount. The correct alternative —
confining each container to a **subdirectory** (`/c/<id>`) — is a change to the
**security-critical Phase 5.2 path resolver** (the `..`-clamp would move from the
mount root to the subdir root) and deserves its own focused sub-phase + Momus
review rather than being bolted on here. The registry already stores each
container's mount id so the confinement lifts in cleanly.

### 6.1b — Per-container rootfs confinement — ✅ DONE (Momus-reviewed)

Replaces the infeasible "mount per container": each container is confined to a
`/c/<id>` **subdirectory** of the shared mount.

**Built**:
- `Process.rootfs_base: Option<String>` + accessors. `host_path(pid, rel)` in
  `linux::fs` maps the `..`-clamped container-relative path to `base + rel` at the
  **5 mount-access sites** (openat kstat/kcreate/kopen, newfstatat kstat, chdir
  kstat). `cwd` stays container-relative (getcwd never leaks the base; chdir
  stats the host path but stores the relative one).
- `container::create_confined(bundle, mount, Some("/c/<id>"))`: assembles under
  the base, loads the entrypoint from there, sets mount **and** base together up
  front (fail-closed). `registry::create_on_mount` generates the id first and
  confines. `create`/`create_with_argv` keep base `None` (mount root).
- **Critical Momus fix** — assembly-time `..` clamp: image paths are untrusted,
  and the ext2 server honors `..`, so a layer member `../../host_secret` would
  escape the base at *assembly*. Every image path (files, dirs, entrypoint) now
  passes through `resolve_path("/", …)` before base-prefixing, symmetric with the
  runtime. `confine-smoke` + `test_container_confinement` prove **both** halves:
  a malicious `../../evil` is clamped into the base (not written at root, and
  `/host_secret` intact), and the confined probe reads its own `/only` but cannot
  open a real `/host_secret` at the mount root (directly or via `..`).

**Deferred (Momus M2, documented)**: the kernel↔fs-server **shared per-mount
region** is copied then handed over a *blocking* `ipc_call` without a lock held
across the pair, so two *concurrently-running* containers on the same mount could
interleave (a pre-existing race, not introduced here). The confinement boundary
is proven for a single running container + static assembly; the concurrent-two-
containers guarantee additionally needs that forwarding path serialized (a
per-mount yielding lock across the IPC). **Must be resolved before the API (6.5)
runs multiple containers concurrently** — tracked as a 6.5 prerequisite. The nit
(reject overlong host paths rather than silently truncate) is noted for the same
hardening pass.

**Acceptance**:
- [x] `test_container_confinement`: assembly `..` clamped into base + `/host_secret`
      intact; confined probe reads `/only` but not `/host_secret` (direct or `..`).
- [x] All Phase 5 container tests still green (create with base `None` unchanged).

**Acceptance**:
- [x] Create two containers; list; look up by id-prefix and name; assert metadata;
      start one → Created→Running→Exited(0); remove → row gone, other remains.
- [x] `run`/`ps`/`stop` shell cmds record/list/update rows.
- [ ] (→ 6.1b) two running containers are rootfs-isolated from each other.

### 6.2 — Per-container stdout/stderr capture (`docker logs` backing) — ✅ DONE

**Built**:
- A per-container `LogRing` (bounded, 16 KiB, oldest-dropped) held in a
  `CONTAINER_LOGS` table **keyed by `ContainerId`** — stored *separately* from the
  metadata row so it isn't cloned on every `ps`. Created with the container,
  dropped only on `remove` (so it **outlives the process** — `docker logs` works on
  an exited container).
- `registry::write_stdout(pid, bytes)`: both Linux stdout paths — `console_write`
  (writev, fd 1/2) and `sys_write` (write, fd 1/2) — route here. It maps `pid →
  ContainerId → buffer` and appends; a non-container Linux process (no row for its
  pid) allocates no buffer. Always mirrors to serial (dev aid).
- `registry::logs(id_or_name, tail) -> Option<Vec<u8>>` (id/prefix/name resolution)
  + a `logs <id>` shell cmd.

**Acceptance**:
- [x] `test_container_logs`: the demo container's `linux-smoke ok` is captured;
      readable **after** the process is destroyed (buffer keyed by id, not pid); a
      second container's buffer is independent; `remove` drops one log but not the
      other; an unknown id yields `None`.
- [x] Bounded (oldest dropped); no buffer for non-container processes.

### 6.3 — Management ABI + `CapType::Management` — ✅ DONE (Momus-reviewed)

**Goal**: The capability-guarded ring-0 surface the api-server drives — including
opening the inbound-TCP listener via the proven kernel path.

**What was built** (`kernel/src/mgmt.rs`):
- **`CapType::Management`** sentinel (`cap/mod.rs`) + `resolve_management` — a
  has-it-or-not check copying `resolve_factory` (rights not consulted). Documented
  invariant: minted only to the api-server, never into a container CSpace.
- **`AuditOp::ApiAccess`** (`audit/mod.rs`); every op audits with the op number.
- A kernel-internal ABI of cap-checked functions returning **owned data**: `list`,
  `inspect(id)`, `create(image,name)`, `start(id)`, `stop(id)`, `logs(id,tail)`,
  `node_info` (compact Docker-Engine-shaped JSON via `oci::json::Value::to_bytes`),
  and `listen(port)` — over the **trusted `ksocket_open_tcp`/`bind`/`listen`**
  path, minting a per-listener `Socket` cap parented to the management handle.
  Lifecycle guards: `start` only from `Created` (flips to `Running` *after* a
  successful spawn); `stop` refuses an already-`Exited` container; `create("")` →
  `InvalidArgument`.
- `test_management_capability`: proves the cap gate (every op denied without the cap
  and with a wrong-type cap), the positive ops, the lifecycle guards, and that
  `ApiAccess` entries are logged. Deterministic (private ext2 mount + local
  net-server bind; no external peer).

**Momus revisions applied** (verdict REVISE → addressed): dropped a separate
`mgmt::accept` (the api-server `accept`s on the minted `Socket` cap via the ordinary
socket ABI); **deferred** the syscall/IPC wrappers **and** the
`ServerConfig`/`spawn_server` sentinel-cap grant to **6.4** (which de-risks the OS's
first ring-3 inbound TCP and first sentinel-cap grant together). `start`/`stop`
state guards and the empty-image rejection were added per the review.

**Acceptance**:
- [x] A process **with** the cap can list/create/start/stop/logs; **without** it (or
      with a wrong-type cap), every op — `listen` included — is a capability denial.
- [x] Each op emits an `ApiAccess` audit entry.

`test_management_capability` is deliberately **fast and self-contained** — no
server spawns, no container run — so it fits inside the suite's 90 s QEMU wall-clock
budget (an earlier version that brought up an ext2 mount + ran a container tipped the
whole run over that ceiling in CI). It injects registry rows in a chosen state (a
test-only helper) to prove the lifecycle *guards*, which reject on state before ever
touching the backing process. What it defers, and where each is instead proven:
- **positive `create`→`start`→run→`exit`** — `test_container_registry` (end-to-end).
- **positive `listen`** (a real inbound-TCP listener) — **6.4**, which de-risks
  ring-3 inbound TCP; 6.3 covers the `listen` *cap gate* via the denial path.

### 6.4 — Prove ring-3 TCP transport + sentinel-cap spawn wiring — ✅ DONE (Momus-reviewed)

**Goal**: De-risk the OS's **first ring-3 inbound-TCP + first sentinel-cap grant**
in isolation, before the api-server depends on them.

**What was built**:
- **`SYS_MGMT`** (syscall #26, `arch/x86_64/syscall.rs`): the op-multiplexed ring-3
  seam onto the kernel `mgmt` module. RDI selects the verb — only `MGMT_OP_LISTEN`
  (RSI = Management cap, RDX = port) is wired in 6.4; the rest (list/inspect/create/…)
  land in 6.5 under the same number. Returns the minted listener `Socket` cap
  handle, or a high-bit-set `MgmtError` code. `MgmtError` got explicit discriminants
  + `as_syscall_ret` so ring 3 can decode `PermissionDenied` specifically.
- **`ServerConfig.grant_management`** (`process/server.rs`): `spawn_server` grants a
  `Management` sentinel cap and passes its handle via a new `mgmt_cap_handle`
  boot-info field (appended in lockstep to `ServerBootInfo` **and** libthemelios
  `BootInfo`; a `const _` size assert on both sides locks the 104-byte layout).
- **`servers/tcp-echo-smoke`** — the first ring-3 inbound-TCP server: `mgmt_listen`
  (libthemelios wrapper) → `accept` → `tcp_recv` → `tcp_send` echo → `close`,
  reporting to a shared result page. Built/embedded like the FS/net servers.
- **`test_ring3_tcp_echo`** (supersedes `test_tcp_server`): Phase 1 spawns the server
  **without** the grant → its listen is `PermissionDenied` before any NIC access →
  reports `DENIED` (fail-closed, no net-server needed); Phase 2 brings up DHCP,
  spawns it **with** the grant, and the host peer (hostfwd 15007→7) drives the echo.

**Chosen design** (Momus DECISION 1 = **Option B**): the server holds a `Management`
cap and lists via the **kernel-listener path** (`mgmt::listen` → `ksocket_*`), so
`sys_socket`'s UDP-only TCP gate is **not** relaxed (honoring 6.3's C1) and the exact
6.5 api-server path is de-risked. DECISION 2 = **supersede** `test_tcp_server`
(reuse port 7 + the host peer) — budget-neutral under the 90 s QEMU ceiling.

**Momus must-fixes applied**: fault-freedom is load-bearing (a ring-3 fault halts the
kernel), so the smoke server is tiny/defensive with all-bounded loops; mandatory
`yield` on every `WouldBlock`; `cpu::sti()` on the `SYS_MGMT` arm; commit-word-last
volatile result writes with a bounded kernel poll; `MgmtError` syscall encoding.
Should-fixes: fail-closed done as a cheap ring-3 spawn (instant denial, no net); docs
reconciled to admit trusted kernel-spawned servers holding `Management`.

**Coverage note**: superseding `test_tcp_server` drops the last caller of kernel-side
`ksocket_accept` (kept, `allow(dead_code)`); `mgmt::listen` still exercises
`ksocket_open_tcp`/`bind_port`/`listen`, and the ring-3 path now covers
`sys_accept`/`sys_recv`/`sys_send` — net coverage gain.

**Acceptance**:
- [x] Over `hostfwd`, the host connects, sends a line, the ring-3 guest echoes it —
      proving ring-3 accept/recv/send end to end (echoed 18 bytes).
- [x] `spawn_server` grants the sentinel cap; a control run without the grant fails
      closed (`DENIED`).

### 6.5 — Ring-3 `api-server` skeleton + Docker Engine API routing (GET pipeline) — Momus-reviewed

**Goal**: The server: accept (6.4) → HTTP parse (6.0) → route → mgmt ABI (6.3) →
JSON → reply. **Scope narrowed to the read (GET) pipeline** (Momus §10): the first
untrusted-input ring-3 parser lands with the smallest blast radius; POST
create/start/stop + logs (and all of `json` request-body parsing) move to **6.5b**.

**What to build**:
- **SYS_MGMT read verbs** — `MGMT_OP_LIST`/`INSPECT`/`NODE_INFO`, each copying the
  mgmt `Vec<u8>` JSON into a user out-buffer (ABI: RDI=op, RSI=mgmt cap, RDX=in_ptr,
  R10=in_len, R8=out_ptr, R9=out_len; return = bytes written, bit-63 = `MgmtError`).
  New `MgmtError::BufferTooSmall` (8), checked **before** `copy_to_user`. Matching
  libthemelios wrappers.
- **`http` single-source into ring 3** — libthemelios `#[path]`s the kernel
  `http/mod.rs` (zero kernel deps; no copy-paste, no drift — Momus #6). Add a
  `http::build_response` there (status line + `Content-Type` + always
  `Content-Length` + `Connection: close`, HTTP/1.1). No `json` in ring 3 yet
  (GET-only; responses are pre-built by the kernel mgmt ops).
- **`servers/api-server`** — strictly sequential accept/serve loop, **fault-free**
  (a ring-3 fault halts the kernel): recv-accumulate bounded to `MAX_REQUEST`
  *before* growing; frame via `find_sub(\r\n\r\n)` + `content_length` *before*
  `parse_request` (which conflates incomplete/malformed); route (tolerating
  `/v1.NN/`); call the mgmt wrapper into a fixed out-buffer; build the response;
  **close the connection socket per request** (or the CSpace fills — Momus #4).
  Endpoints: `GET /_ping`→`200 "OK"`; `GET /version`→static; `GET /info`→node_info;
  `GET /containers/json`→list; `GET /containers/{id}/json`→inspect; else `404`.
- **libthemelios `alloc_error_handler`** — convert an uncatchable OOM abort (→ ring-3
  fault → kernel halt) into a clean marker + `exit` (Momus #1).
- **Boot wiring** — spawn the api-server in `kmain` normal mode after
  `net::boot_net()` with `grant_management: true` (documented as not CI-exercised;
  the *binary/grant/listen/serve* are all covered by the test below).

**Momus decisions**: DECISION 1 = `#[path]` single-source (not copy-paste);
DECISION 3 = **T1**, supersede `test_ring3_tcp_echo` — the api-server is now the
ring-3 inbound-TCP proof, folding in the fail-closed grant control; the throwaway
`tcp-echo-smoke` is removed.

**Acceptance**:
- [x] Fail-closed control: an api-server spawned **without** the grant is denied at
      `listen` before any NIC access (`DENIED`, no net-server).
- [x] Over `hostfwd`, the host sends `GET /_ping` (and a second GET, proving no
      per-connection socket leak), the ring-3 api-server replies `200 OK` framed
      with `Content-Length`; the kernel asserts via the server's result marker
      (`test_api_server` — "served 1 request(s)").

**Status: DONE (Momus-reviewed).** `SYS_MGMT` read verbs (list/inspect/node_info) +
`MgmtError::BufferTooSmall`; `http` single-sourced into libthemelios via `#[path]` +
`build_response`; libthemelios `alloc_error_handler`; `servers/api-server` (GET
pipeline, fault-free framing, per-request close); `test_api_server` supersedes the
echo test (`tcp-echo-smoke` removed); boot spawn in `kmain`. **Deferred to 6.5b**:
POST create/start/stop + logs write verbs and request-body `json` parsing.

### 6.5b — api-server write verbs (POST create/start/stop, logs) — Momus-reviewed

**Goal**: complete the mutating half of the Engine API subset + request-body JSON.

**What was built**:
- **`SYS_MGMT` write verbs** — `CREATE=5`/`START=6`/`STOP=7`/`LOGS=8`. `CREATE` takes
  `"image\0name"` (NUL-separated); `START`/`STOP` take the id and return 0 bytes
  (Docker 204); `LOGS` returns raw bytes with the tail capped at
  `min(out_cap, MGMT_LOGS_MAX=16 KiB)` so `BufferTooSmall` is structurally impossible
  and the copy is bounded (Momus). Kernel arms `from_utf8().ok()`-reject bad input.
- **`json` single-sourced into libthemelios** via `#[path]` (same as `http`), for the
  POST-create request body only.
- **api-server POST routing** — `POST /containers/create` (parse body → extract a
  non-empty, **NUL-free** `Image` → NUL-join → create → `201`/`400`/`500`);
  `POST /containers/{id}/start|stop` → `204`/`404`/`409`; `GET /containers/{id}/logs`.
  A write-verb-specific error mapper (Momus): `NotFound`→404, `InvalidState`→409,
  `InvalidArgument`→400, else→500. Still fault-free (body ≤ `MAX_BODY`; `json::parse`
  depth-guarded; fixed out-buffers).
- **`test_api_server`** now asserts response **status** (Momus #1/#2 — the prior
  count-only test was vacuous), split into three phases:
  1. *fail-closed control* — no grant → `mgmt_listen` denied → `DENIED` (no net).
  2. *routing/JSON self-test* — the api-server runs a fixed request set through
     `route` **in-process** (no TCP; enabled by a `SELF_TEST_FLAG` bit in `arg1`) and
     records `[200, 400, 500, 409]`: `GET /_ping`=200, `POST create {}`=400 (empty
     `Image`), `POST create {"Image":"demo"}`=500 (Image extracted → create verb →
     `CreateFailed`, no `/data` mount), `POST /containers/<running>/start`=409 (a
     pre-injected `Running` container → the start verb's `InvalidState` guard). Each
     status ≠ the catch-all 404, so this deterministically proves the real GET/POST
     routing, untrusted request-body JSON parse, `Image` extraction, and the
     create/start write verbs — with no network in the loop.
  3. *live inbound smoke* — over `hostfwd`, one `GET /_ping` round-trips end to end
     (count-based), proving the accept → HTTP-parse → route → reply wire path still
     works. The write-path *content* is proven in phase 2, so this stays a smoke.

**Why the self-test (noted, not a defect here)**: the ring-3 net server can deliver
**stale RX data across sequential connections** on one listener — invisible to 6.4/6.5
(identical payloads, count-only assertion) but it makes a multi-connection, content-
asserting wire test flaky. Rather than depend on the timing-sensitive inbound path for
correctness assertions, the routing/JSON *content* is proven by the in-process
self-test and the wire path keeps a single-connection count-based smoke. Fixing the
net-server socket recycling is a Phase-4/net follow-up, out of 6.5b scope.

**Security note (for 6.6)**: the write verbs spawn/tear down **real** ring-3
processes on an **unauthenticated** port. Contained for now — the test `hostfwd` is
`127.0.0.1`-only and `run` mode is slirp-isolated — but lifecycle mutation MUST NOT be
exposed on a real NIC until 6.6 lands auth. `POST create` also needs a `/data` mount
(none configured at boot yet) and 500s gracefully without one.

**Acceptance**:
- [x] `test_api_server` proves POST routing + request-body JSON parse + `Image`
      extraction + the create/start write verbs via the deterministic self-test
      (`[200, 400, 500, 409]`, each ≠ catch-all), and the wire path via a live
      `GET /_ping` smoke. Passes locally (34 tests green before the sandbox's
      live-network wall); CI runs the full suite.

### 6.6 — Bearer-token auth — Momus-reviewed

**Goal**: App-layer bearer-token authentication enforced inside the api-server.

**What was built**:
- **Token provisioning via boot-info** — `ServerBootInfo`/`BootInfo` gain
  `api_token: [u8;32]` + `api_token_len: u64` (size assert 104 → 144, plus `offset_of!`
  asserts pinning the two heterogeneous trailing fields so a cross-struct reorder can't
  silently corrupt the token — Momus). `spawn_server` fills the token from a kernel
  `API_TOKEN` const **only for `grant_management` servers** (the api-server), so only
  the control plane holds the node secret. No `ServerConfig` churn (boot-info has one
  constructor). Provenance (fixed vs random-per-boot) is scoped out.
- **Enforcement (api-server)** — every route except the `GET /_ping` / `GET /version`
  health probes requires `Authorization: Bearer <token>` matching the provisioned
  bytes. Missing/wrong → **401** *before* any management op, **including unknown paths**
  (so an unauthenticated client can't enumerate routes). **Wrong token is 401, not 403**
  (RFC 9110: a correct token would work → "unauthorized", never "forbidden"; 403 is
  reserved for an authenticated-but-unauthorized principal, which doesn't exist with one
  all-or-nothing token — Momus must-fix). Scheme is case-insensitive; plain byte compare
  (a constant-time compare guards nothing while the token is plaintext over an
  unencrypted port — Momus).
- **Audit** — success already audits kernel-side (`ApiAccess` on the op). Failures go
  through a new `SYS_MGMT` `AUDIT_DENY=9` verb → a **distinct `AuditOp::ApiAuthReject`**
  (not `ApiAccess` with a magic detail — Momus), cap-checked like every verb.
- **Test** — the deterministic self-test (phase 2) gains auth arms, asserting
  `[200, 401, 401, 200, 400, 500, 409]`: `GET /_ping`=200 (exempt); `GET /containers/json`
  no-token=401, wrong-token=401, correct-token=200 (the 401/200 contrast on the *same*
  route proves auth is the only variable and a valid token passes); then the authed
  write-verb cases. The authed requests are built from `boot_info().api_token` (one fewer
  sync point — Momus). The live smoke (phase 3) is now an authed `GET /containers/json`
  over `hostfwd` — a **transport** check that the header round-trips.

**Security note**: bearer auth over plaintext HTTP is an app-layer gate, **not**
transport security. It gates who can drive the API but does nothing against a wire
sniffer; that waits on TLS (deferred phase-wide). Token is a fixed dev secret kept in
sync between the kernel `API_TOKEN` const and the xtask test peer.

**Acceptance**:
- [x] Absent token → 401, wrong token → 401, correct token → 200 — proven by the
      deterministic self-test (each ≠ the catch-all 404); success audited via
      `ApiAccess`, failure via `ApiAuthReject`.
- [x] One authenticated `GET /containers/json` round-trips over `hostfwd` (transport
      check that the `Authorization` header survives the wire).
- [ ] **Deferred** (renegotiated from the original "create+start+logs end to end" +
      "live curl/`docker` CLI" — Momus must-fix, recorded not dropped): a live,
      multi-request `curl`/`docker` mutation sequence. **Blocked on** (a) the net-server
      stale-RX-across-connections bug (multi-connection wire sequences are flaky —
      documented in 6.5b) and (b) `POST create` needing a `/data` mount + real image not
      configured at boot (standing one up blew the 90s CI budget in 6.3). The mutation
      **success** path remains covered in-kernel by `test_container_registry` /
      `test_container_run`; 6.6 delivers the auth layer specifically. Unblock is a
      net-server RX-recycling fix + `/data`-at-boot (a later net/boot sub-phase).

### 6.7 — Finalize: docs + trackers + hardening pass

**Goal**: Close the phase honestly.

**What to build**:
- mdbook `management-api.md` (ring-3 api-server, management ABI + `Management` cap,
  the kernel-accept shim, Engine API subset, two-layer auth, log capture,
  deferrals). Add to `SUMMARY.md`.
- Reconcile the three synced trackers (CLAUDE.md table, `milestones.md` summary +
  heading) with an honest status string; rewrite CLAUDE.md Current Status.
- A hardening pass (Momus) on the new untrusted-input parsers: request-size/header
  bombs, deep-JSON request bodies, adversarial container ids.

**Acceptance**:
- [ ] Full suite green + 3× soak; **arm64 still builds** (no arm64 net stack exists
      — this is a build check, not a meaningful gate; Momus L3); mdbook builds.
- [ ] Trackers reconciled; docs written; hardening tests added.

## Deferred (documented — out of the off-ramp)

- **Live registry pull for `run <arbitrary-image>`** — rides with the 5.6-deferred
  live TCP registry transport; in-phase `run` uses staged/embedded bundles.
- **Relaxing guest `sys_socket` for general ring-3 TCP servers** — the api-server
  uses the kernel-accept shim; a general guest TCP `socket()` path is a separate
  later hardening.
- **TLS / HTTPS (rustls-in-ring-3)** and **mTLS client-cert auth** — large ring-3
  dependency; plain HTTP first; own bare-metal-build gate when it lands.
- **Interactive `exec` streaming over websocket** (`docker exec -it`) — needs
  container `exec` (deferred from Phase 5) + websocket framing + stream hijack.
- **Full Engine API breadth** — networks, volumes, `build`, `events`, swarm, stats.
- **A real wall clock** for RFC3339 timestamps — monotonic since-boot first.
- **Config via Limine cmdline** — `ServerBootInfo` first.

## Dependencies

| Crate/tool | Used by | Purpose | Status |
|------------|---------|---------|--------|
| (hand-rolled) | api-server | HTTP request parser, JSON serializer, router | in-tree, no new dep |
| (deferred) rustls / no_std TLS | api-server | HTTPS / mTLS | **later — bare-metal-build gate first** |
| `docker` CLI + `curl` (host) | integration test | drive the live API over `hostfwd` | host-only test tooling (Brewfile/dev-setup) |

## Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Ring-3 inbound TCP is unproven (first in the OS) | High | High | **6.4 proves it in isolation before the api-server**, via the trusted kernel-accept shim (reuses green `ksocket_*`), not the unrun `sys_socket` TCP gate. |
| 6.5 api-server scope creep (full Engine API) | High | High | Hard subset (ps/create/start/stop/logs/health); MockConn router test is the primary acceptance; everything else deferred. |
| Sentinel-cap grant path doesn't exist in `spawn_server` | Medium | Medium | 6.4 extends `ServerConfig`/`spawn_server` explicitly, proven by the throwaway crate. |
| Log/mount lifecycle tied to a recycled pid | Medium | High | Both owned by the `ContainerId` row, freed on removal not exit (H2/M2). |
| Docker CLI stricter than curl (version prefix/headers) | Medium | Medium | Router tolerates `/v1.NN/` from day one; curl-first, real-`docker` is a stretch. |
| Single shared socket payload region serializes I/O | High | Low | Sequential accept loop by design; documented throughput ceiling. |
| Untrusted HTTP/JSON parsing bugs | Medium | High | Ring-3 containment + fail-closed parsers + a dedicated 6.7 hardening pass. |

## Sub-phase effort (rough)

| Sub-phase | Est. complexity |
|-----------|-----------------|
| 6.0 HTTP request parser + JSON serializer | Low-Medium |
| 6.1 Container metadata table + mount lifecycle + image glue | Medium |
| 6.2 Per-container log capture (ContainerId-owned) | Medium |
| 6.3 Management ABI + `CapType::Management` (listener via kernel path) | Medium-High |
| 6.4 Prove ring-3 TCP + sentinel-cap spawn wiring | Medium-High |
| 6.5 Ring-3 api-server + Engine API routing | High |
| 6.6 Auth + live curl/docker integration | Medium-High |
| 6.7 Finalize (docs, trackers, hardening) | Low-Medium |

---

_Plan reviewed by Momus (2026-07-31): REVISE. All five must-fixes applied — the
buried ring-3 TCP sub-phase promoted to 6.4 (kernel-accept-shim route chosen);
6.4-was-one-sub-phase split into transport (6.4) + api-server (6.5); the authz
model rewritten as two layers (kernel `Management` cap + app-level token policy);
the log buffer + rootfs mount re-keyed to `ContainerId`; the `run` off-ramp
downgraded to staged images. Lesser items (L1 ring-0-lifecycle decision made
explicit, L2 MockConn as primary HTTP acceptance, L3 arm64 gate downgraded to a
build check, L4 `/v1.NN/` prefix from day one) folded in._
