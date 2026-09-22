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
//! A kernel task and an EL0 task talk to each other across four endpoints, so every syscall
//! runs in the direction a real server would use it:
//!
//! | step | kernel task | EL0 payload | proves |
//! |---|---|---|---|
//! | 1 | `ipc_call(EP_A, …)`, checks the reply words | `SYS_RECEIVE` then `SYS_REPLY` | `RECEIVE`'s six-value return; `REPLY`'s six arguments |
//! | 2 | `ipc_receive(EP_B)`, checks words + badge | `SYS_SEND` | `SEND`'s six arguments |
//! | 3 | `ipc_send(EP_D, …)` after step 2 | `SYS_TRY_RECEIVE`, polled | both branches, and the *shifted* six-value return |
//! | 4 | `ipc_receive(EP_C)`, checks words + badge, then replies | `SYS_CALL` | `CALL`'s six arguments and four-word reply |
//! | 5 | — | `YIELD`, `UPTIME_MS`, `DEBUG_PRINT`, `NULL`, an unimplemented number | the arms with no other caller |
//!
//! Step 1 cannot be faked. `SYS_RECEIVE` returns the reply token in `x5`, and the payload
//! passes that token straight back as `SYS_REPLY`'s second argument. If `set_rets` writes
//! it to the wrong slot, the reply is addressed to nothing, the kernel task stays blocked
//! in `ipc_call`, and the test times out. The token's correctness is *required for the test
//! to terminate*, not asserted afterwards.
//!
//! Every other checked value is folded into one rolling hash, `acc = acc * 31 + value`, in
//! a fixed order — so two values arriving in each other's registers change the exit code
//! rather than cancelling out.
//!
//! ## What an earlier version of this claimed, and did not do
//!
//! This block used to say the accumulator was "sensitive to every slot in all four calls".
//! It was not, and a review demonstrated it: the payload sent `REPLY` with all four words
//! `xzr` and `CALL` with all six arguments `xzr`, and the kernel peer looked at neither. So
//! `SYS_REPLY`'s and `SYS_CALL`'s *argument* mappings could both be reversed in the
//! dispatcher and this test still printed PASS.
//!
//! That mattered specifically rather than abstractly: `echo-server`, the first thing 8.5c
//! runs, does `receive` then `reply(ep, token, [...])`. A wrong `REPLY` word mapping is the
//! first bug it would hit, and it is exactly the bug this test exists to pre-empt.
//!
//! The same review broke all six arms the payload never called — `NULL`, `TRY_RECEIVE`,
//! `YIELD`, `UPTIME_MS`, `DEBUG_PRINT` and the sixteen `ENOSYS` numbers — simultaneously,
//! and the suite still reported 25 passed / 0 failed. Five of fifteen arms were covered.
//! Step 5 and the `TRY_RECEIVE` poll close both gaps; the table above is now a description
//! rather than an intention.

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
/// Distinct bit patterns rather than small integers, so a word arriving in the wrong slot
/// has to change the hash rather than possibly cancelling against another.
const REQ: [u64; 4] = [0x11, 0x22, 0x33, 0x44];
const REQ_BADGE: u64 = 0x55;

/// Words the EL0 payload replies with in step 1, checked by the kernel peer.
///
/// These were all zero until a review reversed `SYS_REPLY`'s four word slots in the
/// dispatcher and this test still printed PASS. `REPLY`'s word mapping is the first thing a
/// real server exercises, so it is now carried by real values that the peer compares.
const REPLY_WORDS: [u64; 4] = [0xC1, 0xC2, 0xC3, 0xC4];

/// Words the EL0 payload sends in step 2, and its badge.
const SEND_WORDS: [u64; 4] = [0x66, 0x77, 0x88, 0x99];
const SEND_BADGE: u64 = 0xAA;

/// Words the kernel peer sends to EP_D for the EL0 task to pick up with `TRY_RECEIVE`.
const TRY_WORDS: [u64; 4] = [0xD1, 0xD2, 0xD3, 0xD4];

/// Words the EL0 payload sends as its `SYS_CALL` request, checked by the kernel peer —
/// uncovered for the same reason `REPLY_WORDS` was.
const CALL_WORDS: [u64; 4] = [0xE1, 0xE2, 0xE3, 0xE4];
const CALL_BADGE: u64 = 0xE5;

/// Words the kernel task replies with to the EL0 task's `SYS_CALL`.
const CALL_REPLY: [u64; 4] = [0xB1, 0xB2, 0xB3, 0xB4];

