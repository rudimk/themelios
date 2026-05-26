//! # Interrupt Descriptor Table (IDT)
//!
//! The IDT tells the CPU how to handle interrupts and exceptions. It's an array
//! of 256 gate descriptors, indexed by vector number:
//!
//! - **Vectors 0-31**: CPU exceptions (divide error, page fault, GPF, etc.)
//! - **Vectors 32-47**: Hardware IRQs (remapped from the 8259 PIC, set up in Sub-phase 1.3)
//! - **Vectors 48-255**: Available for software interrupts and future use
//!
//! ## How interrupts work on x86_64
//!
//! When an interrupt fires (hardware IRQ, CPU exception, or software INT):
//!
//! 1. CPU looks up the vector number in the IDT
//! 2. CPU checks the gate descriptor's DPL, segment selector, and IST field
//! 3. If an IST index is specified, CPU loads a fresh stack from the TSS
//! 4. CPU pushes SS, RSP, RFLAGS, CS, RIP (and error code for some exceptions)
//! 5. CPU clears IF (for interrupt gates) and jumps to the handler address
//!
//! ## Our handler design
//!
//! Each exception vector (0-21) has a small assembly "stub" that:
//! - Pushes a dummy error code if the CPU didn't push one (for uniform stack layout)
//! - Pushes the vector number
//! - Jumps to a common handler
//!
//! The common handler saves all registers, calls a Rust function to print
//! diagnostics, restores registers, and returns via `iretq`.
//!
//! ## Exception error codes
//!
//! Some exceptions push a hardware error code onto the stack; others don't.
//! We pad the non-error-code exceptions with a dummy zero so the stack frame
//! layout is identical for all vectors. Exceptions that push error codes:
//! 8 (#DF), 10 (#TS), 11 (#NP), 12 (#SS), 13 (#GP), 14 (#PF), 17 (#AC), 21 (#CP)

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use super::cpu;
use super::gdt;
use super::pic;
use crate::println;

// --- IDT entry (gate descriptor) ---

/// A single entry in the Interrupt Descriptor Table (16 bytes).
///
/// In 64-bit mode, each IDT entry is a "gate descriptor" that tells the CPU:
/// - Where the handler is (64-bit address split across three fields)
/// - What code segment to run it in (always the kernel code segment)
/// - Whether to switch stacks via IST
/// - Whether to disable interrupts on entry (interrupt gate vs trap gate)
///
/// ## Layout (16 bytes)
///
/// ```text
/// Bytes 0-1:   handler address bits 0-15
/// Bytes 2-3:   code segment selector
/// Byte 4:      IST index (bits 0-2), reserved zeros (bits 3-7)
/// Byte 5:      gate type (bits 0-3), zero (bit 4), DPL (bits 5-6), present (bit 7)
/// Bytes 6-7:   handler address bits 16-31
/// Bytes 8-11:  handler address bits 32-63
/// Bytes 12-15: reserved (must be zero)
/// ```
#[derive(Clone, Copy)]
#[repr(C)]
struct IdtEntry {
    /// Handler address bits 0-15.
    offset_low: u16,
    /// Segment selector — must point to a 64-bit code segment in the GDT.
    selector: u16,
    /// Bits 0-2: IST index (0 = don't use IST, 1-7 = use IST entry N from TSS).
    /// Bits 3-7: reserved, must be zero.
    ist: u8,
    /// Bit 7: Present (P) — must be 1 for valid entries.
    /// Bits 5-6: DPL — ring level allowed to invoke via INT instruction.
    /// Bit 4: must be 0.
    /// Bits 0-3: gate type (0xE = interrupt gate, 0xF = trap gate).
    ///
    /// Interrupt gate (0xE): clears IF on entry → interrupts disabled in handler.
    /// Trap gate (0xF): does not clear IF → interrupts remain enabled.
    /// We use interrupt gates for all handlers to avoid nested interrupts.
    type_attr: u8,
    /// Handler address bits 16-31.
    offset_mid: u16,
    /// Handler address bits 32-63.
    offset_high: u32,
    /// Reserved — must be zero.
    _reserved: u32,
}

