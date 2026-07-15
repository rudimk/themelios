//! # Net server — ring-3 TCP/IP stack (Phase 4)
//!
//! The userspace network stack. The kernel exposes only a thin VirtIO-net driver
//! and a pull-based frame bridge (the net service, sub-phase 4.1); this server
//! runs the whole protocol stack — Ethernet, ARP, IPv4, ICMP, UDP, TCP, and a
//! DHCPv4 client — in ring 3, on top of [`smoltcp`]. A malformed packet can at
//! worst crash this server, never the kernel.
//!
//! ## Status
//!
//! Sub-phase 4.2 (in progress) stands the server up and drives smoltcp's poll
//! loop over the net service's `MSG_POLL`/`MSG_TX_FRAME` IPC. This is currently a
//! skeleton that establishes the crate and the smoltcp dependency (and satisfies
//! the arm64 compile gate); the Device trait, `Interface`, poll loop, and socket
//! wiring land in the following steps.

#![no_std]
#![no_main]

extern crate alloc;

use libthemelios::{boot_info, ipc};

/// The server entry point, called by `libthemelios::_start` after the heap is
/// initialised. Loops forever handling requests; never returns.
#[no_mangle]
pub extern "C" fn server_main() -> ! {
    let info = boot_info();

    loop {
        // Placeholder request loop — replaced by the smoltcp poll loop in 4.2.
        let req = ipc::receive(info.fs_endpoint);
        ipc::reply(info.fs_endpoint, req.reply_token, [0, 0, 0, 0]);
    }
}
