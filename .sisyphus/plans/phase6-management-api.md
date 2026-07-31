# Phase 6 — Docker-compatible Management API

**Status**: PLANNING (drafted 2026-07-31; Momus review pending)
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
$ docker -H tcp://<node>:2375 ps           # lists running containers
$ docker -H tcp://<node>:2375 run <image>  # create + start from a staged image
$ docker -H tcp://<node>:2375 logs <id>    # streams that container's stdout
```

Realistically we prove the JSON/route layer first with `curl` against the exact
Engine API shapes, then validate the real `docker` CLI end-to-end over a QEMU
`hostfwd` in an integration test. **Plain HTTP first; TLS/mTLS is deferred**
(rustls-in-ring-3, the same large dependency Phase 5 deferred).

## The core thesis for this phase

**The API server is a ring-3 process holding a capability, not kernel code.**

An HTTP/1.1 request parser, a JSON serializer, and a Docker route table all
consume **untrusted bytes off a socket** — exactly the class of code Phases 3–5
keep out of the kernel (VFS parsers, the TCP stack, the OCI/registry parser all
run in ring 3). So the management API lives in a new ring-3 **`api-server`**,
symmetric with the `net-server`:

- It **accepts TCP connections** using the Phase 4.6 `listen`/`accept` socket
  wrappers (`servers/libthemelios`), holding a network-authority cap.
- It **parses HTTP + JSON and routes** the Docker API entirely in ring 3.
- It reaches container/process operations only through a **new, minimal,
  capability-guarded "management ABI"** (§6.3) — it cannot do anything its
  **management-authority capability** doesn't permit. A malformed request can, at
  worst, crash the api-server; it can never touch kernel memory or escalate.

This is the payoff of the microkernel model again: "the API is the only
interface" is enforced by *capabilities*, and the risky parsing sits behind the
ring 0/3 boundary. The kernel gains only a small, audited management ABI.

## Grounding (from a codebase survey — anchors in the notes)

**Reusable as-is:**
- TCP **listen/accept/send/recv** with per-connection cap minting
  (`net/socket.rs` `sys_listen`/`sys_accept`; accept mints a fresh
  `CapType::Socket{socket_id}` parented to the listener). Ring-3 wrappers exist
  (`libthemelios`: `socket`/`listen`/`accept`/`tcp_send`). Only ever exercised
  *kernel-internally* (`test_tcp_server` uses `ksocket_*`) — **no ring-3 guest
  TCP server exists yet**.
- Capability mint/check + the **`SOCKET_FACTORY` sentinel** pattern
  (`cap/mod.rs`: a `Socket{socket_id: u64::MAX}` = "authority to create sockets").
  This is the exact template for a **management-authority** sentinel cap.
- Container **create/start/terminate** (`container/mod.rs`); the container-identity
  predicate `personality==Linux && rootfs_mount.is_some()`.
- Process introspection: `process_list() -> Vec<ProcessInfo{pid,name,task_count,
  state,cap_count}>`, `exit_status`, `personality`, `rootfs_mount`.
- HTTP **response** helpers (`oci/registry.rs`: `find_sub`, the `\r\n\r\n` split,
  `find_content_length`) and the JSON **reader** (`oci/json.rs`, depth-bounded).
- `ServerBootInfo` config channel (`process/server.rs`), the audit log, SHA-256,
  and both test harnesses (`hostfwd` host-thread + in-kernel MockConn).

**Net-new (the real work of Phase 6):**
1. An HTTP **request** parser (method/path/version/header-map/body) + a route
   table — the client only parses *responses*.
2. A JSON **serializer** — `json.rs` only *reads*.
3. A **container-metadata table** (id ↔ pid ↔ name/image/created/command/state) —
   today all containers are literally named `"container"`; no id/name/image is
   stored.
4. **Per-container stdout/stderr capture** — today `console_write`/`sys_writev`
   route fd 1/2 straight to the shared serial console; the "RAM ring buffer" in
   CLAUDE.md is aspirational.
5. A **management ABI** (capability-guarded syscalls/IPC) exposing lifecycle +
   introspection to the ring-3 api-server.
6. An **authenticated-principal → capability-set** authz layer + a management
   authority sentinel cap + an `ApiAccess` audit op.
7. A **boot-config path** (extend `ServerBootInfo`, or a Limine cmdline) for the
   API listen port + authority cap.
8. **All TLS** (deferred).

## Key Architectural Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| API server location | **Ring-3 `api-server` process** (symmetric with net-server) | Untrusted HTTP/JSON parsing must not run in ring 0 — same rule as the VFS/net/OCI parsers. The kernel exposes only a small management ABI. |
| How the api-server reaches lifecycle ops | **A new capability-guarded "management ABI"** (a handful of syscalls or an IPC endpoint), gated by a **management-authority sentinel cap** | The api-server can only manage containers because it holds the authority cap; strip the cap and it is inert. Mirrors `SOCKET_FACTORY`. The kernel never trusts the request bytes, only the cap. |
| HTTP + JSON | **Hand-rolled request parser + JSON serializer, `alloc`-only, fail-closed** | Reuse `find_sub`/CRLF-split/`Content-Length`; add request-line/header parsing and a `Value`→bytes serializer. Bounded sizes/depth (like the registry hardening). No new deps. |
| Container identity | **A first-class container-metadata table** keyed by a generated id, mapping id↔pid + name/image/created/command/labels/state | Docker semantics (`docker ps` names, `docker inspect`, id-prefix lookup, filtering) need metadata the process table doesn't hold. Captured at create time. |
| `docker logs` | **Per-container in-memory ring buffer**; `console_write`/`sys_writev` route to the calling pid's buffer (serial kept as a debug mirror) | The audit ring buffer is the structural template. No persistent log storage on-node (CLAUDE.md), so RAM-backed + read-back over the API. |
| Timestamps | **Monotonic "seconds since boot" from the PIT tick** for `Created`/`StartedAt`; a real wall clock is deferred | There is no RTC/wall clock today. Docker CLI tolerates the field's presence; note the caveat. A wall clock is a small later add. |
| Auth | **Token (shared-secret / bearer) mapped to a capability set; client-cert (mTLS) deferred with TLS** | Without TLS there is no cert chain to validate. A bearer token over plain HTTP proves the principal→capability-set mapping; mTLS lands with TLS. |
| Transport security | **Plain HTTP first; TLS (rustls-in-ring-3) deferred** | TLS is a large ring-3 dependency (deferred in Phase 5). The Engine API and auth layer are provable over plain HTTP on a trusted network / `hostfwd`; TLS is a scoped follow-up with its own bare-metal-build gate. |
| Shell in production | **Keep the serial shell for dev/test; gate it out of production builds** | "No shell" is a production posture. The dev shell stays behind a build/boot flag; it is not a Phase 6 deliverable to remove it, only to make the API self-sufficient. |

## Sub-phase breakdown

Each sub-phase is a fresh branch + PR off the latest `main`, reviewed with
sisyphus + Momus, with deterministic tests and a soak before merge — the Phase 5
cadence.

### 6.0 — HTTP request parser + JSON serializer (the parsing primitives)

**Goal**: The two net-new, purely-functional building blocks, unit-tested offline.

**What to build**:
- `http::request` — parse an HTTP/1.1 request into `{method, path, query, headers,
  body}`: request line (`METHOD SP path SP HTTP/1.1`), header map, `Content-Length`
  body. Reuse `find_sub`/CRLF-split/`find_content_length` from `registry.rs`
  (factor the shared bits into a small `http` module). **Fail-closed**: bounded
  request size, bounded header count/length, reject a `Content-Length` larger than
  a cap (`checked_add`, like the 5.6 hardening). Chunked request bodies rejected
  (documented), matching the client's limitation.
- `json::to_bytes` (or a small `JsonBuilder`) — serialize objects/arrays/strings/
  numbers/bools/null with correct escaping. Round-trips with the existing reader.

**Acceptance**:
- [ ] Unit tests: parse a `GET /containers/json?all=1` and a `POST
      /containers/create` with a JSON body; assert method/path/query/headers/body.
- [ ] Malformed inputs (no CRLF, absurd `Content-Length`, oversized header) return
      an error, never panic/hang.
- [ ] JSON serializer round-trips through `json::parse`; strings with `"`, `\`,
      control chars escape correctly.