/// Type attribute for a 64-bit interrupt gate at DPL 0 (kernel only).
/// Present=1, DPL=00, Gate type=0xE (interrupt gate).
/// Binary: 1_00_0_1110 = 0x8E
const INTERRUPT_GATE: u8 = 0x8E;

impl IdtEntry {
    /// An empty (not-present) IDT entry. The CPU ignores this entry and
    /// raises a #GP if the corresponding vector fires.
    const EMPTY: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0, // Present bit = 0
        offset_mid: 0,
        offset_high: 0,
        _reserved: 0,
    };

    /// Create an IDT entry pointing to a handler function.
    ///
    /// - `handler_addr`: virtual address of the ISR stub function
    /// - `selector`: GDT segment selector for the handler's code segment
    /// - `ist`: IST index (0 = no IST, 1-7 = use TSS IST entry)
    fn new(handler_addr: u64, selector: u16, ist: u8) -> Self {
        Self {
            offset_low: handler_addr as u16,
            selector,
            ist,
            type_attr: INTERRUPT_GATE,
            offset_mid: (handler_addr >> 16) as u16,
            offset_high: (handler_addr >> 32) as u32,
            _reserved: 0,
        }
    }
}

// --- Interrupt stack frame ---

/// The complete register state saved when an interrupt or exception occurs.
///
/// This struct maps exactly to the stack layout built by:
/// 1. The CPU (SS, RSP, RFLAGS, CS, RIP, and optionally error code)
/// 2. Our ISR stub (dummy error code if needed, vector number)
/// 3. Our common handler (all 15 general-purpose registers)
///
/// The fields are ordered from lowest address (top of stack) to highest,
/// matching the stack's memory layout after all pushes.
#[repr(C)]
pub struct InterruptStackFrame {
    // --- Saved by isr_common (pushed last → lowest addresses) ---
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,

    // --- Pushed by ISR stub ---
    /// The interrupt vector number (0-255). Pushed by the ISR stub so the
    /// common handler knows which exception/interrupt occurred.
    pub vector: u64,
    /// Hardware error code (for exceptions that provide one) or zero (for
    /// exceptions where the stub pushes a dummy). See module docs for which
    /// exceptions push real error codes.
    pub error_code: u64,

    // --- Pushed by CPU on interrupt/exception entry ---
    /// Instruction pointer at the time of the interrupt. For faults, this
    /// points to the faulting instruction. For traps (like breakpoint), this
    /// points to the instruction AFTER the trap.
    pub rip: u64,
    /// Code segment selector at the time of the interrupt.
    pub cs: u64,
    /// CPU flags register at the time of the interrupt.
    pub rflags: u64,
    /// Stack pointer at the time of the interrupt.
    pub rsp: u64,
    /// Stack segment selector at the time of the interrupt.
    pub ss: u64,
}

// --- ISR stub macros ---
//
// These macros generate small #[unsafe(naked)] assembly functions (ISR stubs) for each
// exception vector. There are two variants:
//
// - `isr_stub_no_error!`: For exceptions that do NOT push an error code.
//   Pushes a dummy zero error code for uniform stack layout.
//
// - `isr_stub_error!`: For exceptions that DO push an error code.
//   The error code is already on the stack from the CPU.
//
// Both variants push the vector number and jump to `isr_common`, which
// saves all registers and calls the Rust exception handler.

/// Generate an ISR stub for an exception that does NOT push an error code.
/// Pushes a dummy zero so the stack frame is uniform across all vectors.
macro_rules! isr_stub_no_error {
    ($name:ident, $vector:expr) => {
        #[unsafe(naked)]
        unsafe extern "C" fn $name() {
            core::arch::naked_asm!(
                "push 0",            // Dummy error code (uniform stack layout)
                "push {vector}",     // Vector number
                "jmp {common}",      // Jump to common register-save handler
                vector = const $vector,
                common = sym isr_common,
            );
        }
    };
}

