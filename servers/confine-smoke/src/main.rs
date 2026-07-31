//! Container rootfs-confinement smoke-test binary (Phase 6.1b).
//!
//! Run as a **confined** container `/init` (the process has rootfs base
//! `/c/<id>`), over a mount that also has a real `/host_secret` file staged at its
//! **root** (outside any container base). `_start` proves the confinement is
//! enforced on the live syscall path:
//!
//!   1. **positive control** — `openat("/only", O_RDONLY)` succeeds (its own file,
//!      staged under `/c/<id>/only`; proves the FS path works and the base maps
//!      correctly, not all-deny);
//!   2. **root escape blocked** — `openat("/host_secret")` must **fail**: the base
//!      maps it to `/c/<id>/host_secret`, which does not exist. If it returned a
//!      valid fd, confinement is broken (the container reached the mount root).
//!   3. **`..` escape blocked** — `openat("../../../../host_secret")` must also
//!      fail: `..` clamps to `/host_secret`, then the base applies → still
//!      `/c/<id>/host_secret`.
//!
//! Reports to the kernel-mapped result page at `0x0600_0000`:
//! ```text
//! [0x0600_0000] = RESULT_MAGIC (0xC0F1_11ED_0ADE_D011)
//! [0x0600_0008] = code (0 = all pass; 1 = own-file open failed; 2 = /host_secret
//!                 was OPENABLE (confinement broken); 3 = ../ escape was openable)
//! ```
//! then `exit_group(0)`.

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

    // ---- (1) positive control: openat("/only") must SUCCEED ----
    "    mov  rax, 257",
    "    mov  rdi, -100",              // AT_FDCWD
    "    lea  rsi, [rip + 10f]",       // "/only"
    "    xor  rdx, rdx",               // O_RDONLY
    "    xor  r10, r10",
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",// error returns are >= -4096 (unsigned)
    "    cmp  rax, rcx",
    "    jae  2f",                     // own file open failed -> code 1

    // ---- (2) openat("/host_secret") must FAIL (confined; not at the root) ----
    "    mov  rax, 257",
    "    mov  rdi, -100",
    "    lea  rsi, [rip + 11f]",       // "/host_secret"
    "    xor  rdx, rdx",
    "    xor  r10, r10",
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",
    "    cmp  rax, rcx",
    "    jb   3f",                     // got a valid fd -> confinement broken -> code 2

    // ---- (3) openat("../../../../host_secret") must FAIL (clamp + base) ----
    "    mov  rax, 257",
    "    mov  rdi, -100",
    "    lea  rsi, [rip + 12f]",       // "../../../../host_secret"
    "    xor  rdx, rdx",
    "    xor  r10, r10",
    "    syscall",
    "    mov  rcx, 0xFFFFFFFFFFFFF000",
    "    cmp  rax, rcx",
    "    jb   4f",                     // escape opened a valid fd -> code 3

    // ---- success: magic + code 0 ----
    "    mov  rbx, 0x6000000",
    "    movabs rax, 0xC0F1110E0ADED011",
    "    mov  qword ptr [rbx], rax",
    "    mov  qword ptr [rbx + 8], 0",
    "    jmp  9f",

    // ---- failure paths ----
    "2:  mov rbx, 0x6000000",
    "    movabs rax, 0xC0F1110E0ADED011",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 1",
    "    jmp 9f",
    "3:  mov rbx, 0x6000000",
    "    movabs rax, 0xC0F1110E0ADED011",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 2",
    "    jmp 9f",
    "4:  mov rbx, 0x6000000",
    "    movabs rax, 0xC0F1110E0ADED011",
    "    mov qword ptr [rbx], rax",
    "    mov qword ptr [rbx + 8], 3",

    // ---- exit_group(0) ----
    "9:  mov rax, 231",
    "    xor rdi, rdi",
    "    syscall",
    "1:  jmp 1b",

    // path data
    "10: .asciz \"/only\"",
    "11: .asciz \"/host_secret\"",
    "12: .asciz \"../../../../host_secret\"",
);
