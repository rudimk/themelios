//! # First userspace process (init)
//!
//! The capstone of Phase 2: boots a process in ring 3 with its own address
//! space, communicates with the kernel via syscalls, and exercises the full
//! isolation stack (page tables, capability system, IPC, audit logging).
//!
//! ## Init process layout
//!
//! - **Code**: one page at 0x400000 (4 MiB). Contains a small assembly loop
//!   that sends IPC messages to the kernel and yields between iterations.
//! - **Stack**: 8 pages at 0x7FFFFFEFC000 - 0x7FFFFFF00000 (32 KiB). RSP
//!   starts at the top: 0x7FFFFFF00000.
//! - **Capabilities**: an endpoint capability for communicating with the kernel.
//!
//! ## Init behavior
//!
//! The init code blob is a tight loop:
//! 1. Send an IPC message containing a counter value to the kernel endpoint
//! 2. Yield the time slice
//! 3. Increment counter and repeat
//!
//! A kernel-side "init server" task receives these messages and prints them
//! to serial, demonstrating the complete IPC path from ring 3 userspace
//! through the syscall interface to the kernel.
//!
//! ## Init code blob
//!
//! The init process doesn't load an ELF binary — the kernel directly writes
//! machine code into the user code page. This is the minimal approach for
//! Phase 2; ELF loading comes in Phase 5 with the Linux compat layer.
//!
//! ## Two blobs, two ways of producing them (Phase 8.5d)
//!
//! The x86_64 blob is hand-assembled byte by byte, in Rust, at runtime — including
//! patching the endpoint id in as a `mov r13, imm64` operand. That is what Phase 2 wrote
//! and it is left alone: it works, it is covered, and rewriting it would be churn.
//!
//! The aarch64 blob is **not** written that way, and the difference is deliberate rather
//! than stylistic. Hand-encoding A64 is not meaningfully harder than x86 — the
//! instructions are all four bytes — but the *endpoint patching* is: a 64-bit immediate
//! needs a `movz`/`movk` quartet with the value split across four instructions' bit
//! 20:5 fields, and an arithmetic slip there produces a blob that runs, sends to the
//! wrong endpoint, and hangs. So the payload is assembled by the toolchain in a
//! `global_asm!` block (the mechanism `arch::aarch64::el0_ipc` already uses) and the
//! endpoint arrives **in a register**, placed there by `enter_el0_with_args` — no
//! patching, nothing to get wrong, and a wrong register is a failure the test reports
//! rather than a hang.

use core::sync::atomic::{AtomicU64, AtomicBool, Ordering};

use crate::println;
use crate::mm;
use crate::mm::addr::{VirtAddr, PhysAddr};
use crate::mm::page_table::PageFlags;
use crate::sched;
use crate::process;
use crate::ipc;
use crate::cap::{Capability, CapType, CapRights};

/// User virtual address where init's code page is mapped.
const INIT_CODE_VIRT: u64 = 0x0000_0000_0040_0000; // 4 MiB

/// User virtual address range for init's stack (8 pages = 32 KiB).
/// Stack grows downward from INIT_STACK_TOP.
const INIT_STACK_BASE: u64 = 0x0000_7FFF_FFEF_C000;
const INIT_STACK_TOP: u64  = 0x0000_7FFF_FFF0_0000;
const INIT_STACK_PAGES: usize = 8;

/// The IPC endpoint ID used for init ↔ kernel communication.
/// Stored as an atomic so both the init code blob and the kernel server
/// task can read it.
static INIT_ENDPOINT_ID: AtomicU64 = AtomicU64::new(0);

/// Set to true when the init server has received at least one message.
/// Used by tests to verify the full stack works.
static INIT_SERVER_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Counter of messages received by the init server.
static INIT_MESSAGE_COUNT: AtomicU64 = AtomicU64::new(0);

