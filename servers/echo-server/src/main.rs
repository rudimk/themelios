//! # Echo server
//!
//! The simplest possible ThemeliOS userspace server: it receives IPC calls on
//! its endpoint and replies with the same four words, plus one (so the caller
//! can tell the reply apart from the request). Its only job is to prove the
//! Phase 3.4 server framework works end to end — the kernel can embed a flat
//! binary, spawn it in ring 3 with a heap and an endpoint, and exchange IPC with
//! it — before the real filesystem servers are built on the same foundation.

#![no_std]
#![no_main]

extern crate alloc;

use libthemelios::{boot_info, ipc};

/// The server entry point, called by `libthemelios::_start` after the heap is
/// initialised. Loops forever handling requests; never returns.
#[no_mangle]
pub extern "C" fn server_main() -> ! {
    let info = boot_info();

    // Touch the heap once to prove allocation works in ring 3 (the FS servers
    // depend heavily on it). A leaked boxed value is fine for this smoke test.
    let probe = alloc::boxed::Box::new(0xECC0_u32);
    let probe_ok = *probe == 0xECC0;

    loop {
        // Block until a client calls us.
        let req = ipc::receive(info.fs_endpoint);

        // Echo the words back, incrementing word0 so the reply is distinguishable,
        // and stash the heap-probe result in word3 so the test can confirm the
        // server's allocator came up.
        let reply = [
            req.words[0].wrapping_add(1),
            req.words[1],
            req.words[2],
            probe_ok as u64,
        ];
        ipc::reply(info.fs_endpoint, req.reply_token, reply);
    }
}