/// Byte the payload passes to `SYS_DEBUG_PRINT`, which appears on the console when that arm
/// works. Chosen printable so a human reading the serial log can see it.
const PRINT_CH: u64 = b'*' as u64;

/// Multiplier for the payload's rolling hash.
///
/// The accumulator is `acc = acc * HASH_MUL + value` per checked value, rather than each
/// value packed at its own bit offset. Packing ran out of bits once the checked set grew
/// past nine values, and worse, it made the *number of things checked* a function of the
/// bit budget. A rolling hash is position-sensitive without a budget, so coverage can grow
/// without re-laying the encoding — and `madd` computes it in one instruction.
const HASH_MUL: u64 = 31;

/// The accumulator the payload is expected to exit with.
///
/// Computed here by the same rule and in the same order as the assembly, from the same
/// constants, so the expectation and the payload cannot disagree about a correct run
/// without one of them being edited in isolation.
const fn expected_acc() -> u64 {
    const fn fold(acc: u64, v: u64) -> u64 {
        acc.wrapping_mul(HASH_MUL).wrapping_add(v)
    }
    let mut a = 0u64;
    // Step 1, RECEIVE: four words then the badge. The reply token is deliberately not
    // folded — its value is dynamic, and it is checked by being required to work.
    a = fold(a, REQ[0]);
    a = fold(a, REQ[1]);
    a = fold(a, REQ[2]);
    a = fold(a, REQ[3]);
    a = fold(a, REQ_BADGE);
    // Step 3, TRY_RECEIVE: the had-a-message flag, four words, then the reply token, which
    // is 0 for a plain send and is checked as being 0.
    a = fold(a, 1);
    a = fold(a, TRY_WORDS[0]);
    a = fold(a, TRY_WORDS[1]);
    a = fold(a, TRY_WORDS[2]);
    a = fold(a, TRY_WORDS[3]);
    a = fold(a, 0);
    // Step 4, CALL: the four reply words.
    a = fold(a, CALL_REPLY[0]);
    a = fold(a, CALL_REPLY[1]);
    a = fold(a, CALL_REPLY[2]);
    a = fold(a, CALL_REPLY[3]);
    // Step 5: YIELD -> 0, UPTIME_MS -> non-zero (folded as 1), DEBUG_PRINT -> 0,
    // NULL -> 0, and an unimplemented ABI number -> ENOSYS.
    a = fold(a, 0);
    a = fold(a, 1);
    a = fold(a, 0);
    a = fold(a, 0);
    a = fold(a, super::syscall::ENOSYS);
    a
}

/// Endpoint ids, published for the payload's sake once created.
static EP_A: AtomicU64 = AtomicU64::new(0);
static EP_B: AtomicU64 = AtomicU64::new(0);
static EP_C: AtomicU64 = AtomicU64::new(0);
static EP_D: AtomicU64 = AtomicU64::new(0);

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
    mov  x23, x3            // EP_D
    mov  x22, xzr           // accumulator
    mov  x25, #{mul}        // the hash multiplier, held for the whole payload

    // --- Step 1: RECEIVE on EP_A, then REPLY with the token we were handed ---
    mov  x8, #{nr_receive}
    mov  x0, x19
    svc  #0

    // Fold the five data slots. The token (x5) is not folded: its value is dynamic, and it
    // is checked far more strongly by being required to work in the REPLY below.
    madd x22, x22, x25, x0
    madd x22, x22, x25, x1
    madd x22, x22, x25, x2
    madd x22, x22, x25, x3
    madd x22, x22, x25, x4

    // REPLY(EP_A, token, [four non-zero words]).
    //
    // Those words were all `xzr` until a review reversed REPLY's four word slots in the
    // dispatcher and this test still printed PASS. They are non-zero now and the kernel
    // peer compares them, because REPLY's word mapping is the first thing a real server
    // exercises — `echo-server` does `receive` then `reply(ep, token, [...])` — and it was
    // the one mapping this test existed to pre-empt and did not cover.
    mov  x1, x5             // the token, straight back out
    mov  x8, #{nr_reply}
    mov  x0, x19
    mov  x2, #{rw0}
    mov  x3, #{rw1}
    mov  x4, #{rw2}
    mov  x5, #{rw3}
    svc  #0

    // --- Step 2: SEND on EP_B ---
    mov  x8, #{nr_send}
    mov  x0, x20
    mov  x1, #{sw0}
    mov  x2, #{sw1}
    mov  x3, #{sw2}
    mov  x4, #{sw3}
    mov  x5, #{sbadge}
    svc  #0

    // --- Step 3: poll TRY_RECEIVE on EP_D until the peer's message lands ---
    //
    // The poll covers both branches: the peer sends only after servicing step 2, so the
    // first calls return 0 (no message) and a later one returns 1 with the words. That is
    // the only exercise of the six-slot return's *shifted* form — the flag in slot 0 moving
    // the words down one relative to RECEIVE — which had no caller anywhere before.