/// Create and boot the init process.
///
/// This is the main entry point for the init subsystem. It:
/// 1. Creates an IPC endpoint for init ↔ kernel communication
/// 2. Creates a new process with its own address space and CSpace
/// 3. Maps a code page and stack pages into the process's address space
/// 4. Writes the init shellcode into the code page
/// 5. Grants the process an Endpoint capability
/// 6. Spawns a kernel-side server task to receive init's messages
/// 7. Spawns the init task which transitions to ring 3
pub fn start() {
    // --- Step 1: Create the IPC endpoint ---
    let endpoint_id = ipc::create_endpoint("init-ep");
    INIT_ENDPOINT_ID.store(endpoint_id, Ordering::SeqCst);

    // --- Step 2: Create the init process ---
    let (pid, _cap_handle) = process::create_process("init", None);

    // --- Step 3: Map code and stack pages ---

    // Allocate and map the code page
    let code_phys = mm::frame::allocate_frame()
        .expect("init: failed to allocate code frame");
    process::with_address_space(pid, |addr_space| {
        addr_space.map_page(
            VirtAddr::new(INIT_CODE_VIRT),
            code_phys,
            PageFlags::PRESENT | PageFlags::USER,
        );
    }).expect("init: process has no address space");

    // Allocate and map the stack pages
    for i in 0..INIT_STACK_PAGES {
        let stack_phys = mm::frame::allocate_frame()
            .expect("init: failed to allocate stack frame");
        let stack_virt = VirtAddr::new(INIT_STACK_BASE + (i as u64) * mm::PAGE_SIZE);
        process::with_address_space(pid, |addr_space| {
            addr_space.map_page(
                stack_virt,
                stack_phys,
                PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER | PageFlags::NO_EXECUTE,
            );
        }).expect("init: process has no address space");
    }

    // --- Step 4: Write the init shellcode ---
    //
    // The shellcode is a loop that:
    //   1. Sends an IPC message to the kernel endpoint (SYS_SEND = 1)
    //      Convention: RDI = endpoint_id, RSI = word0 (counter), RDX = word1, R10 = word2, R8 = badge
    //   2. Yields (SYS_YIELD = 5)
    //   3. Increments the counter and loops
    //
    // We write the endpoint ID as an immediate in the shellcode.
    write_init_shellcode(code_phys, endpoint_id);

    // --- Step 5: Grant init an Endpoint capability ---
    process::with_cspace_mut(pid, |cspace| {
        let ep_cap = Capability {
            cap_type: CapType::Endpoint { endpoint_id, badge: 1 },
            rights: CapRights::READ | CapRights::WRITE,
            parent: None,
        };
        cspace.insert(ep_cap).expect("init: failed to insert endpoint cap");
    }).expect("init: process has no CSpace");

    // --- Step 6: Spawn the kernel-side server task ---
    let server_id = sched::spawn("init-server", init_server_task);
    process::assign_task_to_kernel(server_id);

    // --- Step 7: Spawn the init task in ring 3 ---
    //
    // We spawn a kernel-mode task in the init process. This task's entry
    // function builds an iretq frame and jumps to ring 3 at the user code
    // address. The scheduler handles CR3 switching when this task runs.
    let init_task_id = sched::spawn_in_process("init-main", init_trampoline, pid);

    println!("[init] Init process started: {} (task {}, endpoint {}, server task {})",
        pid, init_task_id, endpoint_id, server_id);
}

