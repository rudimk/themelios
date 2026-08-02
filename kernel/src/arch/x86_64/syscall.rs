//! # System call entry/exit via `syscall`/`sysret`
//!
//! Implements the fast system call path using the x86_64 `syscall` instruction.
//! This is the primary interface for userspace processes to request kernel
//! services (memory mapping, IPC, capability operations, etc.).
//!
//! ## How syscall/sysret works
//!
//! The `syscall` instruction is a fast ring transition mechanism that:
//! 1. Saves the return RIP in RCX and RFLAGS in R11
//! 2. Masks RFLAGS with IA32_FMASK (we clear IF to disable interrupts)
//! 3. Loads CS and SS from STAR[47:32] (kernel segments)
//! 4. Jumps to the address in IA32_LSTAR (our syscall_entry stub)
//!
//! The `sysret` instruction reverses this:
//! 1. Restores RIP from RCX and RFLAGS from R11
//! 2. Loads CS and SS from STAR[63:48] (user segments, with RPL=3 ORed in)
//! 3. Resumes execution at the saved RIP in ring 3
//!
//! ## Stack management
//!
//! `syscall` does NOT change RSP — the CPU is still using the user stack.
//! The entry stub must immediately switch to the kernel stack:
//! 1. `swapgs` — swap user GS base for kernel GS base (PerCpu pointer)
//! 2. Save user RSP to PerCpu scratch space via `gs:[8]`
//! 3. Load kernel RSP from `gs:[0]` (PerCpu.kernel_stack_top)
//! 4. Push a SyscallFrame with all the saved state
//! 5. Call the Rust dispatch function
//!
//! On return, the process reverses: restore registers, `swapgs`, `sysretq`.
//!
//! ## PerCpu struct
//!
//! A static struct whose address is stored in `IA32_KERNEL_GS_BASE`. After
//! `swapgs`, kernel code can access it via `gs:`-relative memory operands.
//! Fields:
//! - `kernel_stack_top` (offset 0): the current task's kernel stack top
//! - `user_rsp_scratch` (offset 8): scratch space to save user RSP during
//!   the window between `swapgs` and the kernel stack switch
//!
//! ## Syscall convention
//!
//! Matches the Linux convention for familiarity (preparing for Phase 5 compat):
//! - RAX = syscall number
//! - RDI, RSI, RDX, R10, R8, R9 = arguments 1-6
//! - Return value in RAX
//! - RCX and R11 are clobbered by the syscall instruction itself
//!
//! ## Security: non-canonical RCX check
//!
//! Intel CPUs raise #GP if `sysret` encounters a non-canonical address in
//! RCX (the return RIP). Critically, this #GP fires in ring 0 but with the
//! user's RSP already loaded — a security vulnerability. We validate RCX
//! for canonicality before `sysretq` and kill the process if it's non-canonical,
//! avoiding the #GP entirely. AMD handles this differently (validates before
//! the ring transition), but our check is correct for both vendors.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::cpu;

// --- MSR addresses ---

/// IA32_EFER — Extended Feature Enable Register.
/// Bit 0 (SCE) enables the `syscall`/`sysret` instructions.
const IA32_EFER: u32 = 0xC000_0080;

/// IA32_STAR — Segment selector base for syscall/sysret.
/// Bits 32-47: kernel CS base (syscall loads CS=this, SS=this+8)
/// Bits 48-63: sysret base (sysret loads SS=this+8|3, CS=this+16|3)
const IA32_STAR: u32 = 0xC000_0081;

/// IA32_LSTAR — Target RIP for 64-bit syscall.
/// The CPU jumps to this address on `syscall` in 64-bit mode.
const IA32_LSTAR: u32 = 0xC000_0082;

/// IA32_FMASK — RFLAGS mask for syscall.
/// Bits set here are CLEARED in RFLAGS on syscall entry.
/// We clear IF (bit 9) to disable interrupts in the kernel.
const IA32_FMASK: u32 = 0xC000_0084;

/// IA32_GS_BASE — the **current** GS segment base. `gs:[...]` accesses use this
/// directly (no swap). We keep it pointing at `PerCpu` while kernel code runs.
const IA32_GS_BASE: u32 = 0xC000_0101;

/// IA32_KERNEL_GS_BASE — Kernel GS base address.
/// Swapped with IA32_GS_BASE by `swapgs`. We store the PerCpu struct
/// address here so `gs:[0]` points to PerCpu after swapgs at syscall entry.
const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// SCE (System Call Enable) bit in IA32_EFER.
const EFER_SCE: u64 = 1 << 0;

/// IF (Interrupt Flag) bit in RFLAGS — masked on syscall entry to prevent
/// interrupts from firing before we've switched to the kernel stack.
const RFLAGS_IF: u64 = 1 << 9;

// --- PerCpu struct offsets ---
//
// These must match the field layout of the PerCpu struct below.
// Used as `const` operands in the naked assembly stubs.

/// Byte offset of `kernel_stack_top` in the PerCpu struct.
const PERCPU_KERNEL_RSP: usize = 0;

/// Byte offset of `user_rsp_scratch` in the PerCpu struct.
const PERCPU_USER_RSP: usize = 8;

// --- Syscall numbers ---

/// SYS_NULL: no-op syscall that returns 0. Used to test the syscall path.
pub const SYS_NULL: u64 = 0;

/// SYS_SEND: send a message to an IPC endpoint (blocks until receiver ready).
/// RDI = endpoint_id, RSI/RDX/R10/R9 = message words[0..3], R8 = badge.
pub const SYS_SEND: u64 = 1;

/// SYS_RECEIVE: receive a message from an IPC endpoint (blocks until sender ready).
/// RDI = endpoint_id. Returns: RAX/RDI/RSI/RDX = words[0..3], R8 = badge,
/// R9 = reply_token.
pub const SYS_RECEIVE: u64 = 2;

/// SYS_CALL: send a message and block waiting for reply (RPC pattern).
/// RDI = endpoint_id, RSI/RDX/R10/R9 = words[0..3], R8 = badge.
/// Returns: RAX/RDI/RSI/RDX = reply words[0..3].
pub const SYS_CALL: u64 = 3;

/// SYS_REPLY: reply to a call, unblocking the caller.
/// RDI = endpoint_id, RSI = reply_token, RDX/R10/R9/R8 = reply words[0..3].
pub const SYS_REPLY: u64 = 4;

/// SYS_YIELD: yield the calling task's time slice.
/// No arguments. Returns 0.
pub const SYS_YIELD: u64 = 5;

/// SYS_EXIT: terminate the calling process.
/// RDI = exit code (currently unused — logged for diagnostics).
pub const SYS_EXIT: u64 = 6;

/// SYS_DEBUG_PRINT: write a character to the serial console.
/// RDI = ASCII character to print.
/// Temporary syscall for Phase 2 debugging — will be removed when
/// drivers move to userspace.
pub const SYS_DEBUG_PRINT: u64 = 7;

// --- Filesystem syscalls (Phase 3) ---
//
// These route through the kernel VFS layer (`crate::fs`), which checks the
// caller's capabilities and forwards to the ring-3 filesystem servers. Path
// strings and data buffers are passed by user pointer and copied in/out by the
// kernel after validation. All return a negative-encoded FsError (high bit set)
// on failure.

/// SYS_OPEN: open a path. RDI = Filesystem cap handle, RSI = path ptr,
/// RDX = path len, R10 = flags. Returns RAX = FileDescriptor cap handle.
pub const SYS_OPEN: u64 = 8;
/// SYS_READ_FILE: RDI = fd cap, RSI = buf ptr, RDX = buf len, R10 = file offset.
/// Returns RAX = bytes read.
pub const SYS_READ_FILE: u64 = 9;
/// SYS_WRITE_FILE: RDI = fd cap, RSI = buf ptr, RDX = buf len, R10 = file offset.
/// Returns RAX = bytes written.
pub const SYS_WRITE_FILE: u64 = 10;
/// SYS_CLOSE: RDI = fd cap. Returns RAX = 0.
pub const SYS_CLOSE: u64 = 11;
/// SYS_STAT: RDI = Filesystem cap, RSI = path ptr, RDX = path len,
/// R10 = stat-out ptr (writes [size:u64, is_dir:u64]). Returns RAX = 0.
pub const SYS_STAT: u64 = 12;
/// SYS_READDIR: RDI = fd cap, RSI = entries-out ptr, RDX = max entries,
/// R10 = out buffer length. Returns RAX = entry count.
pub const SYS_READDIR: u64 = 13;

/// SYS_UPTIME_MS: monotonic milliseconds since boot. No arguments; returns
/// RAX = uptime in ms. Used by the ring-3 net server to drive smoltcp's clock
/// (`Instant`) each poll. Derived from the 100 Hz timer tick (`ticks × 10`).
pub const SYS_UPTIME_MS: u64 = 14;