/// Generate an ISR stub for an exception that DOES push a hardware error code.
/// The CPU has already pushed the error code, so we just add the vector number.
macro_rules! isr_stub_error {
    ($name:ident, $vector:expr) => {
        #[unsafe(naked)]
        unsafe extern "C" fn $name() {
            core::arch::naked_asm!(
                // Error code is already on the stack (pushed by CPU)
                "push {vector}",     // Vector number
                "jmp {common}",      // Jump to common register-save handler
                vector = const $vector,
                common = sym isr_common,
            );
        }
    };
}

// --- Generate ISR stubs for vectors 0-21 ---
//
// Each CPU exception has a fixed vector number. Some push error codes,
// some don't — we use the appropriate macro for each.

isr_stub_no_error!(isr_0, 0);    // #DE — Divide Error
isr_stub_no_error!(isr_1, 1);    // #DB — Debug Exception
isr_stub_no_error!(isr_2, 2);    // NMI — Non-Maskable Interrupt
isr_stub_no_error!(isr_3, 3);    // #BP — Breakpoint (INT3)
isr_stub_no_error!(isr_4, 4);    // #OF — Overflow (INTO)
isr_stub_no_error!(isr_5, 5);    // #BR — Bound Range Exceeded
isr_stub_no_error!(isr_6, 6);    // #UD — Invalid Opcode
isr_stub_no_error!(isr_7, 7);    // #NM — Device Not Available (FPU)
isr_stub_error!(isr_8, 8);       // #DF — Double Fault (error code always 0)
isr_stub_no_error!(isr_9, 9);    // Coprocessor Segment Overrun (legacy)
isr_stub_error!(isr_10, 10);     // #TS — Invalid TSS
isr_stub_error!(isr_11, 11);     // #NP — Segment Not Present
isr_stub_error!(isr_12, 12);     // #SS — Stack-Segment Fault
isr_stub_error!(isr_13, 13);     // #GP — General Protection Fault
isr_stub_error!(isr_14, 14);     // #PF — Page Fault
isr_stub_no_error!(isr_15, 15);  // Reserved (should never fire)
isr_stub_no_error!(isr_16, 16);  // #MF — x87 Floating-Point Error
isr_stub_error!(isr_17, 17);     // #AC — Alignment Check
isr_stub_no_error!(isr_18, 18);  // #MC — Machine Check
isr_stub_no_error!(isr_19, 19);  // #XF — SIMD Floating-Point Error
isr_stub_no_error!(isr_20, 20);  // Virtualization Exception
isr_stub_error!(isr_21, 21);     // #CP — Control Protection Exception

// --- Generate ISR stubs for hardware IRQ vectors 32-47 ---
//
// Hardware IRQs (from the 8259 PIC) are mapped to vectors 32-47 by the PIC
// initialization code in pic.rs. IRQs never push error codes, so all stubs
// use the no-error variant. These share the same isr_common handler as
// exceptions — the Rust handler dispatches based on vector number.

isr_stub_no_error!(isr_32, 32);  // IRQ0  — PIT timer
isr_stub_no_error!(isr_33, 33);  // IRQ1  — Keyboard
isr_stub_no_error!(isr_34, 34);  // IRQ2  — Cascade (slave PIC)
isr_stub_no_error!(isr_35, 35);  // IRQ3  — COM2
isr_stub_no_error!(isr_36, 36);  // IRQ4  — COM1 (serial)
isr_stub_no_error!(isr_37, 37);  // IRQ5  — LPT2
isr_stub_no_error!(isr_38, 38);  // IRQ6  — Floppy
isr_stub_no_error!(isr_39, 39);  // IRQ7  — LPT1 / spurious
isr_stub_no_error!(isr_40, 40);  // IRQ8  — RTC
isr_stub_no_error!(isr_41, 41);  // IRQ9  — ACPI
isr_stub_no_error!(isr_42, 42);  // IRQ10 — Available
isr_stub_no_error!(isr_43, 43);  // IRQ11 — Available
isr_stub_no_error!(isr_44, 44);  // IRQ12 — PS/2 mouse
isr_stub_no_error!(isr_45, 45);  // IRQ13 — FPU
isr_stub_no_error!(isr_46, 46);  // IRQ14 — Primary ATA
isr_stub_no_error!(isr_47, 47);  // IRQ15 — Secondary ATA / spurious

