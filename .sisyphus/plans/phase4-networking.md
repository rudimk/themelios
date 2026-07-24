# Phase 4 — Networking

**Status**: IN PROGRESS
**Created**: 2026-07-15
**Revised**: 2026-07-15 (momus review — pull-based RX to avoid an IPC deadlock,
polled RX (no MSI-X), aarch64 gate scoped to smoltcp only, DNS/ICMP scope fixes)
**Phase**: 4

## Goal

Give ThemeliOS a working network stack: a VirtIO-net driver, a userspace TCP/IP
stack, DHCP-based configuration, and a capability-guarded socket API that lets a
userspace process exchange UDP datagrams and TCP streams with the outside world.

The stack is built with the same **hybrid microkernel** philosophy as Phase 3
storage: a thin, trusted driver stays in the kernel; all protocol parsing —
Ethernet, ARP, IPv4, ICMP, UDP, TCP, DHCP — runs in a ring-3 userspace **net
server**. A bug in the protocol code can crash the net server but cannot touch
the kernel, other processes, or the capability system.

## Key Architectural Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| TCP/IP stack location | **Userspace net server (ring 3)** | Protocol parsing is classic attack surface (fragmentation, option parsing, TCP state machine). Running it unprivileged mirrors the Phase 3 FS servers: a malformed packet crashes the net server, not the kernel. |
| Driver location | **Thin VirtIO-net driver in kernel** | Same split as VirtIO-blk: the driver talks to trusted emulated hardware and does DMA, so it stays in ring 0. It exposes only "send this frame / here's a received frame". |
| TCP/IP implementation | **smoltcp** (no_std Rust) | A correct, robust TCP is essential for Phase 5+ (real K8s workloads hammer TCP). smoltcp is the proven no_std stack (Redox uses it), covers ICMP/DHCP/UDP/TCP behind one `Interface`, and is **fully contained by the ring-3 location** — exactly the miniz_oxide precedent from Phase 3. Hand-rolling production-grade TCP is a multi-month correctness risk. |
| Frame transport to userspace | **Kernel net service over IPC + shared memory, PULL-based** | The current kernel IPC is synchronous rendezvous only — there is no non-blocking send or notification primitive (`ipc/mod.rs`). So, like `block_server`, the **ring-3 net server always initiates and the kernel always replies**: the server issues `MSG_POLL` and the service returns the next RX frame (or "empty") plus the current time; TX is a separate call/reply. This is deadlock-free by construction (no unsolicited kernel→ring-3 send). Frame bytes move through shared regions, never IPC words. |
| RX delivery / NIC interrupts | **Polled RX (no MSI-X)** | The Phase 3 VirtIO transport disables MSI-X for every virtqueue (`virtio/mod.rs`), and the IDT only wires IRQ0/IRQ4 — there is no PCI INTx/ISR plumbing. RX is therefore polled: the net service drains the RX virtqueue on the 100 Hz timer tick and on each `MSG_POLL`. Interrupt-driven RX is deferred (would need INTx discovery + ISR ack, or LAPIC/IO-APIC + MSI-X — out of scope for Phase 4). |
| Device abstraction | **Trait-based (`NetDevice`)** | VirtIO-net implements it now. The trait is the **arm64 seam**: the Phase 7 aarch64 port implements device discovery (`virtio-mmio`/ECAM) behind the same trait without touching the stack or socket API. |
| Socket API | **Capability-based, kernel-routed** | New `CapType::Socket`. Socket syscalls check a capability, then route to the net server via IPC — the same router pattern the VFS uses for FS servers. The **kernel** mints/revokes socket capabilities on the server's replies (as `vfs_open`/`vfs_close` do for `FileDescriptor`); the kernel never parses packets. |
| Time source | **Monotonic-millis syscall** | smoltcp's poll loop needs a monotonic clock (`Instant`). The kernel already maintains a 100 Hz `TICK_COUNT` (`idt.rs`); expose it as `SYS_UPTIME_MS` (= ticks × 10). The net server reads it itself each poll — no periodic kernel→server tick message is needed. |
| Target architecture | **amd64 run/test only; arm64-ready by design** | Consistent with Phases 1–3 (arm64 does not boot until Phase 7). The **TCP/IP stack is architecture-independent**; enforced by compiling **smoltcp alone** (with our feature set) for `aarch64-unknown-none` as a dependency check — the honest miniz_oxide analogue. The kernel and net server (whose `libthemelios` syscall wrappers are raw x86 `syscall` asm) build for aarch64 only in Phase 7. |

