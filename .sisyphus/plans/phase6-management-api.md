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

### 6.2 — Per-container stdout/stderr capture (`docker logs` backing)

**Goal**: Capture container output per-container, surviving exit.

**What to build**:
- A per-container log ring buffer **owned by the `ContainerId` row** (audit ring
  buffer as the template). `console_write`/`sys_writev` map `current pid →
  ContainerId → buffer` (serial kept as a dev mirror). Non-container Linux
  processes allocate no buffer. Buffer freed on container removal, **not** on exit.
- Read-back API (`container::logs(id, tail) -> bytes`) + a `logs <id>` shell cmd.

**Acceptance**:
- [ ] Run a container that writes known bytes; read its buffer back and byte-match;
      a second container's buffer is independent; the buffer is **still readable
      after the container exits**.
- [ ] Bounded (oldest dropped); no unbounded growth; no buffer for non-containers.

### 6.3 — Management ABI + `CapType::Management` + connection-accept shim

**Goal**: The capability-guarded ring-0 surface the api-server drives — including
inbound TCP via the proven kernel accept path.

**What to build**:
- **`CapType::Management`** sentinel + its `resolve_*` check (has-it-or-not).
- A minimal ABI (syscalls or a dedicated IPC endpoint), each op checked against the
  cap and audited (`ApiAccess`): `list`, `inspect(id)`, `create(image,name,cfg)`,
  `start(id)`, `stop(id)`, `logs(id,tail)`, `node_info`, and — the C1 fix —
  `listen(port)` / `accept() -> (conn_cap, peer)` implemented over the **trusted
  `ksocket_listen`/`ksocket_accept`** path, returning a minted per-connection
  `Socket` cap. Without the `Management` cap every op is denied.

**Acceptance**:
- [ ] A process **with** the cap can list/create/start/stop/logs + listen/accept;
      **without** it, every op is a capability denial.
- [ ] Each op emits an `ApiAccess` audit entry.

### 6.4 — Prove ring-3 TCP transport + sentinel-cap spawn wiring (Momus C1/C2)

**Goal**: De-risk the OS's **first ring-3 inbound-TCP + first sentinel-cap grant**
in isolation, before the api-server depends on them.

**What to build**:
- Extend `ServerConfig`/`spawn_server` to **grant sentinel caps** (Management +,
  if needed, network authority) — today it grants only `filesystem_mount`.
- A throwaway ring-3 crate (`servers/tcp-echo-smoke`, `libthemelios` wrappers) that
  holds the caps and, via the 6.3 accept shim, `listen`s → `accept`s → `recv`/
  `send` echoes a line — the `test_tcp_server` round-trip but **from ring 3**.

**Acceptance**:
- [ ] Over `hostfwd`, the host connects, sends a line, the ring-3 guest echoes it —
      proving ring-3 accept/recv/send end to end.
- [ ] `spawn_server` grants the sentinel cap; a control run without the grant fails
      closed.

### 6.5 — Ring-3 `api-server` skeleton + Docker Engine API routing

**Goal**: The server: accept (6.4) → parse (6.0) → route → ABI (6.3) → JSON → reply.

**What to build**:
- `servers/api-server` — **strictly sequential** accept/serve loop; per-request
  HTTP parse → route (tolerating `/v1.NN/`) → management ABI → JSON → response.
- Endpoint subset: `GET /_ping`, `GET /version`, `GET /info`;
  `GET /containers/json`, `GET /containers/{id}/json`, `POST /containers/create`,
  `POST /containers/{id}/start`, `POST /containers/{id}/stop`,
  `GET /containers/{id}/logs`, `GET /images/json`. Correct status codes + Engine
  API JSON shapes.
- Boot wiring: spawn with the Management cap + listen port via `ServerBootInfo`.

**Acceptance**:
- [ ] **Primary (MockConn-style, no NIC):** feed canned HTTP requests to the
      router; assert JSON for `_ping`/`version`/`info`/`containers/json`/
      `create`+`start`+`logs`. This proves the HTTP core even if the socket path
      slips.
- [ ] The api-server boots, listens, and answers one endpoint over the 6.4 path.

### 6.6 — Auth + live curl/`docker` integration

**Goal**: The app-layer token policy and a real client over the wire.

**What to build**:
- Bearer-token auth **inside the api-server** (token via `ServerBootInfo`) mapping a
  request to an allowed-operation set; unauth/wrong → `401`/`403`, audited via the
  ABI. (Kernel side is just the one `Management` cap; ops-policy is app-level — H1.)
- A `hostfwd` integration test (the `spawn_tcp_test_peer` pattern): host issues real
  HTTP via `curl` → asserts `containers/json` + a create+start+logs sequence;
  **stretch:** the real `docker -H tcp://…` CLI.

**Acceptance**:
- [ ] Wrong/absent token → `401`/`403`; correct → success; both audited.
- [ ] Host `curl http://127.0.0.1:<fwd>/containers/json` returns valid Engine API
      JSON; create+start+logs works end to end.
- [ ] (Stretch) the real `docker` CLI lists/runs/logs against the node.

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
| 6.3 Management ABI + `CapType::Management` + accept shim | Medium-High |
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
