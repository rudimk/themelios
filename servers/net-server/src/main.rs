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

use alloc::vec::Vec;

use libthemelios::net_proto::{pack_ipv4, MSG_CONFIG};
use libthemelios::{boot_info, ipc, syscall};
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::dhcpv4;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address};

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

    let mut sockets: SocketSet = SocketSet::new(Vec::new());

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