## Security Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                        Ring 0 (Kernel)                        │
│                                                               │
│  ┌──────────┐   ┌────────────┐   ┌─────────────────────────┐  │
│  │ PCI scan │──▶│ VirtIO-net │──▶│ NetDevice trait         │  │
│  └──────────┘   │  driver    │   │ (RX/TX frames, MAC, MTU)│  │
│                 └────────────┘   └───────────┬─────────────┘  │
│                                              │                │
│                          ┌───────────────────▼─────────────┐  │
│                          │ Net service (kernel task)        │  │
│                          │ RX poll (drains virtqueue) → the │  │
│                          │ net server's MSG_POLL reply      │  │
│                          │ TX request ← net server via IPC  │  │
│                          └───────────────────┬─────────────┘  │
│                                              │ IPC + shared mem│
│  ┌──────────────────────────────────────────┼──────────────┐ │
│  │ Socket dispatch: routes SYS_SOCKET/…      │              │ │
│  │ to the net server, capability-checked     │              │ │
│  └──────────────────────────────────────────┼──────────────┘ │
│                                              │ IPC             │
├──────────────────────────────────────────────┼────────────────┤
│                       Ring 3 (Userspace)     │                │
│                                              ▼                │
│                          ┌────────────────────────────────┐   │
│                          │          net-server            │   │
│                          │  smoltcp: Ethernet/ARP/IPv4/    │   │
│                          │  ICMP/UDP/TCP + DHCPv4 client   │   │
│                          │  Device trait over frame IPC    │   │
│                          │  Wraps smoltcp sockets behind   │   │
│                          │  capability-mapped handles      │   │
│                          └────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────┘
```

**Why this matters**: TCP/IP stacks are, with filesystem parsers, the most
exploited code in monolithic kernels — IP fragment reassembly, TCP option
parsing, and out-of-order segment handling are historical CVE factories. Running
the entire stack (including smoltcp) in ring 3 means a malformed or malicious
packet can at worst crash the net server. It cannot escalate to kernel privilege,
read another process's memory, or bypass a socket capability check.

**On using smoltcp**: smoltcp is a third-party dependency, but it is compiled
into the ring-3 net server and runs with only the capabilities granted to that
server (the net-service endpoint, its shared regions, client socket endpoints).
This is the same containment already accepted for `miniz_oxide` in the SquashFS
server: the ring-3 boundary is the mitigation, so a smoltcp bug is a contained
server crash, not a kernel compromise.

**Upgrade path**: In Phase 5, the Linux syscall compatibility layer maps Linux
socket syscalls (`socket`, `bind`, `connect`, `send`, `recv`, …) onto this
capability socket API. In Phase 7, the arm64 port implements `NetDevice`
discovery for `virtio-mmio`/ECAM and inherits the entire stack unchanged.

## Deliverables

- VirtIO-net driver (RX/TX virtqueues, MAC/MTU from device config) — in kernel,
  **polled RX** (no MSI-X in the current transport)
- `NetDevice` trait and a net device registry (the arm64/bus seam)
- Kernel net service (bridges driver ↔ net server over IPC + shared memory,
  **pull-based**: RX frames returned on the server's `MSG_POLL`, TX forwarding)
- `SYS_UPTIME_MS` monotonic clock syscall for the stack's timers
- Userspace net server: smoltcp integration (Device trait over frame IPC,
  `Interface` + `SocketSet`, poll loop)
- Ethernet + ARP + IPv4 + ICMP bring-up (static IP, ping) via smoltcp
- DHCPv4 client (dynamic address + gateway; DNS server addresses are acquired and
  stored — name **resolution** is deferred, no `socket-dns` in Phase 4)
- `CapType::Socket` and socket dispatch in the kernel
- UDP socket syscalls (socket, bind, sendto, recvfrom, close) + capability checks
- TCP socket syscalls (connect, listen, accept, send, recv, close)
- Audit logging for socket operations
- xtask QEMU networking (user-mode slirp `-netdev`, host/guest port forwarding
  for tests)
- `aarch64-unknown-none` compile gate in CI for **smoltcp alone** (dependency
  check; kernel/net-server aarch64 builds are Phase 7)
- Boot integration (spawn net service + net server, DHCP-configure at boot)
- Shell commands for inspection/testing (`ifconfig`, `sockets`, `ping`,
  `udpsend`, `tcpconnect`)
- Integration tests (link, ARP, ping, DHCP, UDP echo, TCP client + server)
- **Post-phase**: document the network architecture in mdbook

## Workspace Structure (new crates)

```
themelios/
├── kernel/
│   └── src/
│       ├── drivers/virtio/net.rs   # NEW: VirtIO-net driver
│       ├── net/                    # NEW: NetDevice trait, net service, socket dispatch
│       │   ├── mod.rs              #   device registry + socket mount/routing
│       │   ├── device.rs           #   NetDevice trait
│       │   └── net_service.rs      #   kernel bridge task (frame IPC)
│       └── ...
├── servers/
│   ├── net-server/                 # NEW: ring-3 smoltcp TCP/IP stack
│   │   ├── src/main.rs
│   │   └── Cargo.toml              #   no_std, depends on smoltcp
│   └── libthemelios/               # add net/socket protocol types
└── ...
```

The net server follows the exact same embedded-flat-binary pattern as the FS
servers: compiled to a flat binary, embedded via `include_bytes!()`, loaded into
ring 3 by `spawn_server`, granted only the capabilities it needs.

## Sub-phase Dependency Graph

```
4.0 (VirtIO-net + NetDevice) ──▶ 4.1 (Kernel net service, frame IPC)
                                          │
                                          ▼
                            4.2 (Net server + smoltcp Device/poll)
                                          │
                         ┌────────────────┼────────────────┐
                         ▼                                  │
              4.3 (Ethernet/ARP/IPv4/ICMP, static IP, ping) │
                         │                                  │
                         ▼                                  │
              4.4 (DHCPv4 client) ────────────────────────┐ │
                         │                                 │ │
                         ▼                                 ▼ ▼
              4.5 (UDP sockets + CapType::Socket + syscalls)
                         │
                         ▼
              4.6 (TCP sockets: connect/listen/accept)
                         │
                         ▼
              4.7 (Shell + boot integration + tests + arm64 compile gate)