// --- Common ISR handler (assembly) ---

/// Common interrupt handler entered by all ISR stubs via `jmp`.
///
/// At entry, the stack contains (from top):
/// - Vector number (pushed by stub)
/// - Error code (pushed by CPU or dummy zero from stub)
/// - RIP, CS, RFLAGS, RSP, SS (pushed by CPU)
///
/// This function saves all 15 general-purpose registers to build a complete
/// `InterruptStackFrame`, calls the Rust `exception_handler` with a pointer
/// to that frame, then restores everything and returns via `iretq`.
#[unsafe(naked)]
unsafe extern "C" fn isr_common() {
    core::arch::naked_asm!(
        // --- Save all general-purpose registers ---
        // These are callee-saved and caller-saved registers. We save everything
        // because we don't know which registers the interrupted code was using.
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",

        // --- Call Rust exception handler ---
        // Pass a pointer to the InterruptStackFrame (which starts at RSP
        // after all our pushes). System V ABI: first arg in RDI.
        "mov rdi, rsp",
        "call {handler}",

        // --- Restore all registers (reverse order) ---
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",

        // --- Clean up and return ---
        // Skip the vector number and error code that the stub pushed
        "add rsp, 16",
        // Return from interrupt: restores RIP, CS, RFLAGS, RSP, SS
        "iretq",

        handler = sym exception_handler,
    );
}

// --- Rust exception handler ---

/// Exception names indexed by vector number (0-21).
/// Used for human-readable output when an exception fires.
const EXCEPTION_NAMES: [&str; 22] = [
    "Divide Error (#DE)",               // 0
    "Debug (#DB)",                       // 1
    "Non-Maskable Interrupt (NMI)",      // 2
    "Breakpoint (#BP)",                  // 3
    "Overflow (#OF)",                    // 4
    "Bound Range Exceeded (#BR)",        // 5
    "Invalid Opcode (#UD)",              // 6
    "Device Not Available (#NM)",        // 7
    "Double Fault (#DF)",                // 8
    "Coprocessor Segment Overrun",       // 9
    "Invalid TSS (#TS)",                 // 10
    "Segment Not Present (#NP)",         // 11
    "Stack-Segment Fault (#SS)",         // 12
    "General Protection Fault (#GP)",    // 13
    "Page Fault (#PF)",                  // 14
    "Reserved",                          // 15
    "x87 Floating-Point Error (#MF)",    // 16
    "Alignment Check (#AC)",             // 17
    "Machine Check (#MC)",              // 18
    "SIMD Floating-Point Error (#XF)",   // 19
    "Virtualization Exception",          // 20
    "Control Protection (#CP)",          // 21
];

// --- Global tick counter ---

/// Monotonically increasing tick counter, incremented by the IRQ0 (PIT timer)
/// handler. At 100 Hz, this wraps after ~5.8 billion years, so u64 is fine.
///
/// Uses `AtomicU64` with `Relaxed` ordering because there's only one writer
/// (the IRQ0 handler, which runs with interrupts disabled on a single core)
/// and readers just want the latest value — no happens-before relationship
/// is needed.
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

/// Read the current tick count (number of PIT timer interrupts since boot).
///
/// At the default 100 Hz rate, 1 tick ≈ 10 ms. The value is monotonically
/// increasing and never wraps in practice (u64 overflow at ~5.8 billion years).
pub fn tick_count() -> u64 {
    TICK_COUNT.load(Ordering::Relaxed)
}

// --- Interrupt dispatcher ---
//
// All interrupts — both CPU exceptions (vectors 0-31) and hardware IRQs
// (vectors 32-47) — flow through isr_common and into this single Rust
// handler. We dispatch based on vector number:
//
//   0-31:  CPU exception → print diagnostics, recover or halt
//   32-47: Hardware IRQ  → check spurious, handle, send EOI