### 6.1 — Container metadata table + image→create glue

**Goal**: A first-class notion of "a container" with the metadata Docker needs.

**What to build**:
- `container::registry` — an id-keyed table: `ContainerId` (generated, hex),
  `name` (user or auto), `image` ref, `created` (tick), `command` (argv, captured
  before it's consumed into the stack), `state` (Created/Running/Exited+code),
  `pid`. Insert on create, update on start/exit/terminate; id-prefix lookup.
- `container::create_from_image(image_ref, name, mount)` glue that resolves an
  image (local staged bundle now; `oci::registry::pull` when a registry is
  configured — the puller already exists) → `create` → records metadata.
- Keep argv/image/name that `create` currently discards.

**Acceptance**:
- [ ] In-kernel test: create two containers, list the table, look one up by
      id-prefix and by name; assert metadata; start one, assert state transitions.
- [ ] `run` shell cmd records a metadata row; `stop`/exit update it.

### 6.2 — Per-container stdout/stderr capture (`docker logs` backing)

**Goal**: Capture container output per-container instead of dumping to serial.

**What to build**:
- A per-container log ring buffer (bounded, RAM-backed; audit ring buffer as the
  template). `linux::syscall::console_write`/`sys_writev` route fd 1/2 to the
  calling pid's container-log buffer (keep a serial mirror behind a dev flag).
- A read-back API (`container::logs(id) -> bytes`, with a tail/offset) for the
  Engine API and a `logs <id>` shell cmd.

**Acceptance**:
- [ ] Test: run a container that writes known bytes to stdout; read its log buffer
      back and byte-match; a second container's buffer is independent.
- [ ] Buffer is bounded (oldest dropped), no unbounded growth.

### 6.3 — Management ABI (capability-guarded lifecycle/introspection surface)

**Goal**: Let a ring-3 holder of a management-authority cap drive container ops.

**What to build**:
- A **management-authority sentinel capability** (à la `SOCKET_FACTORY`):
  `CapType` carrying a management sentinel, minted to the api-server at spawn.
- A minimal ABI (syscalls or a dedicated IPC endpoint) checked against that cap:
  `list` (→ metadata rows), `inspect(id)`, `create(image,name,cfg)`, `start(id)`,
  `stop(id)`, `logs(id, tail)`, `node_info`. Each call **audits** an `ApiAccess`
  event. No ambient authority: without the cap every call is denied.

**Acceptance**:
- [ ] Test: a process **with** the authority cap can list/create/start/stop/logs;
      a process **without** it gets a capability denial on every call.
- [ ] Each management op emits an `ApiAccess` audit entry.

### 6.4 — Ring-3 `api-server`: HTTP + Docker Engine API routing

**Goal**: The server itself — accept, parse, route, serialize, respond.

**What to build**:
- `servers/api-server` — accept loop over the Phase 4.6 TCP wrappers; per-request
  HTTP parse (6.0) → route → management ABI (6.3) → JSON (6.0) → response.
- Endpoint subset (enough for `docker ps`/`run`/`logs`/health):
  `GET /_ping`, `GET /version`, `GET /info` (node health/status);
  `GET /containers/json` (ps), `GET /containers/{id}/json` (inspect),
  `POST /containers/create`, `POST /containers/{id}/start`,
  `POST /containers/{id}/stop`, `GET /containers/{id}/logs`,
  `GET /images/json`. Correct status codes + Engine API JSON shapes.
- Relax/expose the guest TCP `socket()` path (`sys_socket` currently hard-rejects
  guest TCP) or provide the api-server a listener via the management ABI.
- Boot wiring: spawn the api-server with its authority cap + listen port via
  `ServerBootInfo` (the net-server wiring is the template).

**Acceptance**:
- [ ] In-kernel MockConn-style test: feed canned HTTP requests to the router,
      assert the JSON responses for `_ping`/`version`/`info`/`containers/json`/
      `create`+`start`+`logs`.
- [ ] The api-server boots and listens; a guest-side smoke exercises one endpoint.

### 6.5 — Auth + live `docker`/curl integration

**Goal**: Principal→capability-set auth, and a real client over the wire.

**What to build**:
- Bearer-token auth: a configured token (via `ServerBootInfo`) maps a request to a
  permitted operation set; unauthenticated/again-wrong → `401`/`403`, audited.
- A `hostfwd` integration test (the `test_tcp_server`/`spawn_tcp_test_peer`
  pattern): the host issues real HTTP (`curl`, then the real `docker -H tcp://…`
  CLI if feasible) → asserts `docker ps`/`create`/`start`/`logs` round-trips.

**Acceptance**:
- [ ] Wrong/absent token → `401`/`403`; correct token → success; both audited.
- [ ] Host-side `curl http://127.0.0.1:<fwd>/containers/json` returns valid Engine
      API JSON; a create+start+logs sequence works end to end.
- [ ] (Stretch) the real `docker` CLI lists/runs/logs against the node.

### 6.6 — Finalize: docs + trackers + hardening pass

**Goal**: Close the phase honestly.

**What to build**:
- mdbook `management-api.md` (the ring-3 api-server, the management ABI + authority
  cap, the Engine API subset, auth, log capture, deferrals). Add to `SUMMARY.md`.
- Reconcile the three synced trackers (CLAUDE.md table, `milestones.md` summary +
  heading) with an honest status string; rewrite CLAUDE.md Current Status.
- A hardening pass on the new untrusted-input parsers (Momus): request-size/header
  bombs, deep-JSON in request bodies, id-lookup on adversarial ids.

**Acceptance**:
- [ ] Full suite green + 3× soak + arm64 gate; mdbook builds.
- [ ] Trackers reconciled; docs written; hardening tests added.

## Deferred (documented — out of the off-ramp)

- **TLS / HTTPS (rustls-in-ring-3)** and **mTLS client-cert auth** — large ring-3
  dependency; the API runs plain HTTP first (like the registry client). Own
  bare-metal-build gate when it lands.
- **Interactive `exec` streaming over websocket** (`docker exec -it`) — needs
  container `exec` (itself deferred from Phase 5), websocket framing, and
  bidirectional stream hijack. Large; separate follow-up.
- **Full Engine API breadth** — networks, volumes, `build`, `events` stream,
  swarm, stats — only the container/image/logs/health subset here.
- **A real wall clock** for RFC3339 timestamps — monotonic since-boot first.
- **Config injection via Limine cmdline** — `ServerBootInfo` first; a kernel
  cmdline parser is a later, orthogonal add.

## Dependencies

| Crate/tool | Used by | Purpose | Status |
|------------|---------|---------|--------|
| (hand-rolled) | api-server | HTTP request parser, JSON serializer, router | in-tree, no new dep |
| (deferred) rustls / no_std TLS | api-server | HTTPS / mTLS | **Phase 6.x/later — bare-metal-build gate first** |
| `docker` CLI + `curl` (host) | integration test | drive the live API over `hostfwd` | host-only test tooling (Brewfile/dev-setup) |

## Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| API-server scope creep (full Engine API) | High | High | Hard subset (ps/create/start/stop/logs/health); everything else explicitly deferred. |
| Docker CLI stricter than curl (headers/version negotiation) | Medium | Medium | Prove with curl against exact shapes first; treat the real-`docker` round-trip as a stretch acceptance, not a blocker. |
| Management ABI widens kernel attack surface | Medium | High | Keep it minimal + capability-gated + audited; the api-server holds one authority cap, nothing ambient. |
| Untrusted HTTP/JSON parsing bugs | Medium | High | Ring-3 containment + fail-closed parsers + a dedicated 6.6 hardening pass (bombs, deep JSON, adversarial ids). |
| No wall clock → Docker timestamp fields | Low | Low | Monotonic since-boot; documented; wall clock later. |

## Sub-phase effort (rough)

| Sub-phase | Est. complexity |
|-----------|-----------------|
| 6.0 HTTP request parser + JSON serializer | Low-Medium |
| 6.1 Container metadata table + image glue | Medium |
| 6.2 Per-container log capture | Medium |
| 6.3 Management ABI + authority cap | Medium-High |
| 6.4 Ring-3 api-server + Engine API routing | High |
| 6.5 Auth + live integration test | Medium-High |
| 6.6 Finalize (docs, trackers, hardening) | Low-Medium |