// --- Socket syscalls (Phase 4.5) ---
//
// UDP sockets through a capability-checked API. The kernel is a router: it
// checks a `Socket` capability, forwards to the ring-3 net server via IPC +
// the shared socket-payload region, and never parses packet data.

/// SYS_SOCKET: create a socket. RDI = socket type (0 = UDP). Returns a new
/// `Socket` capability handle in RAX, or a high-bit-set `SockError`. Requires
/// the caller to hold the network-authority capability.
pub const SYS_SOCKET: u64 = 15;
/// SYS_BIND: bind a socket to a local port. RDI = socket cap, RSI = local IPv4
/// (packed, 0 = any), RDX = port. Returns 0 / error.
pub const SYS_BIND: u64 = 16;
/// SYS_SENDTO: send a datagram. RDI = socket cap, RSI = buf ptr, RDX = buf len,
/// R10 = dest IPv4 (packed), R9 = dest port. Returns bytes sent / error.
pub const SYS_SENDTO: u64 = 17;
/// SYS_RECVFROM: receive a datagram. RDI = socket cap, RSI = buf ptr,
/// RDX = buf len, R10 = source-out ptr (`[ip:u32_le, port:u16_le]`, 8 bytes).
/// Returns bytes received / error (WouldBlock if none pending).
pub const SYS_RECVFROM: u64 = 18;
/// SYS_SOCKET_CLOSE: close a socket. RDI = socket cap. Returns 0 / error.
pub const SYS_SOCKET_CLOSE: u64 = 19;

/// SYS_TRY_RECEIVE: non-blocking IPC receive, for servers that poll. RDI =
/// endpoint. Returns RAX = 1 if a message was received (words in
/// RDI/RSI/RDX/R8, reply token in R9), or RAX = 0 if none was waiting. Unlike
/// SYS_RECEIVE it never blocks — the net server uses it to serve socket
/// requests while continuously polling smoltcp.
pub const SYS_TRY_RECEIVE: u64 = 20;

// --- TCP socket syscalls (Phase 4.6). ---

/// SYS_CONNECT: begin an outbound TCP connection. RDI = socket cap, RSI = dest
/// IPv4 (packed), RDX = dest port. Non-blocking; returns 0, or a high-bit error.
pub const SYS_CONNECT: u64 = 21;
/// SYS_LISTEN: listen for inbound TCP connections on a bound socket. RDI = socket
/// cap, RSI = backlog. Returns 0, or a high-bit error.
pub const SYS_LISTEN: u64 = 22;
/// SYS_ACCEPT: accept one established inbound connection. RDI = listening socket
/// cap, RSI = peer-out ptr (`[ip:u32_le, port:u16_le]`, 8 bytes; 0 to skip).
/// Returns a new `Socket` capability handle for the connection, or a high-bit
/// error (`WouldBlock` if none pending).
pub const SYS_ACCEPT: u64 = 23;
/// SYS_TCP_SEND: send bytes on a connected (TCP) socket. RDI = socket cap,
/// RSI = buf ptr, RDX = len. Returns bytes sent, or a high-bit error
/// (`WouldBlock` while connecting / send window full). Named distinctly from the
/// IPC `SYS_SEND` (1).
pub const SYS_TCP_SEND: u64 = 24;
/// SYS_TCP_RECV: receive bytes from a connected (TCP) socket. RDI = socket cap,
/// RSI = buf ptr, RDX = len. Returns bytes read (0 = peer closed), or a
/// high-bit error (`WouldBlock` if connected but no data yet).
pub const SYS_TCP_RECV: u64 = 25;

/// SYS_MGMT: the container **management ABI** (Phase 6.4), the ring-3 seam onto the
/// kernel `mgmt` module that the `api-server` (6.5) drives. It is **op-multiplexed**
/// — RDI selects the verb ([`MGMT_OP_LISTEN`] and, in 6.5, list/inspect/create/
/// start/stop/logs/node_info) — so the whole growing ABI costs one syscall number
/// instead of one per verb. Op-specific args ride RSI/RDX/R10/R8; the return is the
/// verb's success value (a capability handle, a byte count) with bit 63 clear, or a
/// high-bit-set [`MgmtError`](crate::mgmt::MgmtError) code. Every verb is capability-
/// checked inside `mgmt` against the caller's `Management` cap and audited there.
pub const SYS_MGMT: u64 = 26;

/// `SYS_MGMT` verb selector (RDI): open an inbound-TCP listener. RSI = the caller's
/// `Management` cap handle, RDX = TCP port. Returns a freshly minted listener
/// `Socket` cap handle (accept it with `SYS_ACCEPT`), or a high-bit `MgmtError`.
pub const MGMT_OP_LISTEN: u64 = 1;

/// `SYS_MGMT` **read verbs** (Phase 6.5): each returns an owned JSON document copied
/// into a caller-provided output buffer. Shared register ABI: RSI = `Management` cap
/// handle, RDX = input ptr, R10 = input len (the container id/name for `INSPECT`;
/// unused for `LIST`/`NODE_INFO`), R8 = output ptr, R9 = output capacity. The return
/// is the number of JSON bytes written (bit 63 clear), or a high-bit-set `MgmtError`
/// — `BufferTooSmall` if the document exceeds R9. The write verbs (create/start/stop/
/// logs) share this ABI and are defined just below.
pub const MGMT_OP_LIST: u64 = 2;
pub const MGMT_OP_INSPECT: u64 = 3;
pub const MGMT_OP_NODE_INFO: u64 = 4;

/// `SYS_MGMT` **write verbs** (Phase 6.5b): the mutating container lifecycle ops.
/// Same register ABI as the read verbs. Inputs (RDX/R10): `CREATE` takes
/// `"image\0name"` (NUL-separated; empty name → auto-generate); `START`/`STOP`/`LOGS`
/// take the container id/name. Outputs (R8/R9): `CREATE` writes `{"Id":…}` JSON,
/// `LOGS` writes raw log bytes (bounded), `START`/`STOP` write nothing (0 bytes = the
/// Docker 204 success). Return = bytes written (bit 63 clear) or a high-bit
/// `MgmtError`. **These selectors are a distinct namespace from the audit `OP_*` in
/// `mgmt.rs`** — do not conflate them.
pub const MGMT_OP_CREATE: u64 = 5;
pub const MGMT_OP_START: u64 = 6;
pub const MGMT_OP_STOP: u64 = 7;
pub const MGMT_OP_LOGS: u64 = 8;

/// `SYS_MGMT` verb selector (RDI): record a bearer-token **auth rejection** (Phase
/// 6.6). RSI = the caller's `Management` cap handle; no other args. The api-server
/// calls this when it turns away an unauthenticated / wrong-token request, so a
/// failed auth attempt is audited on the same ABI as a successful op. Returns 0 on
/// success (bit 63 clear) or a high-bit `MgmtError` (`PermissionDenied` if the caller
/// lacks the cap).
pub const MGMT_OP_AUDIT_DENY: u64 = 9;

/// Hard cap on the bytes `MGMT_OP_LOGS` returns, regardless of the caller's output
/// capacity — bounds the `logs` allocation so a huge captured log can't be forced
/// into one copy. The effective tail is `min(out_capacity, MGMT_LOGS_MAX)`.
pub const MGMT_LOGS_MAX: usize = 16 * 1024;

/// SYS_TEST_COMPLETE: internal test syscall. The test shellcode calls this
/// after SYS_NULL to report the result back to the kernel test runner.
/// RDI = the SYS_NULL return value. The handler stores the result and
/// kills the test task (never returns to ring 3).
const SYS_TEST_COMPLETE: u64 = 0xFFFF;

// --- Per-CPU data structure ---

/// Per-CPU data structure accessed via the GS segment base after `swapgs`.
///
/// In a single-core system (Phase 2), there's exactly one of these. On SMP,
/// each core would have its own PerCpu struct with its own address in
/// IA32_KERNEL_GS_BASE.
///
/// Layout is accessed from assembly via hardcoded offsets (PERCPU_KERNEL_RSP
/// and PERCPU_USER_RSP). `repr(C)` ensures field ordering matches.
#[repr(C)]
pub struct PerCpu {
    /// Top of the current task's kernel stack (grows downward).
    /// Loaded into RSP on syscall entry so the kernel has a valid stack.
    /// Updated on every context switch by the scheduler.
    pub kernel_stack_top: u64,

    /// Scratch space for saving the user RSP during syscall entry.
    /// The entry stub stores user RSP here (via `gs:[8]`) before switching
    /// to the kernel stack. Restored on syscall exit.
    pub user_rsp_scratch: u64,
}

/// The single PerCpu instance (single-core system).
/// Its address is written to IA32_KERNEL_GS_BASE during init().
static mut PER_CPU: PerCpu = PerCpu {
    kernel_stack_top: 0,
    user_rsp_scratch: 0,
};

// --- Syscall frame ---

