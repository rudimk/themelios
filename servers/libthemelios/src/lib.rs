//! # libthemelios — ThemeliOS userspace server runtime
//!
//! Every ThemeliOS userspace server (the filesystem servers in sub-phases
//! 3.5–3.7, and the echo server used to validate the framework) links against
//! this crate. It provides everything a freestanding ring-3 program needs that
//! a hosted Rust program would get from `std`:
//!
//! - **`_start`**: the program entry point the kernel jumps to. It reads the
//!   boot-info page the kernel prepared, initialises the heap, and calls the
//!   server's `server_main`.
//! - **Syscall wrappers**: safe Rust functions over the raw `syscall`
//!   instruction (IPC send/receive/call/reply, yield, exit, debug print).
//! - **Global allocator**: a heap over the window the kernel maps for the
//!   server, so `alloc` types (`Vec`, `Box`, `String`) work.
//! - **Panic handler**: prints the panic message over the debug-print syscall
//!   and exits — a server panic must never take down the kernel.
//! - **Protocol types**: the IPC message shape and the block/filesystem request
//!   opcodes shared with the kernel.
//!
//! ## How a server uses it
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//! use libthemelios::{boot_info, ipc};
//!
//! #[no_mangle]
//! pub extern "C" fn server_main() -> ! {
//!     let info = boot_info();
//!     loop {
//!         let req = ipc::receive(info.fs_endpoint);
//!         // ...handle req...
//!         ipc::reply(info.fs_endpoint, req.reply_token, [0, 0, 0, 0]);
//!     }
//! }
//! ```

#![no_std]
// A custom allocation-error handler (below) so an out-of-memory condition in a
// server exits cleanly instead of aborting — an abort is a ring-3 fault, which the
// kernel treats as fatal. Still unstable, hence the feature gate.
#![feature(alloc_error_handler)]
// Servers have no Rust runtime; libthemelios provides `_start` as the entry.

extern crate alloc;

use core::panic::PanicInfo;

use linked_list_allocator::LockedHeap;

pub mod block_proto;
pub mod fs_proto;
pub mod net_proto;

/// The HTTP/1.1 request parser + response builder, **single-sourced** from the
/// kernel's `http` module (Phase 6.5). Both are `no_std`/`alloc`-only with zero
/// kernel coupling, so the ring-3 `api-server` compiles the exact same source the
/// kernel does — one implementation, no drift (unlike the hand-duplicated proto
/// tables, this is untrusted-input parser logic where a divergent security fix
/// would be silent).
#[path = "../../../kernel/src/http/mod.rs"]
pub mod http;

/// The minimal JSON parser/serializer, **single-sourced** from the kernel's
/// `oci::json` module (Phase 6.5b). Like `http` it is `alloc`-only with zero kernel
/// coupling, so the api-server parses request bodies (`POST /containers/create`)
/// with the exact same `parse` the kernel uses — one implementation, no drift. Its
/// `MAX_DEPTH` guard bounds the recursion on hostile input.
#[path = "../../../kernel/src/oci/json.rs"]
pub mod json;

// ----- Boot info -----

/// Fixed virtual address where the kernel maps the server's boot-info page.
///
/// MUST match `SERVER_BOOTINFO_VIRT` in the kernel's server loader. The kernel
/// fills in a [`BootInfo`] here before starting the server; `_start` reads it.
pub const BOOTINFO_VIRT: u64 = 0x30_0000;

/// Magic value identifying a valid boot-info page (`"THMSBOOT"` little-endian).
pub const BOOTINFO_MAGIC: u64 = 0x544F_4F42_534D_4854;

