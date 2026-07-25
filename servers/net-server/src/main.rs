//! # Net server — ring-3 TCP/IP stack (Phase 4)
//!
//! The userspace network stack. The kernel exposes only a thin VirtIO-net driver
//! and a pull-based frame bridge (the net service, sub-phase 4.1); this server
//! runs the whole protocol stack — Ethernet, ARP, IPv4, ICMP, UDP, TCP, and a
//! DHCPv4 client — in ring 3, on top of [`smoltcp`]. A malformed packet can at
//! worst crash this server, never the kernel.
//!
//! ## Structure
//!
//! - [`device`] — a smoltcp `Device` over the net service's `MSG_POLL` /
//!   `MSG_TX_FRAME` IPC (sub-phase 4.2).
//! - `server_main` — builds the smoltcp `Interface` + `SocketSet` and runs the
//!   poll loop, using `SYS_UPTIME_MS` for smoltcp's clock.
//!
//! Sockets (UDP/TCP) and their capability API are wired in sub-phases 4.5/4.6;
//! DHCP configuration in 4.4. For now the poll loop stands the stack up and
//! processes ARP/ICMP, keeping smoltcp's timers live.

#![no_std]
#![no_main]

extern crate alloc;

mod device;

use alloc::vec;
use alloc::vec::Vec;

use libthemelios::net_proto::{
    pack_ip_port, pack_ipv4, unpack_ip_port, MSG_CONFIG, OP_SOCK_BIND, OP_SOCK_CLOSE,
    OP_SOCK_CONNECT, OP_SOCK_OPEN, OP_SOCK_RECV, OP_SOCK_SEND, SOCKET_REGION_BYTES,
    SOCKET_REGION_VADDR, SOCK_ERR, SOCK_OK, SOCK_REFUSED, SOCK_TYPE_TCP, SOCK_TYPE_UDP,
    SOCK_WOULDBLOCK,
};
use libthemelios::{boot_info, ipc, syscall, IpcMessage};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::{dhcpv4, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address,
};

use device::IpcDevice;

/// Maximum Ethernet frame smoltcp may build (1500 MTU + 14-byte L2 header).
const MTU: usize = 1514;

/// `BootInfo.arg1` flag bit requesting the DHCPv4 client (set by the kernel's
/// `boot_net`). The MAC occupies the low 48 bits of `arg1`; bit 48 selects DHCP
/// over the static fallback. Mirrors `kernel::net::NET_ARG_DHCP` — kept in sync
/// by hand, since a ring-3 server cannot depend on kernel crates.
const NET_ARG_DHCP: u64 = 1 << 48;

/// Mask selecting the MAC bytes out of `arg1` (the low 48 bits).
const MAC_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Static fallback address, used when DHCP is not requested (the two static-IP
/// round-trip tests). Matches QEMU's user-mode network.
const STATIC_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
const STATIC_PREFIX: u8 = 24;
const STATIC_GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);

