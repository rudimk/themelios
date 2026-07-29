//! ELF loader smoke-test binary (Phase 5.0).
//!
//! A minimal ring-3 program with a hand-written `_start`. The kernel's ELF loader
//! loads it, builds its initial stack, and enters at `_start`. It then proves the
//! load was correct by writing three words to a **fixed shared page** the kernel
//! maps at `0x0600_0000` before launch:
//!
//! ```text
//! [0x0600_0000] = MAGIC       (proves _start ran in ring 3)
//! [0x0600_0008] = argc        (proves the initial stack / argv count is correct)
//! [0x0600_0010] = argv[0][0]  (proves the argv pointers resolve to the strings)
//! ```
//!
//! then exits via the **native** ThemeliOS `SYS_EXIT` (number 6) — so this test
//! validates the loader without depending on the Linux syscall personality (5.1).
//! The kernel test reads the shared page back and checks all three words.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// Halt on panic — there is no runtime here.
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

// The kernel-mapped result page (must match `RESULT_VADDR` in the kernel test)
// and the magic value the kernel checks for (`RESULT_MAGIC`).
//
// `_start` reads argc/argv from the stack the loader built, records the proof
// words, and exits. `mov rax, <imm64>` assembles to `movabs` for the magic; the
// 0x6000000 page address fits a 32-bit (sign-extended) immediate.
core::arch::global_asm!(
    ".global _start",
    ".section .text._start,\"ax\"",
    "_start:",
    "    mov  r8,  [rsp]",              // r8 = argc
    "    mov  r9,  [rsp + 8]",          // r9 = argv[0] pointer
    "    movzx r10, byte ptr [r9]",     // r10 = argv[0][0]
    "    mov  rbx, 0x6000000",          // result page (RESULT_VADDR)
    "    mov  rax, 0xE1FC0DE123456789", // RESULT_MAGIC (movabs)
    "    mov  [rbx],      rax",
    "    mov  [rbx + 8],  r8",
    "    mov  [rbx + 16], r10",
    "    mov  rax, 6",                  // SYS_EXIT
    "    xor  rdi, rdi",                // exit code 0
    "    syscall",
    "2:  jmp 2b",                       // never reached; guard if SYS_EXIT returns
);
