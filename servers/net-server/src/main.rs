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
    pack_ip_port, pack_ipv4, unpack_ip_port, MSG_CONFIG, OP_SOCK_ACCEPT, OP_SOCK_BIND,
    OP_SOCK_CLOSE, OP_SOCK_CONNECT, OP_SOCK_LIST, OP_SOCK_LISTEN, OP_SOCK_OPEN, OP_SOCK_PING,
    OP_SOCK_RECV, OP_SOCK_SEND, SLK_ICMP, SLK_TCP, SLK_UDP, SLS_ICMP, SLS_TCP_CLOSED,
    SLS_TCP_CLOSE_WAIT, SLS_TCP_CLOSING, SLS_TCP_ESTABLISHED, SLS_TCP_FIN_WAIT1, SLS_TCP_FIN_WAIT2,
    SLS_TCP_LAST_ACK, SLS_TCP_LISTEN, SLS_TCP_SYN_RECEIVED, SLS_TCP_SYN_SENT, SLS_TCP_TIME_WAIT,
    SLS_UDP, SOCKET_REGION_BYTES, SOCKET_REGION_VADDR, SOCK_ERR, SOCK_LIST_ENTRY_BYTES, SOCK_OK,
    SOCK_REFUSED, SOCK_TYPE_ICMP, SOCK_TYPE_TCP, SOCK_TYPE_UDP, SOCK_WOULDBLOCK,
};
use libthemelios::{boot_info, ipc, syscall, IpcMessage};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::socket::{dhcpv4, icmp, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, IpEndpoint,
    Ipv4Address,
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

/// Per-socket ICMP buffer sizes: a few echo packets each way.
const ICMP_META_SLOTS: usize = 8;
const ICMP_PAYLOAD_BYTES: usize = 4 * 1024;

/// Fixed ICMP echo-request payload (the classic monotonic byte pattern). The
/// exact bytes do not matter — a correct echo reply returns them verbatim, which
/// smoltcp checks implicitly via the ICMP checksum.
const PING_PAYLOAD: [u8; 32] = [
    0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
    0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27,
];

/// What transport a kernel-facing socket id is backed by.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SockKind {
    Udp,
    Tcp,
    /// An ICMP echo socket (backs `ping`), bound to an identifier.
    Icmp,
}

/// A socket in the net server's table: its kernel-facing id, the smoltcp handle,
/// its kind, and (for a TCP listener) the local port it was bound to.
struct SockEntry {
    id: u64,
    handle: SocketHandle,
    kind: SockKind,
    /// Local port set by `bind` — used by `listen` and to re-arm the replacement
    /// listening socket on `accept`. 0 = unbound.
    local_port: u16,
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

/// Build a fresh ICMP socket with heap-owned RX/TX packet buffers.
fn make_icmp_socket() -> icmp::Socket<'static> {
    let rx = icmp::PacketBuffer::new(
        vec![icmp::PacketMetadata::EMPTY; ICMP_META_SLOTS],
        vec![0u8; ICMP_PAYLOAD_BYTES],
    );
    let tx = icmp::PacketBuffer::new(
        vec![icmp::PacketMetadata::EMPTY; ICMP_META_SLOTS],
        vec![0u8; ICMP_PAYLOAD_BYTES],
    );
    icmp::Socket::new(rx, tx)
}

