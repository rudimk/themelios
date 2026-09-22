//! # Phase 8.5b — the native IPC ABI, exercised from EL0
//!
//! 8.4 proved a syscall can cross EL0→EL1→EL0 at all, using a set of test calls invented
//! for the purpose. This proves the **real ABI** works from EL0: the four IPC syscalls
//! `servers/libthemelios` actually issues, with their real register mappings, their
//! multi-value returns, and their blocking behaviour.
//!
//! ## Why this exists rather than waiting for `echo-server`
//!
//! The obvious place to discover that aarch64's IPC dispatch is wrong is the first server
//! that runs on it. That server arrives in 8.5c, behind six hand-written `_start` routines
//! and a flat-binary link that has never been attempted for this architecture. Debugging a
//! register-mapping bug *through* all of that — as an undefined result in a ring-3 program
//! whose entry sequence is itself new — is far more expensive than finding it here.
//!
//! It is also this sub-phase's answer to its own recurring failure: a dispatcher nothing
//! calls is indistinguishable from a dispatcher that works. Adding 160 lines of `match`
//! arms and declaring the ABI ported would have been exactly that.
//!
//! ## What the round trip checks, and how it self-checks
//!
//! A kernel task and an EL0 task talk to each other across three endpoints, so every
//! syscall runs in the direction a real server would use it:
//!
//! | step | kernel task | EL0 payload | proves |
//! |---|---|---|---|
//! | 1 | `ipc_call(EP_A, …)` | `SYS_RECEIVE` then `SYS_REPLY` | the six-value return; `REPLY`'s mapping |
//! | 2 | `ipc_receive(EP_B)` | `SYS_SEND` | `SEND`'s six arguments |
//! | 3 | `ipc_receive(EP_C)` + `ipc_reply` | `SYS_CALL` | `CALL`'s four-word reply |
//!
//! Step 1 is the interesting one, because it cannot be faked. `SYS_RECEIVE` returns the
//! reply token in `x5`, and the payload passes that token straight back as `SYS_REPLY`'s
//! second argument. If `set_rets` writes the token to the wrong slot — or if `REPLY` reads
//! its arguments from the wrong registers — the reply is addressed to nothing, the kernel
//! task stays blocked in `ipc_call`, and the test times out. The token's correctness is
//! *required for the test to finish at all*, not merely asserted afterwards.
//!
//! Everywhere else the payload folds each returned value into one accumulator at a distinct
//! shift, so two values arriving in each other's registers changes the exit code rather
//! than cancelling out. The single number the test compares is therefore sensitive to every
//! slot in all four calls.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::mm::addr::VirtAddr;
use crate::mm::frame;
use crate::mm::page_table::{AddressSpace, PageFlags};
use crate::println;

/// User VAs for the payload's code and stack.
///
/// Distinct from the 8.4b self-test's and the soak's, so a stale mapping from either cannot
/// satisfy this one. Both pages are per-task in a fresh `AddressSpace`, so the addresses
/// only have to avoid colliding with each other.
const CODE_VA: u64 = 0x70_0000;
const STACK_VA: u64 = 0xb0_0000;

/// Request words the kernel task sends in step 1, and the badge alongside them.
///
/// Distinct bit patterns rather than small integers: a word arriving in the wrong slot has
/// to change the accumulator, and `1`/`2`/`3` differ too little for a shift-and-add to
/// guarantee that.
const REQ: [u64; 4] = [0x11, 0x22, 0x33, 0x44];
const REQ_BADGE: u64 = 0x55;

/// Words the EL0 payload sends back in step 2, and its badge.
const SEND_WORDS: [u64; 4] = [0x66, 0x77, 0x88, 0x99];
const SEND_BADGE: u64 = 0xAA;

/// Words the kernel task replies with in step 3.
const CALL_REPLY: [u64; 4] = [0xB1, 0xB2, 0xB3, 0xB4];

/// The accumulator the payload is expected to exit with.
///
/// Computed here the same way the assembly computes it, from the same constants — so the
/// expectation and the payload cannot disagree about what a correct run produces without
/// one of them being edited in isolation.
const fn expected_acc() -> u64 {
    // Step 1: the four request words at shifts 0/8/16/24, then the badge at 32.
    let mut acc = REQ[0] | (REQ[1] << 8) | (REQ[2] << 16) | (REQ[3] << 24) | (REQ_BADGE << 32);
    // Step 3: the four reply words at shifts 40/44/48/52. Narrower spacing is fine — the
    // values are all under 0x100 and the shifts stay inside 64 bits.
    acc += CALL_REPLY[0] << 40;
    acc += CALL_REPLY[1] << 44;
    acc += CALL_REPLY[2] << 48;
    acc += CALL_REPLY[3] << 52;
    acc
}

