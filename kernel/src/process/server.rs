//! # Userspace server loader
//!
//! Spawns a ThemeliOS userspace server — a `no_std` ring-3 program embedded in
//! the kernel image as a flat binary — into its own process. This is the
//! machinery the Phase 3 hybrid microkernel rests on: the filesystem servers
//! (SquashFS, ext2, overlay) are loaded this way, so that filesystem parsing
//! runs in ring 3 where a corrupt disk image can crash a server but never the
//! kernel.
//!
//! ## Why a flat binary, not ELF
//!
//! The kernel does **not** parse ELF. An ELF parser is itself attack surface,
//! and the threat model deliberately keeps complex parsing out of ring 0. Each
//! server is linked (with the server linker script) to a fixed virtual base and
//! emitted as a raw memory image. Loading is then trivial and unforgeable: zero
//! some pages, copy the bytes in, jump to the first byte.
//!
//! ## Server address-space layout
//!
//! Every server is linked and loaded at the same fixed virtual addresses, so a
//! single trampoline and one set of constants serve them all. These MUST match
//! `libthemelios` (`BOOTINFO_VIRT`) and the server linker script (code base).
//!
//! ```text
//!   0x0020_0000  code + rodata + data + bss   (the flat binary, + bss allowance)
//!   0x0030_0000  boot-info page               (ServerBootInfo, filled by kernel)
//!   0x4000_0000  heap window                  (libthemelios global allocator)
//!   0x5000_0000  shared data region           (block/FS bulk transfer)
//!   0x7FFF_FFF0_0000  stack top (grows down)
//! ```

use core::sync::atomic::{AtomicU64, Ordering};

use crate::cap::{Capability, CapRights, CapType};
use crate::mm;
use crate::mm::addr::{PhysAddr, VirtAddr};
use crate::mm::page_table::PageFlags;
use crate::mm::shared::SharedRegion;
use crate::process::{self, ProcessId};
use crate::sched;

/// Virtual base where the server's code image is loaded. Matches the server
/// linker script's base address and `SERVER_CODE_VIRT` here.
const SERVER_CODE_VIRT: u64 = 0x0020_0000;
/// Virtual address of the boot-info page. Matches `libthemelios::BOOTINFO_VIRT`.
const SERVER_BOOTINFO_VIRT: u64 = 0x0030_0000;
/// Virtual base of the server's heap window.
const SERVER_HEAP_VIRT: u64 = 0x4000_0000;
/// Virtual base where the block shared region is mapped (shared with the block
/// server, for disk-block transfers).
const SERVER_SHARED_VIRT: u64 = 0x5000_0000;
/// Virtual base where the client shared region is mapped (shared with the
/// server's clients, for paths and file data).
const SERVER_CLIENT_SHARED_VIRT: u64 = 0x5100_0000;
/// Top of the server's stack (grows downward).
const SERVER_STACK_TOP: u64 = 0x0000_7FFF_FFF0_0000;
/// Number of stack pages (16 × 4 KiB = 64 KiB).
const SERVER_STACK_PAGES: usize = 16;
/// Extra zeroed pages mapped after the loaded image to cover `.bss` (the flat
/// binary excludes `.bss`, which is NOLOAD). 64 KiB is ample for server statics.
const SERVER_BSS_PAGES: usize = 16;

/// Magic identifying a populated boot-info page (`"THMSBOOT"` little-endian).
/// Must match `libthemelios::BOOTINFO_MAGIC`.
const BOOTINFO_MAGIC: u64 = 0x544F_4F42_534D_4854;

/// Startup parameters handed to a server through its boot-info page.
///
/// `#[repr(C)]`, identical layout to `libthemelios::BootInfo`. Keep the two in
/// sync — the server reads exactly these fields at exactly these offsets.
#[repr(C)]
#[derive(Clone, Copy)]
struct ServerBootInfo {
    magic: u64,
    fs_endpoint: u64,
    block_endpoint: u64,
    shared_vaddr: u64,
    shared_size: u64,
    client_shared_vaddr: u64,
    client_shared_size: u64,
    heap_vaddr: u64,
    heap_size: u64,
    arg0: u64,
    arg1: u64,
    fs_cap_handle: u64,
}

/// Configuration for spawning a server.
pub struct ServerConfig {
    /// Task/process name for diagnostics.
    pub name: &'static str,
    /// The server's flat binary (embedded via `include_bytes!`).
    pub binary: &'static [u8],
    /// IPC endpoint the server receives its requests on.
    pub fs_endpoint: u64,
    /// IPC endpoint of the kernel block server (0 if unused).
    pub block_endpoint: u64,
    /// Block shared region to map at `SERVER_SHARED_VIRT` (None if unused).
    /// Shared with the kernel block server for disk-block transfers.
    pub shared: Option<SharedRegion>,
    /// Client shared region to map at `SERVER_CLIENT_SHARED_VIRT` (None if
    /// unused). Shared with the server's clients for paths and file data.
    pub client_shared: Option<SharedRegion>,
    /// Heap window size in bytes (rounded up to pages).
    pub heap_bytes: u64,
    /// Server-specific argument 0.
    pub arg0: u64,
    /// Server-specific argument 1.
    pub arg1: u64,
    /// If set, grant the server a `Filesystem` capability for this mount id and
    /// pass its handle to the server via `BootInfo.fs_cap_handle`.
    pub filesystem_mount: Option<u64>,
}