/// The server entry point, called by `libthemelios::_start` after the heap is
/// initialised. Builds the stack and runs the poll loop forever.
#[no_mangle]
pub extern "C" fn server_main() -> ! {
    let info = boot_info();

    // The kernel packs the NIC's MAC into the low 48 bits of arg1; the high bits
    // carry flags (bit 48 = NET_ARG_DHCP). Mask the MAC out before use.
    let m = info.arg1 & MAC_MASK;
    let mac = [
        m as u8,
        (m >> 8) as u8,
        (m >> 16) as u8,
        (m >> 24) as u8,
        (m >> 32) as u8,
        (m >> 40) as u8,
    ];

    // Whether to configure via DHCP (live boot) or a static IP (round-trip tests).
    let use_dhcp = (info.arg1 & NET_ARG_DHCP) != 0;

    // The net-service endpoint. As well as MSG_POLL / MSG_TX_FRAME (the frame
    // bridge), we send MSG_CONFIG here to report the acquired IP configuration.
    let service_ep = info.arg0;

    // Build the smoltcp Device over the net-service frame bridge:
    //   arg0                  = net-service endpoint (MSG_POLL / MSG_TX_FRAME)
    //   shared region         = RX frames (service -> us)
    //   client-shared region  = TX frames (us -> service)
    let mut dev = IpcDevice::new(
        service_ep,
        info.shared_vaddr as *mut u8,
        info.shared_size as usize,
        info.client_shared_vaddr as *mut u8,
        info.client_shared_size as usize,
        MTU,
    );

    // smoltcp's monotonic clock, from the kernel's uptime syscall.
    let now = || Instant::from_millis(syscall::uptime_ms() as i64);

    let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    let mut iface = Interface::new(config, &mut dev, now());

    // Owned socket storage, so the set (and its UDP sockets, whose buffers are
    // heap-allocated Vecs) is `'static`.
    let mut sockets: SocketSet<'static> = SocketSet::new(Vec::new());

    // The kernel socket router (Phase 4.5) sends OP_SOCK_* requests to our
    // request endpoint; `socket_table` maps a kernel-facing socket id to the
    // smoltcp socket handle, and `next_socket_id` hands out fresh ids.
    let socket_ep = info.fs_endpoint;
    let mut socket_table: Vec<SockEntry> = Vec::new();
    let mut next_socket_id: u64 = 0;

    // A DHCP socket is added only in DHCP mode. In static mode the interface is
    // configured up-front and the socket set stays empty (the two static-IP
    // round-trip tests depend on the server answering ARP/ICMP for 10.0.2.15).
    let dhcp_handle = if use_dhcp {
        Some(sockets.add(dhcpv4::Socket::new()))
    } else {
        // Static fallback: address + default route, exactly as before 4.4. With
        // an address configured, smoltcp answers ARP for it — what the 4.2/4.3
        // round-trip tests exercise.
        let o = STATIC_IP.octets();
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(
                IpAddress::v4(o[0], o[1], o[2], o[3]),
                STATIC_PREFIX,
            ));
        });
        let _ = iface.routes_mut().add_default_ipv4_route(STATIC_GATEWAY);
        None
    };

    loop {
        // Serve one pending socket request from the kernel router (non-blocking,
        // so the poll loop keeps running). One per iteration is enough — requests
        // drain over successive iterations, interleaved with smoltcp polling.
        if let Some(req) = syscall::try_receive(socket_ep) {
            serve_socket(
                &mut iface,
                &mut sockets,
                &mut socket_table,
                &mut next_socket_id,
                socket_ep,
                req,
            );
        }

        // Drive smoltcp: process any received frames, run timers, emit output.
        iface.poll(now(), &mut dev, &mut sockets);

        // Service the DHCP client: apply any lease change to the interface and
        // report it to the kernel (for `ifconfig`).
        if let Some(handle) = dhcp_handle {
            match sockets.get_mut::<dhcpv4::Socket>(handle).poll() {
                None => {}
                Some(dhcpv4::Event::Configured(cfg)) => {
                    apply_dhcp_config(&mut iface, &cfg);
                    report_config(service_ep, &cfg);
                }
                Some(dhcpv4::Event::Deconfigured) => {
                    // Lease lost / not yet acquired: drop the address and route.
                    iface.update_ip_addrs(|addrs| addrs.clear());
                    iface.routes_mut().remove_default_ipv4_route();
                    report_deconfig(service_ep);
                }
            }
        }

        // Yield so we poll on the scheduler's cadence rather than spinning hot.
        syscall::yield_now();
    }
}

/// Apply a freshly-acquired DHCP lease to the smoltcp interface: replace the
/// interface address and install (or clear) the default IPv4 route.
///
/// `update_ip_addrs` clears first so a renewal to a different address does not
/// leave the old one behind; `add_default_ipv4_route` already removes any prior
/// default before installing the new one.
fn apply_dhcp_config(iface: &mut Interface, cfg: &dhcpv4::Config<'_>) {
    let o = cfg.address.address().octets();
    let prefix = cfg.address.prefix_len();
    iface.update_ip_addrs(|addrs| {
        addrs.clear();
        let _ = addrs.push(IpCidr::new(
            IpAddress::v4(o[0], o[1], o[2], o[3]),
            prefix,
        ));
    });
    match cfg.router {
        Some(router) => {
            let _ = iface.routes_mut().add_default_ipv4_route(router);
        }
        None => {
            iface.routes_mut().remove_default_ipv4_route();
        }
    }
}

