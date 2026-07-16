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

use libthemelios::{boot_info, syscall};
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress};

use device::IpcDevice;

/// Maximum Ethernet frame smoltcp may build (1500 MTU + 14-byte L2 header).
const MTU: usize = 1514;

/// The server entry point, called by `libthemelios::_start` after the heap is
/// initialised. Builds the stack and runs the poll loop forever.
#[no_mangle]
pub extern "C" fn server_main() -> ! {
    let info = boot_info();

    // The kernel packs the NIC's MAC into arg1 (little-endian, 6 bytes).
    let m = info.arg1;
    let mac = [
        m as u8,
        (m >> 8) as u8,
        (m >> 16) as u8,
        (m >> 24) as u8,
        (m >> 32) as u8,
        (m >> 40) as u8,
    ];

    // Build the smoltcp Device over the net-service frame bridge:
    //   arg0                  = net-service endpoint (MSG_POLL / MSG_TX_FRAME)
    //   shared region         = RX frames (service -> us)
    //   client-shared region  = TX frames (us -> service)
    let mut dev = IpcDevice::new(
        info.arg0,
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

    loop {
        // Drive smoltcp: process any received frames, run timers, emit output.
        iface.poll(now(), &mut dev, &mut sockets);
        // Yield so we poll on the scheduler's cadence rather than spinning hot.
        syscall::yield_now();
    }
}