```

**Off-ramp**: after 4.4 the node has a link, an address, and can ping — a
demonstrable milestone. UDP (4.5) and TCP (4.6) are additive.

## IPC Protocol

All communication reuses the Phase 2 IPC system with Phase 3-style shared regions.

### Frame Protocol (net server ↔ kernel net service) — pull-based

The ring-3 net server always initiates; the kernel service always replies. This
is the only shape the current synchronous-rendezvous IPC supports without a new
notification primitive, and it is deadlock-free (no unsolicited kernel→ring-3
send). The net server's event loop is: `MSG_POLL` → drain any RX frame → run
`iface.poll()` → emit any pending TX frames → repeat (yielding when idle).

POLL (net server → service, "give me the next RX frame and the time"):
```
word0 = MSG_POLL
Reply: word0 = status
       word1 = RX frame length (0 = no frame waiting; else bytes at offset 0 of
               the RX shared region)
       word2 = monotonic timestamp (ms)   [= SYS_UPTIME_MS, for smoltcp's clock]
```

TX (net server → service, "transmit this frame"):
```
word0 = MSG_TX_FRAME
word1 = frame length (bytes, at offset 0 of the TX shared region)
Reply: word0 = status (0 = OK, 1 = ERROR)
```

> **Design note (RX buffering & timers)**: RX frames arrive unsolicited at the
> NIC. The service drains the RX virtqueue into a small ring of RX shared-region
> slots (on the 100 Hz timer tick and on each `MSG_POLL`); `MSG_POLL` hands back
> one slot per call. Overflow drops the oldest slot and increments a counter. Slot
> count and overflow behaviour are specified in sub-phase 4.1. Because the server
> reads the time on every `MSG_POLL` and polls on a cadence, smoltcp's
> retransmit/DHCP timers advance without any periodic kernel→server message.

### Socket Protocol (kernel socket dispatch ↔ net server)

```
SOCK_OPEN   [OP_SOCK_OPEN, type(UDP/TCP), 0, 0]            → [status, socket_id]
SOCK_BIND   [OP_SOCK_BIND, socket_id, ip, port]           → [status]
SOCK_CONNECT[OP_SOCK_CONNECT, socket_id, ip, port]        → [status]
SOCK_LISTEN [OP_SOCK_LISTEN, socket_id, backlog, 0]       → [status]
SOCK_ACCEPT [OP_SOCK_ACCEPT, socket_id, 0, 0]             → [status, new_socket_id]
SOCK_SEND   [OP_SOCK_SEND, socket_id, buf_off, buf_len]   → [status, bytes]  (+ dest for UDP)
SOCK_RECV   [OP_SOCK_RECV, socket_id, buf_off, buf_len]   → [status, bytes]  (+ src for UDP)
SOCK_CLOSE  [OP_SOCK_CLOSE, socket_id, 0, 0]              → [status]
```

Payloads travel through a per-client shared region. UDP send/recv carry the peer
address in additional words / a small header in the shared region.

## Error Types

```rust
/// Network device errors — returned by NetDevice trait methods.
pub enum NetError {
    DeviceError,     // VirtIO status != OK
    QueueFull,       // TX virtqueue full
    NoBuffer,        // No RX buffer available
    NotReady,        // Device not initialised
    TooLarge,        // Frame exceeds MTU
}

/// Socket errors — returned by socket operations and syscalls.
pub enum SockError {
    WouldBlock,      // Non-blocking op with no data / not connected yet
    NotConnected,    // Operation requires an established connection
    ConnectionReset, // Peer reset the connection
    ConnectionRefused,
    AddrInUse,       // bind() to an occupied port
    Timeout,
    Unreachable,     // No route / ARP failure
    PermissionDenied,// Capability check failed
    InvalidArgument,
    NoResources,     // Out of socket handles / buffers
    ServerUnavailable,// Net server unreachable
}
```

## Memory Budget

Phase 4 runs with the existing QEMU RAM (`-m 256M`).

| Component | Budget | Rationale |
|-----------|--------|-----------|
| Net server process | 8 MiB address space | Code + smoltcp (~150–250 KiB) + socket buffers. |
| smoltcp socket buffers | 4 MiB | Per-socket RX/TX ring buffers; TCP windows dominate. Bounded per socket, capped socket count. |
| RX shared-region ring | 8 × 2 KiB = 16 KiB | Small ring of frame slots between service and net server. |
| TX shared region | 2 KiB | One in-flight TX frame (MTU-sized). |
| Per-client socket region | 64 KiB | Payload transfer between a client process and the net server. |
| VirtIO-net RX/TX rings | 2 × 4 KiB | 256 descriptors per queue. |
| Kernel RX buffers | 64 × 2 KiB = 128 KiB | Pre-posted receive buffers for the NIC. |

## Sub-phases

### Sub-phase 4.0 — VirtIO-net driver and `NetDevice` trait ✅ COMPLETE

**Implementation notes** (commits `0eaf87e`, `37b34e9`, `8bd0c71`): `kernel/src/net/device.rs`
(`NetDevice` trait — transmit/receive(polled, Ok(0)=none)/mac/mtu — `NetError`, and
a leaked-`'static` registry mirroring the block registry). `drivers/virtio/net.rs`:
two virtqueues (receiveq 0 / transmitq 1), MAC+MTU from device config (negotiates
`F_MAC`|`F_MTU`), a ring of 16 pre-posted 2 KiB RX buffers drained by polling, a
staged TX buffer; 12-byte modern VirtIO-net header added on TX / stripped on RX so
callers see only the Ethernet frame; no offload/mergeable features. Added three
non-blocking virtqueue primitives to `virtio/mod.rs` (`publish`/`kick`/`poll_used`)
and generalised the notify doorbell to the queue index (was hardcoded 0; block's
queue 0 unaffected). xtask attaches `virtio-net-pci` on slirp to run+test.
`test_virtio_net` ARP-resolves the 10.0.2.2 gateway end-to-end (TX + polled RX +
header alignment in one shot). 26 tests pass. Polled RX confirmed working; no
interrupts needed. **Acceptance: all criteria met.**

**Goal**: Send and receive raw Ethernet frames over VirtIO-net from the kernel.

**Rationale**: The driver is the trusted, in-kernel hardware endpoint, built on the
Phase 3 VirtIO transport (which already handles PCI capability discovery, the
init handshake, and split virtqueues). The `NetDevice` trait decouples the stack
from the bus, which is the seam the arm64 port implements in Phase 7.

**What to build**:
- `NetDevice` trait: `transmit(&self, frame: &[u8]) -> Result<(), NetError>`,
  `receive(&self, buf: &mut [u8]) -> Result<usize, NetError>` (or a poll that
  returns the next received frame), `mac(&self) -> [u8; 6]`, `mtu(&self) -> usize`.
- VirtIO-net driver implementing `NetDevice`:
  - Read MAC and MTU from the device-specific config region.
  - Two virtqueues: RX (receiveq) with pre-posted buffers, TX (transmitq).
  - `virtio_net_hdr` prepended to each frame (flags, gso_type; zeroed for the
    basic path — no checksum/GSO offload initially).
  - **Polled RX**: pre-post RX buffers and drain the RX used-ring by polling. The
    current VirtIO transport disables MSI-X for every virtqueue (`virtio/mod.rs`)
    and no PCI INTx/ISR path exists, so polling is the only option (and matches the
    block driver's polled completion). Interrupt-driven RX is explicitly deferred.
- Net device registry (names → `&dyn NetDevice`), mirroring the block registry.
- xtask: attach a NIC to QEMU (`-netdev user,id=n0 -device virtio-net-pci,netdev=n0`).

**Module**: `kernel/src/drivers/virtio/net.rs`, `kernel/src/net/device.rs`

**Acceptance criteria**:
- [x] VirtIO-net device discovered via PCI and initialised through the handshake
- [x] MAC address and MTU read correctly from device config
- [x] A frame can be transmitted (ARP request → gateway ARP reply received back)
- [x] Frames can be received into pre-posted RX buffers (polled RX)
- [x] `NetError` variants propagated (too large; device/queue errors)

---

### Sub-phase 4.1 — Kernel net service (frame bridge) ✅ COMPLETE

**Implementation notes** (commits `844b31c`, `ef242ed`): `SYS_UPTIME_MS` (14) added
(`tick_count × 10`). `kernel/src/net/net_service.rs`: a kernel task on an IPC
endpoint; `MSG_POLL` drains all frames from the driver's RX ring into a bounded
service queue (recycling driver buffers), returns one frame in the RX shared
region plus the current time; `MSG_TX_FRAME` transmits a frame staged in the TX
shared region. Pull-based (ring-3 initiates, kernel replies) → deadlock-free with
the synchronous-rendezvous IPC. `test_net_service` plays the ring-3 server over
real IPC (stage ARP → `MSG_TX_FRAME` → `MSG_POLL` until the gateway reply, checking
the monotonic timestamp). 27 tests pass. **Deviations from plan (justified):**
(1) **Single-instance** rather than the block_server slot-claim pattern — Phase 4
has exactly one NIC; config is published once and read at task startup. Adopt the
slot pattern if multiple NICs ever appear. (2) **RX drained at `MSG_POLL` only**,
not also on the timer tick — the net server polls continuously, so the driver's
16-buffer ring + the service queue absorb bursts; a timer-tick drain would do NIC
MMIO in IRQ context for no benefit. (3) The service RX-queue **overflow drop+count**
logic is correct by construction but not stress-tested (generating 64+ queued
frames isn't feasible in the harness); the counter feeds `ifconfig` in 4.4.

**Goal**: Bridge the in-kernel driver to a ring-3 net server over IPC + shared
memory, delivering RX frames asynchronously and forwarding TX frames.

**Rationale**: Ring-3 servers cannot touch the NIC directly. The net service is the
`block_server` analogue for networking. RX is unsolicited, but the kernel IPC is
synchronous rendezvous only — there is no non-blocking send or notification, so a
kernel task **cannot** push to a ring-3 server that isn't currently parked in
`ipc_receive`, and a single task that both pushes RX and serves TX would mutually
deadlock. Therefore the model is **pull-based**: the ring-3 net server always
initiates (like every `block_server` client), and the service always replies —
RX frames are buffered in the kernel and handed back one per `MSG_POLL`.

**What to build**:
- Net service kernel task on a dedicated IPC endpoint. Like the refactored
  `block_server` (which uses a `CLAIM`/`INSTANCE_COUNT` slot claimed by the bare
  `sched::spawn`ed task), it reads its config from a published slot at startup.
- RX buffering: drain the NIC's RX used-ring (on the 100 Hz timer tick and at the
  top of each `MSG_POLL`) into a small ring of RX shared-region slots. Overflow
  drops the oldest slot and increments a counter (exposed for `ifconfig`).
- `MSG_POLL` handler: pop one RX slot (or report none) and return the current
  time; reply. `MSG_TX_FRAME` handler: transmit via `NetDevice`, reply status.
- Shared regions (RX ring + TX) allocated and mapped into the net server.
- `SYS_UPTIME_MS`: a monotonic milliseconds syscall (`tick_count() × 10`).
- **Deadlock argument (document it here)**: only the ring-3 server ever initiates
  IPC to the service; the service only ever replies. No cycle is possible.

**Module**: `kernel/src/net/net_service.rs`, `kernel/src/arch/x86_64/syscall.rs`

**Acceptance criteria**:
- [x] Net service starts and reads its config at task startup (single-instance)
- [x] `MSG_POLL` returns a buffered RX frame (or "none") plus a monotonic time
- [x] A frame sent via `MSG_TX_FRAME` is transmitted by the driver
- [~] RX queue overflow drops oldest and counts (logic correct by construction;
      not stress-tested — see notes)
- [x] `SYS_UPTIME_MS` / `MSG_POLL` timestamp is monotonically non-decreasing
- [x] The server↔service loop never deadlocks (ring-3 always initiates)

---

### Sub-phase 4.2 — Net server + smoltcp integration ✅ COMPLETE

**Implementation notes** (commits `79b401b` gate, `865a4dc` Device, `eabae91` boot+test):
net-server crate on smoltcp 0.12 (compile gate: builds for x86_64 AND aarch64).
`IpcDevice` implements smoltcp's `phy::Device` over the net service's
`MSG_POLL`/`MSG_TX_FRAME` (device.rs); `server_main` builds the Interface (MAC via
`BootInfo.arg1`, static IP 10.0.2.15/24 for now) + SocketSet and runs the poll
loop with the clock from `SYS_UPTIME_MS`. libthemelios gained `uptime_ms()` +
`net_proto`. `net::boot_net()` spawns the NIC + service + net server at boot
(prints "virtio-net0 up"). `test_net_server_stack` plays the service, injects an
ARP request, and verifies the real ring-3 smoltcp stack replies correctly. 28
tests pass; live boot spawns the net server with no fault. **NOTE:** finishing
4.2 surfaced a pre-existing intermittent kernel double-fault (KERNEL_GS_BASE not
re-pointed on switches through zero-stack tasks) — fixed in `b1b7cea` (verified
18/18 clean runs).

**Goal**: Stand up the ring-3 net server and drive smoltcp's poll loop over the
frame IPC transport.

**Pre-requisite (compile gate) ✅ PASSED** (commit `79b401b`): smoltcp **0.12.0**
builds clean for both `x86_64-unknown-none` and `aarch64-unknown-none` with the
feature set below; `net-server` crate created and linked into a flat binary.
Remaining 4.2: Device trait over the net-service IPC, `Interface`/`SocketSet`,
poll loop, and xtask/kernel embed.

**Pre-requisite (compile gate)**: Verify `smoltcp` compiles for **both**
`x86_64-unknown-none` and `aarch64-unknown-none` on the pinned nightly with
`-Zbuild-std=core,alloc`, `default-features = false`, features
`["alloc", "medium-ethernet", "proto-ipv4", "proto-dhcpv4", "socket-udp",
"socket-tcp", "socket-dhcpv4", "socket-icmp"]`. This checks **smoltcp as an
isolated dependency** (the exact miniz_oxide precedent — Phase 3 verified the
library built bare-metal, not that a whole server did). The net-server binary
itself builds only for x86_64 in Phase 4, because `libthemelios`'s syscall
wrappers are raw x86 `syscall` asm; its aarch64 build is Phase 7. If a smoltcp
feature pulls in `std`, resolve before proceeding.

**What to build**:
- `servers/net-server` crate (flat binary, embedded, spawned in ring 3 via
  `spawn_server`/`ServerConfig`).
- Implement smoltcp's `Device`/`RxToken`/`TxToken` over the frame IPC transport.
  Note the smoltcp contract: `Device::receive()` returns **both** an `RxToken`
  (the received frame) **and** a `TxToken` (a received packet may need an
  immediate reply); tokens are consumed synchronously *inside* `poll()`, so a
  blocking `ipc_call(MSG_TX_FRAME)` inside `TxToken::consume` is legal in ring 3
  and composes cleanly. The single TX shared slot serialises multiple transmits
  within one poll (fine — poll is single-threaded).
- Construct `Interface` (with the NIC's MAC) and a `SocketSet`.
- Event loop (**pull**): `ipc_call(MSG_POLL)` → if a frame came back, feed it to
  the Device; set smoltcp's clock from the returned time (or `SYS_UPTIME_MS`);
  call `iface.poll()`; `yield_now()` when idle so the loop doesn't spin hot.
- Panic handler / bounded socket set.

**Module**: `servers/net-server/src/main.rs`, `servers/libthemelios` (net types)

**Acceptance criteria**:
- [ ] smoltcp (isolated) builds for x86_64-unknown-none AND aarch64-unknown-none
- [ ] Net server boots in ring 3 and initialises a smoltcp `Interface`
- [ ] The Device trait moves a frame in and out through the `MSG_POLL`/`MSG_TX` IPC
- [ ] `iface.poll()` runs on polled RX and idle without faulting
- [ ] A server panic does not affect the kernel or other servers

---

### Sub-phase 4.3 — Ethernet / ARP / IPv4 / ICMP bring-up ✅ CORE COMPLETE

**Implementation notes** (commit `29c8b41`): the ring-3 smoltcp stack now handles
Ethernet/ARP/IPv4/ICMP — it answers a ping to its own IP with a correct echo
reply (smoltcp auto-replies at the interface level, so the only net-server change
was adding the default IPv4 route via 10.0.2.2). `test_net_icmp_echo` (plays the
service) seeds the neighbour cache with an ARP request, injects a checksum-valid
ICMP echo request, and verifies the echo reply. 29 tests pass; 5/5 clean runs.
**Deferred to 4.5** (with justification): the interactive `ping <ip>` shell command
and outbound round-trip result. Both need the net server to serve *client*
requests while polling smoltcp — a non-blocking `ipc_try_receive` + a client
request/reply path — and that machinery is shared with the socket API, so it
belongs in 4.5. (Also: slirp's ICMP proxy for outbound round-trips is best-effort
and untestable in CI, per the acceptance notes.)

**Goal**: With a static IP, resolve ARP and answer/emit ICMP echo (ping).

**Rationale**: Proves L2/L3 end-to-end before layering DHCP and sockets on top. ARP
and ping are the simplest round-trips and isolate driver/transport bugs from
socket-API bugs.

**What to build**:
- Configure the smoltcp interface with a static address matching QEMU's slirp
  network (guest `10.0.2.15/24`, gateway `10.0.2.2`).
- Enable ICMP handling (smoltcp answers echo requests; an ICMP socket for
  outbound ping).
- Shell `ping <ip>` command (drives an ICMP echo via the net server).

**Module**: `servers/net-server/src/main.rs`, `kernel/src/shell/commands.rs`

**Acceptance criteria**:
- [ ] Guest resolves the gateway via ARP and populates its neighbour cache
      (deterministic — the primary L2/L3 assertion)
- [ ] Guest→gateway (`10.0.2.2`) ICMP echo round-trips (**best-effort**: slirp's
      ICMP proxy needs host raw-socket/ping privileges that CI may lack; do not
      gate the phase on inbound host→guest ping — `hostfwd` forwards TCP/UDP only)
- [ ] `ping` shell command reports round-trip success/failure
- [ ] (Full deterministic round-trip liveness is asserted in 4.5 via UDP echo)

---

### Sub-phase 4.4 — DHCPv4 client

**Goal**: Acquire the interface address/gateway/DNS dynamically via DHCP.

**Rationale**: Cloud and K8s nodes are configured by DHCP, not static IPs. QEMU's
slirp includes a DHCP server, so this is testable out of the box.

**What to build**:
- Add smoltcp's `dhcpv4::Socket` to the socket set; apply the acquired config to
  the interface (address, default gateway). DNS server addresses from the offer
  are **stored for display only** — name resolution (a DNS resolver) is out of
  scope for Phase 4.
- Handle lease renewal and reconfiguration events.
- Shell `ifconfig` command shows the current address, gateway, DNS servers, MAC,
  MTU, and the RX-overflow counter.

**Module**: `servers/net-server/src/main.rs`, `kernel/src/shell/commands.rs`

**Acceptance criteria**:
- [ ] Guest acquires `10.0.2.15` (or slirp-assigned) via DHCP at boot
- [ ] Gateway and DNS are configured from the DHCP offer
- [ ] `ifconfig` shows the acquired configuration
- [ ] Lease renewal does not wedge the stack

---

### Sub-phase 4.5 — UDP sockets, `CapType::Socket`, and syscalls

**Goal**: A userspace process sends and receives UDP datagrams through a
capability-checked socket API.

**Rationale**: UDP is the simpler transport (no connection state), so it validates
the whole socket-API + capability + IPC-routing path before TCP adds lifecycle
complexity. Establishes the kernel's role as a capability-checking router.

**What to build**:
- `CapType::Socket { socket_id }` (rights: SEND, RECV) and a socket routing table
  (socket_id → net server endpoint), analogous to the VFS mount table.
- Socket dispatch in the kernel: check capability → forward to net server via IPC
  → copy payloads via shared memory → audit-log.
- New syscalls (continue after `SYS_UPTIME_MS` = 14, added in 4.1):
  - `SYS_SOCKET (15)`: type → socket capability
  - `SYS_BIND (16)`, `SYS_SENDTO (17)`, `SYS_RECVFROM (18)`, `SYS_SOCK_CLOSE (19)`
- Net server: create/destroy smoltcp UDP sockets, map to `socket_id`s.
- User-pointer validation and per-transfer size caps (reuse the Phase 3 helpers).
- `AuditOp::NetAccess` variants for socket operations.
- Shell `udpsend <ip> <port> <msg>` for manual testing.

**Module**: `kernel/src/net/mod.rs`, `kernel/src/arch/x86_64/syscall.rs`,
`kernel/src/cap/`, `servers/net-server/src/main.rs`

**Acceptance criteria**:
- [ ] Process with a `Socket` capability can create/bind/send/recv UDP
- [ ] Process WITHOUT the capability gets `PermissionDenied` on `SYS_SOCKET`
- [ ] UDP echo against a host listener round-trips correct bytes
- [ ] Kernel routes to the net server and never parses packet data
- [ ] Bad user pointers return an error, not a kernel fault
- [ ] Audit log records socket operations with PID, op, result

---

### Sub-phase 4.6 — TCP sockets

**Goal**: Establish outbound TCP connections and accept inbound ones, exchanging
stream data.

**Rationale**: TCP is what real workloads use. This exercises smoltcp's connection
state machine and the socket API's connection lifecycle (connect/listen/accept)
and backpressure.

**What to build**:
- Extend the socket API: `SYS_CONNECT (19)`, `SYS_LISTEN (20)`,
  `SYS_ACCEPT (21)`, `SYS_SEND (22)`, `SYS_RECV (23)`.
- Net server: smoltcp TCP sockets; the net server returns a new `socket_id` for an
  accepted connection, and the **kernel** mints the per-connection `Socket`
  capability from that reply (mirroring how `vfs_open` mints a `FileDescriptor`
  cap and `vfs_close` revokes it via `cspace`) — the server never fabricates caps.
- Handle `WouldBlock` semantics (poll-based, non-blocking sockets first;
  blocking wrappers can come later) and half-close.
- **Note (busy-poll)**: with non-blocking sockets and no socket readiness
  wait/wake, a client that wants to block on `recv` busy-polls via `yield_now`
  (the same pattern as `fs::boot_storage`'s yield loop). A readiness/wake
  mechanism is deferred; acknowledge the CPU cost.
- Shell `tcpconnect <ip> <port>` (connect, send a line, print the reply).

**Module**: `kernel/src/net/mod.rs`, `kernel/src/arch/x86_64/syscall.rs`,
`servers/net-server/src/main.rs`

**Acceptance criteria**:
- [ ] Guest connects out to a host TCP server and exchanges data both ways
- [ ] Guest listens, accepts an inbound connection, and echoes data
- [ ] `accept` returns a distinct per-connection socket capability
- [ ] Connection reset / refused surfaced as `SockError`, not a hang
- [ ] Data integrity verified over a multi-segment transfer

---

### Sub-phase 4.7 — Shell, boot integration, and tests

**Goal**: Boot brings the network up automatically; the shell exposes it; the full
stack is covered by integration tests; the arm64 compile gate is wired into CI.

**What to build**:
- Boot sequence: discover the NIC, start the net service, spawn the net server,
  run DHCP, print the acquired configuration.
- Shell: `ifconfig`, `sockets` (list open sockets/state), `ping`, `udpsend`,
  `tcpconnect`.
- xtask: QEMU `-netdev user` with `hostfwd`/`guestfwd` for tests; a host-side
  echo endpoint (or slirp services) the guest talks to.
- CI: add an `aarch64-unknown-none` build job that compiles **smoltcp alone**
  (with our feature set), proving we haven't taken an amd64-only dependency. The
  kernel and net-server aarch64 builds come in Phase 7 (they need aarch64 arch
  code and aarch64 syscall stubs in `libthemelios`).
- Integration tests:
  - `test_virtio_net`: driver TX/RX round-trip
  - `test_net_service`: frame delivered to a stub server, TX transmitted
  - `test_arp_icmp`: ARP resolve + ICMP echo
  - `test_dhcp`: address acquired
  - `test_udp_echo`: UDP send/recv round-trip
  - `test_socket_capability`: cap grant allows, absence denies
  - `test_tcp_client`: outbound connect + data exchange
  - `test_tcp_server`: listen/accept + echo
  - `test_net_server_crash_isolation`: crash the net server, kernel + others fine

**Module**: `kernel/src/main.rs`, `kernel/src/shell/commands.rs`,
`kernel/src/test_runner.rs`, `xtask/src/main.rs`, `.github/workflows/build.yml`

**Acceptance criteria**:
- [ ] Kernel boots, brings up the NIC, DHCP-configures, prints the config
- [ ] `ifconfig`, `sockets`, `ping`, `udpsend`, `tcpconnect` work live
- [ ] All new integration tests pass; all Phase 1–3 tests still pass
- [ ] `cargo xtask test` passes end-to-end with a NIC attached
- [ ] aarch64 compile job is green in CI
- [ ] Net server crash test: server dies, kernel logs it, other servers unaffected

---

## Dependencies

| Crate | Used by | Version | Purpose | no_std? |
|-------|---------|---------|---------|---------|
| smoltcp | net-server | latest | Ethernet/ARP/IPv4/ICMP/UDP/TCP + DHCPv4 | Yes (`alloc`) — **verify bare-metal build for x86_64 AND aarch64 before 4.2** |

## Host Tool Dependencies

None new. QEMU's built-in user-mode networking (slirp) provides DHCP, a gateway,
and DNS with no host configuration. `hostfwd`/`guestfwd` enable inbound/outbound
test traffic. (Per the project convention, if any new host tool is introduced it
goes in the `Brewfile`, `dev-setup.md`, and `CLAUDE.md` — none is anticipated.)

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| smoltcp doesn't build bare-metal (esp. aarch64) | Low | High | Compile gate (smoltcp alone) in 4.2 before any integration; smoltcp is widely used no_std incl. on ARM. |
| RX delivery requires an IPC mechanism the kernel lacks | Resolved by design | High | The kernel IPC is synchronous rendezvous only (no push/notification). **Resolved** by the pull model (4.1): ring-3 server always initiates `MSG_POLL`, kernel always replies — deadlock-free with existing primitives. No new IPC primitive needed. |
| RX buffering (bursts, overflow) is subtle | Medium | Medium | Bounded RX ring with explicit drop-oldest + counter (4.1); tested with a stub server before smoltcp. |
| smoltcp poll/token integration | Low | Medium | The `Rx`/`Tx` token model composes with a blocking `ipc_call` inside `poll()` (consumed synchronously); 4.2 validates. Lower risk than first assumed. |
| VirtIO-net header / offload subtleties | Medium | Medium | Start with no checksum/GSO offload (zeroed `virtio_net_hdr`); add offload later if needed. |
| TCP backpressure / WouldBlock semantics over IPC | Medium | Medium | Non-blocking sockets first; explicit `WouldBlock`; blocking wrappers deferred. |
| Testing inbound TCP reliably in CI (slirp quirks) | Medium | Low | Use guest-initiated flows where possible; hostfwd for inbound; keep timing-sensitive assertions generous. |
| Socket capability lifecycle (accept mints caps, close revokes) | Medium | Medium | Model on the Phase 3 FileDescriptor capability lifecycle; per-connection caps; revoke on close. |
| Phase scope (8 sub-phases, whole new stack) | Medium | Medium | Natural off-ramp at 4.4 (link + DHCP + ping). UDP and TCP are additive layers. |

## Estimated Effort

| Sub-phase | Estimated commits | Complexity |
|-----------|------------------|------------|
| 4.0 VirtIO-net + NetDevice | 4 | Medium-High |
| 4.1 Kernel net service | 4 | High (pull-based RX buffering) |
| 4.2 Net server + smoltcp | 5 | High |
| 4.3 Ethernet/ARP/IPv4/ICMP | 3 | Medium |
| 4.4 DHCPv4 | 2 | Low-Medium |
| 4.5 UDP + CapType::Socket + syscalls | 6 | High |
| 4.6 TCP sockets | 5 | High |
| 4.7 Shell + boot + tests | 6 | Medium |
| **Total** | **~35** | |

## Verification Checklist

- [ ] VirtIO-net driver transmits and receives frames
- [ ] `NetDevice` trait cleanly isolates the bus (arm64 seam validated by compile gate)
- [ ] Kernel net service bridges driver ↔ ring-3 server with pull-based, polled RX
- [ ] `SYS_UPTIME_MS` provides a monotonic clock for the stack
- [ ] smoltcp builds and runs in ring 3; builds for aarch64 too
- [ ] ARP resolution and ICMP echo (ping) work
- [ ] DHCP acquires address, gateway, DNS (DNS stored for display only)
- [ ] UDP datagrams round-trip through the capability socket API
- [ ] TCP connect/listen/accept/send/recv work with data integrity
- [ ] Socket capabilities enforce access control; absence denies
- [ ] Kernel never parses packets — only routes IPC and checks capabilities
- [ ] User-pointer validation prevents kernel faults from bad socket buffers
- [ ] Audit log captures socket operations
- [ ] Net server crash does not affect the kernel or other servers
- [ ] `cargo xtask test` passes all new and existing tests with a NIC attached
- [ ] aarch64 compile gate green in CI
- [ ] No regressions in Phase 1–3 tests
- [ ] milestones.md and CLAUDE.md updated
- [ ] **Post-phase**: network architecture documented in mdbook
```
