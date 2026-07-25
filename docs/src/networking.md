# Network Architecture

This document describes ThemeliOS's network stack — the VirtIO-net driver, the
kernel net service, the ring-3 TCP/IP stack, and the capability-guarded socket
API that connects a userspace process to the outside world. It reflects the
system as built in Phase 4.

> **Status**: Implemented in Phase 4 (amd64; arm64-ready by design).

## The core idea: a hybrid microkernel

TCP/IP stacks are, alongside filesystem parsers, the most exploited code in a
monolithic kernel. IP fragment reassembly, TCP option parsing, out-of-order
segment handling, DHCP option walking — all of it consumes untrusted bytes off
the wire, and all of it historically runs with full kernel privilege. A single
bug becomes a kernel compromise.

ThemeliOS splits the network stack across the privilege boundary, exactly as the
[storage stack](./storage.md) splits filesystems:

- **The VirtIO-net driver stays in the kernel (ring 0).** It is thin, talks only
  to trusted, emulated VirtIO hardware, does the DMA, and exposes a single
  abstraction: send this Ethernet frame / here is a received one.
- **The entire TCP/IP stack runs in userspace (ring 3)**, in a single **net
  server** process built on [smoltcp](https://github.com/smoltcp-rs/smoltcp).
  Ethernet, ARP, IPv4, ICMP, UDP, TCP, and the DHCPv4 client all parse untrusted
  network data entirely outside the kernel.

A malformed or malicious packet can, at worst, crash the net server that parses
it. It cannot touch kernel memory, cannot read another process's data, and cannot
bypass a socket capability check — because the code doing the parsing never had
those privileges to begin with.

```
┌───────────────────────────────────────────────────────────────┐
│                        Ring 0 (Kernel)                        │
│                                                               │
│   PCI scan ─▶ VirtIO-net driver ─▶ NetDevice trait            │
│                                        │                      │
│                                  Net service                  │
│                                  (kernel task on an           │
│                                   IPC endpoint; drains        │
│                                   RX, forwards TX)            │
│                                        ▲                      │
│         Socket dispatch                │ IPC + shared memory   │
│    (routes SYS_SOCKET/SENDTO/…)        │  (pull-based frames)  │
│                   │                    │                      │
├───────────────────┼─────────────────────┼──────────────────────┤
│                   │  Ring 3 (Userspace) │                      │
│                   ▼                    ▼                      │
│              ┌──────────────────────────────────┐             │
│              │            net-server            │             │
│              │  smoltcp: Ethernet/ARP/IPv4/     │             │
│              │  ICMP/UDP/TCP + DHCPv4 client    │             │
│              │  Device-over-IPC frame transport │             │
│              │  Socket table (UDP/TCP/ICMP)     │             │
│              └──────────────────────────────────┘             │
└───────────────────────────────────────────────────────────────┘
```

Everything above the driver is a message. The kernel is a *router*, not a
protocol implementor — it validates capabilities and moves bytes, but never
parses an Ethernet header, an IP option, or a TCP segment.

## The frame bridge: pull-based by necessity

Ring-3 servers cannot touch the NIC directly, so the kernel **net service**
bridges the in-kernel driver to the ring-3 stack over IPC and shared memory.

The subtlety is that received frames arrive **unsolicited** at the NIC, but the
kernel's IPC is *synchronous rendezvous only* — there is no non-blocking send and
no notification primitive, so a kernel task cannot push a frame to a ring-3 server
that is not currently parked in `receive`. A single task that both pushed RX and
served TX would deadlock.

The bridge is therefore **pull-based**: the ring-3 net server always initiates and
the kernel service always replies. This is deadlock-free by construction — there
is never an unsolicited kernel→ring-3 send.

The net server's event loop, once per iteration:

1. `MSG_POLL` — "give me the next RX frame and the current time." The service
   pops one frame from its RX ring (or reports none) and returns the monotonic
   clock ([`SYS_UPTIME_MS`](#the-clock), for smoltcp's timers).
2. Feed any frame to smoltcp and run `iface.poll()`.
3. For each frame smoltcp wants to send, `MSG_TX_FRAME` — the service transmits
   it via the driver.
4. Service the DHCP client and any pending socket request, then `yield`.

Frame bytes travel through shared memory regions (an RX ring and a single TX
slot); the four IPC words carry only lengths and status. The service drains the
NIC's RX virtqueue into the RX ring on each `MSG_POLL`; overflow drops the oldest
frame and bumps a counter surfaced by `ifconfig`.

> **Polled RX, no interrupts.** The VirtIO transport disables MSI-X and there is
> no PCI INTx/ISR path, so RX is polled — the net server's continuous poll loop
> plus the driver's pre-posted buffer ring absorb bursts. Interrupt-driven RX is
> deferred (it would need INTx discovery or an IO-APIC + MSI-X).

## The clock

smoltcp's poll loop needs a monotonic clock for retransmit and DHCP timers. The
kernel already maintains a 100 Hz tick; it is exposed as `SYS_UPTIME_MS`
(`ticks × 10`), which the net server reads on every poll. No periodic
kernel→server tick message is needed — the server pulls the time whenever it
polls.

## Addressing: DHCP

On a live boot the net server runs smoltcp's `dhcpv4::Socket` and applies each
lease to the interface (address, prefix, default route). It reports every
acquired, renewed, or lost lease to the kernel over a `MSG_CONFIG` message so the
`ifconfig` shell command can display the live configuration — the kernel only
*records* what the server tells it. DNS server addresses from the offer are
captured for display but there is no resolver in Phase 4.

DHCP is gated behind a boot-argument flag (`NET_ARG_DHCP`, packed alongside the
NIC's MAC in `arg1`): the live node sets it, while the deterministic static-IP
round-trip tests spawn the server without it and keep a fixed `10.0.2.15`.

## The socket API: capability-checked, kernel-routed

Sockets follow the same pattern as the [VFS](./storage.md): a new
[`CapType::Socket`](./capabilities.md), with two roles.

- The **network authority** (`socket_id == SOCKET_FACTORY`) is the right to
  *create* sockets. `SYS_SOCKET` requires the caller to present it — a process
  without it cannot make a socket at all.
- A **per-socket capability** is minted by `SYS_SOCKET` (or by `accept`, for an
  inbound connection) and consumed by the send/recv/close syscalls. `WRITE`
  grants send, `READ` grants receive. Closing a socket revokes its capability,
  exactly as closing a file descriptor does.

The data path mirrors the VFS too: the payload region is shared between the
**kernel and the net server**, not the client process. The kernel copies
validated user bytes into it and reads results out; a client never shares memory
with the server directly, and the kernel mediates and audit-logs every transfer
(`AuditOp::NetAccess`). **The kernel never parses packet data** — it checks a
capability, forwards an `OP_SOCK_*` request, and moves bytes.

### Transports

| Transport | Syscalls | Notes |
|-----------|----------|-------|
| **UDP**   | `socket`, `bind`, `sendto`, `recvfrom`, `close` | Connectionless datagrams. |
| **TCP**   | `connect`, `listen`, `accept`, `tcp_send`, `tcp_recv`, `close` | Non-blocking; a `WouldBlock`/`ConnectionRefused` state machine. `accept` promotes smoltcp's listening socket into the connection and re-arms a fresh listener; the **kernel** mints the per-connection capability from the accept reply. |
| **ICMP**  | `ping` (shell) | An echo socket bound to an identifier; backs the `ping` command. |

Sockets are **non-blocking**. With no socket readiness wait/wake mechanism yet, a
caller that wants to block busy-polls with `yield` (the same pattern the storage
boot path uses). A readiness/wake primitive is deferred.

## Shell commands

The debug shell exposes the stack for inspection and manual testing. These run in
the kernel and use kernel-internal socket helpers (the kernel is trusted);
userspace must go through the capability-checked syscalls.

| Command | Description |
|---------|-------------|
| `ifconfig` | NIC MAC/MTU, the acquired IPv4 address/gateway/DNS, and the RX-drop counter. |
| `sockets` | Lists the net server's open sockets: id, kind, TCP state, bound port, and connected peer. |
| `ping <ip> [n]` | Sends `n` ICMP echo requests (default 4) and reports replies. |
| `udpsend <ip> <port> <msg>` | Sends one UDP datagram. |
| `tcpconnect <ip> <port>` | Opens a TCP connection, sends a line, prints the reply. |

The `sockets` listing is itself a round-trip: the kernel asks the net server to
serialise its socket table into the shared region (`OP_SOCK_LIST`) and decodes
the entries — the socket state lives entirely in the ring-3 server.

## The arm64 seam

Phase 4 runs and is tested only on amd64, but the stack is designed to port
unchanged:

- The **`NetDevice` trait** is the bus/arch seam. VirtIO-net implements it today;
  the Phase 7 arm64 port implements device discovery for `virtio-mmio`/ECAM
  behind the same trait without touching the stack or the socket API.
- The **TCP/IP stack is architecture-independent**. To keep it honest, CI
  compiles **smoltcp alone** — with the net server's exact feature set — for
  `aarch64-unknown-none` (`cargo xtask arm64-gate`, the `servers/smoltcp-gate`
  crate). If a dependency ever pulled in amd64-only or `std` code, that job goes
  red long before the arm64 port would trip over it. This is the same
  "compile the library bare-metal" check Phase 3 used for miniz_oxide.

The net server binary itself builds only for x86_64 in Phase 4 because its
`libthemelios` syscall wrappers are raw x86 `syscall` assembly; the aarch64 build
of the kernel and servers is Phase 7 work.

## What is contained, and what is deferred

**Contained by the ring-3 boundary**: the whole smoltcp stack, all protocol
parsing, and the DHCP client. A bug there is a net-server crash, not a kernel
compromise — the same containment already accepted for miniz_oxide in the
SquashFS server.

**Deferred** (with rationale):

- **Interrupt-driven RX** — needs INTx/ISR or IO-APIC + MSI-X; polled RX suffices
  for now.
- **A DNS resolver** — DNS server addresses are captured and displayed, but name
  resolution is out of scope for Phase 4.
- **Socket readiness wait/wake** — callers busy-poll with `yield`; a blocking
  wrapper and a wake primitive come later.
- **Per-client payload regions** — a single kernel↔server region serves one
  request at a time in practice, matching the FS servers.
- **A net-server crash-isolation test** — the ring-3 containment is structural
  (identical to the FS servers) and exercised implicitly whenever a server is
  spawned; a dedicated fault-injection test is deferred rather than adding an
  artificial panic path.

## Where the code lives

| Component | Location |
|-----------|----------|
| VirtIO-net driver | `kernel/src/drivers/virtio/net.rs` |
| `NetDevice` trait + registry | `kernel/src/net/device.rs` |
| Kernel net service (frame bridge) | `kernel/src/net/net_service.rs` |
| Socket router (capability checks) | `kernel/src/net/socket.rs` |
| Boot integration | `kernel/src/net/mod.rs` (`boot_net`) |
| Socket syscalls | `kernel/src/arch/x86_64/syscall.rs` |
| Ring-3 TCP/IP stack | `servers/net-server/src/main.rs` |
| Frame/socket IPC protocol | `servers/libthemelios/src/net_proto.rs` |
| arm64 compile gate | `servers/smoltcp-gate/` |
| Shell commands | `kernel/src/shell/commands.rs` |