/// Endpoint ids, published for the payload's sake once created.
static EP_A: AtomicU64 = AtomicU64::new(0);
static EP_B: AtomicU64 = AtomicU64::new(0);
static EP_C: AtomicU64 = AtomicU64::new(0);

/// Set when the kernel task has completed all three steps without a mismatch.
static KERNEL_SIDE_OK: AtomicBool = AtomicBool::new(false);
/// Set when the kernel task has finished, pass or fail, so `run` can distinguish
/// "still going" from "gave up".
static KERNEL_SIDE_DONE: AtomicBool = AtomicBool::new(false);
/// What the EL0 payload exited with, and whether it exited at all.
static EL0_ACC: AtomicU64 = AtomicU64::new(0);
static EL0_EXITED: AtomicBool = AtomicBool::new(false);

/// Record the EL0 task's exit, from the `SYS_EXIT` arm of the native dispatcher.
///
/// Returns `true` if this test owns the exit, so the 8.4b self-test's single global pair is
/// left alone — the same arrangement the soak uses, and for the same reason: two EL0 tests'
/// exits landing in one slot would have the second overwrite the first.
pub fn note_exit(code: u64) -> bool {
    if !ARMED.load(Ordering::Relaxed) {
        return false;
    }
    EL0_ACC.store(code, Ordering::Relaxed);
    EL0_EXITED.store(true, Ordering::Release);
    true
}

/// Whether this test's EL0 task is live. Gates [`note_exit`] so the hook is inert
/// otherwise.
static ARMED: AtomicBool = AtomicBool::new(false);

// --- The EL0 payload ---
//
// Position-independent for the same reason as the other two payloads: it executes at a user
// VA unrelated to where it was linked. Only immediate `mov`, `svc`, shifted-register `add`,
// and `adr`-free control flow.
//
// The endpoint ids are *not* baked in — they are allocated at runtime by `create_endpoint`.
// They arrive in x0/x1/x2, placed there by `enter_el0`'s caller before the drop to EL0, and
// the payload parks them in callee-saved registers immediately. That is itself a small
// check on the frame: if the exception return does not restore x0-x2 as set up, every
// subsequent syscall addresses the wrong endpoint and the test times out.
core::arch::global_asm!(
    r#"
.section .rodata
.balign 4
.globl ipc_payload_start
.globl ipc_payload_end
ipc_payload_start:
    mov  x19, x0            // EP_A
    mov  x20, x1            // EP_B
    mov  x21, x2            // EP_C
    mov  x22, xzr           // accumulator

    // --- Step 1: RECEIVE on EP_A, then REPLY with the token we were handed ---
    //
    // Returns words[0..4] in x0-x3, the badge in x4 and the reply token in x5.
    mov  x8, #{nr_receive}
    mov  x0, x19
    svc  #0

    // Fold all five *data* slots in at distinct shifts. The token is deliberately not
    // folded in: its value is dynamic, and it is checked far more strongly by being
    // required to work in the REPLY below.
    add  x22, x22, x0
    add  x22, x22, x1, lsl #8
    add  x22, x22, x2, lsl #16
    add  x22, x22, x3, lsl #24
    add  x22, x22, x4, lsl #32

    // REPLY(EP_A, token, [0,0,0,0]). x5 still holds the token from the receive above.
    mov  x1, x5
    mov  x8, #{nr_reply}
    mov  x0, x19
    mov  x2, xzr
    mov  x3, xzr
    mov  x4, xzr
    mov  x5, xzr
    svc  #0

    // --- Step 2: SEND on EP_B ---
    //
    // x0 = endpoint, x1-x4 = words, x5 = badge. Six arguments, which is the widest the
    // positional ABI goes and the only call here that uses every one of them.
    mov  x8, #{nr_send}
    mov  x0, x20
    mov  x1, #{sw0}
    mov  x2, #{sw1}
    mov  x3, #{sw2}
    mov  x4, #{sw3}
    mov  x5, #{sbadge}
    svc  #0

    // --- Step 3: CALL on EP_C ---
    //
    // Same six arguments in, four reply words back in x0-x3. The request words are not
    // checked by the kernel side here (step 2 already covers argument passing); what this
    // step exists for is the four-word reply.
    mov  x8, #{nr_call}
    mov  x0, x21
    mov  x1, xzr
    mov  x2, xzr
    mov  x3, xzr
    mov  x4, xzr
    mov  x5, xzr
    svc  #0

    add  x22, x22, x0, lsl #40
    add  x22, x22, x1, lsl #44
    add  x22, x22, x2, lsl #48
    add  x22, x22, x3, lsl #52

    // --- Exit with the accumulator ---
    mov  x8, #{nr_exit}
    mov  x0, x22
    svc  #0
1:  b    1b                 // unreachable: SYS_EXIT blocks the task
ipc_payload_end:
"#,
    // Every number and immediate substituted, never written as a literal — see the note on
    // the EL0 payload in `syscall.rs`. Here it matters twice over: these are the *ABI*
    // numbers, so a literal that drifted would not merely call the wrong thing, it would
    // call the wrong thing in a way that still looked like a valid server request.
    nr_receive = const crate::arch::syscall::abi::SYS_RECEIVE,
    nr_reply = const crate::arch::syscall::abi::SYS_REPLY,
    nr_send = const crate::arch::syscall::abi::SYS_SEND,
    nr_call = const crate::arch::syscall::abi::SYS_CALL,
    nr_exit = const crate::arch::syscall::abi::SYS_EXIT,
    sw0 = const SEND_WORDS[0],
    sw1 = const SEND_WORDS[1],
    sw2 = const SEND_WORDS[2],
    sw3 = const SEND_WORDS[3],
    sbadge = const SEND_BADGE,
);