/// Startup parameters the kernel passes to a server through the boot-info page.
///
/// `#[repr(C)]` with a fixed field order so the kernel and this crate agree on
/// the layout byte-for-byte. Keep this struct in sync with the kernel's
/// `ServerBootInfo` in `kernel/src/process/server.rs`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootInfo {
    /// `BOOTINFO_MAGIC` — sanity-checks that the page was populated.
    pub magic: u64,
    /// IPC endpoint this server receives requests on.
    pub fs_endpoint: u64,
    /// IPC endpoint of the kernel block server (0 if this server needs none).
    pub block_endpoint: u64,
    /// Virtual address of the **block** shared region (shared with the kernel
    /// block server, for disk-block transfers). 0 if the server needs none.
    pub shared_vaddr: u64,
    /// Size of the block shared region in bytes.
    pub shared_size: u64,
    /// Virtual address of the **client** shared region (shared with this
    /// server's clients, for paths and file data). 0 if unused.
    pub client_shared_vaddr: u64,
    /// Size of the client shared region in bytes.
    pub client_shared_size: u64,
    /// Virtual address of the server's heap window.
    pub heap_vaddr: u64,
    /// Size of the heap window in bytes.
    pub heap_size: u64,
    /// Server-specific argument 0 (e.g. a mount id or backing endpoint).
    pub arg0: u64,
    /// Server-specific argument 1.
    pub arg1: u64,
    /// Capability handle of a `Filesystem` capability granted to this process
    /// (0 if none). Used with the filesystem syscalls (`open`, `stat`, …).
    pub fs_cap_handle: u64,
    /// Capability handle of a `Management` capability granted to this process
    /// (0 if none — an unambiguous null handle). Used with `SYS_MGMT`
    /// (`syscall::mgmt_listen`, …) by trusted control-plane servers (Phase 6.4).
    pub mgmt_cap_handle: u64,
}

/// Lock the kernel↔userspace boot-info layout: the two `#[repr(C)]` structs
/// (`ServerBootInfo` in the kernel and this `BootInfo`) must stay byte-identical.
/// 13 `u64` fields × 8 bytes = 104. If either side adds/removes a field without the
/// other, this fails to compile — catching the drift at build time.
const _: () = assert!(core::mem::size_of::<BootInfo>() == 104);

/// Read the boot-info page the kernel populated for this server.
///
/// Panics if the magic value is wrong (the page wasn't set up correctly).
pub fn boot_info() -> BootInfo {
    // SAFETY: the kernel maps and fills this page before starting the server.
    let info = unsafe { core::ptr::read_volatile(BOOTINFO_VIRT as *const BootInfo) };
    assert!(info.magic == BOOTINFO_MAGIC, "libthemelios: bad boot-info magic");
    info
}

// ----- Global allocator -----

/// The server's global heap allocator, backed by the kernel-provided heap
/// window. Initialised in `_start` from the boot info.
#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

// ----- Entry point -----

extern "C" {
    /// Each server defines this; it is the server's real `main`. It must not
    /// return (servers loop forever handling requests).
    fn server_main() -> !;
}

/// The program entry point. The kernel loads the server's flat binary at
/// `SERVER_CODE_VIRT` and jumps here with the stack pointer set.
///
/// Placed in `.text.start` so the linker script lays it down at the very start
/// of the image (the kernel jumps to the load base). It reads the boot info,
/// initialises the heap, and hands control to `server_main`.
#[no_mangle]
#[link_section = ".text.start"]
pub extern "C" fn _start() -> ! {
    let info = boot_info();

    // Initialise the heap over the kernel-provided window.
    // SAFETY: the kernel mapped [heap_vaddr, heap_vaddr + heap_size) as writable
    // user memory for this server, exclusively ours.
    unsafe {
        ALLOCATOR
            .lock()
            .init(info.heap_vaddr as *mut u8, info.heap_size as usize);
    }

    // SAFETY: provided by the server crate; never returns.
    unsafe { server_main() }
}

// ----- Panic handler -----

/// Panic handler: report the panic over the debug-print syscall, then exit.
///
/// A server panic is contained — it terminates only this ring-3 process, never
/// the kernel. That isolation is the whole point of running filesystem code in
/// userspace.
/// Allocation-error handler: fired when the heap window is exhausted. Without it,
/// `alloc` aborts (a ring-3 fault → the kernel halts the whole machine to its
/// watchdog). Instead we report over serial and `exit` cleanly, so a server that
/// runs out of memory (e.g. the api-server on a hostile request) terminates only
/// itself — a bounded caller/test observes the exit, never a hang.
#[alloc_error_handler]
fn on_oom(_layout: core::alloc::Layout) -> ! {
    debug_print("\n[server oom]\n");
    syscall::exit(2)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    debug_print("\n[server panic] ");
    // The message payload isn't always a plain &str; print the location if we
    // have it, which is cheap and always available.
    if let Some(loc) = info.location() {
        debug_print(loc.file());
        debug_print(":");
        print_u64(loc.line() as u64);
    }
    debug_print("\n");
    syscall::exit(1)
}