/// An ICMP identifier derived from a kernel-facing socket id. Echo replies carry
/// the identifier of the request, and the ICMP socket is bound to it so smoltcp
/// routes matching replies back to this socket. Kept in the low 16 bits — the
/// small socket space this server serves makes collisions negligible.
fn icmp_ident(socket_id: u64) -> u16 {
    socket_id as u16
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
            let id = *next_id;
            let (handle, kind, local_port) = match req.words[1] {
                SOCK_TYPE_UDP => (sockets.add(make_udp_socket()), SockKind::Udp, 0),
                SOCK_TYPE_TCP => (sockets.add(make_tcp_socket()), SockKind::Tcp, 0),
                SOCK_TYPE_ICMP => {
                    // An ICMP socket is usable only once bound to an identifier
                    // (so echo replies route back to it), so bind it here at open
                    // time. `local_port` records the identifier for the listing.
                    let ident = icmp_ident(id);
                    let handle = sockets.add(make_icmp_socket());
                    if sockets
                        .get_mut::<icmp::Socket>(handle)
                        .bind(icmp::Endpoint::Ident(ident))
                        .is_err()
                    {
                        sockets.remove(handle);
                        reply([SOCK_ERR, 0, 0, 0]);
                        return;
                    }
                    (handle, SockKind::Icmp, ident)
                }
                _ => {
                    reply([SOCK_ERR, 0, 0, 0]);
                    return;
                }
            };
            *next_id += 1;
            table.push(SockEntry { id, handle, kind, local_port });
            reply([SOCK_OK, id, 0, 0]);
        }
        OP_SOCK_BIND => {
            let id = req.words[1];
            let port = req.words[3] as u16;
            match table.iter().position(|e| e.id == id) {
                Some(i) => match table[i].kind {
                    // UDP: bind to the local port now (and record it for the
                    // `sockets` listing).
                    SockKind::Udp => match sockets.get_mut::<udp::Socket>(table[i].handle).bind(port)
                    {
                        Ok(()) => {
                            table[i].local_port = port;
                            reply([SOCK_OK, 0, 0, 0]);
                        }
                        Err(_) => reply([SOCK_ERR, 0, 0, 0]),
                    },
                    // TCP: record the port; `listen` consumes it.
                    SockKind::Tcp => {
                        table[i].local_port = port;
                        reply([SOCK_OK, 0, 0, 0]);
                    }
                    // ICMP has no bind (it is bound to an identifier at open).
                    SockKind::Icmp => reply([SOCK_ERR, 0, 0, 0]),
                },
                None => reply([SOCK_ERR, 0, 0, 0]),
            }
        }
        OP_SOCK_LISTEN => {
            // Put the (bound) TCP socket into LISTEN on its local port.
            let id = req.words[1];
            match table.iter().position(|e| e.id == id) {
                Some(i) if table[i].kind == SockKind::Tcp && table[i].local_port != 0 => {
                    let port = table[i].local_port;
                    match sockets.get_mut::<tcp::Socket>(table[i].handle).listen(port) {
                        Ok(()) => reply([SOCK_OK, 0, 0, 0]),
                        Err(_) => reply([SOCK_ERR, 0, 0, 0]),
                    }
                }
                _ => reply([SOCK_ERR, 0, 0, 0]),
            }
        }
        OP_SOCK_ACCEPT => {
            // If the listener socket has an established connection, hand it back as
            // a new socket id and re-arm the listener with a fresh LISTEN socket.
            let id = req.words[1];
            // Only a bound listener (local_port != 0) may be accepted on — this
            // rejects `accept` on a connected/outbound socket (local_port == 0),
            // which would otherwise re-arm a bogus listener on port 0.
            let idx = match table.iter().position(|e| e.id == id) {
                Some(i) if table[i].kind == SockKind::Tcp && table[i].local_port != 0 => i,
                _ => {
                    reply([SOCK_ERR, 0, 0, 0]);
                    return;
                }
            };
            let old_handle = table[idx].handle;
            let port = table[idx].local_port;
            let state = sockets.get_mut::<tcp::Socket>(old_handle).state();
            match state {
                // Still waiting for a SYN.
                tcp::State::Listen => reply([SOCK_WOULDBLOCK, 0, 0, 0]),
                // Fell back to Closed (e.g. peer reset before we accepted): re-arm.
                tcp::State::Closed => {
                    let _ = sockets.get_mut::<tcp::Socket>(old_handle).listen(port);
                    reply([SOCK_WOULDBLOCK, 0, 0, 0]);
                }
                // A connection is on `old_handle`. Promote it and replace the
                // listener with a fresh LISTEN socket on the same port.
                _ => {
                    let peer = sockets
                        .get_mut::<tcp::Socket>(old_handle)
                        .remote_endpoint()
                        .map(endpoint_octets)
                        .unwrap_or(([0; 4], 0));
                    let new_listen = sockets.add(make_tcp_socket());
                    let _ = sockets.get_mut::<tcp::Socket>(new_listen).listen(port);
                    // The listener keeps its id but now points at the new socket.
                    table[idx].handle = new_listen;
                    // The accepted connection gets a fresh id on the old handle.
                    let conn_id = *next_id;
                    *next_id += 1;
                    table.push(SockEntry {
                        id: conn_id,
                        handle: old_handle,
                        kind: SockKind::Tcp,
                        local_port: 0,
                    });
                    reply([SOCK_OK, conn_id, pack_ip_port(peer.0, peer.1), 0]);
                }
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
                // ICMP sockets send via OP_SOCK_PING, not the byte-stream path.
                Some((_, SockKind::Icmp)) => reply([SOCK_ERR, 0, 0, 0]),
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
                Some((h, SockKind::Icmp)) => {
                    // Deliver the next buffered ICMP echo reply, if any. We report
                    // the sequence number (word1) and source address (word2) — the
                    // ping data itself stays in the server (a correct reply echoes
                    // our payload, which smoltcp validates via the ICMP checksum).
                    let s = sockets.get_mut::<icmp::Socket>(h);
                    if !s.can_recv() {
                        reply([SOCK_WOULDBLOCK, 0, 0, 0]);
                        return;
                    }
                    match s.recv() {
                        Ok((payload, src)) => {
                            let caps = ChecksumCapabilities::default();
                            let parsed = Icmpv4Packet::new_checked(payload)
                                .ok()
                                .and_then(|p| Icmpv4Repr::parse(&p, &caps).ok());
                            match parsed {
                                Some(Icmpv4Repr::EchoReply { seq_no, .. }) => {
                                    let src_octets = match src {
                                        IpAddress::Ipv4(a) => a.octets(),
                                    };
                                    reply([SOCK_OK, seq_no as u64, pack_ip_port(src_octets, 0), 0]);
                                }
                                // Some other ICMP message (unreachable, etc.) —
                                // not an echo reply, so nothing pending for ping.
                                _ => reply([SOCK_WOULDBLOCK, 0, 0, 0]),
                            }
                        }
                        Err(_) => reply([SOCK_WOULDBLOCK, 0, 0, 0]),
                    }
                }
                None => reply([SOCK_ERR, 0, 0, 0]),
            }
        }
        OP_SOCK_PING => {
            // Emit one ICMPv4 echo request from an ICMP socket. word2 = dest IP
            // (bare packed IPv4), word3 = sequence number.
            let id = req.words[1];
            let dest = unpack_ipv4_word(req.words[2]);
            let seq_no = req.words[3] as u16;
            match table.iter().find(|e| e.id == id) {
                Some(e) if e.kind == SockKind::Icmp => {
                    let ident = e.local_port; // the identifier we bound at open
                    let handle = e.handle;
                    let dest_addr = IpAddress::v4(dest[0], dest[1], dest[2], dest[3]);
                    let caps = ChecksumCapabilities::default();
                    let repr = Icmpv4Repr::EchoRequest {
                        ident,
                        seq_no,
                        data: &PING_PAYLOAD,
                    };
                    let s = sockets.get_mut::<icmp::Socket>(handle);
                    if !s.can_send() {
                        reply([SOCK_WOULDBLOCK, 0, 0, 0]);
                        return;
                    }
                    match s.send(repr.buffer_len(), dest_addr) {
                        Ok(buf) => {
                            let mut packet = Icmpv4Packet::new_unchecked(buf);
                            repr.emit(&mut packet, &caps);
                            reply([SOCK_OK, 0, 0, 0]);
                        }
                        Err(_) => reply([SOCK_WOULDBLOCK, 0, 0, 0]),
                    }
                }
                _ => reply([SOCK_ERR, 0, 0, 0]),
            }
        }
        OP_SOCK_LIST => {
            // Serialise the socket table into the shared region as fixed-size
            // entries (see SOCK_LIST_ENTRY_BYTES), returning the count. The kernel
            // reads the entries back out to print the `sockets` shell command.
            let max_entries = SOCKET_REGION_BYTES / SOCK_LIST_ENTRY_BYTES;
            let mut count = 0usize;
            for i in 0..table.len() {
                if count >= max_entries {
                    break;
                }
                let (kind_code, state_code, remote_ip, remote_port) = list_describe(&table[i], sockets);
                let id = table[i].id;
                let local_port = table[i].local_port;
                // SAFETY: see socket_region; used and dropped within this handler.
                let region = unsafe { socket_region() };
                let off = count * SOCK_LIST_ENTRY_BYTES;
                let slot = &mut region[off..off + SOCK_LIST_ENTRY_BYTES];
                slot.fill(0);
                slot[0..8].copy_from_slice(&id.to_le_bytes());
                slot[8] = kind_code;
                slot[9] = state_code;
                slot[10..12].copy_from_slice(&local_port.to_le_bytes());
                slot[12..16].copy_from_slice(&remote_ip);
                slot[16..18].copy_from_slice(&remote_port.to_le_bytes());
                count += 1;
            }
            reply([SOCK_OK, count as u64, 0, 0]);
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

/// Map a smoltcp `tcp::State` to the wire state code used by `OP_SOCK_LIST`.
fn tcp_state_code(state: tcp::State) -> u8 {
    use tcp::State::*;
    match state {
        Closed => SLS_TCP_CLOSED,
        Listen => SLS_TCP_LISTEN,
        SynSent => SLS_TCP_SYN_SENT,
        SynReceived => SLS_TCP_SYN_RECEIVED,
        Established => SLS_TCP_ESTABLISHED,
        FinWait1 => SLS_TCP_FIN_WAIT1,
        FinWait2 => SLS_TCP_FIN_WAIT2,
        CloseWait => SLS_TCP_CLOSE_WAIT,
        Closing => SLS_TCP_CLOSING,
        LastAck => SLS_TCP_LAST_ACK,
        TimeWait => SLS_TCP_TIME_WAIT,
    }
}

/// Describe a socket for the `OP_SOCK_LIST` listing: its kind code, state code,
/// and remote endpoint (0.0.0.0:0 when there is none). TCP reads live state and
/// the connected peer from smoltcp; UDP and ICMP have fixed state codes and no
/// single remote endpoint.
fn list_describe(
    entry: &SockEntry,
    sockets: &mut SocketSet<'static>,
) -> (u8, u8, [u8; 4], u16) {
    match entry.kind {
        SockKind::Udp => (SLK_UDP, SLS_UDP, [0; 4], 0),
        SockKind::Icmp => (SLK_ICMP, SLS_ICMP, [0; 4], 0),
        SockKind::Tcp => {
            let s = sockets.get_mut::<tcp::Socket>(entry.handle);
            let state = tcp_state_code(s.state());
            let (ip, port) = s
                .remote_endpoint()
                .map(endpoint_octets)
                .unwrap_or(([0; 4], 0));
            (SLK_TCP, state, ip, port)
        }
    }
}