/// Saved register state from a syscall entry.
///
/// Pushed onto the kernel stack by the syscall entry stub and passed as
/// `&mut SyscallFrame` to the Rust dispatch function. The exit stub
/// restores these registers before `sysretq`.
///
/// Field order matches the push sequence in the entry stub (last pushed =
/// lowest address = first field, since RSP points to the top of the frame).
#[repr(C)]
pub struct SyscallFrame {
    // Callee-saved registers (preserved across the syscall for userspace)
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbx: u64,
    pub rbp: u64,

    // Syscall arguments (from user registers)
    pub r9: u64,
    pub r8: u64,
    pub r10: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,

    // Syscall metadata
    pub rax: u64,           // syscall number (input) / return value (output)
    pub user_rsp: u64,      // saved user stack pointer
    pub rcx: u64,           // user RIP (saved by syscall instruction)
    pub r11: u64,           // user RFLAGS (saved by syscall instruction)
}

// --- Syscall entry/exit stub ---

/// The naked assembly syscall entry point, registered in IA32_LSTAR.
///
/// When userspace executes `syscall`, the CPU jumps here with:
/// - RCX = user RIP (return address)
/// - R11 = user RFLAGS
/// - CS/SS = kernel segments (from STAR[47:32])
/// - RFLAGS masked (IF cleared — interrupts disabled)
/// - RSP = UNCHANGED (still the user stack — untrusted!)
///
/// The stub switches to the kernel stack, builds a SyscallFrame, calls the
/// Rust dispatcher, then restores state and returns via `sysretq`.
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        // ===== ENTRY: ring 3 → ring 0 =====

        // Step 1: Swap to kernel GS base.
        // After this, gs:[0] = PerCpu.kernel_stack_top,
        //            gs:[8] = PerCpu.user_rsp_scratch
        "swapgs",

        // Step 2: Save user RSP and switch to kernel stack.
        // We can't touch the stack yet — RSP still points to the user stack
        // (untrusted memory). Save user RSP in PerCpu scratch, then load
        // the kernel stack pointer.
        "mov gs:[{user_rsp}], rsp",            // PerCpu.user_rsp_scratch = user RSP
        "mov rsp, gs:[{kernel_rsp}]",          // RSP = kernel stack top

        // Step 3: Build the SyscallFrame on the kernel stack.
        // Push in reverse struct order (high-address fields first, since
        // the stack grows downward and RSP points to the lowest field).
        "push r11",                             // user RFLAGS (saved by CPU)
        "push rcx",                             // user RIP (saved by CPU)

        // Push user RSP from PerCpu scratch. RCX is now saved on the stack,
        // so we can borrow it as a scratch register.
        "mov rcx, gs:[{user_rsp}]",
        "push rcx",                             // user_rsp

        "push rax",                             // syscall number

        // Syscall arguments (order matches SyscallFrame struct)
        "push rdi",
        "push rsi",
        "push rdx",
        "push r10",
        "push r8",
        "push r9",

        // Callee-saved registers (preserved for userspace)
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",

        // Step 4: Call the Rust syscall dispatch function.
        // RDI = pointer to SyscallFrame (= current RSP, since the frame
        // starts at the top of the stack).
        "mov rdi, rsp",
        "call {dispatch}",

        // ===== EXIT: ring 0 → ring 3 =====

        // Disable interrupts for the whole exit tail. The dispatcher may have
        // enabled them (blocking syscalls call `sti`), but the tail below is a
        // critical section that MUST run uninterrupted:
        //  - `PerCpu.user_rsp_scratch` (gs:0x8) is a single shared slot; we
        //    re-stash the user RSP into it and read it back two instructions
        //    later. A preempting syscall's entry would overwrite it, so the task
        //    would `sysretq` onto another task's stack.
        //  - After `mov gs:0x8, rsp` loads the user RSP we are briefly in ring 0
        //    with a ring-3 RSP; an interrupt there would push onto the user
        //    stack. And an interrupt between `swapgs` and `sysretq` would run
        //    with the user GS base.
        // `sysretq` restores the user RFLAGS (with IF) from R11, so interrupts
        // come back on atomically as we return to ring 3.
        "cli",

        // Step 5: Restore registers from the SyscallFrame.
        // The dispatch function may have modified frame.rax (return value).

        // Callee-saved registers
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",

        // Argument registers
        "pop r9",
        "pop r8",
        "pop r10",
        "pop rdx",
        "pop rsi",
        "pop rdi",

        // Return value
        "pop rax",

        // Pop user_rsp into RCX (scratch — the real user RIP hasn't been
        // popped yet). We stash user RSP into PerCpu scratch for later.
        "pop rcx",                              // user_rsp
        "mov gs:[{user_rsp}], rcx",            // stash in PerCpu

        // Pop user RIP and user RFLAGS
        "pop rcx",                              // user RIP (for sysretq)
        "pop r11",                              // user RFLAGS (for sysretq)

        // Step 6: Validate RCX (user RIP) is canonical.
        //
        // On Intel, sysretq with a non-canonical RCX causes #GP in ring 0
        // but with the user's RSP already loaded — a privilege escalation
        // vector. AMD checks before the transition (safe), but we defend
        // against both by checking manually.
        //
        // A canonical 48-bit virtual address has bits 47-63 all identical.
        // We sign-extend bit 47 and compare: if the result is 0 (lower half)
        // or -1 (upper half), the address is canonical.
        "mov rsp, rcx",                         // borrow RSP as scratch
        "sar rsp, 47",                          // sign-extend bit 47
        "cmp rsp, 0",
        "je 2f",                                // canonical (lower half)
        "cmp rsp, -1",
        "je 2f",                                // canonical (upper half)

        // Non-canonical: kill the process. In a full implementation this
        // would terminate the task and switch to the next one. For now,
        // trigger #UD which our exception handler will catch.
        "ud2",

        "2:",
        // Step 7: Restore user RSP from PerCpu scratch.
        "mov rsp, gs:[{user_rsp}]",

        // Step 8: Swap back to user GS base before returning to ring 3.
        "swapgs",

        // Step 9: Return to userspace via sysretq.
        // sysretq loads:
        //   RIP ← RCX
        //   RFLAGS ← R11
        //   CS ← STAR[63:48]+16 | 3 = 0x23 (user code)
        //   SS ← STAR[63:48]+8 | 3 = 0x1B (user data)
        "sysretq",

        kernel_rsp = const PERCPU_KERNEL_RSP,
        user_rsp = const PERCPU_USER_RSP,
        dispatch = sym syscall_dispatch,
    );
}

// --- Syscall dispatch ---

