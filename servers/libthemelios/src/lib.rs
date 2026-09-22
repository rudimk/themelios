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

/// The native syscall numbers, **single-sourced** from the kernel's
/// `arch/syscall_abi.rs` (Phase 8.5b). That file deliberately names nothing from either
/// crate so it can be included from both sides, which makes these numbers one list rather
/// than two lists and a checker — see its module docs for why that distinction was earned
/// rather than assumed.
#[path = "../../../kernel/src/arch/syscall_abi.rs"]
mod syscall_abi;
pub use syscall_abi::abi;

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
    /// Bearer token provisioned to the api-server (Phase 6.6), in the first
    /// `api_token_len` bytes. Populated only for the control plane (spawned with
    /// `grant_management`); zeroed with `api_token_len == 0` otherwise. The api-server
    /// compares the `Authorization: Bearer …` header against these bytes.
    pub api_token: [u8; 32],
    /// Length of the provisioned token in `api_token` (0 = none).
    pub api_token_len: u64,
}

/// Lock the kernel↔userspace boot-info layout: the two `#[repr(C)]` structs
/// (`ServerBootInfo` in the kernel and this `BootInfo`) must stay byte-identical.
/// 13 `u64` fields (104) + `[u8;32]` token (→ 136) + a `u64` len (→ 144). The size
/// assert catches a field added on one side but not the other; the `offset_of!`
/// asserts pin the two *heterogeneous* trailing fields so a cross-struct reorder
/// (which the size check alone would pass, silently corrupting the token) also fails
/// to compile.
const _: () = assert!(core::mem::size_of::<BootInfo>() == 144);
const _: () = assert!(core::mem::offset_of!(BootInfo, api_token) == 104);
const _: () = assert!(core::mem::offset_of!(BootInfo, api_token_len) == 136);

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
    // **The syscall numbers are not declared here.** They come from the kernel's own file,
    // included the same way `http` and `json` are.
    //
    // Until a review of 8.5b they were 26 local `const`s duplicating the kernel's, pinned to
    // nothing. Setting this crate's `SYS_SEND` to 99 built the whole project green; the only
    // symptom was an amd64 test timing out after three minutes, and on aarch64 — where no
    // server runs yet — there would have been none at all. A number meaning two different
    // things on two sides of a boundary is the precise defect 8.5b exists to remove, and a
    // checker comparing two lists only removes it where the checker can see.
    use super::abi::*;

    // ----- The raw syscall primitives -----
    //
    // Every wrapper below is architecture-neutral Rust. These three functions are the only
    // place the register mapping appears, and the only `asm!` in this crate.
    //
    // Until Phase 8.5b there were 25 `asm!` blocks here, one per wrapper, each naming
    // `rax`/`rdi`/`rsi`/`rdx`/`r10`/`r9`/`r8` by hand. Porting that shape would have meant
    // writing 25 more for aarch64 and keeping 50 hand-written register lists in step — and
    // a register list is not something a compiler checks. The failure mode is specific and
    // silent: one wrapper passing an argument in the wrong position compiles, links, and
    // sends the kernel a plausible value in the wrong slot.
    //
    // The ABI is positional on both architectures, so it collapses to this:
    //
    //   |          | x86_64                           | aarch64   |
    //   |----------|----------------------------------|-----------|
    //   | number   | `rax`                            | `x8`      |
    //   | args 0-5 | `rdi`, `rsi`, `rdx`, `r10`, `r9`, `r8` | `x0`-`x5` |
    //   | rets 0-5 | `rax`, `rdi`, `rsi`, `rdx`, `r8`, `r9` | `x0`-`x5` |
    //
    // Two asymmetries on the x86 side are deliberate and were preserved verbatim from the
    // 25 blocks this replaced, not tidied:
    //
    //   * Argument 3 is `r10`, not `rcx`, because the `syscall` instruction overwrites
    //     `rcx` with the return address. `r11` is likewise destroyed (it takes RFLAGS), so
    //     both are declared clobbered.
    //   * Argument position 4 is `r9` and 5 is `r8`, while *return* position 4 is `r8` and
    //     5 is `r9`. The in and out mappings genuinely differ for those two registers. On
    //     aarch64 both are simply `x4`/`x5`.
    //
    // `nostack` holds on both: none of these touch the stack red zone.

    /// A syscall returning a single value in return position 0.
    ///
    /// Unused argument positions are passed as zero rather than left undefined. Slightly
    /// more work than the old per-wrapper blocks, which simply did not set registers they
    /// had no use for — and worth it: an argument register the caller leaves alone holds
    /// whatever the last call left there, so a kernel that later starts reading that
    /// position sees stale data rather than an obvious zero.
    #[cfg(target_arch = "x86_64")]
    #[inline]
    unsafe fn sys(nr: u64, a: [u64; 6]) -> u64 {
        let ret: u64;
        // SAFETY: the caller guarantees `nr` and `a` form a valid call under the kernel's
        // documented ABI; `rcx` and `r11` are declared clobbered as `syscall` requires.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") nr => ret,
                in("rdi") a[0], in("rsi") a[1], in("rdx") a[2],
                in("r10") a[3], in("r9") a[4], in("r8") a[5],
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        ret
    }

    /// A syscall returning up to six values, in return positions 0-5.
    ///
    /// Callers that want fewer simply ignore the tail; `SYS_CALL` uses the first four and
    /// `SYS_RECEIVE` all six.
    #[cfg(target_arch = "x86_64")]
    #[inline]
    unsafe fn sys_multi(nr: u64, a: [u64; 6]) -> [u64; 6] {
        let (r0, r1, r2, r3, r4, r5): (u64, u64, u64, u64, u64, u64);
        // SAFETY: as `sys`. Note `r9`/`r8` carry argument positions 4/5 on the way in and
        // return positions 5/4 on the way out, which is why their `inout` pairs look
        // crossed — see the table above.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") nr => r0,
                inout("rdi") a[0] => r1,
                inout("rsi") a[1] => r2,
                inout("rdx") a[2] => r3,
                in("r10") a[3],
                inout("r9") a[4] => r5,
                inout("r8") a[5] => r4,
                out("rcx") _, out("r11") _,
                options(nostack),
            );
        }
        [r0, r1, r2, r3, r4, r5]
    }

    /// A syscall that does not return — `SYS_EXIT`.
    #[cfg(target_arch = "x86_64")]
    #[inline]
    unsafe fn sys_noreturn(nr: u64, a: [u64; 6]) -> ! {
        // SAFETY: the kernel never returns to the caller of this syscall.
        unsafe {
            core::arch::asm!(
                "syscall",
                in("rax") nr,
                in("rdi") a[0], in("rsi") a[1], in("rdx") a[2],
                in("r10") a[3], in("r9") a[4], in("r8") a[5],
                options(nostack, noreturn),
            );
        }
    }

    /// A syscall returning a single value in `x0`.
    ///
    /// `x8` carries the number because that is what Linux and `asm-generic` use on
    /// aarch64, so every toolchain, debugger and `strace` already agrees — see the module
    /// docs in the kernel's `arch/aarch64/syscall.rs`. The `svc` immediate is ignored.
    #[cfg(target_arch = "aarch64")]
    #[inline]
    unsafe fn sys(nr: u64, a: [u64; 6]) -> u64 {
        let ret: u64;
        // SAFETY: the caller guarantees `nr` and `a` form a valid call under the kernel's
        // documented ABI. `svc` clobbers no general-purpose register beyond those named.
        unsafe {
            core::arch::asm!(
                "svc #0",
                in("x8") nr,
                inout("x0") a[0] => ret,
                in("x1") a[1], in("x2") a[2], in("x3") a[3],
                in("x4") a[4], in("x5") a[5],
                options(nostack),
            );
        }
        ret
    }

    /// A syscall returning up to six values, in `x0`-`x5`.
    #[cfg(target_arch = "aarch64")]
    #[inline]
    unsafe fn sys_multi(nr: u64, a: [u64; 6]) -> [u64; 6] {
        let (r0, r1, r2, r3, r4, r5): (u64, u64, u64, u64, u64, u64);
        // SAFETY: as `sys`.
        unsafe {
            core::arch::asm!(
                "svc #0",
                in("x8") nr,
                inout("x0") a[0] => r0,
                inout("x1") a[1] => r1,
                inout("x2") a[2] => r2,
                inout("x3") a[3] => r3,
                inout("x4") a[4] => r4,
                inout("x5") a[5] => r5,
                options(nostack),
            );
        }
        [r0, r1, r2, r3, r4, r5]
    }

    /// A syscall that does not return — `SYS_EXIT`.
    #[cfg(target_arch = "aarch64")]
    #[inline]
    unsafe fn sys_noreturn(nr: u64, a: [u64; 6]) -> ! {
        // SAFETY: the kernel never returns to the caller of this syscall.
        unsafe {
            core::arch::asm!(
                "svc #0",
                in("x8") nr,
                in("x0") a[0], in("x1") a[1], in("x2") a[2],
                in("x3") a[3], in("x4") a[4], in("x5") a[5],
                options(nostack, noreturn),
            );
        }
    }


    /// Send a message to `endpoint` with `badge`. Returns 0 on success.
    pub fn send(endpoint: u64, words: [u64; 4], badge: u64) -> u64 {
        let ret: u64;
        // SAFETY: a syscall with the kernel's documented SEND register ABI.
        ret = unsafe { sys(SYS_SEND, [endpoint, words[0], words[1], words[2], words[3], badge]) };
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
        let __r = unsafe { sys_multi(SYS_RECEIVE, [endpoint, 0, 0, 0, 0, 0]) };
            w0 = __r[0];
            w1 = __r[1];
            w2 = __r[2];
            w3 = __r[3];
            badge = __r[4];
            token = __r[5];
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
        let __r = unsafe { sys_multi(SYS_CALL, [endpoint, words[0], words[1], words[2], words[3], badge]) };
            w0 = __r[0];
            w1 = __r[1];
            w2 = __r[2];
            w3 = __r[3];
        IpcMessage { words: [w0, w1, w2, w3], badge: 0, reply_token: 0 }
    }

    /// Reply to a received call, unblocking the caller.
    pub fn reply(endpoint: u64, reply_token: u64, words: [u64; 4]) -> u64 {
        let ret: u64;
        // SAFETY: REPLY register ABI.
        ret = unsafe { sys(SYS_REPLY, [endpoint, reply_token, words[0], words[1], words[2], words[3]]) };
        ret
    }

    /// Yield the current time slice to the scheduler.
    pub fn yield_now() {
        // SAFETY: YIELD takes no arguments.
        unsafe { sys(SYS_YIELD, [0, 0, 0, 0, 0, 0]) };
    }

    /// Milliseconds since boot (monotonic). Drives smoltcp's `Instant` clock in
    /// the net server's poll loop.
    pub fn uptime_ms() -> u64 {
        let ms: u64;
        // SAFETY: UPTIME_MS takes no arguments and returns the value in RAX.
        ms = unsafe { sys(SYS_UPTIME_MS, [0, 0, 0, 0, 0, 0]) };
        ms
    }

    /// Non-blocking IPC receive (SYS_TRY_RECEIVE = 20). Returns `Some(msg)` if a
    /// message was waiting, `None` otherwise — never blocks. A polling server
    /// (the net server) uses this to serve requests on its endpoint while
    /// continuously driving smoltcp; a blocking `receive` would stall the poll
    /// loop. On return RAX = 1/0 (had message?), with the words in
    /// RDI/RSI/RDX/R8 and the reply token in R9.
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
        let __r = unsafe { sys_multi(SYS_TRY_RECEIVE, [endpoint, 0, 0, 0, 0, 0]) };
            has = __r[0];
            w0 = __r[1];
            w1 = __r[2];
            w2 = __r[3];
            w3 = __r[4];
            token = __r[5];
        if has == 0 {
            None
        } else {
            Some(IpcMessage { words: [w0, w1, w2, w3], badge: 0, reply_token: token })
        }
    }

    /// Terminate this server process. Never returns.
    pub fn exit(code: u64) -> ! {
        // SAFETY: EXIT terminates the task; the kernel never returns here.
        unsafe { sys_noreturn(SYS_EXIT, [code, 0, 0, 0, 0, 0]) }
    }

    // --- Filesystem syscalls (Phase 3) ---
    //
    // Number in RAX, args in RDI/RSI/RDX/R10; result in RAX. A return value with
    // the high bit set is an encoded `fs_proto::FsError`.


    /// Raw 4-argument syscall helper for the filesystem calls.
    #[inline]
    fn fs_syscall(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
        let ret: u64;
        // SAFETY: a syscall with the kernel's documented FS register ABI.
        ret = unsafe { sys(num, [a1, a2, a3, a4, 0, 0]) };
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


    /// Create a socket of `sock_type` (0 = UDP) using the network-authority
    /// capability `factory`. Returns a socket capability handle, or a high-bit
    /// error.
    pub fn socket(sock_type: u64, factory: u64) -> u64 {
        // RDI = type, RSI = factory handle.
        let ret: u64;
        // SAFETY: SYS_SOCKET register ABI.
        ret = unsafe { sys(SYS_SOCKET, [sock_type, factory, 0, 0, 0, 0]) };
        ret
    }

    /// Bind socket `sock` to `(local_ip, port)` (local_ip packed; 0 = any).
    pub fn bind(sock: u64, local_ip: u64, port: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_BIND register ABI.
        ret = unsafe { sys(SYS_BIND, [sock, local_ip, port, 0, 0, 0]) };
        ret
    }

    /// Send `len` bytes at `buf` from socket `sock` to `(ip, port)` (ip packed).
    /// Returns bytes sent, or a high-bit error.
    pub fn sendto(sock: u64, buf: *const u8, len: usize, ip: u64, port: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_SENDTO register ABI (buf/len validated by the kernel).
        ret = unsafe { sys(SYS_SENDTO, [sock, buf as u64, len as u64, ip, port, 0]) };
        ret
    }

    /// Receive a datagram on `sock` into `buf` (up to `len`), writing the source
    /// `[ip:u32_le, port:u16_le]` to `src_out` (8 bytes; pass null to skip).
    /// Returns bytes received, or a high-bit error (WouldBlock if none pending).
    pub fn recvfrom(sock: u64, buf: *mut u8, len: usize, src_out: *mut u8) -> u64 {
        let ret: u64;
        // SAFETY: SYS_RECVFROM register ABI (pointers validated by the kernel).
        ret = unsafe { sys(SYS_RECVFROM, [sock, buf as u64, len as u64, src_out as u64, 0, 0]) };
        ret
    }

    /// Close socket `sock`.
    pub fn socket_close(sock: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_SOCKET_CLOSE register ABI.
        ret = unsafe { sys(SYS_SOCKET_CLOSE, [sock, 0, 0, 0, 0, 0]) };
        ret
    }

    // --- TCP stream syscalls (Phase 4.6) ---


    /// `SYS_MGMT` (the op-multiplexed container management ABI) and its verb
    /// selectors. Only `listen` is wired in Phase 6.4; the rest arrive with the
    /// ring-3 api-server (6.5).
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
    // Auth-rejection audit verb (Phase 6.6).
    const MGMT_OP_AUDIT_DENY: u64 = 9;

    /// Listen for inbound TCP connections on the bound socket `sock`. Returns 0,
    /// or a high-bit error.
    pub fn listen(sock: u64, backlog: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_LISTEN register ABI.
        ret = unsafe { sys(SYS_LISTEN, [sock, backlog, 0, 0, 0, 0]) };
        ret
    }

    /// Accept one connection on listening socket `sock`, writing the peer
    /// `[ip:u32_le, port:u16_le]` to `peer_out` (8 bytes; pass null to skip).
    /// Returns a new socket capability handle, or a high-bit error (WouldBlock if
    /// none pending).
    pub fn accept(sock: u64, peer_out: *mut u8) -> u64 {
        let ret: u64;
        // SAFETY: SYS_ACCEPT register ABI (peer_out validated by the kernel).
        ret = unsafe { sys(SYS_ACCEPT, [sock, peer_out as u64, 0, 0, 0, 0]) };
        ret
    }

    /// Begin an outbound TCP connection on `sock` to `(ip, port)` (ip packed).
    /// Non-blocking; returns 0, or a high-bit error.
    pub fn connect(sock: u64, ip: u64, port: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_CONNECT register ABI.
        ret = unsafe { sys(SYS_CONNECT, [sock, ip, port, 0, 0, 0]) };
        ret
    }

    /// Send `len` bytes at `buf` on connected socket `sock`. Returns bytes sent,
    /// or a high-bit error (WouldBlock while connecting / window full). Named
    /// distinctly from the IPC [`send`](self::send).
    pub fn tcp_send(sock: u64, buf: *const u8, len: usize) -> u64 {
        let ret: u64;
        // SAFETY: SYS_TCP_SEND register ABI (buf/len validated by the kernel).
        ret = unsafe { sys(SYS_TCP_SEND, [sock, buf as u64, len as u64, 0, 0, 0]) };
        ret
    }

    /// Receive up to `len` bytes into `buf` from connected socket `sock`.
    /// Returns bytes read (0 = peer closed), or a high-bit error (WouldBlock if
    /// connected but no data yet).
    pub fn tcp_recv(sock: u64, buf: *mut u8, len: usize) -> u64 {
        let ret: u64;
        // SAFETY: SYS_TCP_RECV register ABI (pointers validated by the kernel).
        ret = unsafe { sys(SYS_TCP_RECV, [sock, buf as u64, len as u64, 0, 0, 0]) };
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
        ret = unsafe { sys(SYS_MGMT, [MGMT_OP_LISTEN, mgmt_cap, port, 0, 0, 0]) };
        ret
    }

    /// A `SYS_MGMT` read verb (Phase 6.5) with no input: copy a JSON document into
    /// `out[..out_len]`. Returns bytes written, or a high-bit `MgmtError`
    /// (`BufferTooSmall` = bit 63 | 8 if the document exceeds `out_len`). Used for
    /// `MGMT_OP_LIST` and `MGMT_OP_NODE_INFO`.
    fn mgmt_read0(op: u64, mgmt_cap: u64, out: *mut u8, out_len: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_MGMT register ABI; the kernel validates out[..out_len].
        ret = unsafe { sys(SYS_MGMT, [op, mgmt_cap, 0u64, 0u64, out_len, out as u64]) };
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
        ret = unsafe { sys(SYS_MGMT, [MGMT_OP_INSPECT, mgmt_cap, id as u64, id_len, out_len, out as u64]) };
        ret
    }

    /// Generic `SYS_MGMT` call (Phase 6.5b): input `in[..in_len]` → verb `op` →
    /// output `out[..out_len]`. Returns bytes written (bit 63 clear), or a high-bit
    /// `MgmtError`. The kernel validates both buffers.
    fn mgmt_call(op: u64, mgmt_cap: u64, in_ptr: *const u8, in_len: u64, out: *mut u8, out_len: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_MGMT register ABI.
        ret = unsafe { sys(SYS_MGMT, [op, mgmt_cap, in_ptr as u64, in_len, out_len, out as u64]) };
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

    /// Record a bearer-token auth rejection (Phase 6.6): the api-server calls this
    /// when it turns away an unauthenticated / wrong-token request, so a failed auth
    /// attempt is audited on the same ABI as a successful op. Returns 0 on success or
    /// a high-bit `MgmtError`.
    pub fn mgmt_audit_deny(mgmt_cap: u64) -> u64 {
        let ret: u64;
        // SAFETY: SYS_MGMT register ABI — RDI = verb, RSI = mgmt cap; no other args.
        ret = unsafe { sys(SYS_MGMT, [MGMT_OP_AUDIT_DENY, mgmt_cap, 0, 0, 0, 0]) };
        ret
    }

    /// Print a single byte to the kernel serial console (debugging only).
    pub fn debug_print_char(ch: u8) {
        // SAFETY: DEBUG_PRINT takes the character in RDI.
        unsafe { sys(SYS_DEBUG_PRINT, [ch as u64, 0, 0, 0, 0, 0]) };
    }
}

/// Ergonomic IPC helpers re-exported at the crate root.
pub mod ipc {
    pub use super::syscall::{call, receive, reply, send};
    pub use super::IpcMessage;
}
