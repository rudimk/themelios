# Management API

ThemeliOS is managed entirely through an external HTTP API — there is **no SSH,
no shell, no interactive login**. A node is driven the way a container host is
driven: a control-plane process listens on a TCP port and speaks a subset of the
**Docker Engine API**, so existing tooling and habits transfer. This chapter
describes that control plane: where it runs, how it is authorized, the ABI it
drives into the kernel, and the two layers of authentication that gate it.

## The api-server is a ring-3 process

The management API is served by `api-server`, an ordinary **userspace (ring-3)
process** — not kernel code. This is a direct consequence of the microkernel
design: parsing untrusted HTTP off a socket is exactly the kind of complex,
attack-exposed work that does **not** belong in the kernel. The kernel exposes a
narrow, capability-checked seam; everything above it — HTTP framing, request
routing, JSON, authentication policy — lives in the api-server, where a bug is a
crashed process, not a compromised kernel.

The full request path is:

```
accept → HTTP parse → authenticate → route → management ABI (SYS_MGMT) → JSON → reply
```

The first four stages are ring-3 code; only the management ABI crosses into the
kernel, and only after the request has been authenticated.

### Fault-freedom is mandatory

There is one sharp constraint on a ring-3 server: **a user-mode fault halts the
whole kernel** (the IDT halts on faults taken from ring 3, rather than killing
just the faulting task — a deliberate fail-stop for this stage of the project).
A page fault, a panic, an out-of-bounds slice, an arithmetic overflow, or an
unbounded recursion in the api-server is therefore a **node-wide denial of
service**, reachable by anyone who can send it a request.

The api-server is written defensively against this:

- The request buffer is bounded to `MAX_REQUEST` (64 KiB) **before** it grows,
  so a hostile client cannot make it allocate without limit.
- Every syscall return value is checked; no `unwrap`/`expect` on socket data.
- Every loop is bounded (a large spin cap, yielding on `WouldBlock`) so a slow or
  stalled peer cannot wedge the single core.
- The HTTP parser and JSON parser are `None`-on-malformed, never panicking:
  bounded header counts, bounded body size, `checked_add` on lengths, and a
  recursion-depth guard on the JSON parser (a deeply-nested `[[[[…` body cannot
  overflow the stack).
- The accepted socket is closed after every request; `libthemelios` installs an
  allocation-error handler so an OOM exits the process cleanly instead of
  aborting.

## Two layers of authorization

Access to the management API is gated at **two independent layers**, one in the
kernel and one in the api-server.

### Layer 1 — the `Management` capability (kernel)

The kernel does not know about HTTP, tokens, or clients. It knows one thing: the
authority to drive the management ABI is a **capability**. `CapType::Management`
is a coarse, fieldless *sentinel* capability — holding it grants **every**
management operation; not holding it denies **every** one — exactly analogous to
the `SOCKET_FACTORY` authority for networking. It is minted **only** to the
trusted, kernel-spawned api-server (via `ServerConfig::grant_management`) and
**never** placed in a container's capability space. A container is created with
an empty CSpace, so it can never even name the management ABI, let alone call it.

This is the microkernel's answer to *ambient authority*: without a capability
model, any process could enumerate or stop every container on the node. Here,
that power is an unforgeable token held by exactly one process.

### Layer 2 — bearer-token authentication (api-server)

The `Management` capability answers "may this *process* drive the ABI?" It says
nothing about "may this *remote client* make this API call?" That second question
is **application-level policy**, and it lives in the api-server.

Every route except the `GET /_ping` and `GET /version` health/version probes
requires an HTTP `Authorization: Bearer <token>` header whose token matches the
one the kernel **provisioned to the api-server via boot-info**. The token is
provisioned only to the control plane (the sole `grant_management` server), so
only it holds the node secret in its address space; it rides in the boot-info
page rather than being baked into the binary image, modelling a per-node secret
handed over at spawn.

Enforcement details:

- A missing or wrong token is rejected with **`401 Unauthorized`** *before any
  management op runs* — including on **unknown paths**, so an unauthenticated
  client cannot even enumerate which routes exist.
- A **wrong** token is `401`, not `403`: per RFC 9110 a correct token *would*
  work, so the request is "unauthorized", never "forbidden". `403` is reserved
  for an authenticated-but-unauthorized principal, a distinction a single
  all-or-nothing token does not have.
- The `Bearer` scheme is matched case-insensitively; the token itself is compared
  exactly.
- Authentication outcomes are **audited on the same ABI as operations**:
  successful calls audit as `ApiAccess` (the management op), and rejections go
  through a dedicated `SYS_MGMT` audit verb that records a distinct
  `ApiAuthReject` event — so a failed auth attempt is as visible in the audit log
  as a successful op.

> **Not transport security.** Bearer auth over plaintext HTTP gates *who can drive
> the API*, but does nothing against a wire sniffer — the token travels in
> cleartext. Transport confidentiality (TLS/mTLS) is deferred; until it lands, the
> API must not be exposed on an untrusted network. For this reason the token
> compare is a plain byte compare, not constant-time: a timing oracle would reveal
> nothing the cleartext transport does not already give away.

## The `SYS_MGMT` ABI