/// Rust-side syscall dispatcher.
///
/// Called from the assembly entry stub with a pointer to the SyscallFrame
/// on the kernel stack. The frame's `rax` field contains the syscall number;
/// on return, it should contain the return value (which the exit stub pops
/// into RAX for the userspace caller).
///
/// Runs with interrupts disabled (FMASK cleared IF on syscall entry).
/// For syscalls that may block (IPC send/receive in later phases),
/// interrupts should be re-enabled after the kernel stack switch is complete.
#[no_mangle]
extern "C" fn syscall_dispatch(frame: &mut SyscallFrame) {
    // Audit log every syscall invocation. The detail field carries the
    // syscall number so the audit trail shows exactly what userspace requested.
    crate::audit::log_event(
        crate::sched::current_process_id(),
        crate::audit::AuditOp::Syscall,
        crate::cap::CapType::Null,
        frame.rax,
    );

    // Linux-personality processes (Phase 5.1) route their `syscall`s through the
    // Linux dispatch table — Linux syscall numbers collide with the native ones
    // (Linux `write`=1 vs native `SYS_SEND`=1), so the process's personality, not
    // the number, selects the ABI. A native process falls through to the match.
    if crate::process::personality(crate::sched::current_process_id())
        == crate::process::Personality::Linux
    {
        crate::linux::syscall::dispatch(frame);
        return;
    }

    match frame.rax {
        SYS_NULL => {
            // No-op syscall. Returns 0 to confirm the syscall path works.
            frame.rax = 0;
        }
        SYS_SEND => {
            // Userspace IPC send via registers.
            // Convention: RDI = endpoint_id, RSI/RDX/R10/R9 = words[0..3], R8 = badge
            let endpoint_id = frame.rdi;
            let badge = frame.r8;
            let msg = crate::ipc::IpcMessage::new([frame.rsi, frame.rdx, frame.r10, frame.r9]);

            // Re-enable interrupts before the potentially-blocking IPC call.
            // The syscall entry stub disabled interrupts (FMASK clears IF),
            // but IPC blocking needs the timer to preempt and schedule.
            cpu::sti();

            match crate::ipc::ipc_send(endpoint_id, msg, badge) {
                Ok(()) => frame.rax = 0,
                Err(_) => frame.rax = !0u64,
            }
        }
        SYS_RECEIVE => {
            // RDI = endpoint_id. Returns word0 in RAX (or -1 on error).
            // Full message would need a user buffer — for Phase 2, we return
            // word0 in RAX to confirm the message arrived.
            let endpoint_id = frame.rdi;

            cpu::sti();

            match crate::ipc::ipc_receive(endpoint_id) {
                Ok(msg) => {
                    frame.rax = msg.words[0];
                    frame.rdi = msg.words[1];
                    frame.rsi = msg.words[2];
                    frame.rdx = msg.words[3];
                    frame.r8 = msg.badge;
                    frame.r9 = msg.reply_token;
                }
                Err(_) => frame.rax = !0u64,
            }
        }
        SYS_CALL => {
            // Userspace IPC call (RPC): send a request and block for the reply.
            // Convention: RDI = endpoint_id, RSI/RDX/R10/R9 = words[0..3],
            // R8 = badge. On return: RAX/RDI/RSI/RDX = reply words[0..3].
            let endpoint_id = frame.rdi;
            let badge = frame.r8;
            let msg = crate::ipc::IpcMessage::new([frame.rsi, frame.rdx, frame.r10, frame.r9]);

            // Re-enable interrupts before blocking (call waits for the reply).
            cpu::sti();

            match crate::ipc::ipc_call(endpoint_id, msg, badge) {
                Ok(reply) => {
                    frame.rax = reply.words[0];
                    frame.rdi = reply.words[1];
                    frame.rsi = reply.words[2];
                    frame.rdx = reply.words[3];
                }
                Err(_) => frame.rax = !0u64,
            }
        }
        SYS_REPLY => {
            // Userspace IPC reply: unblock a caller waiting on our endpoint.
            // Convention: RDI = endpoint_id, RSI = reply_token,
            // RDX/R10/R9/R8 = reply words[0..3].
            let endpoint_id = frame.rdi;
            let reply_token = frame.rsi;
            let msg = crate::ipc::IpcMessage::new([frame.rdx, frame.r10, frame.r9, frame.r8]);

            cpu::sti();

            match crate::ipc::ipc_reply(endpoint_id, reply_token, msg) {
                Ok(()) => frame.rax = 0,
                Err(_) => frame.rax = !0u64,
            }
        }
        SYS_YIELD => {
            // Yield the current task's time slice. Re-enable interrupts first
            // since yield_now() calls schedule().
            cpu::sti();
            crate::sched::yield_now();
            frame.rax = 0;
        }
        SYS_EXIT => {
            // Terminate the calling process. RDI = exit code (logged).
            crate::println!("[syscall] SYS_EXIT: task {} exit code {}",
                crate::sched::current_task_id(), frame.rdi);

            // Undo swapgs from syscall entry before exiting.
            unsafe { cpu::swapgs(); }

            crate::sched::exit_current_task();
        }
        SYS_DEBUG_PRINT => {
            // Print a single character to serial. Temporary for Phase 2.
            let ch = (frame.rdi & 0xFF) as u8 as char;
            crate::print!("{}", ch);
            frame.rax = 0;
        }
        SYS_OPEN | SYS_READ_FILE | SYS_WRITE_FILE | SYS_CLOSE | SYS_STAT | SYS_READDIR => {
            // Filesystem syscalls block on the FS server, so enable interrupts.
            cpu::sti();
            dispatch_fs_syscall(frame);
        }
        SYS_UPTIME_MS => {
            // Monotonic milliseconds since boot: 100 Hz tick × 10. A plain
            // atomic read, so no need to re-enable interrupts or block.
            frame.rax = crate::arch::x86_64::idt::tick_count().wrapping_mul(10);
        }
        SYS_TRY_RECEIVE => {
            // Non-blocking receive for polling servers (the net server). RDI =
            // endpoint. RAX = 1 with words in RDI/RSI/RDX/R8 + token in R9 if a
            // message was waiting, else RAX = 0. Never blocks, so we don't touch
            // the interrupt flag.
            let endpoint_id = frame.rdi;
            match crate::ipc::ipc_try_receive(endpoint_id) {
                Ok(Some(msg)) => {
                    frame.rax = 1;
                    frame.rdi = msg.words[0];
                    frame.rsi = msg.words[1];
                    frame.rdx = msg.words[2];
                    frame.r8 = msg.words[3];
                    frame.r9 = msg.reply_token;
                }
                Ok(None) => frame.rax = 0,
                Err(_) => frame.rax = !0u64,
            }
        }
        SYS_SOCKET | SYS_BIND | SYS_SENDTO | SYS_RECVFROM | SYS_SOCKET_CLOSE | SYS_CONNECT
        | SYS_TCP_SEND | SYS_TCP_RECV | SYS_LISTEN | SYS_ACCEPT => {
            // Socket syscalls block on the net server, so enable interrupts.
            cpu::sti();
            dispatch_socket_syscall(frame);
        }
        SYS_MGMT => {
            // Management ABI (Phase 6.4). The `listen` verb blocks on the net
            // server (ksocket_open_tcp → IPC), so enable interrupts like the socket
            // path. Only a native process reaches here — Linux-personality tasks
            // short-circuit above, so a container can never invoke the mgmt ABI.
            cpu::sti();
            dispatch_mgmt_syscall(frame);
        }
        SYS_TEST_COMPLETE => {
            // Internal test syscall: store the test result and kill the task.
            // RDI contains the SYS_NULL return value from the test shellcode.
            //
            // We don't return from this handler — instead we fix up the GS
            // state (undo the syscall entry's swapgs) and exit the task.
            // The assembly exit path (sysretq) is never reached.
            SYSCALL_TEST_RESULT.store(frame.rdi, Ordering::SeqCst);
            SYSCALL_TEST_DONE.store(true, Ordering::SeqCst);

            // Undo the swapgs from syscall entry. Without this, IA32_KERNEL_GS_BASE
            // would hold the user's GS base instead of &PER_CPU, breaking future
            // syscall entries.
            unsafe { cpu::swapgs(); }

            // Kill this task and switch to the next one. This function never
            // returns, so the assembly exit stub's sysretq is never reached.
            crate::sched::exit_current_task();
        }
        unknown => {
            // Unknown syscall number. Return -1 (0xFFFF...FFFF as unsigned).
            // In a full implementation, this might terminate the offending process.
            frame.rax = !0u64;
            crate::println!("[syscall] unknown syscall number: {}", unknown);
        }
    }
}

// --- Filesystem syscall dispatch ---
//
// These thin handlers validate and copy user pointers, then delegate to the
// capability-checked VFS operations in `crate::fs`. They run with interrupts
// enabled (the VFS calls block on the ring-3 filesystem servers).

/// Upper bound on the non-canonical user/kernel split — any user pointer at or
/// above this is rejected. Userspace lives strictly in the lower half.
const USER_ADDR_LIMIT: u64 = 0x0000_8000_0000_0000;
/// Cap on a single filesystem syscall transfer, to bound kernel allocation
/// against a hostile or buggy user length.
const FS_MAX_XFER: usize = 256 * 1024;

/// Validate that `[uptr, uptr+len)` is wholly mapped in the current process's
/// address space and lies in the user half. Returns false on any gap.
pub(crate) fn user_range_ok(uptr: u64, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let end = match uptr.checked_add(len as u64) {
        Some(e) => e,
        None => return false,
    };
    if uptr == 0 || end > USER_ADDR_LIMIT {
        return false;
    }
    let pid = crate::sched::current_process_id();
    crate::process::with_address_space(pid, |a| {
        let mut page = uptr & !0xFFF;
        while page < end {
            if a.translate(crate::mm::addr::VirtAddr::new(page)).is_none() {
                return false;
            }
            page += 0x1000;
        }
        true
    })
    .unwrap_or(false)
}

/// Copy `len` bytes from a validated user pointer into a kernel `Vec`.
pub(crate) fn copy_from_user(uptr: u64, len: usize) -> Option<alloc::vec::Vec<u8>> {
    // A zero-length transfer touches no memory — never dereference the pointer
    // (which may be null): `from_raw_parts(null, 0)` is UB even for len 0.
    if len == 0 {
        return Some(alloc::vec::Vec::new());
    }
    if len > FS_MAX_XFER || !user_range_ok(uptr, len) {
        return None;
    }
    // SAFETY: we are in the calling process's address space (its CR3 is active
    // during the syscall) and have verified every page of the range is mapped.
    let slice = unsafe { core::slice::from_raw_parts(uptr as *const u8, len) };
    Some(slice.to_vec())
}

/// Copy `data` to a validated user pointer. Returns false if the range is bad.
pub(crate) fn copy_to_user(uptr: u64, data: &[u8]) -> bool {
    // Nothing to copy → success without dereferencing a possibly-null pointer.
    if data.is_empty() {
        return true;
    }
    if !user_range_ok(uptr, data.len()) {
        return false;
    }
    // SAFETY: validated mapped user range in the active address space.
    let dst = unsafe { core::slice::from_raw_parts_mut(uptr as *mut u8, data.len()) };
    dst.copy_from_slice(data);
    true
}