/// Write the init shellcode into the code page via the HHDM mapping.
///
/// The shellcode is hand-assembled x86_64 instructions that implement:
/// ```asm
/// ; r12 = counter (preserved across syscalls, callee-saved)
/// ; r13 = endpoint_id (preserved across syscalls, callee-saved)
/// xor r12d, r12d          ; counter = 0
/// mov r13, <endpoint_id>  ; endpoint_id (patched in by kernel)
/// loop:
///   mov rax, 1            ; SYS_SEND
///   mov rdi, r13          ; endpoint_id
///   mov rsi, r12          ; word0 = counter
///   xor edx, edx          ; word1 = 0
///   xor r10d, r10d        ; word2 = 0
///   xor r9d, r9d          ; word3 = 0
///   xor r8d, r8d          ; badge = 0
///   syscall
///   mov rax, 5            ; SYS_YIELD
///   syscall
///   inc r12               ; counter++
///   jmp loop
/// ```
#[cfg(target_arch = "x86_64")]
fn write_init_shellcode(code_phys: PhysAddr, endpoint_id: u64) {
    let code_ptr = code_phys.to_virt().as_u64() as *mut u8;

    // Hand-assembled x86_64 shellcode
    let mut code: [u8; 64] = [0; 64];
    let mut i = 0;

    // xor r12d, r12d — clear counter (3 bytes: 45 31 E4)
    code[i] = 0x45; code[i+1] = 0x31; code[i+2] = 0xE4;
    i += 3;

    // mov r13, imm64 — load endpoint_id (10 bytes: 49 BD <8 bytes le>)
    code[i] = 0x49; code[i+1] = 0xBD;
    let ep_bytes = endpoint_id.to_le_bytes();
    code[i+2..i+10].copy_from_slice(&ep_bytes);
    i += 10;

    // -- loop start (offset = i = 13) --
    let loop_start = i;

    // mov rax, 1 — SYS_SEND (7 bytes: 48 C7 C0 01 00 00 00)
    code[i] = 0x48; code[i+1] = 0xC7; code[i+2] = 0xC0;
    code[i+3] = 0x01; code[i+4] = 0x00; code[i+5] = 0x00; code[i+6] = 0x00;
    i += 7;

    // mov rdi, r13 — endpoint_id (3 bytes: 4C 89 EF)
    code[i] = 0x4C; code[i+1] = 0x89; code[i+2] = 0xEF;
    i += 3;

    // mov rsi, r12 — word0 = counter (3 bytes: 4C 89 E6)
    code[i] = 0x4C; code[i+1] = 0x89; code[i+2] = 0xE6;
    i += 3;

    // xor edx, edx — word1 = 0 (2 bytes: 31 D2)
    code[i] = 0x31; code[i+1] = 0xD2;
    i += 2;

    // xor r10d, r10d — word2 = 0 (3 bytes: 45 31 D2)
    code[i] = 0x45; code[i+1] = 0x31; code[i+2] = 0xD2;
    i += 3;

    // xor r9d, r9d — word3 = 0 (3 bytes: 45 31 C9)
    code[i] = 0x45; code[i+1] = 0x31; code[i+2] = 0xC9;
    i += 3;

    // xor r8d, r8d — badge = 0 (3 bytes: 45 31 C0)
    code[i] = 0x45; code[i+1] = 0x31; code[i+2] = 0xC0;
    i += 3;

    // syscall (2 bytes: 0F 05)
    code[i] = 0x0F; code[i+1] = 0x05;
    i += 2;

    // mov rax, 5 — SYS_YIELD (7 bytes: 48 C7 C0 05 00 00 00)
    code[i] = 0x48; code[i+1] = 0xC7; code[i+2] = 0xC0;
    code[i+3] = 0x05; code[i+4] = 0x00; code[i+5] = 0x00; code[i+6] = 0x00;
    i += 7;

    // syscall (2 bytes: 0F 05)
    code[i] = 0x0F; code[i+1] = 0x05;
    i += 2;

    // inc r12 — counter++ (3 bytes: 49 FF C4)
    code[i] = 0x49; code[i+1] = 0xFF; code[i+2] = 0xC4;
    i += 3;

    // jmp loop_start — relative jump back (2 bytes: EB <offset>)
    // offset = loop_start - (i + 2) [2-byte instruction, offset from after jmp]
    let jmp_offset = (loop_start as i8) - ((i + 2) as i8);
    code[i] = 0xEB; code[i+1] = jmp_offset as u8;

    // Write the shellcode to the physical page via HHDM
    unsafe {
        core::ptr::copy_nonoverlapping(code.as_ptr(), code_ptr, code.len());
    }
}

// --- The aarch64 init payload (Phase 8.5d) ---
//
// The same loop as the x86 shellcode above — SEND a counter, YIELD, increment, repeat —
// assembled by the toolchain rather than by hand.
//
// It is **position-independent and branches only backwards by a literal offset**, because
// it is copied to a user page at a fixed virtual address that has nothing to do with where
// the assembler laid it out. No `adr`, no literal pool, no relocations: every instruction
// here is self-contained.
//
// The endpoint id is *not* baked in. It arrives in `x0` from `enter_el0_with_args` and is
// parked in `x19` before the first syscall. Contrast the x86 side, which patches it into a
// `mov r13, imm64`; see the module docs for why the two differ.
//
// Register choice: `x19`/`x20` are callee-saved under AAPCS64, which is irrelevant here
// (nothing is called) but is the convention `el0_ipc`'s payload follows, and the syscall
// path restores all of `x0`-`x30` from the exception frame regardless.
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    r#"
.section .rodata
.balign 4
.globl init_payload_start
.globl init_payload_end
init_payload_start:
    mov  x19, x0            // endpoint id, handed over in x0
    mov  x20, xzr           // counter = 0
1:
    mov  x8, #{nr_send}
    mov  x0, x19            // endpoint
    mov  x1, x20            // word0 = counter
    mov  x2, xzr            // word1
    mov  x3, xzr            // word2
    mov  x4, xzr            // word3
    mov  x5, xzr            // badge
    svc  #0

    mov  x8, #{nr_yield}
    svc  #0

    add  x20, x20, #1
    b    1b
init_payload_end:
"#,
    nr_send = const crate::arch::syscall::abi::SYS_SEND,
    nr_yield = const crate::arch::syscall::abi::SYS_YIELD,
);

#[cfg(target_arch = "aarch64")]
unsafe extern "C" {
    static init_payload_start: u8;
    static init_payload_end: u8;
}

