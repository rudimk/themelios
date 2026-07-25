//! # aarch64 smoltcp compile gate
//!
//! A do-nothing `no_std` library whose only purpose is to make the compiler
//! build `smoltcp` — with the net server's exact feature set — for a bare-metal
//! target. CI builds this crate for `aarch64-unknown-none`; a green build is the
//! evidence that ThemeliOS has not taken an amd64-only (or `std`) dependency into
//! its TCP/IP stack, so the Phase 7 arm64 port can inherit the stack unchanged.
//!
//! The kernel and net server do not depend on this crate — it is compiled only by
//! the gate job. See this crate's `Cargo.toml` for the full rationale.

#![no_std]

extern crate alloc;

use alloc::vec;

use smoltcp::iface::SocketSet;
use smoltcp::socket::{dhcpv4, icmp, tcp, udp};

/// Reference each socket type the net server uses so the gate actually
/// monomorphises the code paths ThemeliOS relies on (not merely the crate's
/// top-level items). Never called — its value is that it type-checks and
/// codegens for the target under test.
#[allow(dead_code)]
fn touch_sockets() {
    // UDP: heap-owned packet buffers, exactly as `make_udp_socket` builds them.
    let udp_rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 1024]);
    let udp_tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 1024]);
    let udp_sock = udp::Socket::new(udp_rx, udp_tx);

    // TCP: stream buffers.
    let tcp_rx = tcp::SocketBuffer::new(vec![0u8; 1024]);
    let tcp_tx = tcp::SocketBuffer::new(vec![0u8; 1024]);
    let tcp_sock = tcp::Socket::new(tcp_rx, tcp_tx);

    // ICMP: packet buffers (backs `ping`).
    let icmp_rx = icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0u8; 1024]);
    let icmp_tx = icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0u8; 1024]);
    let icmp_sock = icmp::Socket::new(icmp_rx, icmp_tx);

    // DHCPv4 client socket.
    let dhcp_sock = dhcpv4::Socket::new();

    let mut set: SocketSet<'static> = SocketSet::new(vec![]);
    let _ = set.add(udp_sock);
    let _ = set.add(tcp_sock);
    let _ = set.add(icmp_sock);
    let _ = set.add(dhcp_sock);
}