unsafe extern "C" {
    static ipc_payload_start: u8;
    static ipc_payload_end: u8;
}

fn payload() -> &'static [u8] {
    // SAFETY: both symbols are defined by the `global_asm!` above and bracket a contiguous
    // run of bytes in `.rodata`.
    unsafe {
        let start = &raw const ipc_payload_start;
        let end = &raw const ipc_payload_end;
        core::slice::from_raw_parts(start, end as usize - start as usize)
    }
}

/// The kernel half of the conversation.
///
/// Runs as an ordinary kernel task so every call here blocks the way a real peer's would;
/// driving both halves from the boot path would deadlock the moment the first one blocked.
fn kernel_peer() {
    let ep_a = EP_A.load(Ordering::Relaxed);
    let ep_b = EP_B.load(Ordering::Relaxed);
    let ep_c = EP_C.load(Ordering::Relaxed);

    // Step 1: call into EL0 and wait for its reply. If the payload mishandles the token
    // this never returns, which is the timeout `run` bounds.
    let step1 = crate::ipc::ipc_call(ep_a, crate::ipc::IpcMessage::new(REQ), REQ_BADGE);
    if step1.is_err() {
        println!("[el0-ipc] FAIL — kernel ipc_call on EP_A errored");
        KERNEL_SIDE_DONE.store(true, Ordering::Release);
        return;
    }

    // Step 2: receive what EL0 sent, and check every word and the badge. This is the only
    // check on `SYS_SEND`'s argument mapping, so it compares all six positions.
    match crate::ipc::ipc_receive(ep_b) {
        Ok(m) => {
            if m.words != SEND_WORDS || m.badge != SEND_BADGE {
                println!(
                    "[el0-ipc] FAIL — SEND arrived as words {:#x?} badge {:#x}, expected {:#x?} / {:#x}",
                    m.words, m.badge, SEND_WORDS, SEND_BADGE
                );
                KERNEL_SIDE_DONE.store(true, Ordering::Release);
                return;
            }
        }
        Err(_) => {
            println!("[el0-ipc] FAIL — kernel ipc_receive on EP_B errored");
            KERNEL_SIDE_DONE.store(true, Ordering::Release);
            return;
        }
    }

    // Step 3: serve EL0's CALL, replying with the four words it folds into its accumulator.
    match crate::ipc::ipc_receive(ep_c) {
        Ok(m) => {
            let r = crate::ipc::ipc_reply(ep_c, m.reply_token, crate::ipc::IpcMessage::new(CALL_REPLY));
            if r.is_err() {
                println!("[el0-ipc] FAIL — kernel ipc_reply on EP_C errored");
                KERNEL_SIDE_DONE.store(true, Ordering::Release);
                return;
            }
        }
        Err(_) => {
            println!("[el0-ipc] FAIL — kernel ipc_receive on EP_C errored");
            KERNEL_SIDE_DONE.store(true, Ordering::Release);
            return;
        }
    }

    KERNEL_SIDE_OK.store(true, Ordering::Relaxed);
    KERNEL_SIDE_DONE.store(true, Ordering::Release);
}

