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
fn user_range_ok(uptr: u64, len: usize) -> bool {
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
fn copy_from_user(uptr: u64, len: usize) -> Option<alloc::vec::Vec<u8>> {
    if len > FS_MAX_XFER || !user_range_ok(uptr, len) {
        return None;
    }
    // SAFETY: we are in the calling process's address space (its CR3 is active
    // during the syscall) and have verified every page of the range is mapped.
    let slice = unsafe { core::slice::from_raw_parts(uptr as *const u8, len) };
    Some(slice.to_vec())
}

/// Copy `data` to a validated user pointer. Returns false if the range is bad.
fn copy_to_user(uptr: u64, data: &[u8]) -> bool {
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

        // Point IA32_KERNEL_GS_BASE to our PerCpu struct.
        // After `swapgs` in the syscall entry stub, `gs:[0]` addresses
        // PerCpu.kernel_stack_top and `gs:[8]` addresses PerCpu.user_rsp_scratch.
        let per_cpu_addr = &raw const PER_CPU as u64;
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