/// Report an acquired IP configuration to the kernel net service via `MSG_CONFIG`
/// so `ifconfig` can display it. The kernel only records what we send — it never
/// parses packets.
fn report_config(service_ep: u64, cfg: &dhcpv4::Config<'_>) {
    let o = cfg.address.address().octets();
    let prefix = cfg.address.prefix_len();
    // word1: packed address in the low 32 bits, prefix length in bits 32..40.
    let addr_word = pack_ipv4(o) | ((prefix as u64) << 32);
    let gw_word = cfg
        .router
        .map(|r| pack_ipv4(r.octets()))
        .unwrap_or(0);
    // DHCP may hand back several DNS servers; we report the first (display only —
    // Phase 4 has no resolver).
    let dns_word = cfg
        .dns_servers
        .first()
        .map(|d| pack_ipv4(d.octets()))
        .unwrap_or(0);
    ipc::call(service_ep, [MSG_CONFIG, addr_word, gw_word, dns_word], 0);
}

/// Report loss of configuration (a zero address word) to the kernel net service.
fn report_deconfig(service_ep: u64) {
    ipc::call(service_ep, [MSG_CONFIG, 0, 0, 0], 0);
}

// --- UDP socket API (Phase 4.5) ---
//
// The kernel socket router forwards capability-checked socket syscalls here as
// OP_SOCK_* requests. We keep a table of smoltcp UDP sockets keyed by the id we
// hand back on OP_SOCK_OPEN; datagram bytes travel through the shared socket
// region the kernel maps at SOCKET_REGION_VADDR.

/// Per-socket UDP buffer sizes: a handful of datagrams each way.
const UDP_META_SLOTS: usize = 8;
const UDP_PAYLOAD_BYTES: usize = 8 * 1024;
/// Per-socket TCP stream buffer sizes (RX/TX windows).
const TCP_BUFFER_BYTES: usize = 16 * 1024;

/// What transport a kernel-facing socket id is backed by.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SockKind {
    Udp,
    Tcp,
}

/// A socket in the net server's table: its kernel-facing id, the smoltcp handle,
/// and its kind.
struct SockEntry {
    id: u64,
    handle: SocketHandle,
    kind: SockKind,
}

/// Build a fresh UDP socket with heap-owned RX/TX ring buffers (so it is `'static`).
fn make_udp_socket() -> udp::Socket<'static> {
    let rx = udp::PacketBuffer::new(
        vec![udp::PacketMetadata::EMPTY; UDP_META_SLOTS],
        vec![0u8; UDP_PAYLOAD_BYTES],
    );
    let tx = udp::PacketBuffer::new(
        vec![udp::PacketMetadata::EMPTY; UDP_META_SLOTS],
        vec![0u8; UDP_PAYLOAD_BYTES],
    );
    udp::Socket::new(rx, tx)
}

/// Build a fresh TCP socket with heap-owned RX/TX stream buffers.
fn make_tcp_socket() -> tcp::Socket<'static> {
    let rx = tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER_BYTES]);
    let tx = tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER_BYTES]);
    tcp::Socket::new(rx, tx)
}

/// The shared socket payload region the kernel mapped at `SOCKET_REGION_VADDR`.
///
/// # Safety
/// Only called while serving a socket request, which the kernel only sends once
/// it has mapped the region — so the address is backed by writable shared RAM.
/// The returned reference is used and dropped entirely within one handler.
unsafe fn socket_region() -> &'static mut [u8] {
    core::slice::from_raw_parts_mut(SOCKET_REGION_VADDR as *mut u8, SOCKET_REGION_BYTES)
}

/// Look up a socket entry (handle + kind) by kernel-facing id.
fn find_entry(table: &[SockEntry], id: u64) -> Option<(SocketHandle, SockKind)> {
    table.iter().find(|e| e.id == id).map(|e| (e.handle, e.kind))
}

/// Split a smoltcp `IpEndpoint` into `([a,b,c,d], port)` (0.0.0.0 for non-IPv4).
fn endpoint_octets(ep: IpEndpoint) -> ([u8; 4], u16) {
    match ep.addr {
        IpAddress::Ipv4(a) => (a.octets(), ep.port),
    }
}

/// A deterministic ephemeral local port for a TCP connect, derived from the
/// socket id (avoids threading a separate counter; collisions are unlikely
/// within the small socket space this server serves).
fn ephemeral_port(socket_id: u64) -> u16 {
    49152u16.wrapping_add((socket_id % 16000) as u16)
}

