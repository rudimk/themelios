//! Container-isolation smoke-test binary (Phase 5.7).
//!
//! Run with `Personality::Linux` and a container rootfs mount that stages a file
//! `/only` (known contents). As a container `/init`, `_start` proves the
//! isolation boundary is *enforced on the live syscall path*:
//!
//!   1. **positive control** — `openat("/only", O_RDONLY)` + `read` succeeds
//!      (the FS syscall path works; this is not a vacuous all-deny test);
//!   2. **live-clamp proof** — `openat("../../../../only", O_RDONLY)` must succeed
//!      *and* read bytes **identical** to (1). `..` clamps back to `/only`, so the
//!      escape resolves to the same file. (A bare `-ENOENT` on a host path would
//!      prove nothing — a container has one mount and no host root, so the miss
//!      happens whether the clamp works or not. The byte-match is what proves the
//!      clamp is live.)
//!   3. **genuine absence** — `openat("/nonesuch")` must fail (`-ENOENT`);
//!   4. **socket denied** — `socket(AF_INET, SOCK_DGRAM, 0)` must return exactly
//!      `-EPERM` (the container holds no `SOCKET_FACTORY` capability).
//!
//! Reports to the kernel-mapped result page at `0x0600_0000`:
//! ```text
//! [0x0600_0000] = RESULT_MAGIC (0x1501_A710_0ADE_D011)
//! [0x0600_0008] = code  (0 = all checks passed; else the first failing step:
//!                        1=open, 2=read, 3=clamp-open/read, 4=byte-mismatch,
//!                        5=phantom-file, 6=socket-allowed, 7=socket-wrong-errno)
//! [0x0600_0010] = first 8 bytes read from /only (debug)
//! ```
//! then `exit_group(0)`. Read buffers live inside the result page
//! (`0x0600_0800` direct, `0x0600_0840` clamped).

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

    // ---- (1) openat(AT_FDCWD, "/only", O_RDONLY) ----
    "    mov  rax, 257",
    "    mov  rdi, -100",              // AT_FDCWD
    "    lea  rsi, [rip + 10f]",       // "/only"
    "    xor  rdx, rdx",               // O_RDONLY
    "    xor  r10, r10",
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",// error returns are >= -4096 (unsigned)
    "    cmp  rax, rcx",
    "    jae  2f",                     // open failed -> code 1
    "    mov  r14, rax",               // r14 = fd

    // ---- read(fd, 0x6000800, 8) ----
    "    xor  rax, rax",               // read = 0
    "    mov  rdi, r14",
    "    mov  rsi, 0x6000800",
    "    mov  rdx, 8",
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",
    "    cmp  rax, rcx",
    "    jae  3f",                     // read error -> code 2
    "    test rax, rax",
    "    jz   3f",                     // read 0 -> code 2
    "    mov  r12, qword ptr [0x6000800]", // r12 = direct-read word

    // ---- (2) openat("../../../../only") must SUCCEED (clamp back to /only) ----
    "    mov  rax, 257",
    "    mov  rdi, -100",
    "    lea  rsi, [rip + 11f]",       // "../../../../only"
    "    xor  rdx, rdx",
    "    xor  r10, r10",
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",
    "    cmp  rax, rcx",
    "    jae  4f",                     // clamped-open failed -> code 3
    "    mov  r14, rax",

    // ---- read(fd, 0x6000840, 8) ----
    "    xor  rax, rax",
    "    mov  rdi, r14",
    "    mov  rsi, 0x6000840",
    "    mov  rdx, 8",
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",
    "    cmp  rax, rcx",
    "    jae  4f",                     // clamped-read failed -> code 3
    "    test rax, rax",
    "    jz   4f",
    "    mov  r13, qword ptr [0x6000840]", // r13 = clamped-read word

    // ---- (3) the clamped read must match the direct read (same file) ----
    "    cmp  r12, r13",
    "    jne  5f",                     // bytes differ -> code 4 (clamp not live)

    // ---- (4) openat("/nonesuch") must FAIL (ENOENT) ----
    "    mov  rax, 257",
    "    mov  rdi, -100",
    "    lea  rsi, [rip + 12f]",       // "/nonesuch"
    "    xor  rdx, rdx",
    "    xor  r10, r10",
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",
    "    cmp  rax, rcx",
    "    jb   6f",                     // a valid fd -> phantom file -> code 5

    // ---- (5) socket(AF_INET, SOCK_DGRAM, 0) must be exactly -EPERM ----
    "    mov  rax, 41",                // socket
    "    mov  rdi, 2",                 // AF_INET
    "    mov  rsi, 2",                 // SOCK_DGRAM
    "    xor  rdx, rdx",               // protocol 0
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",
    "    cmp  rax, rcx",
    "    jb   7f",                     // a valid fd -> socket allowed -> code 6
    "    cmp  rax, -1",                // -EPERM == -1 (EPERM = 1)
    "    jne  8f",                     // denied but wrong errno -> code 7

    // ---- success: magic + code 0 + the read word (debug) ----
    "    mov  rbx, 0x6000000",
    "    movabs rax, 0x1501A7100ADED011",
    "    mov  qword ptr [rbx], rax",
    "    mov  qword ptr [rbx + 8], 0",
    "    mov  qword ptr [rbx + 16], r12",
    "    jmp  9f",

    // ---- failure paths (magic + code) ----
    "2:  mov rbx, 0x6000000",
    "    movabs rax, 0x1501A7100ADED011",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 1",
    "    jmp 9f",
    "3:  mov rbx, 0x6000000",
    "    movabs rax, 0x1501A7100ADED011",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 2",
    "    jmp 9f",
    "4:  mov rbx, 0x6000000",
    "    movabs rax, 0x1501A7100ADED011",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 3",
    "    jmp 9f",
    "5:  mov rbx, 0x6000000",
    "    movabs rax, 0x1501A7100ADED011",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 4",
    "    jmp 9f",
    "6:  mov rbx, 0x6000000",
    "    movabs rax, 0x1501A7100ADED011",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 5",
    "    jmp 9f",
    "7:  mov rbx, 0x6000000",
    "    movabs rax, 0x1501A7100ADED011",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 6",
    "    jmp 9f",
    "8:  mov rbx, 0x6000000",
    "    movabs rax, 0x1501A7100ADED011",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 7",

    // ---- exit_group(0) ----
    "9:  mov rax, 231",
    "    xor rdi, rdi",
    "    syscall",
    "1:  jmp 1b",

    // path data
    "10: .asciz \"/only\"",
    "11: .asciz \"../../../../only\"",
    "12: .asciz \"/nonesuch\"",
);
