//! # Network device abstraction
//!
//! A *network device* (NIC) sends and receives raw Ethernet frames. The kernel
//! talks to network hardware exclusively through the [`NetDevice`] trait, which
//! decouples the higher layers — the kernel net service and, above it, the
//! ring-3 TCP/IP stack — from the specific hardware underneath.
//!
//! ## Why a trait (the arm64 seam)
//!
//! In Phase 4 the only implementation is VirtIO-net (QEMU's virtual NIC), reached
//! over PCI. But the roadmap runs on real cloud instances and, in Phase 7, on
//! arm64 — where the same VirtIO device is discovered over `virtio-mmio` or a
//! memory-mapped ECAM PCI bridge rather than x86 port-I/O PCI. That port
//! implements this exact trait for its bus; nothing above the driver — not the
//! net service, not the socket API, not the stack — changes. This mirrors how the
//! [`BlockDevice`](crate::drivers::block::BlockDevice) trait decoupled storage
//! from the bus in Phase 3.
//!
//! ## Where this sits in the hybrid microkernel
//!
//! The NIC driver is one of the few drivers that stays in ring 0: it is thin, it
//! only talks to trusted hypervisor hardware, and it does DMA. All protocol
//! parsing — Ethernet, ARP, IPv4, TCP, … — the real attack surface — runs in the
//! ring-3 net server, which reaches this driver through the kernel net service's
//! frame IPC interface (sub-phase 4.1). A bug here is a bug in a few hundred lines
//! of well-scoped DMA code, not in a sprawling protocol stack.

use alloc::vec::Vec;

use crate::sync::InterruptMutex;

/// Errors returned by [`NetDevice`] operations.
///
/// Deliberately small and concrete: every variant maps to a specific failure a
/// caller can act on. "No frame is waiting" is *not* an error — [`NetDevice::receive`]
/// reports that by returning `Ok(0)`, since a zero-length Ethernet frame is never
/// valid and so is an unambiguous "empty" signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    /// I/O error reported by the device.
    DeviceError,
    /// The transmit virtqueue was full; the frame could not be submitted.
    QueueFull,
    /// No receive buffer was available to (re)post to the device.
    NoBuffer,
    /// The device is not ready or has not been initialised.
    NotReady,
    /// The frame is larger than the device's MTU (plus L2 header) allows.
    TooLarge,
}

/// A network interface that sends and receives raw Ethernet frames.
///
/// Implementors must be `Send + Sync`: NICs are registered globally and driven
/// from the kernel net service task. Methods take `&self` and use interior
/// mutability for the hardware queues, so a single shared reference drives the
/// device.
///
/// Frames handed to and returned from this trait are **complete Ethernet frames**
/// (destination MAC, source MAC, EtherType, payload) — the driver is responsible
/// for adding and stripping any device-specific framing (e.g. the VirtIO-net
/// header) so callers never see it.
pub trait NetDevice: Send + Sync {
    /// Transmit one Ethernet `frame`. Blocks until the device accepts it.
    ///
    /// Returns [`NetError::TooLarge`] if the frame exceeds the MTU + L2 header.
    fn transmit(&self, frame: &[u8]) -> Result<(), NetError>;

    /// Poll for one received Ethernet frame, copying it into `buf`.
    ///
    /// Returns `Ok(n)` where `n` is the frame length in bytes (`0` if no frame is
    /// currently waiting). If the frame is longer than `buf`, it is truncated to
    /// `buf.len()` and the truncated length is returned. Non-blocking: this is the
    /// polled-RX primitive the net service drains on its poll cadence.
    fn receive(&self, buf: &mut [u8]) -> Result<usize, NetError>;

    /// The device's MAC address.
    fn mac(&self) -> [u8; 6];

    /// The interface MTU in bytes (payload size, excluding the 14-byte Ethernet
    /// header). VirtIO-net defaults to 1500.
    fn mtu(&self) -> usize;
}

// ----- Global network device registry -----
//
// Discovered NICs are registered here at boot so the net service and diagnostics
// can look them up by index. Devices live for the kernel's lifetime, so we store
// `&'static dyn NetDevice` handles (obtained by leaking the boxed driver) — no
// reference counting needed. Mirrors the block device registry.

/// A registered network device: a stable name plus a static handle to the driver.
pub struct RegisteredNetDevice {
    /// Human-readable name, e.g. "virtio-net0".
    pub name: &'static str,
    /// The driver, behind a `'static` reference (the driver is leaked at
    /// registration since NICs are never torn down).
    pub device: &'static dyn NetDevice,
}

/// The global network device registry.
static REGISTRY: InterruptMutex<Vec<RegisteredNetDevice>> = InterruptMutex::new(Vec::new());

/// Register a network device under `name`, returning its registry index.
///
/// Takes ownership of the boxed driver and leaks it to obtain a `'static` handle
/// (NICs live forever). The returned index is stable for the kernel's lifetime.
pub fn register(name: &'static str, device: alloc::boxed::Box<dyn NetDevice>) -> usize {
    // Leak the box: the driver must outlive every reference handed out, and it is
    // never deallocated, so a permanent leak is the correct lifetime.
    let static_dev: &'static dyn NetDevice = alloc::boxed::Box::leak(device);
    let mut reg = REGISTRY.lock();
    let index = reg.len();
    reg.push(RegisteredNetDevice { name, device: static_dev });
    index
}

/// Look up a registered network device by index.
pub fn get(index: usize) -> Option<&'static dyn NetDevice> {
    REGISTRY.lock().get(index).map(|r| r.device)
}

/// Number of registered network devices.
#[allow(dead_code)]
pub fn count() -> usize {
    REGISTRY.lock().len()
}

/// Print the network device registry to the serial console for diagnostics.
#[allow(dead_code)]
pub fn print_registry() {
    use crate::println;
    let reg = REGISTRY.lock();
    println!("Network devices: {} registered", reg.len());
    for (i, r) in reg.iter().enumerate() {
        let m = r.device.mac();
        println!(
            "  [{}] {}  {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  MTU {}",
            i, r.name, m[0], m[1], m[2], m[3], m[4], m[5], r.device.mtu(),
        );
    }
}