2:  mov  x8, #{nr_try_receive}
    mov  x0, x23
    svc  #0
    cbz  x0, 2b

    madd x22, x22, x25, x0  // the flag itself
    madd x22, x22, x25, x1
    madd x22, x22, x25, x2
    madd x22, x22, x25, x3
    madd x22, x22, x25, x4
    madd x22, x22, x25, x5  // reply token: 0 for a plain send, and checked as such

    // --- Step 4: CALL on EP_C, with a real request the peer verifies ---
    mov  x8, #{nr_call}
    mov  x0, x21
    mov  x1, #{cw0}
    mov  x2, #{cw1}
    mov  x3, #{cw2}
    mov  x4, #{cw3}
    mov  x5, #{cbadge}
    svc  #0

    madd x22, x22, x25, x0
    madd x22, x22, x25, x1
    madd x22, x22, x25, x2
    madd x22, x22, x25, x3

    // --- Step 5: the arms with no other caller ---
    //
    // A review broke all six of these at once — NULL, TRY_RECEIVE, YIELD, UPTIME_MS,
    // DEBUG_PRINT and the sixteen ENOSYS numbers — and the suite still reported 25/0/30.
    // Folding each return into the same accumulator is what makes them load-bearing.

    mov  x8, #{nr_yield}    // YIELD -> 0
    svc  #0
    madd x22, x22, x25, x0

    mov  x8, #{nr_uptime}   // UPTIME_MS -> a live tick count; fold whether it is non-zero,
    svc  #0                 // since the value itself is not reproducible
    cmp  x0, #0
    cset x0, ne
    madd x22, x22, x25, x0

    mov  x8, #{nr_print}    // DEBUG_PRINT(byte) -> 0, and puts a character on the console
    mov  x0, #{print_ch}
    svc  #0
    madd x22, x22, x25, x0

    mov  x8, #{nr_null}     // NULL -> 0
    svc  #0
    madd x22, x22, x25, x0

    mov  x8, #{nr_unimpl}   // an ABI number aarch64 does not implement yet -> ENOSYS.
    svc  #0                 // Folded raw, so returning 0 or falling through to the test
    madd x22, x22, x25, x0  // range's own ENOSYS by accident both change the accumulator.

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
    nr_try_receive = const crate::arch::syscall::abi::SYS_TRY_RECEIVE,
    nr_call = const crate::arch::syscall::abi::SYS_CALL,
    nr_yield = const crate::arch::syscall::abi::SYS_YIELD,
    nr_uptime = const crate::arch::syscall::abi::SYS_UPTIME_MS,
    nr_print = const crate::arch::syscall::abi::SYS_DEBUG_PRINT,
    nr_null = const crate::arch::syscall::abi::SYS_NULL,
    nr_unimpl = const crate::arch::syscall::abi::SYS_OPEN,
    nr_exit = const crate::arch::syscall::abi::SYS_EXIT,
    mul = const HASH_MUL,
    print_ch = const PRINT_CH,
    rw0 = const REPLY_WORDS[0],
    rw1 = const REPLY_WORDS[1],
    rw2 = const REPLY_WORDS[2],
    rw3 = const REPLY_WORDS[3],
    sw0 = const SEND_WORDS[0],
    sw1 = const SEND_WORDS[1],
    sw2 = const SEND_WORDS[2],
    sw3 = const SEND_WORDS[3],
    sbadge = const SEND_BADGE,
    cw0 = const CALL_WORDS[0],
    cw1 = const CALL_WORDS[1],
    cw2 = const CALL_WORDS[2],
    cw3 = const CALL_WORDS[3],
    cbadge = const CALL_BADGE,
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
    let ep_d = EP_D.load(Ordering::Relaxed);

    macro_rules! fail {
        ($($arg:tt)*) => {{
            println!($($arg)*);
            KERNEL_SIDE_DONE.store(true, Ordering::Release);
            return;
        }};
    }

    // Step 1: call into EL0 and wait for its reply. If the payload mishandles the token
    // this never returns, which is the timeout `run` bounds.
    //
    // The reply's *words* are checked too. They were ignored until a review reversed
    // `SYS_REPLY`'s four word slots and this test still passed.
    match crate::ipc::ipc_call(ep_a, crate::ipc::IpcMessage::new(REQ), REQ_BADGE) {
        Ok(reply) => {
            if reply.words != REPLY_WORDS {
                fail!(
                    "[el0-ipc] FAIL — REPLY words arrived as {:#x?}, expected {:#x?}",
                    reply.words, REPLY_WORDS
                );
            }
        }
        Err(_) => fail!("[el0-ipc] FAIL — kernel ipc_call on EP_A errored"),
    }

    // Step 2: receive what EL0 sent, checking every word and the badge — the only check on
    // `SYS_SEND`'s six argument positions.
    match crate::ipc::ipc_receive(ep_b) {
        Ok(m) => {
            if m.words != SEND_WORDS || m.badge != SEND_BADGE {
                fail!(
                    "[el0-ipc] FAIL — SEND arrived as words {:#x?} badge {:#x}, expected {:#x?} / {:#x}",
                    m.words, m.badge, SEND_WORDS, SEND_BADGE
                );
            }
        }
        Err(_) => fail!("[el0-ipc] FAIL — kernel ipc_receive on EP_B errored"),
    }

    // Step 3: post a message for the EL0 task to collect with `TRY_RECEIVE`.
    //
    // Sent only now, after step 2, so the payload's poll loop is guaranteed to observe the
    // empty case first — which is how both branches of that arm get covered rather than
    // just the one.
    if crate::ipc::ipc_send(ep_d, crate::ipc::IpcMessage::new(TRY_WORDS), 0).is_err() {
        fail!("[el0-ipc] FAIL — kernel ipc_send on EP_D errored");
    }

    // Step 4: serve EL0's CALL. Its *request* words and badge are checked here for the same
    // reason step 1's reply words are: they were `xzr` and nothing looked at them, so
    // `SYS_CALL`'s argument mapping was unverified.
    match crate::ipc::ipc_receive(ep_c) {
        Ok(m) => {
            if m.words != CALL_WORDS || m.badge != CALL_BADGE {
                fail!(
                    "[el0-ipc] FAIL — CALL request arrived as words {:#x?} badge {:#x}, expected {:#x?} / {:#x}",
                    m.words, m.badge, CALL_WORDS, CALL_BADGE
                );
            }
            if crate::ipc::ipc_reply(ep_c, m.reply_token, crate::ipc::IpcMessage::new(CALL_REPLY))
                .is_err()
            {
                fail!("[el0-ipc] FAIL — kernel ipc_reply on EP_C errored");
            }
        }
        Err(_) => fail!("[el0-ipc] FAIL — kernel ipc_receive on EP_C errored"),
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
                EP_D.load(Ordering::Relaxed),
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
    EP_D.store(crate::ipc::create_endpoint("el0-ipc-d"), Ordering::Relaxed);

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
        // Deliberately does not name a cause. An earlier version asserted "a lost reply
        // token leaves both halves blocked", which is the *most common* cause but not the
        // only one — breaking `TRY_RECEIVE` strands the peer in `ipc_send` on EP_D instead,
        // and the message then confidently blamed the wrong mechanism. Any arm that fails
        // to hand control back leaves this same footprint.
        println!(
            "[el0-ipc] FAIL — EL0 task never exited (kernel side {}). Some call did not \
             return control: a reply token lost in `SYS_RECEIVE`'s return and a broken \
             `TRY_RECEIVE` both look like this",
            if kernel_ok { "completed" } else { "also stuck or failed" }
        );
        return false;
    }
    if !kernel_ok {
        // The kernel half prints its own specific failure before setting DONE — but only
        // when it reached one. A review noted the `!DONE` case returns here having printed
        // nothing; CI still catches it through the `[boot] …FAILED` sentinel, but a silent
        // branch in a self-test is worth closing rather than relying on the backstop.
        if !KERNEL_SIDE_DONE.load(Ordering::Acquire) {
            println!("[el0-ipc] FAIL — kernel peer never finished (still blocked?)");
        }
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
        "[el0-ipc] PASS (all 15 native-ABI arms from EL0 across 4 endpoints — \
         SEND/RECEIVE/REPLY/CALL/TRY_RECEIVE/YIELD/UPTIME_MS/DEBUG_PRINT/NULL/EXIT plus \
         ENOSYS; hash {:#x} matches over 20 checked values, reply token round-tripped \
         through userspace)",
        acc
    );
    true
}
