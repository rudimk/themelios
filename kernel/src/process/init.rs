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

/// Trampoline function for the init task.
///
/// This runs in kernel mode (ring 0) within the init process's address space.
/// It builds an iretq frame on the kernel stack and transitions to ring 3
/// at the init code page address. This function never returns.
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
