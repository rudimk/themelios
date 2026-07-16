//! # smoltcp `Device` over the kernel net service
//!
//! smoltcp drives itself by asking a [`Device`] to receive and transmit raw
//! Ethernet frames inside its `poll()`. Our frames don't come from hardware —
//! they come from the kernel net service (sub-phase 4.1) over the pull-based
//! IPC bridge. [`IpcDevice`] implements smoltcp's `Device` on top of that:
//!
//! - `receive()` issues `MSG_POLL` to the service; if a frame comes back (in the
//!   RX shared region) it hands smoltcp an [`IpcRxToken`] holding that frame.
//! - `transmit()` / a token's `consume()` lets smoltcp fill the TX shared region
//!   in place, then issues `MSG_TX_FRAME` so the service transmits it.
//!
//! Both `consume` closures run synchronously inside `poll()`, so a blocking
//! `ipc::call` inside them is fine — the ring-3 server simply blocks for the
//! service's reply, then continues.

use alloc::vec::Vec;

use libthemelios::ipc;
use libthemelios::net_proto::{MSG_POLL, MSG_TX_FRAME};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

/// A smoltcp `Device` backed by the kernel net service's frame bridge.
///
/// Single-threaded (the net server is one task), so the raw pointers into the
/// shared regions — which live for the server's lifetime — need no synchronisation.
pub struct IpcDevice {
    /// Net service endpoint that `MSG_POLL` / `MSG_TX_FRAME` are sent to.
    service_ep: u64,
    /// Base of the RX shared region (the service writes received frames here).
    rx_ptr: *mut u8,
    /// Size of the RX shared region in bytes.
    rx_len: usize,
    /// Base of the TX shared region (we stage frames to transmit here).
    tx_ptr: *mut u8,
    /// Size of the TX shared region in bytes.
    tx_len: usize,
    /// Maximum frame size (including the 14-byte Ethernet header) smoltcp may use.
    mtu: usize,
}

impl IpcDevice {
    /// Build a device over the given net-service endpoint and shared regions.
    pub fn new(
        service_ep: u64,
        rx_ptr: *mut u8,
        rx_len: usize,
        tx_ptr: *mut u8,
        tx_len: usize,
        mtu: usize,
    ) -> Self {
        Self { service_ep, rx_ptr, rx_len, tx_ptr, tx_len, mtu }
    }
}

impl Device for IpcDevice {
    type RxToken<'a> = IpcRxToken;
    type TxToken<'a> = IpcTxToken;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Pull the next received frame (if any) from the service.
        let reply = ipc::call(self.service_ep, [MSG_POLL, 0, 0, 0], 0);
        let n = (reply.words[1] as usize).min(self.rx_len);
        if n == 0 {
            return None;
        }
        // Copy the frame out of the RX region so the token owns it — the region
        // is reused by the next MSG_POLL.
        // SAFETY: rx_ptr backs rx_len bytes of mapped shared memory; the service
        // wrote `n` (<= rx_len) bytes there.
        let frame = unsafe { core::slice::from_raw_parts(self.rx_ptr, n).to_vec() };
        Some((
            IpcRxToken { frame },
            IpcTxToken { service_ep: self.service_ep, tx_ptr: self.tx_ptr, tx_len: self.tx_len },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(IpcTxToken { service_ep: self.service_ep, tx_ptr: self.tx_ptr, tx_len: self.tx_len })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = self.mtu;
        caps
    }
}

/// A received frame, owned by the token (copied out of the RX shared region).
pub struct IpcRxToken {
    frame: Vec<u8>,
}

impl RxToken for IpcRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.frame)
    }
}

/// A permit to transmit one frame through the net service.
pub struct IpcTxToken {
    service_ep: u64,
    tx_ptr: *mut u8,
    tx_len: usize,
}

impl TxToken for IpcTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let len = len.min(self.tx_len);
        // smoltcp fills the frame in place in the TX shared region...
        // SAFETY: tx_ptr backs tx_len bytes of mapped shared memory; len is
        // clamped to that.
        let buf = unsafe { core::slice::from_raw_parts_mut(self.tx_ptr, len) };
        let result = f(buf);
        // ...then we ask the service to transmit it.
        ipc::call(self.service_ep, [MSG_TX_FRAME, len as u64, 0, 0], 0);
        result
    }
}