/// Unified interrupt handler called from `isr_common` assembly.
///
/// Dispatches CPU exceptions (vectors 0-31) and hardware IRQs (vectors 32-47).
/// For exceptions: prints diagnostic info, recovers from breakpoints, halts on
/// fatal errors. For IRQs: checks for spurious interrupts, runs the device
/// handler, and sends EOI to the PIC.
extern "C" fn exception_handler(frame: &InterruptStackFrame) {
    let vector = frame.vector as usize;

    // --- Hardware IRQ dispatch (vectors 32-47) ---
    if vector >= 32 && vector < 48 {
        let irq = (vector - 32) as u8;

        // Check for spurious IRQs before doing anything else.
        // Spurious IRQs on IRQ7/IRQ15 are phantom signals from the PIC.
        // is_spurious() handles any necessary EOI internally.
        if pic::is_spurious(irq) {
            return;
        }

        irq_handler(irq);

        // Acknowledge the interrupt so the PIC delivers the next one.
        pic::send_eoi(irq);
        return;
    }

    // --- CPU exception dispatch (vectors 0-31) ---

    let name = if vector < EXCEPTION_NAMES.len() {
        EXCEPTION_NAMES[vector]
    } else {
        "Unknown"
    };

    println!();
    println!("=== CPU EXCEPTION ===");
    println!("  Vector:     {} — {}", vector, name);
    println!("  Error code: {:#X}", frame.error_code);
    println!("  RIP:        {:#018X}", frame.rip);
    println!("  RSP:        {:#018X}", frame.rsp);
    println!("  CS:         {:#06X}", frame.cs);
    println!("  RFLAGS:     {:#018X}", frame.rflags);

    // Page faults: print CR2, which holds the virtual address that caused the fault.
    // CR2 is set by the CPU before invoking the #PF handler.
    if vector == 14 {
        let cr2: u64;
        unsafe {
            asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags));
        }
        println!("  CR2 (fault addr): {:#018X}", cr2);

        // Decode the page fault error code bits for easier debugging
        let code = frame.error_code;
        println!("  PF flags: {}{}{}{}",
            if code & 1 != 0 { "protection " } else { "not-present " },
            if code & 2 != 0 { "write " } else { "read " },
            if code & 4 != 0 { "user " } else { "kernel " },
            if code & 16 != 0 { "instruction-fetch" } else { "" },
        );
    }

    // Breakpoint (#BP, vector 3) is the only exception we expect to recover
    // from during normal operation. The IDT uses an interrupt gate so IF is
    // cleared on entry — iretq will restore RFLAGS and re-enable interrupts.
    if vector == 3 {
        println!("  (Breakpoint — resuming execution)");
        return;
    }

    // All other exceptions in kernel mode are fatal. We don't have recovery
    // mechanisms yet (no page fault handler that maps pages, no GPF recovery).
    // Print a message and halt permanently.
    println!("  FATAL — halting CPU");
    loop {
        cpu::cli();
        cpu::halt();
    }
}

/// Handle a hardware IRQ after spurious check has passed.
///
/// Called from the unified interrupt dispatcher with the IRQ number (0-15).
/// Each IRQ line gets its own handling logic here.
fn irq_handler(irq: u8) {
    match irq {
        // IRQ0 — PIT timer: increment the global tick counter.
        // Print a status line every 100 ticks (~1 second at 100 Hz) as proof
        // of life during early boot. This periodic printing will be removed
        // once the scheduler is running.
        0 => {
            let ticks = TICK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if ticks % 100 == 0 {
                println!("[timer] tick {}", ticks);
            }
        }

        // All other IRQs are unhandled for now. Log the first few to serial
        // so we notice unexpected hardware activity.
        _ => {
            println!("[irq] unhandled IRQ{}", irq);
        }
    }
}

// --- Static IDT ---

/// The full 256-entry IDT. Only vectors 0-21 are populated initially
/// (CPU exceptions). Hardware IRQ vectors (32-47) are added in Sub-phase 1.3.
static mut IDT: [IdtEntry; 256] = [IdtEntry::EMPTY; 256];

// --- Initialization ---