/// Copy the assembled aarch64 init payload into the code page.
///
/// Takes `endpoint_id` only to keep one signature across the two architectures — this
/// blob receives it in a register at entry, so there is nothing to patch. Named
/// `_endpoint_id` rather than dropped from the signature so the two bodies stay
/// interchangeable at the call site and a reader sees immediately that the parameter is
/// unused *here*, not that the endpoint is ignored.
#[cfg(target_arch = "aarch64")]
fn write_init_shellcode(code_phys: PhysAddr, _endpoint_id: u64) {
    // SAFETY: both symbols are defined by the `global_asm!` above and bracket a
    // contiguous run of bytes in `.rodata`.
    let blob = unsafe {
        let start = &raw const init_payload_start;
        let end = &raw const init_payload_end;
        core::slice::from_raw_parts(start, end as usize - start as usize)
    };
    assert!(
        blob.len() <= mm::PAGE_SIZE as usize,
        "init payload does not fit in its single code page"
    );

    // SAFETY: `code_phys` is a freshly allocated frame reachable through the HHDM, and
    // the assertion above bounds the copy to the page.
    unsafe {
        core::ptr::copy_nonoverlapping(
            blob.as_ptr(),
            code_phys.to_virt().as_mut_ptr::<u8>(),
            blob.len(),
        );
    }
}

/// Trampoline function for the init task.
///
/// This runs in kernel mode (ring 0) within the init process's address space
/// and transitions to userspace at the init code page address. Never returns.
///
/// As with `server::server_trampoline`, the contract is shared and the mechanism is not:
/// x86 builds an `iretq` frame by hand, aarch64 hands the register discipline to
/// `enter_el0_with_args` — which additionally delivers the endpoint id in `x0`, since the
/// aarch64 payload is not patched with it.
#[cfg(target_arch = "x86_64")]
fn init_trampoline() {
    let user_rip = INIT_CODE_VIRT;
    let user_rsp = INIT_STACK_TOP;

    // Transition to ring 3 via iretq.
    // The iretq frame (from top of stack):
    //   RIP  = user code address
    //   CS   = user code selector | RPL=3 = 0x23
    //   RFLAGS = IF=1 (enable timer interrupts), bit 1 set (reserved)
    //   RSP  = user stack top
    //   SS   = user data selector | RPL=3 = 0x1B
    //
    // SAFETY: the code and stack pages are mapped with USER flag in the
    // init process's address space, and the segment selectors match valid
    // GDT entries with DPL=3.
    unsafe {
        core::arch::asm!(
            "push {ss}",
            "push {user_rsp}",
            "push {rflags}",
            "push {cs}",
            "push {user_rip}",
            "iretq",
            ss = in(reg) 0x1Bu64,           // USER_DATA_SELECTOR | RPL=3
            user_rsp = in(reg) user_rsp,
            rflags = in(reg) 0x202u64,      // IF=1 (bit 9) + reserved bit 1
            cs = in(reg) 0x23u64,           // USER_CODE_SELECTOR | RPL=3
            user_rip = in(reg) user_rip,
            options(noreturn)
        );
    }
}

/// The aarch64 half of [`init_trampoline`]. See the x86_64 sibling for the contract.
#[cfg(target_arch = "aarch64")]
fn init_trampoline() {
    let endpoint_id = INIT_ENDPOINT_ID.load(Ordering::SeqCst);

    // SAFETY: the code page is mapped USER (executable) and the stack pages USER|WRITABLE
    // in this process's `TTBR0_EL1` tree, which `sched::spawn_in_process` attached to this
    // task — so `schedule()` installed it before this ran. `x0` carries the endpoint the
    // payload sends on. Never returns.
    unsafe {
        crate::arch::aarch64::syscall::enter_el0_with_args(
            INIT_CODE_VIRT,
            INIT_STACK_TOP,
            [endpoint_id, 0, 0, 0],
        )
    }
}

/// Kernel-side server task that receives messages from the init process.
///
/// Runs an infinite loop calling `ipc_receive()` on the init endpoint.
/// Each received message is printed to serial, demonstrating the full
/// ring 3 → syscall → IPC → kernel receive pipeline.
fn init_server_task() {
    let endpoint_id = INIT_ENDPOINT_ID.load(Ordering::SeqCst);

    loop {
        match ipc::ipc_receive(endpoint_id) {
            Ok(msg) => {
                let count = INIT_MESSAGE_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
                INIT_SERVER_RECEIVED.store(true, Ordering::SeqCst);

                // Only print the first few messages to avoid flooding serial
                if count <= 5 {
                    println!("[init-server] Message #{}: word0={}, badge={}",
                        count, msg.words[0], msg.badge);
                } else if count == 6 {
                    println!("[init-server] (suppressing further messages)");
                }
            }
            Err(e) => {
                println!("[init-server] Receive error: {:?}", e);
                break;
            }
        }
    }
}

/// Check whether the init server has received at least one message.
///
/// Used by tests to verify the full ring 3 → syscall → IPC pipeline works.
pub fn server_has_received() -> bool {
    INIT_SERVER_RECEIVED.load(Ordering::SeqCst)
}

/// Get the number of messages the init server has received.
pub fn server_message_count() -> u64 {
    INIT_MESSAGE_COUNT.load(Ordering::SeqCst)
}
