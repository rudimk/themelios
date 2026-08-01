//! # Network stack
//!
//! TCP/IP networking for ThemeliOS. The network stack serves two purposes:
//!
//! 1. **Container networking**: Providing network connectivity to running containers.
//!    Each container gets an isolated virtual network interface with capabilities
//!    controlling what it can access.
//!
//! 2. **Management API**: The external API for managing the node (starting/stopping
//!    containers, querying status, streaming logs). Since ThemeliOS has no SSH,
//!    this API is the only way to interact with a running node.
//!
//! ## Implementation approach
//!
//! The network stack will be built on top of the VirtIO network driver and will
//! implement:
//! - Ethernet frame handling
//! - ARP (address resolution)
//! - IPv4 (and eventually IPv6)
//! - TCP and UDP
//! - A simple HTTP/gRPC server for the management API
//!
//! ## Microkernel considerations
//!
//! In the full microkernel design, the network stack runs in userspace. The kernel
//! only provides the raw VirtIO device access (via capabilities) and IPC channels
//! for the network service to communicate with other processes.
//!
//! ## Phase 4 status
//!
//! Sub-phase 4.0 adds the [`device`] module: the [`NetDevice`](device::NetDevice)
//! trait (the bus-independent NIC abstraction and the arm64 seam) and the global
//! network device registry. The VirtIO-net driver that implements it lives in
//! [`crate::drivers::virtio::net`]. Higher layers — the kernel net service and the
//! ring-3 TCP/IP stack — build on this in later sub-phases.

/// Network device abstraction (`NetDevice` trait) and the global NIC registry.
pub mod device;

/// Kernel net service: the pull-based IPC + shared-memory bridge between the
/// in-kernel NIC driver and the ring-3 TCP/IP stack.
pub mod net_service;

/// Socket routing: the kernel's capability-checked UDP socket API (Phase 4.5).
/// Routes socket syscalls to the ring-3 net server; never parses packets.
pub mod socket;

/// Pack a 6-byte MAC into a little-endian u64, for passing to the net server via
/// `BootInfo.arg1`.
///
/// A MAC occupies the low 48 bits, so the high 16 bits are free to carry flags —
/// see [`NET_ARG_DHCP`]. The net server unpacks the MAC by masking off those flag
/// bits (`arg1 & 0x0000_FFFF_FFFF_FFFF`).
fn pack_mac(mac: [u8; 6]) -> u64 {
    let mut v = 0u64;
    for (i, b) in mac.iter().enumerate() {
        v |= (*b as u64) << (8 * i);
    }
    v
}

/// `BootInfo.arg1` flag bit: run the DHCPv4 client instead of using a static IP.
///
/// The net server's `arg1` packs the NIC's MAC in its low 48 bits; bit 48 selects
/// the addressing mode. Set at boot (`boot_net`) so the live node configures
/// itself via DHCP; the two static-IP round-trip tests spawn the server without
/// this bit, so they keep their deterministic `10.0.2.15`. The net server mirrors
/// this constant by hand (it cannot depend on kernel crates).
pub const NET_ARG_DHCP: u64 = 1 << 48;

/// Bring up the network stack at boot: discover the VirtIO NIC, start the kernel
/// net service over it, and spawn the ring-3 net server (smoltcp) wired to the
/// service. Idempotent-ish; does nothing if no NIC is present.
///
/// The net server receives:
/// - `arg0` = the net-service endpoint (it sends `MSG_POLL`/`MSG_TX_FRAME` there)
/// - `shared` region = RX frames (service → server)
/// - `client_shared` region = TX frames (server → service)
/// - `arg1` = the NIC's MAC (packed), so it can configure smoltcp's interface
pub fn boot_net() {
    #[cfg(target_arch = "x86_64")]
    {
        use crate::drivers::pci;
        use crate::drivers::virtio::net::VirtioNet;
        use crate::net::device::{self, NetDevice};
        use crate::process::embedded;
        use crate::process::server::{spawn_server, ServerConfig};

        // Find the first VirtIO network device (vendor 0x1AF4, PCI class 0x02).
        let devs = pci::devices_by_vendor(pci::VIRTIO_VENDOR_ID);
        let nic_dev = match devs.iter().find(|d| d.class == 0x02) {
            Some(d) => d,
            None => return, // no NIC — nothing to bring up
        };
        let nic = match VirtioNet::init_from_pci(nic_dev) {
            Ok(n) => n,
            Err(_) => return,
        };
        let mac = nic.mac();
        let nic_index = device::register("virtio-net0", alloc::boxed::Box::new(nic));

        // Start the kernel net service over the NIC.
        let handle = match net_service::start(nic_index) {
            Some(h) => h,
            None => return,
        };

        // Spawn the ring-3 net server, wired to the service. `fs_ep` is the
        // server's request endpoint: it serves socket requests (Phase 4.5) here,
        // and the kernel socket router routes SYS_SOCKET/… to it.
        let fs_ep = crate::ipc::create_endpoint("net-server");
        let pid = spawn_server(ServerConfig {
            name: "net-server",
            binary: embedded::NET_SERVER,
            fs_endpoint: fs_ep,
            block_endpoint: 0,
            shared: Some(handle.rx_region),
            client_shared: Some(handle.tx_region),
            heap_bytes: 8 * 1024 * 1024,
            arg0: handle.endpoint,
            // Live boot: run the DHCPv4 client (sub-phase 4.4). The MAC rides in
            // the low 48 bits; NET_ARG_DHCP (bit 48) requests DHCP.
            arg1: pack_mac(mac) | NET_ARG_DHCP,
            filesystem_mount: None,
            grant_management: false,
        });

        // Set up the capability-checked socket API (Phase 4.5): allocate the
        // kernel↔net-server socket payload region, map it into the server at the
        // fixed SERVER_SOCKET_VIRT (which the server hardcodes), and register the
        // router so socket syscalls can reach the net server.
        use crate::mm::addr::VirtAddr;
        use crate::mm::shared::SharedRegion;
        use crate::process::server::{SERVER_SOCKET_BYTES, SERVER_SOCKET_VIRT};
        if let Some(sock_region) = SharedRegion::alloc(SERVER_SOCKET_BYTES) {
            let mapped = crate::process::with_address_space(pid, |a| {
                sock_region.map_into(a, VirtAddr::new(SERVER_SOCKET_VIRT));
            })
            .is_some();
            if mapped {
                socket::init(fs_ep, sock_region);
            }
        }

        crate::println!(
            "Network: virtio-net0 up ({:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}), net server spawned (DHCP)",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
        );
    }
}