/// Spawn a userspace server from an embedded flat binary.
///
/// Creates a process, maps the code image (zeroed pages + the binary bytes +
/// a `.bss` allowance), a stack, a heap window, a boot-info page, and the
/// optional shared region, records endpoint/shared capabilities in the server's
/// CSpace, and starts it in ring 3. Returns the new process id.
///
/// Panics on resource exhaustion (out of frames) — server spawning happens at
/// boot, where failure is fatal anyway.
pub fn spawn_server(config: ServerConfig) -> ProcessId {
    let (pid, _) = process::create_process(config.name, None);

    // --- Map and load the code image ---
    //
    // Pages cover the binary plus a bss allowance. We zero every page first so
    // any region past the file's end (i.e. .bss) starts zeroed, then copy the
    // binary bytes in. The image region is mapped writable AND executable: a
    // flat binary mixes .text/.rodata/.data/.bss in one span and we don't parse
    // section boundaries, so we can't enforce W^X here (acceptable for Phase 3;
    // a future loader emitting per-segment permissions can tighten this).
    let image_pages = config.binary.len().div_ceil(mm::PAGE_SIZE as usize) + SERVER_BSS_PAGES;
    for i in 0..image_pages {
        let phys = mm::frame::allocate_frame().expect("spawn_server: no frame for code");
        // Zero the page (covers .bss for the trailing pages).
        // SAFETY: freshly allocated frame, HHDM-mapped, exclusive.
        unsafe {
            core::ptr::write_bytes(phys.to_virt().as_mut_ptr::<u8>(), 0, mm::PAGE_SIZE as usize);
        }
        let virt = VirtAddr::new(SERVER_CODE_VIRT + (i as u64) * mm::PAGE_SIZE);
        process::with_address_space(pid, |a| {
            a.map_page(virt, phys, PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE);
        })
        .expect("spawn_server: no address space");

        // Copy this page's slice of the binary in.
        let start = i * mm::PAGE_SIZE as usize;
        if start < config.binary.len() {
            let end = (start + mm::PAGE_SIZE as usize).min(config.binary.len());
            // SAFETY: phys is mapped via HHDM; we copy at most one page.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    config.binary[start..end].as_ptr(),
                    phys.to_virt().as_mut_ptr::<u8>(),
                    end - start,
                );
            }
        }
    }

    // --- Map the stack ---
    for i in 0..SERVER_STACK_PAGES {
        let phys = mm::frame::allocate_frame().expect("spawn_server: no frame for stack");
        let virt = VirtAddr::new(SERVER_STACK_TOP - ((SERVER_STACK_PAGES - i) as u64) * mm::PAGE_SIZE);
        process::with_address_space(pid, |a| {
            a.map_page(
                virt,
                phys,
                PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE | PageFlags::NO_EXECUTE,
            );
        })
        .expect("spawn_server: no address space");
    }

    // --- Map the heap window ---
    let heap_pages = config.heap_bytes.div_ceil(mm::PAGE_SIZE) as usize;
    for i in 0..heap_pages {
        let phys = mm::frame::allocate_frame().expect("spawn_server: no frame for heap");
        let virt = VirtAddr::new(SERVER_HEAP_VIRT + (i as u64) * mm::PAGE_SIZE);
        process::with_address_space(pid, |a| {
            a.map_page(
                virt,
                phys,
                PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE | PageFlags::NO_EXECUTE,
            );
        })
        .expect("spawn_server: no address space");
    }

    // --- Map the optional shared regions ---
    if let Some(region) = config.shared {
        process::with_address_space(pid, |a| {
            region.map_into(a, VirtAddr::new(SERVER_SHARED_VIRT));
        })
        .expect("spawn_server: no address space");
    }
    if let Some(region) = config.client_shared {
        process::with_address_space(pid, |a| {
            region.map_into(a, VirtAddr::new(SERVER_CLIENT_SHARED_VIRT));
        })
        .expect("spawn_server: no address space");
    }

    // --- Optionally grant a Filesystem capability (before the boot-info write,
    //     so its handle can be passed to the server). ---
    let fs_cap_handle = if let Some(mount_id) = config.filesystem_mount {
        process::with_cspace_mut(pid, |cspace| {
            cspace
                .insert(Capability {
                    cap_type: CapType::Filesystem { mount_id },
                    rights: CapRights::READ | CapRights::WRITE,
                    parent: None,
                })
                .map(|h| h.as_raw() as u64)
                .unwrap_or(0)
        })
        .unwrap_or(0)
    } else {
        0
    };

    // --- Map and fill the boot-info page ---
    let bootinfo_phys = mm::frame::allocate_frame().expect("spawn_server: no frame for bootinfo");
    let boot_info = ServerBootInfo {
        magic: BOOTINFO_MAGIC,
        fs_endpoint: config.fs_endpoint,
        block_endpoint: config.block_endpoint,
        shared_vaddr: if config.shared.is_some() { SERVER_SHARED_VIRT } else { 0 },
        shared_size: config.shared.map_or(0, |r| r.size),
        client_shared_vaddr: if config.client_shared.is_some() { SERVER_CLIENT_SHARED_VIRT } else { 0 },
        client_shared_size: config.client_shared.map_or(0, |r| r.size),
        heap_vaddr: SERVER_HEAP_VIRT,
        heap_size: (heap_pages as u64) * mm::PAGE_SIZE,
        arg0: config.arg0,
        arg1: config.arg1,
        fs_cap_handle,
    };
    // SAFETY: bootinfo_phys is a fresh HHDM-mapped frame; ServerBootInfo fits in
    // one page.
    unsafe {
        core::ptr::write_volatile(bootinfo_phys.to_virt().as_mut_ptr::<ServerBootInfo>(), boot_info);
    }
    process::with_address_space(pid, |a| {
        a.map_page(
            VirtAddr::new(SERVER_BOOTINFO_VIRT),
            bootinfo_phys,
            PageFlags::PRESENT | PageFlags::USER | PageFlags::NO_EXECUTE,
        );
    })
    .expect("spawn_server: no address space");

    // --- Record capabilities in the server's CSpace ---
    //
    // IPC syscalls currently key on raw endpoint ids (Phase 2 design), so these
    // capabilities are not yet enforced on the IPC path — but recording them
    // models the resources the server legitimately holds and sets up the
    // capability-checked filesystem syscalls of sub-phase 3.8.
    process::with_cspace_mut(pid, |cspace| {
        let _ = cspace.insert(Capability {
            cap_type: CapType::Endpoint { endpoint_id: config.fs_endpoint, badge: 0 },
            rights: CapRights::READ | CapRights::WRITE,
            parent: None,
        });
        if config.block_endpoint != 0 {
            let _ = cspace.insert(Capability {
                cap_type: CapType::Endpoint { endpoint_id: config.block_endpoint, badge: 0 },
                rights: CapRights::READ | CapRights::WRITE,
                parent: None,
            });
        }
        if let Some(region) = config.shared {
            let _ = cspace.insert(Capability {
                cap_type: region.cap_type(pid.as_usize()),
                rights: CapRights::READ | CapRights::WRITE,
                parent: None,
            });
        }
    })
    .expect("spawn_server: no CSpace");

    // --- Start the server in ring 3 ---
    sched::spawn_in_process(config.name, server_trampoline, pid);

    pid
}