The seam between the ring-3 api-server and the kernel is a single syscall,
`SYS_MGMT`, **op-multiplexed** on a verb selector in `RDI` — so the whole growing
ABI costs one syscall number instead of one per verb. Op-specific arguments ride
the remaining registers; the return value is the verb's success value (a
capability handle or a byte count) with bit 63 clear, or a high-bit-set
`MgmtError` code.

| Verb | Selector | Input | Output |
|------|----------|-------|--------|
| `LISTEN` | 1 | TCP port | a listener `Socket` cap handle |
| `LIST` | 2 | — | `/containers/json` summary array |
| `INSPECT` | 3 | id/name | `/containers/{id}/json` detail |
| `NODE_INFO` | 4 | — | `/info` counts |
| `CREATE` | 5 | `"image\0name"` | `{"Id":…}` |
| `START` | 6 | id/name | — (204) |
| `STOP` | 7 | id/name | — (204) |
| `LOGS` | 8 | id/name | raw log bytes (bounded) |
| `AUDIT_DENY` | 9 | — | — (records an auth rejection) |

Every verb is capability-checked against the caller's `Management` cap and audited
inside the kernel `mgmt` module *before* it touches any backing service — the
fail-closed property. Each op returns **owned bytes** (a `Vec<u8>` of compact
Engine-API JSON, or a freshly minted capability handle), so the api-server
consumes the result with no shared-lifetime hazard across the ring boundary.

`MgmtError` is a stable, numbered space (`PermissionDenied`, `NotFound`,
`InvalidState`, `InvalidArgument`, `CreateFailed`, `ServerUnavailable`,
`NoResources`, `BufferTooSmall`) that the api-server maps to Docker-style HTTP
statuses (`404`/`409`/`400`/`500`). A read verb whose JSON exceeds the caller's
output buffer fails closed as `BufferTooSmall` rather than truncating.

### The kernel-accept shim

The listener is opened through the ABI, not by the ring-3 server binding a socket
directly. `LISTEN` runs the trusted kernel socket path (open → bind → listen) and
mints a per-listener `Socket` capability **parented to the management handle**, so
it is revoked together with the management grant. The api-server then `accept`s on
that handle through the ordinary socket ABI — there is no separate management
`accept`. This keeps the privileged bind/listen inside the kernel while the
untrusted accept-loop lives in ring 3.

## The Engine API subset

The api-server implements a hard subset of the Docker Engine API — enough to list,
create, start, stop, and inspect containers and read their logs:

| Method & path | Maps to |
|---------------|---------|
| `GET /_ping` | health probe (no auth) |
| `GET /version` | version JSON (no auth) |
| `GET /info` | `NODE_INFO` |
| `GET /containers/json` | `LIST` |
| `GET /containers/{id}/json` | `INSPECT` |
| `GET /containers/{id}/logs` | `LOGS` |
| `POST /containers/create` | `CREATE` (JSON body → `Image`) |
| `POST /containers/{id}/start` | `START` |
| `POST /containers/{id}/stop` | `STOP` |

A Docker `/v1.NN` API-version prefix on the path is stripped before routing.
Container **logs** are captured into a per-container RAM ring buffer as the
container writes to stdout/stderr (fd 1/2 through the Linux `write`/`writev`
path), keyed by container id so the log survives the process; `LOGS` reads back a
bounded tail.

## Testing

The management API is proven by `test_api_server`, in three phases:

1. **Fail-closed control** — the api-server spawned *without* the `Management`
   grant has its `LISTEN` denied (`PermissionDenied`) before any NIC access, and
   reports `DENIED`. This proves the capability gate.
2. **Routing / auth / JSON self-test** — a deterministic, in-process run (no
   network) drives a fixed set of requests through the router and asserts the exact
   HTTP statuses `[200, 401, 401, 200, 400, 500, 409]`. Each status is impossible
   for the catch-all `404`, so observing it proves the specific arm ran: GET/POST
   routing, the `401`/`200` auth contrast on one route, the untrusted request-body
   JSON parse, `Image` extraction, and the create/start write verbs — all without
   depending on the timing-sensitive inbound-TCP path.
3. **Live inbound smoke** — a single authenticated `GET /containers/json` sent over
   a host-to-guest port forward, proving the accept → parse → authenticate → route
   → reply path (and that the `Authorization` header) round-trips over real TCP.

The in-process self-test exists because the immature ring-3 net server can deliver
stale RX data across sequential connections on one listener, which makes a
multi-connection, content-asserting wire test flaky; proving the *content* of the
routing and auth logic in-process removes that dependency, leaving the wire path a
single-connection smoke.

## Deferrals

The management API is functionally complete for its core (list/create/start/stop/
inspect/logs + auth), with several capabilities explicitly deferred:

- **TLS / mTLS** — transport confidentiality and client-certificate auth. Until it
  lands, the API is a plaintext, app-token-gated interface not to be exposed on an
  untrusted network.
- **`exec` and interactive streaming** — `docker exec` and websocket-based
  bidirectional streams for interactive sessions.
- **A live `docker` CLI / multi-request `curl` mutation sequence** end to end —
  blocked on the net-server's stale-RX-across-connections behavior and on
  `POST create` needing a `/data` mount and a real image provisioned at boot. The
  container-creation *success* path is covered in-kernel by the container-runtime
  tests; the API layer's create/start/logs verbs are proven at the ABI and routing
  levels.
- **Networks and image management endpoints** beyond the container lifecycle
  subset.