/// Entry point for the EL0 task: drop to EL0 with the endpoint ids in x0-x2.
fn el0_entry() {
    let sp = STACK_VA + crate::mm::PAGE_SIZE;
    // SAFETY: code and stack are mapped in this task's installed tree with the permissions
    // the payload needs; the scheduler installed its TTBR0 on the switch that got us here.
    unsafe {
        crate::arch::aarch64::syscall::enter_el0_with_args(
            CODE_VA,
            sp,
            [
                EP_A.load(Ordering::Relaxed),
                EP_B.load(Ordering::Relaxed),
                EP_C.load(Ordering::Relaxed),
            ],
        )
    }
}

/// Run the round trip. Returns `true` if both halves agree it completed correctly.
pub fn run() -> bool {
    assert!(
        !crate::arch::irq::are_enabled(),
        "el0_ipc::run() requires interrupts masked on entry"
    );

    EP_A.store(crate::ipc::create_endpoint("el0-ipc-a"), Ordering::Relaxed);
    EP_B.store(crate::ipc::create_endpoint("el0-ipc-b"), Ordering::Relaxed);
    EP_C.store(crate::ipc::create_endpoint("el0-ipc-c"), Ordering::Relaxed);

    let bytes = payload();
    assert!(
        bytes.len() <= crate::mm::PAGE_SIZE as usize,
        "ipc payload is {} bytes, larger than the single page it is copied into",
        bytes.len()
    );

    let (Some(code_frame), Some(stack_frame)) = (frame::allocate_frame(), frame::allocate_frame())
    else {
        println!("[el0-ipc] FAIL — out of frames");
        return false;
    };
    let space = AddressSpace::new_user();
    // SAFETY: freshly allocated frame, reachable through the HHDM, exclusively ours.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), code_frame.as_mut_ptr::<u8>(), bytes.len());
    }
    space.map_page(
        VirtAddr::new(CODE_VA),
        code_frame,
        PageFlags::PRESENT.union(PageFlags::USER),
    );
    space.map_page(
        VirtAddr::new(STACK_VA),
        stack_frame,
        PageFlags::PRESENT
            .union(PageFlags::WRITABLE)
            .union(PageFlags::USER)
            .union(PageFlags::NO_EXECUTE),
    );

    ARMED.store(true, Ordering::Relaxed);
    let el0_task = crate::sched::spawn("el0-ipc", el0_entry);
    crate::sched::set_task_user_space(el0_task, space.root_phys().as_u64(), space.asid());
    // No teardown for EL0 tasks yet; the space must outlive the task, which never exits.
    core::mem::forget(space);

    crate::sched::spawn("el0-ipc-peer", kernel_peer);

    // Both halves block, so the timer must be running for either to be scheduled — the same
    // reasoning as the 8.4b self-test's wait loop. The bound is in ticks, which only
    // advance because interrupts are on here.
    crate::arch::irq::enable();
    let start = crate::arch::time::tick_count();
    while !(EL0_EXITED.load(Ordering::Acquire) && KERNEL_SIDE_DONE.load(Ordering::Acquire))
        && crate::arch::time::tick_count() - start < 400
    {
        core::hint::spin_loop();
    }
    crate::arch::irq::disable();
    ARMED.store(false, Ordering::Relaxed);

    let kernel_ok = KERNEL_SIDE_OK.load(Ordering::Relaxed);
    let exited = EL0_EXITED.load(Ordering::Acquire);
    let acc = EL0_ACC.load(Ordering::Relaxed);
    let want = expected_acc();

    if !exited {
        // The most likely cause is the one described in the module docs: a reply token that
        // did not survive `SYS_RECEIVE`'s return, leaving both halves blocked forever.
        println!(
            "[el0-ipc] FAIL — EL0 task never exited (kernel side {}); a lost reply token \
             leaves both halves blocked",
            if kernel_ok { "completed" } else { "also stuck or failed" }
        );
        return false;
    }
    if !kernel_ok {
        // The kernel half prints its own specific failure before setting DONE.
        return false;
    }
    if acc != want {
        println!(
            "[el0-ipc] FAIL — accumulator {:#x}, expected {:#x} (xor {:#x} names the slots \
             that moved)",
            acc,
            want,
            acc ^ want
        );
        return false;
    }

    println!(
        "[el0-ipc] PASS (SEND/RECEIVE/REPLY/CALL across 3 endpoints from EL0; \
         accumulator {:#x} matches, reply token round-tripped through userspace)",
        acc
    );
    true
}