/// Trampoline that drops the freshly-spawned task into ring 3 at the server's
/// entry point. Runs in kernel mode within the server's address space; builds
/// an `iretq` frame and never returns.
///
/// All servers share this trampoline because they all load at the same fixed
/// `SERVER_CODE_VIRT` with the same `SERVER_STACK_TOP`.
fn server_trampoline() {
    let user_rip = SERVER_CODE_VIRT;
    let user_rsp = SERVER_STACK_TOP;
    // SAFETY: the code/stack pages are mapped USER in this process's address
    // space; the selectors match the ring-3 GDT entries (DPL=3).
    unsafe {
        core::arch::asm!(
            "push {ss}",
            "push {rsp}",
            "push {rflags}",
            "push {cs}",
            "push {rip}",
            "iretq",
            ss = in(reg) 0x1Bu64,       // USER_DATA_SELECTOR | RPL=3
            rsp = in(reg) user_rsp,
            rflags = in(reg) 0x202u64,  // IF=1 + reserved bit 1
            cs = in(reg) 0x23u64,       // USER_CODE_SELECTOR | RPL=3
            rip = in(reg) user_rip,
            options(noreturn),
        );
    }
}

/// A monotonically increasing counter for minting distinct server endpoint
/// names is unnecessary (callers pass explicit endpoints), but a small helper
/// keeps an unused-warning-free seam for future per-server identifiers.
#[allow(dead_code)]
static SERVER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Reserve and return the next server sequence number (unused for now).
#[allow(dead_code)]
pub fn next_server_seq() -> u64 {
    SERVER_SEQ.fetch_add(1, Ordering::SeqCst)
}
