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
// Servers have no Rust runtime; libthemelios provides `_start` as the entry.

extern crate alloc;

use core::panic::PanicInfo;

use linked_list_allocator::LockedHeap;

pub mod block_proto;
pub mod fs_proto;

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
}

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