/// Initialize the IDT and load it into the CPU.
///
/// Sets up gate descriptors for:
/// - CPU exception vectors 0-21 (with double fault on IST1)
/// - Hardware IRQ vectors 32-47 (from the 8259 PIC)
///
/// Must be called after `gdt::init()` (since IDT entries reference the kernel
/// code segment selector from the GDT and the double-fault IST from the TSS).
pub fn init() {
    // Exception handlers (vectors 0-21): each entry is (stub_fn, IST_index).
    // Most exceptions use IST 0 (= don't switch stacks, use current stack).
    // Double fault uses IST 1 (= dedicated stack from TSS) because the
    // kernel stack might be corrupted when a double fault fires.
    let exception_handlers: [(unsafe extern "C" fn(), u8); 22] = [
        (isr_0, 0),                              // 0:  #DE
        (isr_1, 0),                              // 1:  #DB
        (isr_2, 0),                              // 2:  NMI
        (isr_3, 0),                              // 3:  #BP
        (isr_4, 0),                              // 4:  #OF
        (isr_5, 0),                              // 5:  #BR
        (isr_6, 0),                              // 6:  #UD
        (isr_7, 0),                              // 7:  #NM
        (isr_8, gdt::DOUBLE_FAULT_IST_INDEX),    // 8:  #DF — uses IST stack!
        (isr_9, 0),                              // 9:  Coprocessor overrun
        (isr_10, 0),                             // 10: #TS
        (isr_11, 0),                             // 11: #NP
        (isr_12, 0),                             // 12: #SS
        (isr_13, 0),                             // 13: #GP
        (isr_14, 0),                             // 14: #PF
        (isr_15, 0),                             // 15: Reserved
        (isr_16, 0),                             // 16: #MF
        (isr_17, 0),                             // 17: #AC
        (isr_18, 0),                             // 18: #MC
        (isr_19, 0),                             // 19: #XF
        (isr_20, 0),                             // 20: Virtualization
        (isr_21, 0),                             // 21: #CP
    ];

    // Hardware IRQ handlers (vectors 32-47): all use IST 0 (current stack).
    // These stubs all flow through isr_common → exception_handler, which
    // dispatches to irq_handler() for vectors 32-47.
    let irq_handlers: [unsafe extern "C" fn(); 16] = [
        isr_32,  // IRQ0  — PIT timer
        isr_33,  // IRQ1  — Keyboard
        isr_34,  // IRQ2  — Cascade
        isr_35,  // IRQ3  — COM2
        isr_36,  // IRQ4  — COM1
        isr_37,  // IRQ5  — LPT2
        isr_38,  // IRQ6  — Floppy
        isr_39,  // IRQ7  — LPT1 / spurious
        isr_40,  // IRQ8  — RTC
        isr_41,  // IRQ9  — ACPI
        isr_42,  // IRQ10 — Available
        isr_43,  // IRQ11 — Available
        isr_44,  // IRQ12 — PS/2 mouse
        isr_45,  // IRQ13 — FPU
        isr_46,  // IRQ14 — Primary ATA
        isr_47,  // IRQ15 — Secondary ATA / spurious
    ];

    // SAFETY: Single-threaded early boot, no interrupts enabled yet.
    unsafe {
        let idt = &raw mut IDT;

        // Register exception handlers (vectors 0-21)
        for (vector, &(handler, ist)) in exception_handlers.iter().enumerate() {
            let addr = handler as usize as u64;
            (*idt)[vector] = IdtEntry::new(addr, gdt::KERNEL_CODE_SELECTOR, ist);
        }

        // Register IRQ handlers (vectors 32-47)
        for (i, &handler) in irq_handlers.iter().enumerate() {
            let vector = 32 + i;
            let addr = handler as usize as u64;
            (*idt)[vector] = IdtEntry::new(addr, gdt::KERNEL_CODE_SELECTOR, 0);
        }

        // Load the IDT into the CPU
        let idtr = cpu::IdtRegister {
            limit: (core::mem::size_of_val(&*idt) - 1) as u16,
            base: idt as u64,
        };
        cpu::lidt(&idtr);
    }

    println!("IDT loaded (22 exception handlers + 16 IRQ handlers, double fault on IST1)");
}