/// Handle one OP_SOCK_* request and reply to the kernel router. `iface` is
/// needed for the TCP connect handshake (`iface.context()`).
fn serve_socket(
    iface: &mut Interface,
    sockets: &mut SocketSet<'static>,
    table: &mut Vec<SockEntry>,
    next_id: &mut u64,
    ep: u64,
    req: IpcMessage,
) {
    let token = req.reply_token;
    let reply = |words: [u64; 4]| {
        syscall::reply(ep, token, words);
    };

    match req.words[0] {
        OP_SOCK_OPEN => {
            let (handle, kind) = match req.words[1] {
                SOCK_TYPE_UDP => (sockets.add(make_udp_socket()), SockKind::Udp),
                SOCK_TYPE_TCP => (sockets.add(make_tcp_socket()), SockKind::Tcp),
                _ => {
                    reply([SOCK_ERR, 0, 0, 0]);
                    return;
                }
            };
            let id = *next_id;
            *next_id += 1;
            table.push(SockEntry { id, handle, kind });
            reply([SOCK_OK, id, 0, 0]);
        }
        OP_SOCK_BIND => {
            let id = req.words[1];
            let port = req.words[3] as u16;
            match find_entry(table, id) {
                // UDP: bind to the local port. TCP: bind is only meaningful for a
                // future listen (Phase 4.7); accept it as a no-op for now.
                Some((h, SockKind::Udp)) => match sockets.get_mut::<udp::Socket>(h).bind(port) {
                    Ok(()) => reply([SOCK_OK, 0, 0, 0]),
                    Err(_) => reply([SOCK_ERR, 0, 0, 0]),
                },
                Some((_, SockKind::Tcp)) => reply([SOCK_OK, 0, 0, 0]),
                None => reply([SOCK_ERR, 0, 0, 0]),
            }
        }
        OP_SOCK_CONNECT => {
            let id = req.words[1];
            let (ip, port) = (unpack_ipv4_word(req.words[2]), req.words[3] as u16);
            match find_entry(table, id) {
                Some((h, SockKind::Tcp)) => {
                    let remote = IpEndpoint::new(IpAddress::v4(ip[0], ip[1], ip[2], ip[3]), port);
                    let local = ephemeral_port(id);
                    let s = sockets.get_mut::<tcp::Socket>(h);
                    match s.connect(iface.context(), remote, local) {
                        Ok(()) => reply([SOCK_OK, 0, 0, 0]),
                        Err(_) => reply([SOCK_ERR, 0, 0, 0]),
                    }
                }
                // connect() is TCP-only.
                _ => reply([SOCK_ERR, 0, 0, 0]),
            }
        }
        OP_SOCK_SEND => {
            let id = req.words[1];
            let len = (req.words[2] as usize).min(SOCKET_REGION_BYTES);
            match find_entry(table, id) {
                Some((h, SockKind::Udp)) => {
                    let (ip, port) = unpack_ip_port(req.words[3]);
                    let dest = IpEndpoint::new(IpAddress::v4(ip[0], ip[1], ip[2], ip[3]), port);
                    // SAFETY: see socket_region; the slice is used only here.
                    let region = unsafe { socket_region() };
                    let data: &[u8] = &region[..len];
                    let s = sockets.get_mut::<udp::Socket>(h);
                    match s.send_slice(data, dest) {
                        Ok(()) => reply([SOCK_OK, len as u64, 0, 0]),
                        Err(udp::SendError::BufferFull) => reply([SOCK_WOULDBLOCK, 0, 0, 0]),
                        Err(_) => reply([SOCK_ERR, 0, 0, 0]),
                    }
                }
                Some((h, SockKind::Tcp)) => {
                    let s = sockets.get_mut::<tcp::Socket>(h);
                    // A zero-length send doubles as a connection-state probe:
                    // WouldBlock while connecting, OK once established, Refused on
                    // reset. Otherwise it's a stream send bounded by the TX window.
                    match tcp_phase(s) {
                        TcpPhase::Connecting => reply([SOCK_WOULDBLOCK, 0, 0, 0]),
                        TcpPhase::Refused => reply([SOCK_REFUSED, 0, 0, 0]),
                        TcpPhase::Ready => {
                            if len == 0 {
                                reply([SOCK_OK, 0, 0, 0]);
                            } else if s.can_send() {
                                // SAFETY: see socket_region.
                                let region = unsafe { socket_region() };
                                match s.send_slice(&region[..len]) {
                                    Ok(n) => reply([SOCK_OK, n as u64, 0, 0]),
                                    Err(_) => reply([SOCK_REFUSED, 0, 0, 0]),
                                }
                            } else {
                                // TX window full — retry later.
                                reply([SOCK_WOULDBLOCK, 0, 0, 0]);
                            }
                        }
                    }
                }
                None => reply([SOCK_ERR, 0, 0, 0]),
            }
        }
        OP_SOCK_RECV => {
            let id = req.words[1];
            let max = (req.words[2] as usize).min(SOCKET_REGION_BYTES);
            match find_entry(table, id) {
                Some((h, SockKind::Udp)) => {
                    // SAFETY: see socket_region; the slice is used only here.
                    let region = unsafe { socket_region() };
                    let s = sockets.get_mut::<udp::Socket>(h);
                    match s.recv_slice(&mut region[..max]) {
                        Ok((n, meta)) => {
                            let (ip, port) = endpoint_octets(meta.endpoint);
                            reply([SOCK_OK, n as u64, pack_ip_port(ip, port), 0]);
                        }
                        Err(_) => reply([SOCK_WOULDBLOCK, 0, 0, 0]),
                    }
                }
                Some((h, SockKind::Tcp)) => {
                    let s = sockets.get_mut::<tcp::Socket>(h);
                    match tcp_phase(s) {
                        TcpPhase::Connecting => reply([SOCK_WOULDBLOCK, 0, 0, 0]),
                        TcpPhase::Refused => reply([SOCK_REFUSED, 0, 0, 0]),
                        TcpPhase::Ready => {
                            if s.can_recv() {
                                // SAFETY: see socket_region.
                                let region = unsafe { socket_region() };
                                match s.recv_slice(&mut region[..max]) {
                                    // No peer address on a stream recv (word2 = 0).
                                    Ok(n) => reply([SOCK_OK, n as u64, 0, 0]),
                                    Err(_) => reply([SOCK_REFUSED, 0, 0, 0]),
                                }
                            } else if s.may_recv() {
                                // Connected, no data yet — retry later.
                                reply([SOCK_WOULDBLOCK, 0, 0, 0]);
                            } else {
                                // Peer closed and the buffer is drained → EOF.
                                reply([SOCK_OK, 0, 0, 0]);
                            }
                        }
                    }
                }
                None => reply([SOCK_ERR, 0, 0, 0]),
            }
        }
        OP_SOCK_CLOSE => {
            let id = req.words[1];
            match table.iter().position(|e| e.id == id) {
                Some(pos) => {
                    let entry = table.remove(pos);
                    if entry.kind == SockKind::Tcp {
                        // Send a FIN so the peer sees a clean close before we drop
                        // the socket on the next poll.
                        sockets.get_mut::<tcp::Socket>(entry.handle).close();
                    }
                    sockets.remove(entry.handle);
                    reply([SOCK_OK, 0, 0, 0]);
                }
                None => reply([SOCK_ERR, 0, 0, 0]),
            }
        }
        _ => reply([SOCK_ERR, 0, 0, 0]),
    }
}

/// The client-visible phase of a TCP socket, derived from its smoltcp state.
enum TcpPhase {
    /// Handshake in progress (SYN sent / received) — the caller should retry.
    Connecting,
    /// Established (or half-closed) — send/recv are meaningful.
    Ready,
    /// The connection was refused/reset or is fully closed.
    Refused,
}

/// Classify a TCP socket into a client-visible [`TcpPhase`].
fn tcp_phase(s: &tcp::Socket) -> TcpPhase {
    use tcp::State::*;
    match s.state() {
        SynSent | SynReceived => TcpPhase::Connecting,
        Established | FinWait1 | FinWait2 | CloseWait | Closing | LastAck | TimeWait => {
            TcpPhase::Ready
        }
        // Closed / Listen: no active connection. For a client that has called
        // connect(), reaching Closed means the peer refused or reset it.
        Closed | Listen => TcpPhase::Refused,
    }
}

/// Unpack a bare packed-IPv4 word (as sent by OP_SOCK_CONNECT `word2`) to octets.
fn unpack_ipv4_word(w: u64) -> [u8; 4] {
    [(w >> 24) as u8, (w >> 16) as u8, (w >> 8) as u8, w as u8]
}
