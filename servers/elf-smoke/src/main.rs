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
//! then exits via the **native** ThemeliOS `SYS_EXIT` — so this test validates the
//! loader without depending on the Linux syscall personality (5.1). The kernel test
//! reads the shared page back and checks all three words.
//!
//! ## Two architectures, one set of constants (Phase 8.5c)
//!
//! The `_start` bodies are per-architecture assembly and always will be: reading
//! `argc`/`argv` off the initial stack and storing to a fixed address is exactly the
//! work a calling convention does not abstract.
//!
//! What is *not* duplicated is the numbers. `RESULT_VADDR`, `RESULT_MAGIC` and the
//! syscall number are Rust `const`s substituted into both blocks as `const` operands,
//! so the two can differ in instructions but cannot differ in values. That distinction
//! is the whole lesson of 8.5b, where nine hand-written `mov x8, #N` literals inside
//! `global_asm!` strings had to be converted for the same reason: a number in a string
//! is not checked against anything, and a missed edit does not fail to build — it makes
//! the program do something else, correctly.
//!
//! The syscall number comes from the kernel's own list, included the same way
//! `libthemelios` includes it.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// The native syscall numbers, single-sourced from the kernel (Phase 8.5b/8.5c).
///
/// This crate is a detached workspace with no dependency on `libthemelios`, so it
/// includes the same constants file directly. That file deliberately names nothing from
/// any crate, which is what makes it includable from here.
#[path = "../../../kernel/src/arch/syscall_abi.rs"]
mod syscall_abi;
use syscall_abi::abi;

/// Halt on panic — there is no runtime here.
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// The kernel-mapped result page. Must match `RESULT_VADDR` in the kernel test.
const RESULT_VADDR: u64 = 0x0600_0000;

/// The value the kernel checks for. Must match `RESULT_MAGIC` in the kernel test.
const RESULT_MAGIC: u64 = 0xE1FC_0DE1_2345_6789;

// x86_64: `mov rax, <imm64>` assembles to `movabs` for the magic; the result page
// address fits a 32-bit sign-extended immediate.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".global _start",
    ".section .text._start,\"ax\"",
    "_start:",
    "    mov  r8,  [rsp]",          // r8 = argc
    "    mov  r9,  [rsp + 8]",      // r9 = argv[0] pointer
    "    movzx r10, byte ptr [r9]", // r10 = argv[0][0]
    "    mov  rbx, {result}",       // result page
    "    mov  rax, {magic}",        // RESULT_MAGIC (movabs)
    "    mov  [rbx],      rax",
    "    mov  [rbx + 8],  r8",
    "    mov  [rbx + 16], r10",
    "    mov  rax, {nr_exit}",
    "    xor  rdi, rdi", // exit code 0
    "    syscall",
    "2:  jmp 2b", // never reached; guard if SYS_EXIT returns
    result = const RESULT_VADDR,
    magic = const RESULT_MAGIC,
    nr_exit = const abi::SYS_EXIT,
);

// aarch64: the same three proofs, in the same order, to the same addresses.
//
// Two differences the instruction set forces, neither of them a change in contract:
//
//   * The 64-bit magic needs a `movz`/`movk` quartet rather than one `movabs`. Written as
//     `{magic}` split into four 16-bit lanes by the assembler's own `:abs_g0_nc:` family,
//     so the value still comes from the `const` above and is never spelled out in hex here.
//   * `argv[0][0]` loads with `ldrb w`, whose zero-extension to the full X register is
//     implicit — the x86 side has to say `movzx`.
//
// The initial stack layout is identical because the kernel's loader builds it: `argc` at
// `[sp]`, `argv[0]` at `[sp, #8]`. That contract is the loader's, not the architecture's.
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".global _start",
    ".section .text._start,\"ax\"",
    "_start:",
    "    ldr  x8,  [sp]",      // x8 = argc
    "    ldr  x9,  [sp, #8]",  // x9 = argv[0] pointer
    "    ldrb w10, [x9]",      // x10 = argv[0][0], zero-extended
    "    mov  x11, {result}",  // result page
    "    movz x12, :abs_g0_nc:{magic}",
    "    movk x12, :abs_g1_nc:{magic}",
    "    movk x12, :abs_g2_nc:{magic}",
    "    movk x12, :abs_g3:{magic}",
    "    str  x12, [x11]",
    "    str  x8,  [x11, #8]",
    "    str  x10, [x11, #16]",
    "    mov  x8, {nr_exit}",
    "    mov  x0, xzr", // exit code 0
    "    svc  #0",
    "2:  b 2b", // never reached; guard if SYS_EXIT returns
    result = const RESULT_VADDR,
    magic = const RESULT_MAGIC,
    nr_exit = const abi::SYS_EXIT,
);
