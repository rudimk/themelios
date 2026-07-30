//! Linux filesystem-syscall smoke-test binary (Phase 5.2).
//!
//! Run with `Personality::Linux` and a container rootfs mount that has a staged
//! file `/hello.txt`. `_start`:
//!   1. `openat(AT_FDCWD, "/hello.txt", O_RDONLY)` and `read` it,
//!   2. checks `..` is clamped — `openat("../../../hello.txt")` must succeed
//!      (resolves back to `/hello.txt`), and `openat("/nope_xyz")` must fail,
//!   3. reports to the kernel-mapped result page at `0x0600_0000`:
//! ```text
//! [0x0600_0000] = RESULT_MAGIC
//! [0x0600_0008] = code  (0 = ok; 1=open, 2=read, 3=clamp, 4=phantom-file)
//! [0x0600_0010] = first 8 bytes read from /hello.txt
//! [0x0600_0018] = number of bytes read
//! ```
//! then `exit_group`. Read buffer is `0x0600_0800` (inside the result page).

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

    // ---- openat(AT_FDCWD, "/hello.txt", O_RDONLY) ----
    "    mov  rax, 257",
    "    mov  rdi, -100",              // AT_FDCWD
    "    lea  rsi, [rip + 10f]",       // "/hello.txt"
    "    xor  rdx, rdx",               // O_RDONLY
    "    xor  r10, r10",               // mode
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",// error returns are >= -4096 (unsigned)
    "    cmp  rax, rcx",
    "    jae  2f",                     // open failed -> code 1
    "    mov  r14, rax",               // r14 = fd

    // ---- read(fd, 0x6000800, 64) ----
    "    xor  rax, rax",               // read = 0
    "    mov  rdi, r14",
    "    mov  rsi, 0x6000800",
    "    mov  rdx, 64",
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",
    "    cmp  rax, rcx",
    "    jae  3f",                     // read error -> code 2
    "    test rax, rax",
    "    jz   3f",                     // read 0 -> code 2
    "    mov  r13, rax",               // r13 = bytes read

    // ---- clamp check: "../../../hello.txt" must resolve back to /hello.txt ----
    "    mov  rax, 257",
    "    mov  rdi, -100",
    "    lea  rsi, [rip + 11f]",       // "../../../hello.txt"
    "    xor  rdx, rdx",
    "    xor  r10, r10",
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",
    "    cmp  rax, rcx",
    "    jae  4f",                     // clamped-open failed -> code 3

    // ---- negative check: "/nope_xyz" must FAIL (ENOENT) ----
    "    mov  rax, 257",
    "    mov  rdi, -100",
    "    lea  rsi, [rip + 12f]",       // "/nope_xyz"
    "    xor  rdx, rdx",
    "    xor  r10, r10",
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",
    "    cmp  rax, rcx",
    "    jb   5f",                     // a valid fd -> phantom file -> code 4

    // ---- success: magic + code 0 + first 8 bytes + length ----
    "    mov  rbx, 0x6000000",
    "    movabs rax, 0xF5C0DE5510ADED55",
    "    mov  qword ptr [rbx], rax",
    "    mov  qword ptr [rbx + 8], 0",
    "    mov  rcx, 0x6000800",
    "    mov  rax, qword ptr [rcx]",   // first 8 bytes read
    "    mov  qword ptr [rbx + 16], rax",
    "    mov  qword ptr [rbx + 24], r13",
    "    jmp  9f",

    // ---- failure paths ----
    "2:  mov rbx, 0x6000000",
    "    movabs rax, 0xF5C0DE5510ADED55",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 1",
    "    jmp 9f",
    "3:  mov rbx, 0x6000000",
    "    movabs rax, 0xF5C0DE5510ADED55",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 2",
    "    jmp 9f",
    "4:  mov rbx, 0x6000000",
    "    movabs rax, 0xF5C0DE5510ADED55",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 3",
    "    jmp 9f",
    "5:  mov rbx, 0x6000000",
    "    movabs rax, 0xF5C0DE5510ADED55",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 4",

    // ---- exit_group(0) ----
    "9:  mov rax, 231",
    "    xor rdi, rdi",
    "    syscall",
    "1:  jmp 1b",

    // path data
    "10: .asciz \"/hello.txt\"",
    "11: .asciz \"../../../hello.txt\"",
    "12: .asciz \"/nope_xyz\"",
);