/// Dispatch a filesystem syscall (SYS_OPEN .. SYS_READDIR) from `frame`.
fn dispatch_fs_syscall(frame: &mut SyscallFrame) {
    use crate::cap::CapHandle;
    use crate::fs::{self, FsError};

    let pid = crate::sched::current_process_id();
    crate::audit::log_event(pid, crate::audit::AuditOp::FsAccess, crate::cap::CapType::Null, frame.rax);
    let bad_arg = FsError::InvalidArgument.as_syscall_ret();

    match frame.rax {
        SYS_OPEN => {
            let fs_handle = CapHandle::from_raw(frame.rdi as u32);
            match copy_from_user(frame.rsi, frame.rdx as usize) {
                Some(path) => match fs::vfs_open(pid, fs_handle, &path) {
                    Ok(fd_raw) => frame.rax = fd_raw as u64,
                    Err(e) => frame.rax = e.as_syscall_ret(),
                },
                None => frame.rax = bad_arg,
            }
        }
        SYS_READ_FILE => {
            let fd = CapHandle::from_raw(frame.rdi as u32);
            let len = (frame.rdx as usize).min(FS_MAX_XFER);
            let off = frame.r10;
            if !user_range_ok(frame.rsi, len) {
                frame.rax = bad_arg;
                return;
            }
            let mut kbuf = alloc::vec![0u8; len];
            match fs::vfs_read(pid, fd, off, &mut kbuf) {
                Ok(n) if copy_to_user(frame.rsi, &kbuf[..n]) => frame.rax = n as u64,
                Ok(_) => frame.rax = bad_arg,
                Err(e) => frame.rax = e.as_syscall_ret(),
            }
        }
        SYS_WRITE_FILE => {
            let fd = CapHandle::from_raw(frame.rdi as u32);
            let off = frame.r10;
            match copy_from_user(frame.rsi, frame.rdx as usize) {
                Some(data) => match fs::vfs_write(pid, fd, off, &data) {
                    Ok(n) => frame.rax = n as u64,
                    Err(e) => frame.rax = e.as_syscall_ret(),
                },
                None => frame.rax = bad_arg,
            }
        }
        SYS_CLOSE => {
            let fd = CapHandle::from_raw(frame.rdi as u32);
            match fs::vfs_close(pid, fd) {
                Ok(()) => frame.rax = 0,
                Err(e) => frame.rax = e.as_syscall_ret(),
            }
        }
        SYS_STAT => {
            let fs_handle = CapHandle::from_raw(frame.rdi as u32);
            let stat_ptr = frame.r10;
            match copy_from_user(frame.rsi, frame.rdx as usize) {
                Some(path) => match fs::vfs_stat(pid, fs_handle, &path) {
                    Ok((size, is_dir)) => {
                        // Write [size:u64, is_dir:u64] to the user stat buffer.
                        let mut out = [0u8; 16];
                        out[0..8].copy_from_slice(&size.to_le_bytes());
                        out[8..16].copy_from_slice(&(is_dir as u64).to_le_bytes());
                        if copy_to_user(stat_ptr, &out) {
                            frame.rax = 0;
                        } else {
                            frame.rax = bad_arg;
                        }
                    }
                    Err(e) => frame.rax = e.as_syscall_ret(),
                },
                None => frame.rax = bad_arg,
            }
        }
        SYS_READDIR => {
            let fd = CapHandle::from_raw(frame.rdi as u32);
            let entries_ptr = frame.rsi;
            let max = frame.rdx;
            let out_len = (frame.r10 as usize).min(FS_MAX_XFER);
            if !user_range_ok(entries_ptr, out_len) {
                frame.rax = bad_arg;
                return;
            }
            let mut kbuf = alloc::vec![0u8; out_len];
            match fs::vfs_readdir(pid, fd, max, &mut kbuf) {
                Ok(count) if copy_to_user(entries_ptr, &kbuf) => frame.rax = count,
                Ok(_) => frame.rax = bad_arg,
                Err(e) => frame.rax = e.as_syscall_ret(),
            }
        }
        _ => frame.rax = bad_arg,
    }
}

// --- Socket syscall dispatch (Phase 4.5) ---
//
// Thin handlers that validate/copy user pointers, then delegate to the
// capability-checked socket router in `crate::net::socket`. They run with
// interrupts enabled (the router blocks on the ring-3 net server). A packed
// IPv4 address arrives as a big-endian `a<<24|b<<16|c<<8|d` word.

/// Unpack a syscall IPv4 argument (`a<<24|b<<16|c<<8|d`) into octets.
fn unpack_ipv4_arg(w: u64) -> [u8; 4] {
    [(w >> 24) as u8, (w >> 16) as u8, (w >> 8) as u8, w as u8]
}

/// Dispatch a socket syscall (SYS_SOCKET .. SYS_SOCKET_CLOSE) from `frame`.
fn dispatch_socket_syscall(frame: &mut SyscallFrame) {
    use crate::cap::CapHandle;
    use crate::net::socket::{self, SockError};

    let pid = crate::sched::current_process_id();
    crate::audit::log_event(pid, crate::audit::AuditOp::NetAccess, crate::cap::CapType::Null, frame.rax);
    let bad_arg = SockError::InvalidArgument.as_syscall_ret();
    let num = frame.rax;

    match num {
        SYS_SOCKET => {
            // RDI = socket type, RSI = network-authority (factory) cap handle.
            let sock_type = frame.rdi;
            let factory = CapHandle::from_raw(frame.rsi as u32);
            match socket::sys_socket(pid, factory, sock_type, num) {
                Ok(handle) => frame.rax = handle as u64,
                Err(e) => frame.rax = e.as_syscall_ret(),
            }
        }
        SYS_BIND => {
            // RDI = socket cap, RSI = local IPv4 (packed), RDX = port.
            let handle = CapHandle::from_raw(frame.rdi as u32);
            let ip = unpack_ipv4_arg(frame.rsi);
            let port = frame.rdx as u16;
            match socket::sys_bind(pid, handle, ip, port, num) {
                Ok(()) => frame.rax = 0,
                Err(e) => frame.rax = e.as_syscall_ret(),
            }
        }
        SYS_SENDTO => {
            // RDI = socket cap, RSI = buf ptr, RDX = len, R10 = dest IPv4, R9 = port.
            let handle = CapHandle::from_raw(frame.rdi as u32);
            let ip = unpack_ipv4_arg(frame.r10);
            let port = frame.r9 as u16;
            match copy_from_user(frame.rsi, frame.rdx as usize) {
                Some(data) => match socket::sys_sendto(pid, handle, &data, ip, port, num) {
                    Ok(n) => frame.rax = n as u64,
                    Err(e) => frame.rax = e.as_syscall_ret(),
                },
                None => frame.rax = bad_arg,
            }
        }
        SYS_RECVFROM => {
            // RDI = socket cap, RSI = buf ptr, RDX = len, R10 = source-out ptr.
            let handle = CapHandle::from_raw(frame.rdi as u32);
            let len = (frame.rdx as usize).min(FS_MAX_XFER);
            let src_ptr = frame.r10;
            if !user_range_ok(frame.rsi, len) {
                frame.rax = bad_arg;
                return;
            }
            let mut kbuf = alloc::vec![0u8; len];
            match socket::sys_recvfrom(pid, handle, &mut kbuf, num) {
                Ok((n, ip, port)) => {
                    if !copy_to_user(frame.rsi, &kbuf[..n]) {
                        frame.rax = bad_arg;
                        return;
                    }
                    // Write [ip:u32_le, port:u16_le] to the source-out buffer if
                    // the caller supplied one (src_ptr != 0).
                    if src_ptr != 0 {
                        let mut out = [0u8; 8];
                        out[0..4].copy_from_slice(&ip);
                        out[4..6].copy_from_slice(&port.to_le_bytes());
                        if !copy_to_user(src_ptr, &out) {
                            frame.rax = bad_arg;
                            return;
                        }
                    }
                    frame.rax = n as u64;
                }
                Err(e) => frame.rax = e.as_syscall_ret(),
            }
        }
        SYS_SOCKET_CLOSE => {
            let handle = CapHandle::from_raw(frame.rdi as u32);
            match socket::sys_close(pid, handle, num) {
                Ok(()) => frame.rax = 0,
                Err(e) => frame.rax = e.as_syscall_ret(),
            }
        }
        SYS_CONNECT => {
            // RDI = socket cap, RSI = dest IPv4 (packed), RDX = dest port.
            let handle = CapHandle::from_raw(frame.rdi as u32);
            let ip = unpack_ipv4_arg(frame.rsi);
            let port = frame.rdx as u16;
            match socket::sys_connect(pid, handle, ip, port, num) {
                Ok(()) => frame.rax = 0,
                Err(e) => frame.rax = e.as_syscall_ret(),
            }
        }
        SYS_TCP_SEND => {
            // RDI = socket cap, RSI = buf ptr, RDX = len (stream send).
            let handle = CapHandle::from_raw(frame.rdi as u32);
            match copy_from_user(frame.rsi, frame.rdx as usize) {
                Some(data) => match socket::sys_send(pid, handle, &data, num) {
                    Ok(n) => frame.rax = n as u64,
                    Err(e) => frame.rax = e.as_syscall_ret(),
                },
                None => frame.rax = bad_arg,
            }
        }
        SYS_TCP_RECV => {
            // RDI = socket cap, RSI = buf ptr, RDX = len (stream recv).
            let handle = CapHandle::from_raw(frame.rdi as u32);
            let len = (frame.rdx as usize).min(FS_MAX_XFER);
            if !user_range_ok(frame.rsi, len) {
                frame.rax = bad_arg;
                return;
            }
            let mut kbuf = alloc::vec![0u8; len];
            match socket::sys_recv(pid, handle, &mut kbuf, num) {
                Ok(n) if copy_to_user(frame.rsi, &kbuf[..n]) => frame.rax = n as u64,
                Ok(_) => frame.rax = bad_arg,
                Err(e) => frame.rax = e.as_syscall_ret(),
            }
        }
        SYS_LISTEN => {
            // RDI = socket cap, RSI = backlog.
            let handle = CapHandle::from_raw(frame.rdi as u32);
            match socket::sys_listen(pid, handle, frame.rsi, num) {
                Ok(()) => frame.rax = 0,
                Err(e) => frame.rax = e.as_syscall_ret(),
            }
        }
        SYS_ACCEPT => {
            // RDI = listening socket cap, RSI = peer-out ptr (8 bytes, 0 to skip).
            let handle = CapHandle::from_raw(frame.rdi as u32);
            let peer_ptr = frame.rsi;
            // Validate the peer buffer BEFORE accepting: sys_accept promotes the
            // connection and mints its capability, so a copy failure afterwards
            // would orphan a live connection the caller never learns the handle of.
            if peer_ptr != 0 && !user_range_ok(peer_ptr, 8) {
                frame.rax = bad_arg;
                return;
            }
            match socket::sys_accept(pid, handle, num) {
                Ok((conn_handle, ip, port)) => {
                    if peer_ptr != 0 {
                        let mut out = [0u8; 8];
                        out[0..4].copy_from_slice(&ip);
                        out[4..6].copy_from_slice(&port.to_le_bytes());
                        // Range already validated above, so this write succeeds.
                        copy_to_user(peer_ptr, &out);
                    }
                    frame.rax = conn_handle as u64;
                }
                Err(e) => frame.rax = e.as_syscall_ret(),
            }
        }
        _ => frame.rax = bad_arg,
    }
}