/// Print a string over the debug-print syscall, one byte at a time.
pub fn debug_print(s: &str) {
    for b in s.bytes() {
        syscall::debug_print_char(b);
    }
}

/// Print a u64 in decimal over the debug-print syscall (no alloc, for panics).
fn print_u64(mut n: u64) {
    if n == 0 {
        syscall::debug_print_char(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    for &b in &buf[i..] {
        syscall::debug_print_char(b);
    }
}

// ----- IPC message type -----

/// A four-word IPC message, mirroring the kernel's `IpcMessage`.
///
/// Bulk data does not travel in these words — it goes through the shared memory
/// region. The words carry a small request/response header.
#[derive(Clone, Copy, Debug)]
pub struct IpcMessage {
    /// The four message words.
    pub words: [u64; 4],
    /// Sender badge (set by the kernel; identifies the sender).
    pub badge: u64,
    /// Reply token — pass this to `ipc::reply` to answer a received call.
    pub reply_token: u64,
}

// ----- Syscall wrappers -----

/// Thin safe wrappers over the raw `syscall` instruction.
///
/// The ABI matches the kernel's dispatcher (`arch/x86_64/syscall.rs`): the
/// syscall number is in RAX, arguments in RDI/RSI/RDX/R10/R9/R8, and `syscall`
/// clobbers RCX (return RIP) and R11 (saved RFLAGS).
pub mod syscall {
    use super::IpcMessage;

    const SYS_SEND: u64 = 1;
    const SYS_RECEIVE: u64 = 2;
    const SYS_CALL: u64 = 3;
    const SYS_REPLY: u64 = 4;
    const SYS_YIELD: u64 = 5;
    const SYS_EXIT: u64 = 6;
    const SYS_DEBUG_PRINT: u64 = 7;

    /// Send a message to `endpoint` with `badge`. Returns 0 on success.
    pub fn send(endpoint: u64, words: [u64; 4], badge: u64) -> u64 {
        let ret: u64;
        // SAFETY: a syscall with the kernel's documented SEND register ABI.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_SEND => ret,
                in("rdi") endpoint,
                in("rsi") words[0],
                in("rdx") words[1],
                in("r10") words[2],
                in("r9") words[3],
                in("r8") badge,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Block until a message arrives on `endpoint`, then return it.
    pub fn receive(endpoint: u64) -> IpcMessage {
        let w0: u64;
        let w1: u64;
        let w2: u64;
        let w3: u64;
        let badge: u64;
        let token: u64;
        // SAFETY: RECEIVE register ABI; kernel returns words/badge/token in
        // RAX/RDI/RSI/RDX/R8/R9.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_RECEIVE => w0,
                inout("rdi") endpoint => w1,
                out("rsi") w2,
                out("rdx") w3,
                out("r8") badge,
                out("r9") token,
                out("rcx") _, out("r11") _, out("r10") _,
                options(nostack),
            );
        }
        IpcMessage { words: [w0, w1, w2, w3], badge, reply_token: token }
    }

    /// Send a request and block for the reply (RPC). Returns the reply words.
    pub fn call(endpoint: u64, words: [u64; 4], badge: u64) -> IpcMessage {
        let w0: u64;
        let w1: u64;
        let w2: u64;
        let w3: u64;
        // SAFETY: CALL register ABI; kernel returns reply words in
        // RAX/RDI/RSI/RDX.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_CALL => w0,
                inout("rdi") endpoint => w1,
                inout("rsi") words[0] => w2,
                inout("rdx") words[1] => w3,
                in("r10") words[2],
                in("r9") words[3],
                in("r8") badge,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        IpcMessage { words: [w0, w1, w2, w3], badge: 0, reply_token: 0 }
    }

    /// Reply to a received call, unblocking the caller.
    pub fn reply(endpoint: u64, reply_token: u64, words: [u64; 4]) -> u64 {
        let ret: u64;
        // SAFETY: REPLY register ABI.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_REPLY => ret,
                in("rdi") endpoint,
                in("rsi") reply_token,
                in("rdx") words[0],
                in("r10") words[1],
                in("r9") words[2],
                in("r8") words[3],
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Yield the current time slice to the scheduler.
    pub fn yield_now() {
        // SAFETY: YIELD takes no arguments.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_YIELD => _,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
    }

    /// Milliseconds since boot (monotonic). Drives smoltcp's `Instant` clock in
    /// the net server's poll loop.
    const SYS_UPTIME_MS: u64 = 14;
    pub fn uptime_ms() -> u64 {
        let ms: u64;
        // SAFETY: UPTIME_MS takes no arguments and returns the value in RAX.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_UPTIME_MS => ms,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ms
    }

    /// Non-blocking IPC receive (SYS_TRY_RECEIVE = 20). Returns `Some(msg)` if a
    /// message was waiting, `None` otherwise — never blocks. A polling server
    /// (the net server) uses this to serve requests on its endpoint while
    /// continuously driving smoltcp; a blocking `receive` would stall the poll
    /// loop. On return RAX = 1/0 (had message?), with the words in
    /// RDI/RSI/RDX/R8 and the reply token in R9.
    const SYS_TRY_RECEIVE: u64 = 20;
    pub fn try_receive(endpoint: u64) -> Option<IpcMessage> {
        let has: u64;
        let w0: u64;
        let w1: u64;
        let w2: u64;
        let w3: u64;
        let token: u64;
        // SAFETY: TRY_RECEIVE takes the endpoint in RDI and returns the
        // has-message flag in RAX plus the message words/token in the registers
        // above. It never blocks.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_TRY_RECEIVE => has,
                inout("rdi") endpoint => w0,
                out("rsi") w1,
                out("rdx") w2,
                out("r8") w3,
                out("r9") token,
                out("rcx") _, out("r11") _, out("r10") _,
                options(nostack),
            );
        }
        if has == 0 {
            None
        } else {
            Some(IpcMessage { words: [w0, w1, w2, w3], badge: 0, reply_token: token })
        }
    }

    /// Terminate this server process. Never returns.
    pub fn exit(code: u64) -> ! {
        // SAFETY: EXIT terminates the task; the kernel never returns here.
        unsafe {
            core::arch::asm!(
                "syscall",
                in("rax") SYS_EXIT,
                in("rdi") code,
                options(nostack, noreturn),
            );
        }
    }

    // --- Filesystem syscalls (Phase 3) ---
    //
    // Number in RAX, args in RDI/RSI/RDX/R10; result in RAX. A return value with
    // the high bit set is an encoded `fs_proto::FsError`.

    const SYS_OPEN: u64 = 8;
    const SYS_READ_FILE: u64 = 9;
    const SYS_WRITE_FILE: u64 = 10;
    const SYS_CLOSE: u64 = 11;
    const SYS_STAT: u64 = 12;
    const SYS_READDIR: u64 = 13;

    /// Raw 4-argument syscall helper for the filesystem calls.
    #[inline]
    fn fs_syscall(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
        let ret: u64;
        // SAFETY: a syscall with the kernel's documented FS register ABI.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") num => ret,
                in("rdi") a1,
                in("rsi") a2,
                in("rdx") a3,
                in("r10") a4,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Open `path` under the filesystem named by `fs_cap`. Returns a file
    /// descriptor capability handle, or a high-bit-set error.
    pub fn open(fs_cap: u64, path: *const u8, path_len: usize, flags: u64) -> u64 {
        fs_syscall(SYS_OPEN, fs_cap, path as u64, path_len as u64, flags)
    }

    /// Read `len` bytes at `offset` from `fd` into `buf`. Returns bytes read.
    pub fn read_file(fd: u64, buf: *mut u8, len: usize, offset: u64) -> u64 {
        fs_syscall(SYS_READ_FILE, fd, buf as u64, len as u64, offset)
    }

    /// Write `len` bytes at `offset` from `buf` to `fd`. Returns bytes written.
    pub fn write_file(fd: u64, buf: *const u8, len: usize, offset: u64) -> u64 {
        fs_syscall(SYS_WRITE_FILE, fd, buf as u64, len as u64, offset)
    }

    /// Close a file descriptor capability.
    pub fn close(fd: u64) -> u64 {
        fs_syscall(SYS_CLOSE, fd, 0, 0, 0)
    }

    /// Stat `path` under `fs_cap`, writing `[size:u64, is_dir:u64]` to `stat_out`.
    pub fn stat(fs_cap: u64, path: *const u8, path_len: usize, stat_out: *mut u8) -> u64 {
        fs_syscall(SYS_STAT, fs_cap, path as u64, path_len as u64, stat_out as u64)
    }

    /// List directory `fd`: write up to `out_len` bytes of packed entries to
    /// `entries_out`. Returns the entry count.
    pub fn readdir(fd: u64, entries_out: *mut u8, max: u64, out_len: usize) -> u64 {
        fs_syscall(SYS_READDIR, fd, entries_out as u64, max, out_len as u64)
    }

    // --- Socket syscalls (Phase 4.5) ---
    //
    // UDP sockets through the capability-checked API. A return with the high bit
    // set is an encoded `net_proto` socket error. IPv4 addresses are packed
    // `a<<24|b<<16|c<<8|d`.

    const SYS_SOCKET: u64 = 15;
    const SYS_BIND: u64 = 16;
    const SYS_SENDTO: u64 = 17;
    const SYS_RECVFROM: u64 = 18;
    const SYS_SOCKET_CLOSE: u64 = 19;

    /// Create a socket of `sock_type` (0 = UDP) using the network-authority
    /// capability `factory`. Returns a socket capability handle, or a high-bit
    /// error.
    pub fn socket(sock_type: u64, factory: u64) -> u64 {
        // RDI = type, RSI = factory handle.
        let ret: u64;
        // SAFETY: SYS_SOCKET register ABI.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_SOCKET => ret,
                in("rdi") sock_type,
                in("rsi") factory,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Bind socket `sock` to `(local_ip, port)` (local_ip packed; 0 = any).
    pub fn bind(sock: u64, local_ip: u64, port: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_BIND register ABI.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_BIND => ret,
                in("rdi") sock,
                in("rsi") local_ip,
                in("rdx") port,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Send `len` bytes at `buf` from socket `sock` to `(ip, port)` (ip packed).
    /// Returns bytes sent, or a high-bit error.
    pub fn sendto(sock: u64, buf: *const u8, len: usize, ip: u64, port: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_SENDTO register ABI (buf/len validated by the kernel).
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_SENDTO => ret,
                in("rdi") sock,
                in("rsi") buf as u64,
                in("rdx") len as u64,
                in("r10") ip,
                in("r9") port,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Receive a datagram on `sock` into `buf` (up to `len`), writing the source
    /// `[ip:u32_le, port:u16_le]` to `src_out` (8 bytes; pass null to skip).
    /// Returns bytes received, or a high-bit error (WouldBlock if none pending).
    pub fn recvfrom(sock: u64, buf: *mut u8, len: usize, src_out: *mut u8) -> u64 {
        let ret: u64;
        // SAFETY: SYS_RECVFROM register ABI (pointers validated by the kernel).
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_RECVFROM => ret,
                in("rdi") sock,
                in("rsi") buf as u64,
                in("rdx") len as u64,
                in("r10") src_out as u64,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Close socket `sock`.
    pub fn socket_close(sock: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_SOCKET_CLOSE register ABI.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_SOCKET_CLOSE => ret,
                in("rdi") sock,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    // --- TCP stream syscalls (Phase 4.6) ---

    const SYS_CONNECT: u64 = 21;
    const SYS_LISTEN: u64 = 22;
    const SYS_ACCEPT: u64 = 23;
    const SYS_TCP_SEND: u64 = 24;
    const SYS_TCP_RECV: u64 = 25;

    /// `SYS_MGMT` (the op-multiplexed container management ABI) and its verb
    /// selectors. Only `listen` is wired in Phase 6.4; the rest arrive with the
    /// ring-3 api-server (6.5).
    const SYS_MGMT: u64 = 26;
    const MGMT_OP_LISTEN: u64 = 1;
    const MGMT_OP_LIST: u64 = 2;
    const MGMT_OP_INSPECT: u64 = 3;
    const MGMT_OP_NODE_INFO: u64 = 4;
    // Write verbs (Phase 6.5b). These must match the kernel's MGMT_OP_* selectors
    // in arch/x86_64/syscall.rs (kept in sync by hand, like the other proto tables).
    const MGMT_OP_CREATE: u64 = 5;
    const MGMT_OP_START: u64 = 6;
    const MGMT_OP_STOP: u64 = 7;
    const MGMT_OP_LOGS: u64 = 8;

    /// Listen for inbound TCP connections on the bound socket `sock`. Returns 0,
    /// or a high-bit error.
    pub fn listen(sock: u64, backlog: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_LISTEN register ABI.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_LISTEN => ret,
                in("rdi") sock,
                in("rsi") backlog,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Accept one connection on listening socket `sock`, writing the peer
    /// `[ip:u32_le, port:u16_le]` to `peer_out` (8 bytes; pass null to skip).
    /// Returns a new socket capability handle, or a high-bit error (WouldBlock if
    /// none pending).
    pub fn accept(sock: u64, peer_out: *mut u8) -> u64 {
        let ret: u64;
        // SAFETY: SYS_ACCEPT register ABI (peer_out validated by the kernel).
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_ACCEPT => ret,
                in("rdi") sock,
                in("rsi") peer_out as u64,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Begin an outbound TCP connection on `sock` to `(ip, port)` (ip packed).
    /// Non-blocking; returns 0, or a high-bit error.
    pub fn connect(sock: u64, ip: u64, port: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_CONNECT register ABI.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_CONNECT => ret,
                in("rdi") sock,
                in("rsi") ip,
                in("rdx") port,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Send `len` bytes at `buf` on connected socket `sock`. Returns bytes sent,
    /// or a high-bit error (WouldBlock while connecting / window full). Named
    /// distinctly from the IPC [`send`](self::send).
    pub fn tcp_send(sock: u64, buf: *const u8, len: usize) -> u64 {
        let ret: u64;
        // SAFETY: SYS_TCP_SEND register ABI (buf/len validated by the kernel).
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_TCP_SEND => ret,
                in("rdi") sock,
                in("rsi") buf as u64,
                in("rdx") len as u64,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Receive up to `len` bytes into `buf` from connected socket `sock`.
    /// Returns bytes read (0 = peer closed), or a high-bit error (WouldBlock if
    /// connected but no data yet).
    pub fn tcp_recv(sock: u64, buf: *mut u8, len: usize) -> u64 {
        let ret: u64;
        // SAFETY: SYS_TCP_RECV register ABI (pointers validated by the kernel).
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_TCP_RECV => ret,
                in("rdi") sock,
                in("rsi") buf as u64,
                in("rdx") len as u64,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Open an inbound-TCP listener on `port` via the management ABI (Phase 6.4).
    /// `mgmt_cap` is this server's `Management` capability handle (from
    /// `boot_info().mgmt_cap_handle`). Returns a fresh listener socket capability
    /// handle (accept connections on it with [`accept`]), or a high-bit-set
    /// `MgmtError` (bit 63 set; low bits = the discriminant, e.g. 2 =
    /// `PermissionDenied` when the caller lacks the Management cap).
    pub fn mgmt_listen(mgmt_cap: u64, port: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_MGMT register ABI — RDI = verb, RSI = mgmt cap, RDX = port.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_MGMT => ret,
                in("rdi") MGMT_OP_LISTEN,
                in("rsi") mgmt_cap,
                in("rdx") port,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// A `SYS_MGMT` read verb (Phase 6.5) with no input: copy a JSON document into
    /// `out[..out_len]`. Returns bytes written, or a high-bit `MgmtError`
    /// (`BufferTooSmall` = bit 63 | 8 if the document exceeds `out_len`). Used for
    /// `MGMT_OP_LIST` and `MGMT_OP_NODE_INFO`.
    fn mgmt_read0(op: u64, mgmt_cap: u64, out: *mut u8, out_len: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_MGMT register ABI; the kernel validates out[..out_len].
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_MGMT => ret,
                in("rdi") op,
                in("rsi") mgmt_cap,
                in("rdx") 0u64,          // no input ptr
                in("r10") 0u64,          // no input len
                in("r8") out as u64,
                in("r9") out_len,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// List all containers as a JSON array into `out` (`docker ps -a`). See
    /// [`mgmt_read0`] for the return convention.
    pub fn mgmt_list(mgmt_cap: u64, out: *mut u8, out_len: u64) -> u64 {
        mgmt_read0(MGMT_OP_LIST, mgmt_cap, out, out_len)
    }

    /// Node-info JSON object into `out` (`docker info` subset).
    pub fn mgmt_node_info(mgmt_cap: u64, out: *mut u8, out_len: u64) -> u64 {
        mgmt_read0(MGMT_OP_NODE_INFO, mgmt_cap, out, out_len)
    }

    /// Inspect one container by id/name (`id[..id_len]`), writing its JSON detail
    /// object into `out[..out_len]`. Returns bytes written, or a high-bit
    /// `MgmtError` (`NotFound` = bit 63 | 2, `BufferTooSmall` = bit 63 | 8).
    pub fn mgmt_inspect(mgmt_cap: u64, id: *const u8, id_len: u64, out: *mut u8, out_len: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_MGMT register ABI; the kernel validates id[..id_len] and
        // out[..out_len].
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_MGMT => ret,
                in("rdi") MGMT_OP_INSPECT,
                in("rsi") mgmt_cap,
                in("rdx") id as u64,
                in("r10") id_len,
                in("r8") out as u64,
                in("r9") out_len,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Generic `SYS_MGMT` call (Phase 6.5b): input `in[..in_len]` → verb `op` →
    /// output `out[..out_len]`. Returns bytes written (bit 63 clear), or a high-bit
    /// `MgmtError`. The kernel validates both buffers.
    fn mgmt_call(op: u64, mgmt_cap: u64, in_ptr: *const u8, in_len: u64, out: *mut u8, out_len: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_MGMT register ABI.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_MGMT => ret,
                in("rdi") op,
                in("rsi") mgmt_cap,
                in("rdx") in_ptr as u64,
                in("r10") in_len,
                in("r8") out as u64,
                in("r9") out_len,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// Create a container (`docker create`). `spec[..spec_len]` is `"image\0name"`
    /// (NUL-separated; no NUL → empty name = auto-generate). Writes `{"Id":…}` JSON
    /// into `out`. Returns bytes written or a high-bit `MgmtError` (e.g.
    /// `InvalidArgument` = bit 63 | 4 for an empty image).
    pub fn mgmt_create(mgmt_cap: u64, spec: *const u8, spec_len: u64, out: *mut u8, out_len: u64) -> u64 {
        mgmt_call(MGMT_OP_CREATE, mgmt_cap, spec, spec_len, out, out_len)
    }

    /// Start a created container (`docker start`). `id[..id_len]` = id/name. Returns
    /// 0 on success (Docker 204), or a high-bit `MgmtError` (`NotFound` = 2,
    /// `InvalidState` = 3).
    pub fn mgmt_start(mgmt_cap: u64, id: *const u8, id_len: u64) -> u64 {
        mgmt_call(MGMT_OP_START, mgmt_cap, id, id_len, core::ptr::null_mut(), 0)
    }

    /// Stop a container (`docker stop`). Returns 0 on success (204) or a high-bit
    /// `MgmtError`.
    pub fn mgmt_stop(mgmt_cap: u64, id: *const u8, id_len: u64) -> u64 {
        mgmt_call(MGMT_OP_STOP, mgmt_cap, id, id_len, core::ptr::null_mut(), 0)
    }

    /// Read a container's captured logs (`docker logs`). `id[..id_len]` = id/name;
    /// raw bytes into `out` (bounded by the kernel). Returns bytes written or a
    /// high-bit `MgmtError`.
    pub fn mgmt_logs(mgmt_cap: u64, id: *const u8, id_len: u64, out: *mut u8, out_len: u64) -> u64 {
        mgmt_call(MGMT_OP_LOGS, mgmt_cap, id, id_len, out, out_len)
    }

    /// Print a single byte to the kernel serial console (debugging only).
    pub fn debug_print_char(ch: u8) {
        // SAFETY: DEBUG_PRINT takes the character in RDI.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") SYS_DEBUG_PRINT => _,
                in("rdi") ch as u64,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
    }
}

/// Ergonomic IPC helpers re-exported at the crate root.
pub mod ipc {
    pub use super::syscall::{call, receive, reply, send};
    pub use super::IpcMessage;
}
