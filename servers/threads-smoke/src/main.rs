//! Linux threads/futex smoke-test binary (Phase 5.3).
//!
//! `_start` mmaps a child stack, `clone`s a thread, has the child write a magic
//! and `exit(60)`, then **joins** by `futex`-waiting on the `CLONE_CHILD_CLEARTID`
//! word until the kernel zeroes it on the child's exit — exercising clone +
//! CLONE_SETTLS + CHILD_SETTID/CLEARTID + futex WAIT/WAKE + thread exit. Reports
//! to the result page at `0x0600_0000`:
//! ```text
//! [0x0600_0000] = RESULT_MAGIC
//! [0x0600_0008] = code   (0 = ok; 1 = child magic missing; 2 = clone failed)
//! ```
//! Scratch: child magic at `0x0600_0808`, CLEARTID/join word at `0x0600_0810`.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

core::arch::global_asm!(
    ".global _start",
    ".section .text._start,\"ax\"",
    "_start:",

    // ---- mmap a child stack (anon, 0x1000) ----
    "    mov  rax, 9",
    "    xor  rdi, rdi",
    "    mov  rsi, 0x1000",
    "    mov  rdx, 3",                 // PROT_READ|WRITE
    "    mov  r10, 0x22",              // MAP_PRIVATE|MAP_ANONYMOUS
    "    mov  r8,  -1",
    "    xor  r9,  r9",
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",
    "    cmp  rax, rcx",
    "    jae  8f",                     // mmap failed -> code 2
    "    lea  r15, [rax + 0x1000]",    // r15 = child stack top

    // ---- clone(thread) ----
    "    mov  rax, 56",
    "    mov  rdi, 0x1290F00",         // VM|FS|FILES|SIGHAND|THREAD|SETTLS|CHILD_SETTID|CHILD_CLEARTID
    "    mov  rsi, r15",               // child stack
    "    xor  rdx, rdx",              // parent_tid (unused)
    "    mov  r10, 0x6000810",        // child_tid / CLEARTID join word
    "    mov  r8,  0x6000900",        // tls (fs base for the child)
    "    syscall",
    "    test rax, rax",
    "    jz   3f",                     // rax == 0 -> child

    // ---- parent: join by futex-waiting until the CLEARTID word is 0 ----
    "    mov  rcx, 0xFFFFFFFFFFFFF000",
    "    cmp  rax, rcx",
    "    jae  8f",                     // clone returned an error -> code 2
    "2:  mov ecx, dword ptr [0x6000810]",
    "    test ecx, ecx",
    "    jz   4f",                     // word cleared -> child exited -> joined
    "    mov  rax, 202",              // futex
    "    mov  rdi, 0x6000810",
    "    xor  rsi, rsi",              // FUTEX_WAIT
    "    mov  edx, ecx",              // expected current value
    "    xor  r10, r10",             // no timeout
    "    syscall",
    "    jmp  2b",

    // ---- child: write magic, then exit(60) (thread exit) ----
    "3:  mov rcx, 0x6000808",
    "    movabs rax, 0xC411D0C0DE110001",
    "    mov  qword ptr [rcx], rax",
    "    mov  rax, 60",               // exit (this thread)
    "    xor  rdi, rdi",
    "    syscall",
    "9:  jmp 9b",                      // unreachable

    // ---- joined: verify the child ran, then report success ----
    "4:  mov rcx, 0x6000808",
    "    mov  rax, qword ptr [rcx]",
    "    movabs rdx, 0xC411D0C0DE110001",
    "    cmp  rax, rdx",
    "    jne  7f",                     // child magic missing -> code 1
    "    mov  rbx, 0x6000000",
    "    movabs rax, 0x7C0DE57C0DE57C01",
    "    mov  qword ptr [rbx], rax",
    "    mov  qword ptr [rbx + 8], 0",
    "    jmp  10f",

    // ---- failure paths ----
    "7:  mov rbx, 0x6000000",          // child magic missing
    "    movabs rax, 0x7C0DE57C0DE57C01",
    "    mov  qword ptr [rbx], rax",
    "    mov  qword ptr [rbx + 8], 1",
    "    jmp  10f",
    "8:  mov rbx, 0x6000000",          // clone/mmap failed
    "    movabs rax, 0x7C0DE57C0DE57C01",
    "    mov  qword ptr [rbx], rax",
    "    mov  qword ptr [rbx + 8], 2",

    // ---- exit_group(0) ----
    "10: mov rax, 231",
    "    xor rdi, rdi",
    "    syscall",
    "1:  jmp 1b",
);