/// Dispatch a `SYS_MGMT` invocation (Phase 6.4) to the kernel `mgmt` module.
///
/// Op-multiplexed on RDI. Each verb is capability-checked and audited inside
/// `mgmt` against the caller's `Management` cap; a caller without it gets
/// `MgmtError::PermissionDenied` **before** any backing service is touched (the
/// fail-closed property). `MGMT_OP_LISTEN` landed in 6.4; the read verbs
/// (list/inspect/node_info) in 6.5; and the write verbs (create/start/stop/logs)
/// in 6.5b — the full set the ring-3 api-server drives.
fn dispatch_mgmt_syscall(frame: &mut SyscallFrame) {
    use crate::cap::CapHandle;
    use crate::mgmt::MgmtError;
    let pid = crate::sched::current_process_id();
    let bad_op = MgmtError::InvalidArgument.as_syscall_ret();
    let mgmt_handle = CapHandle::from_raw(frame.rsi as u32);

    // Copy a read verb's JSON result into the caller's output buffer (R8 = ptr,
    // R9 = capacity), returning the byte count or a high-bit `MgmtError`. The
    // length is checked against the capacity BEFORE `copy_to_user`, so an
    // over-length response fails closed as `BufferTooSmall` (never truncates).
    fn emit(frame: &mut SyscallFrame, result: Result<alloc::vec::Vec<u8>, MgmtError>) {
        frame.rax = match result {
            Ok(json) => {
                let out_ptr = frame.r8;
                let out_cap = frame.r9 as usize;
                if json.len() > out_cap {
                    MgmtError::BufferTooSmall.as_syscall_ret()
                } else if copy_to_user(out_ptr, &json) {
                    json.len() as u64
                } else {
                    MgmtError::InvalidArgument.as_syscall_ret()
                }
            }
            Err(e) => e.as_syscall_ret(),
        };
    }

    match frame.rdi {
        MGMT_OP_LISTEN => {
            // RSI = Management cap handle, RDX = TCP port.
            let port = frame.rdx as u16;
            match crate::mgmt::listen(pid, mgmt_handle, port) {
                // Success: the freshly minted listener Socket cap handle (bit 63
                // clear). The caller accepts on it via SYS_ACCEPT.
                Ok(listener) => frame.rax = listener as u64,
                Err(e) => frame.rax = e.as_syscall_ret(),
            }
        }
        MGMT_OP_LIST => {
            // No input; JSON array of container summaries into R8[..R9].
            emit(frame, crate::mgmt::list(pid, mgmt_handle));
        }
        MGMT_OP_NODE_INFO => {
            // No input; node-info JSON object into R8[..R9].
            emit(frame, crate::mgmt::node_info(pid, mgmt_handle));
        }
        MGMT_OP_INSPECT => {
            // RDX = id/name ptr, R10 = id/name len; JSON detail object into R8[..R9].
            let id = match copy_from_user(frame.rdx, frame.r10 as usize) {
                Some(bytes) => bytes,
                None => {
                    frame.rax = bad_op;
                    return;
                }
            };
            match core::str::from_utf8(&id) {
                Ok(id) => emit(frame, crate::mgmt::inspect(pid, mgmt_handle, id)),
                Err(_) => frame.rax = bad_op,
            }
        }
        MGMT_OP_CREATE => {
            // RDX/R10 = "image\0name" (NUL-separated). Splits on the first NUL; no NUL
            // → empty name (auto-generated); leading NUL → empty image → the mgmt
            // layer rejects it (InvalidArgument). Output: {"Id":…} JSON.
            let input = match copy_from_user(frame.rdx, frame.r10 as usize) {
                Some(bytes) => bytes,
                None => {
                    frame.rax = bad_op;
                    return;
                }
            };
            let (image_b, name_b) = match input.iter().position(|&b| b == 0) {
                Some(nul) => (&input[..nul], &input[nul + 1..]),
                None => (&input[..], &input[..0]),
            };
            match (core::str::from_utf8(image_b), core::str::from_utf8(name_b)) {
                (Ok(image), Ok(name)) => {
                    emit(frame, crate::mgmt::create(pid, mgmt_handle, image, name))
                }
                _ => frame.rax = bad_op,
            }
        }
        MGMT_OP_START | MGMT_OP_STOP => {
            // RDX/R10 = id/name. Success writes 0 bytes (Docker 204).
            let id = match copy_from_user(frame.rdx, frame.r10 as usize) {
                Some(bytes) => bytes,
                None => {
                    frame.rax = bad_op;
                    return;
                }
            };
            match core::str::from_utf8(&id) {
                Ok(id) if frame.rdi == MGMT_OP_START => {
                    emit(frame, crate::mgmt::start(pid, mgmt_handle, id))
                }
                Ok(id) => emit(frame, crate::mgmt::stop(pid, mgmt_handle, id)),
                Err(_) => frame.rax = bad_op,
            }
        }
        MGMT_OP_LOGS => {
            // RDX/R10 = id/name; raw log bytes into R8[..R9]. The tail is capped at
            // min(out_capacity, MGMT_LOGS_MAX) so the result always fits the buffer
            // (BufferTooSmall is structurally impossible) and the copy is bounded.
            let id = match copy_from_user(frame.rdx, frame.r10 as usize) {
                Some(bytes) => bytes,
                None => {
                    frame.rax = bad_op;
                    return;
                }
            };
            let tail = (frame.r9 as usize).min(MGMT_LOGS_MAX);
            match core::str::from_utf8(&id) {
                Ok(id) => emit(frame, crate::mgmt::logs(pid, mgmt_handle, id, Some(tail))),
                Err(_) => frame.rax = bad_op,
            }
        }
        MGMT_OP_AUDIT_DENY => {
            // No input; records an auth-rejection audit entry. Success writes 0.
            frame.rax = match crate::mgmt::audit_deny(pid, mgmt_handle) {
                Ok(()) => 0,
                Err(e) => e.as_syscall_ret(),
            };
        }
        _ => frame.rax = bad_op,
    }
}

// --- Initialization ---

