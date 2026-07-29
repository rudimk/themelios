//! Linux syscall-personality smoke-test binary (Phase 5.1).
//!
//! Loaded by the ELF loader and run with `Personality::Linux`, so its `syscall`s
//! go through the kernel's Linux dispatch table. Its `_start` exercises the core
//! 5.1 syscall surface and self-checks each result, then reports to a
//! kernel-mapped result page at `0x0600_0000`:
//!
//! ```text
//! [0x0600_0000] = RESULT_MAGIC
//! [0x0600_0008] = code   (0 = all checks passed; 1=fs, 2=brk, 3=mmap on failure)
//! ```
//!
//! Checks: `arch_prctl(ARCH_SET_FS)` + a `%fs`-relative write/read (TLS works and
//! survives scheduling), `brk` growth + a write to the new heap page, anonymous
//! `mmap` + a write to the mapping, and a `write(1, …)` to the console. Then
//! `exit_group`. `0x0600_0800` (inside the result page) is used as the TLS base.

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

    // ---- arch_prctl(ARCH_SET_FS, 0x6000800) : set the TLS base ----
    "    mov  rax, 158",             // arch_prctl
    "    mov  rdi, 0x1002",          // ARCH_SET_FS
    "    mov  rsi, 0x6000800",       // TLS base (inside the result page, mapped RW)
    "    syscall",
    "    test rax, rax",
    "    jnz  2f",                   // arch_prctl must return 0
    "    movabs rax, 0xF5BA5E1234567890",
    "    mov  qword ptr fs:[0], rax",// write via %fs
    "    mov  rbx, qword ptr fs:[0]",// read back via %fs
    "    cmp  rbx, rax",
    "    jne  2f",                   // TLS mismatch -> fail code 1

    // ---- brk(0) then grow, write to the new page ----
    "    mov  rax, 12",             // brk
    "    xor  rdi, rdi",            // brk(0) -> current break
    "    syscall",
    "    mov  r12, rax",            // r12 = brk base
    "    lea  rdi, [r12 + 0x1000]",
    "    mov  rax, 12",
    "    syscall",                  // brk(base + 0x1000)
    "    cmp  rax, r12",
    "    jbe  3f",                  // must advance -> else fail code 2
    "    mov  qword ptr [r12], 0x34343434",
    "    mov  rcx, qword ptr [r12]",
    "    cmp  rcx, 0x34343434",
    "    jne  3f",

    // ---- mmap(NULL, 0x1000, RW, MAP_ANON|MAP_PRIVATE, -1, 0) ----
    "    mov  rax, 9",              // mmap
    "    xor  rdi, rdi",            // addr = NULL
    "    mov  rsi, 0x1000",         // len
    "    mov  rdx, 3",              // PROT_READ|PROT_WRITE
    "    mov  r10, 0x22",           // MAP_PRIVATE|MAP_ANONYMOUS
    "    mov  r8,  -1",             // fd
    "    xor  r9,  r9",             // offset
    "    syscall",
    "    mov  r13, rax",
    "    mov  rcx, 0xFFFFFFFFFFFFF000", // error returns are >= -4096 (unsigned high)
    "    cmp  r13, rcx",
    "    jae  4f",                  // mmap failed -> fail code 3
    "    mov  qword ptr [r13], 0x3D3D3D3D",
    "    mov  rdx, qword ptr [r13]",
    "    cmp  rdx, 0x3D3D3D3D",
    "    jne  4f",

    // ---- write(1, msg, len) : prove stdout reaches the console ----
    "    mov  rax, 1",             // write
    "    mov  rdi, 1",             // fd 1
    "    lea  rsi, [rip + 5f]",    // msg
    "    mov  rdx, 15",            // len of "linux-smoke ok\n"
    "    syscall",

    // ---- success: magic + code 0 ----
    "    mov  rbx, 0x6000000",
    "    movabs rax, 0x5A11D0C510ADED11",
    "    mov  qword ptr [rbx], rax",
    "    mov  qword ptr [rbx + 8], 0",
    "    jmp  9f",

    // ---- failure paths: magic + code {1,2,3} ----
    "2:  mov rbx, 0x6000000",       // fs failure
    "    movabs rax, 0x5A11D0C510ADED11",
    "    mov  qword ptr [rbx], rax",
    "    mov  qword ptr [rbx + 8], 1",
    "    jmp  9f",
    "3:  mov rbx, 0x6000000",       // brk failure
    "    movabs rax, 0x5A11D0C510ADED11",
    "    mov  qword ptr [rbx], rax",
    "    mov  qword ptr [rbx + 8], 2",
    "    jmp  9f",
    "4:  mov rbx, 0x6000000",       // mmap failure
    "    movabs rax, 0x5A11D0C510ADED11",
    "    mov  qword ptr [rbx], rax",
    "    mov  qword ptr [rbx + 8], 3",

    // ---- exit_group(0) ----
    "9:  mov rax, 231",
    "    xor rdi, rdi",
    "    syscall",
    "1:  jmp 1b",                   // never reached

    // message data
    "5:  .ascii \"linux-smoke ok\\n\"",
);