/// Initialize the syscall/sysret mechanism.
///
/// Configures the MSRs that control `syscall` behavior:
/// - IA32_EFER: enable SCE (syscall enable) bit
/// - IA32_STAR: set kernel and user segment selector bases
/// - IA32_LSTAR: set the syscall entry point address
/// - IA32_FMASK: mask IF on entry (disable interrupts in kernel)
/// - IA32_KERNEL_GS_BASE: point to the PerCpu struct
///
/// Must be called after GDT init (segment selectors must be valid) and
/// before any userspace code runs.
pub fn init() {
    unsafe {
        // Enable the SCE (System Call Enable) bit in EFER.
        // Without this bit, `syscall`/`sysret` raise #UD (Invalid Opcode).
        let efer = cpu::rdmsr(IA32_EFER);
        cpu::wrmsr(IA32_EFER, efer | EFER_SCE);

        // Configure STAR: segment selectors for ring transitions.
        //
        // STAR[47:32] = 0x08 (kernel CS base)
        //   syscall loads: CS = 0x08 (kernel code), SS = 0x08+8 = 0x10 (kernel data)
        //
        // STAR[63:48] = 0x10 (sysret base)
        //   sysret loads: SS = 0x10+8 = 0x18 | RPL=3 = 0x1B (user data)
        //                 CS = 0x10+16 = 0x20 | RPL=3 = 0x23 (user code)
        //
        // STAR[31:0] = 0 (unused in 64-bit mode — this is the 32-bit syscall target)
        let star_value: u64 = (0x08u64 << 32) | (0x10u64 << 48);
        cpu::wrmsr(IA32_STAR, star_value);

        // Set LSTAR to the address of our syscall entry stub.
        // Every `syscall` instruction in 64-bit mode jumps to this address.
        cpu::wrmsr(IA32_LSTAR, syscall_entry as *const () as u64);

        // Set FMASK to clear the IF flag on syscall entry.
        // This prevents timer interrupts from firing before we've switched
        // to the kernel stack (which would push an interrupt frame onto the
        // user stack, corrupting it).
        cpu::wrmsr(IA32_FMASK, RFLAGS_IF);

        // Point both GS-base MSRs at our PerCpu struct so the invariant
        // "kernel code runs with GS_BASE == &PER_CPU" holds from the very first
        // task, before any context switch calls `refresh_kernel_gs_base`.
        // After `swapgs` in the syscall entry stub, `gs:[0]` addresses
        // PerCpu.kernel_stack_top and `gs:[8]` addresses PerCpu.user_rsp_scratch.
        let per_cpu_addr = &raw const PER_CPU as u64;
        cpu::wrmsr(IA32_GS_BASE, per_cpu_addr);
        cpu::wrmsr(IA32_KERNEL_GS_BASE, per_cpu_addr);
    }

    crate::println!("[syscall] Initialized (STAR={:#018x}, LSTAR={:#x})",
        (0x08u64 << 32) | (0x10u64 << 48),
        syscall_entry as *const () as u64);
}

/// Update the PerCpu kernel stack top and re-establish the kernel GS base.
///
/// Called from the scheduler's context switch path. Two jobs:
///
/// 1. Set `PerCpu.kernel_stack_top` so the syscall entry stub loads the correct
///    kernel stack for the newly running task. Without this, a syscall from
///    userspace would use the previous task's kernel stack — corrupting both.
///
/// 2. Re-point `IA32_KERNEL_GS_BASE` at `PER_CPU`. The syscall entry stub does
///    `swapgs` to bring the kernel GS base into GS; that only works if
///    `KERNEL_GS_BASE` holds `&PER_CPU` at entry. But a syscall that *blocks*
///    (e.g. `ipc_receive` with no sender ready) context-switches away **after**
///    its entry `swapgs` and **before** the matching exit `swapgs` — leaving
///    `KERNEL_GS_BASE` holding the blocked task's (zero) user GS base. The next
///    task to enter the kernel via `swapgs` would then get GS = 0 and fault
///    touching `gs:[...]`. Re-writing the MSR on every switch restores the
///    single-core invariant "whenever a task runs, `KERNEL_GS_BASE == &PER_CPU`",
///    so a fresh ring-3 task's first syscall always swaps in the right GS base.
///    (Safe for a task resuming mid-syscall too: its exit `swapgs` then just
///    leaves GS = &PER_CPU, which ring 3 never reads.)
///
/// # Safety
///
/// Must be called with interrupts disabled (during context switch).
pub fn set_kernel_stack(stack_top: u64) {
    // SAFETY: interrupts are disabled during context switch, ensuring
    // exclusive access. Single-core system, no data races possible.
    unsafe {
        let per_cpu = &raw mut PER_CPU;
        (*per_cpu).kernel_stack_top = stack_top;

        // Re-establish KERNEL_GS_BASE = &PER_CPU (see point 2 above).
        cpu::wrmsr(IA32_KERNEL_GS_BASE, per_cpu as u64);
    }
}

/// Re-establish the GS-base invariant on a context switch: both the **current**
/// GS base (`IA32_GS_BASE`) and the swap target (`IA32_KERNEL_GS_BASE`) point at
/// `&PER_CPU`. Must run on **every** switch — including switches to kernel-only
/// tasks (idle, bootstrap) that skip [`set_kernel_stack`].
///
/// ## Why set the *current* GS base too (the real fix)
///
/// The invariant we need is "**whenever kernel code runs, `GS_BASE == &PER_CPU`**"
/// — every `gs:[…]` access (syscall entry loading the kernel stack, syscall exit
/// restoring the user stack) depends on it. The interrupt/exception stubs in
/// `idt.rs` do **not** `swapgs`, so an interrupt taken in ring 3 runs with the
/// task's *user* GS base (0). When that handler calls `schedule()` and switches
/// into another task, `switch_context` does not touch `GS_BASE` (it is a global
/// CPU register, not saved per task), so the resumed task inherits `GS_BASE = 0`
/// and its next `gs:[…]` (e.g. the syscall-exit `mov gs:0x8, rsp`) faults — with
/// `RSP` transiently 0 from the canonical-RIP check, the fault's frame push then
/// double-faults. Writing `IA32_KERNEL_GS_BASE` alone (the previous fix) only
/// repaired the *next* `swapgs`, not the GS base the resumed task actually runs
/// with. Writing `IA32_GS_BASE = &PER_CPU` here guarantees every switched-to task
/// resumes kernel execution with the correct GS base.
///
/// This leaves ring 3 running with `GS_BASE = &PER_CPU` after an `iretq`/`sysretq`
/// that doesn't swap it back, which is harmless: ring-3 code here never reads GS,
/// and the next syscall's `swapgs` (with `KERNEL_GS_BASE = &PER_CPU`) keeps the
/// kernel base correct.
///
/// # Safety
///
/// Must be called with interrupts disabled (during context switch).
pub fn refresh_kernel_gs_base() {
    // SAFETY: single-core, interrupts disabled during the context switch.
    unsafe {
        let per_cpu = &raw mut PER_CPU as u64;
        // Current GS base — what `gs:[…]` uses right now and after the switch.
        cpu::wrmsr(IA32_GS_BASE, per_cpu);
        // Swap target — what the next `swapgs` (syscall entry) brings in.
        cpu::wrmsr(IA32_KERNEL_GS_BASE, per_cpu);
    }
}

/// `IA32_FS_BASE` — the base of the `%fs` segment, used by Linux for thread-local
/// storage (`arch_prctl(ARCH_SET_FS)`). See [`write_fs_base`].
const IA32_FS_BASE: u32 = 0xC000_0100;

/// Write `base` into `IA32_FS_BASE` (Phase 5.1). Called by the scheduler on every
/// context switch to restore a Linux task's TLS base, and by `arch_prctl` to set
/// it. FS base is a global register `switch_context` doesn't save, so — like the
/// GS base — it must be reloaded per task.
pub fn write_fs_base(base: u64) {
    // SAFETY: writing IA32_FS_BASE only changes the %fs segment base; single-core,
    // called with interrupts disabled (context switch) or from a syscall handler.
    unsafe {
        cpu::wrmsr(IA32_FS_BASE, base);
    }
}

// --- Test infrastructure ---
//
// These globals are used by the ring 3 round-trip test (test_syscall_round_trip).
// The test spawns a task that transitions to ring 3 via iretq, executes test
// shellcode that performs syscalls, and the SYS_TEST_COMPLETE handler stores the
// result here.

/// Virtual address of the user code page for the test task.
static TEST_USER_RIP: AtomicU64 = AtomicU64::new(0);

/// User stack pointer for the test task.
static TEST_USER_RSP: AtomicU64 = AtomicU64::new(0);

/// Result of the SYS_NULL syscall, stored by SYS_TEST_COMPLETE handler.
static SYSCALL_TEST_RESULT: AtomicU64 = AtomicU64::new(u64::MAX);

/// Set to true when the test task has completed its syscall round trip.
static SYSCALL_TEST_DONE: AtomicBool = AtomicBool::new(false);

/// Entry function for the syscall test task.
///
/// Reads the user code and stack addresses from the test globals, then
/// transitions to ring 3 via `iretq`. The iretq frame is built on the
/// kernel stack with the correct user segment selectors, RIP, RSP, and
/// RFLAGS (with IF=1 to allow timer interrupts in userspace).
///
/// This function never returns — after iretq, execution continues in
/// ring 3 at the test shellcode address.
fn syscall_test_task() {
    let user_rip = TEST_USER_RIP.load(Ordering::SeqCst);
    let user_rsp = TEST_USER_RSP.load(Ordering::SeqCst);

    // Transition to ring 3 by building a fake iretq frame and executing iretq.
    // The iretq frame format (from top of stack):
    //   RIP (user code address)
    //   CS (user code selector with RPL=3)
    //   RFLAGS (IF=1 for timer interrupts, bit 1 always set)
    //   RSP (user stack pointer)
    //   SS (user data selector with RPL=3)
    //
    // SAFETY: the user code page and stack page are mapped with USER flag,
    // and the segment selectors match valid GDT entries with DPL=3.
    unsafe {
        core::arch::asm!(
            "push {ss}",
            "push {user_rsp}",
            "push {rflags}",
            "push {cs}",
            "push {user_rip}",
            "iretq",
            ss = in(reg) 0x1Bu64,           // USER_DATA_SELECTOR | RPL=3 = 0x18 | 3
            user_rsp = in(reg) user_rsp,
            rflags = in(reg) 0x202u64,      // IF=1 (bit 9) + reserved bit 1
            cs = in(reg) 0x23u64,           // USER_CODE_SELECTOR | RPL=3 = 0x20 | 3
            user_rip = in(reg) user_rip,
            options(noreturn)
        );
    }
}

/// Test the full syscall/sysret round trip from ring 3.
///
/// This is the acceptance test for Sub-phase 2.3. It:
/// 1. Verifies MSR configuration (EFER, STAR, LSTAR, FMASK)
/// 2. Tests the dispatch function directly (SYS_NULL → 0, unknown → -1)
/// 3. Performs a real ring 3 → ring 0 → ring 3 → ring 0 round trip:
///    - Maps user-accessible code and stack pages in the lower canonical half
///    - Writes test shellcode (SYS_NULL, then SYS_TEST_COMPLETE)
///    - Spawns a task that transitions to ring 3 via iretq
///    - Waits for the test task to complete via the SYS_TEST_COMPLETE handler
///    - Verifies SYS_NULL returned 0 to userspace
///
/// Returns `Ok(())` if all checks pass.
pub fn test_syscall_round_trip() -> Result<(), &'static str> {
    use crate::mm;
    use crate::mm::addr::VirtAddr;
    use crate::mm::page_table::{PageFlags, kernel_address_space};

    // --- Part 1: Verify MSR configuration ---

    let efer = unsafe { cpu::rdmsr(IA32_EFER) };
    if efer & EFER_SCE == 0 {
        return Err("EFER.SCE not set — syscall/sysret won't work");
    }

    let star = unsafe { cpu::rdmsr(IA32_STAR) };
    let star_kernel_cs = ((star >> 32) & 0xFFFF) as u16;
    let star_sysret_base = ((star >> 48) & 0xFFFF) as u16;
    if star_kernel_cs != 0x08 {
        return Err("STAR[47:32] != 0x08 — syscall would load wrong kernel CS");
    }
    if star_sysret_base != 0x10 {
        return Err("STAR[63:48] != 0x10 — sysret would load wrong user segments");
    }

    let lstar = unsafe { cpu::rdmsr(IA32_LSTAR) };
    if lstar != syscall_entry as *const () as u64 {
        return Err("LSTAR doesn't point to syscall_entry");
    }

    let fmask = unsafe { cpu::rdmsr(IA32_FMASK) };
    if fmask & RFLAGS_IF == 0 {
        return Err("FMASK doesn't mask IF — interrupts won't be disabled on syscall entry");
    }

    // --- Part 2: Test dispatch function directly ---

    let mut frame = SyscallFrame {
        r15: 0, r14: 0, r13: 0, r12: 0, rbx: 0, rbp: 0,
        r9: 0, r8: 0, r10: 0, rdx: 0, rsi: 0, rdi: 0,
        rax: SYS_NULL, user_rsp: 0, rcx: 0, r11: 0,
    };
    syscall_dispatch(&mut frame);
    if frame.rax != 0 {
        return Err("SYS_NULL dispatch did not return 0");
    }

    frame.rax = 9999;
    syscall_dispatch(&mut frame);
    if frame.rax != !0u64 {
        return Err("unknown syscall dispatch did not return -1");
    }

    // --- Part 3: Real ring 3 round trip ---

    // Allocate frames for user code and stack pages.
    let code_phys = mm::frame::allocate_frame()
        .ok_or("test_syscall: failed to allocate code frame")?;
    let stack_phys = mm::frame::allocate_frame()
        .ok_or("test_syscall: failed to allocate stack frame")?;

    // Pick virtual addresses in the lower canonical half (user space).
    // These must be canonical: bits 47-63 all zero (lower half).
    let code_virt = VirtAddr::new(0x0000_0040_0000_0000);   // 256 GiB
    let stack_virt = VirtAddr::new(0x0000_0040_0000_1000);   // next page

    // Map both pages as user-accessible (USER flag on the leaf PTE,
    // ensure_table already sets USER on intermediate entries).
    let kernel_as = kernel_address_space();
    kernel_as.map_page(
        code_virt, code_phys,
        PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER,
    );
    kernel_as.map_page(
        stack_virt, stack_phys,
        PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER | PageFlags::NO_EXECUTE,
    );
    core::mem::forget(kernel_as);

    // Write test shellcode to the user code page via HHDM.
    //
    // The shellcode:
    //   xor eax, eax        ; rax = 0 (SYS_NULL)
    //   syscall              ; returns 0 in rax
    //   mov rdi, rax         ; pass SYS_NULL result as arg1 to next syscall
    //   mov eax, 0xFFFF      ; rax = SYS_TEST_COMPLETE
    //   syscall              ; handler stores result and kills task
    //   jmp $                ; fallback (never reached)
    let shellcode: [u8; 16] = [
        0x31, 0xC0,                             // xor eax, eax
        0x0F, 0x05,                             // syscall
        0x48, 0x89, 0xC7,                       // mov rdi, rax
        0xB8, 0xFF, 0xFF, 0x00, 0x00,          // mov eax, 0x0000FFFF
        0x0F, 0x05,                             // syscall
        0xEB, 0xFE,                             // jmp $ (infinite loop)
    ];

    let code_hhdm = code_phys.to_virt().as_u64() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(shellcode.as_ptr(), code_hhdm, shellcode.len());
    }

    // User stack top = top of the stack page (stack grows downward).
    let user_rsp_value = stack_virt.as_u64() + mm::PAGE_SIZE;

    // Set up test globals for the spawned task.
    TEST_USER_RIP.store(code_virt.as_u64(), Ordering::SeqCst);
    TEST_USER_RSP.store(user_rsp_value, Ordering::SeqCst);
    SYSCALL_TEST_DONE.store(false, Ordering::SeqCst);
    SYSCALL_TEST_RESULT.store(u64::MAX, Ordering::SeqCst);

    // Spawn a kernel task that will transition to ring 3.
    // The scheduler's context switch will set PerCpu.kernel_stack_top and
    // TSS.RSP0 for this task, so syscall entry will use the correct stack.
    crate::sched::spawn("syscall-test", syscall_test_task);

    // Yield repeatedly to let the test task run. It will:
    // 1. Get scheduled (context switch sets PerCpu and TSS)
    // 2. Call iretq to jump to ring 3
    // 3. Execute shellcode: SYS_NULL → SYS_TEST_COMPLETE
    // 4. SYS_TEST_COMPLETE handler stores result and kills the task
    for _ in 0..10_000 {
        if SYSCALL_TEST_DONE.load(Ordering::SeqCst) {
            break;
        }
        crate::sched::yield_now();
    }

    // Clean up the mapped user pages.
    let kernel_as = kernel_address_space();
    kernel_as.unmap_page(code_virt);
    kernel_as.unmap_page(stack_virt);
    core::mem::forget(kernel_as);
    mm::frame::deallocate_frame(code_phys);
    mm::frame::deallocate_frame(stack_phys);

    // Check the result.
    if !SYSCALL_TEST_DONE.load(Ordering::SeqCst) {
        return Err("syscall test timed out — task never completed");
    }

    let result = SYSCALL_TEST_RESULT.load(Ordering::SeqCst);
    if result != 0 {
        return Err("SYS_NULL returned non-zero to userspace");
    }

    Ok(())
}
