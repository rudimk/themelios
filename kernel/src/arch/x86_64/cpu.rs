//! # x86_64 CPU operations
//!
//! Low-level CPU instructions for the x86_64 architecture. These are thin
//! wrappers around inline assembly for common operations like I/O port
//! access and CPU control.
//!
//! ## I/O Ports
//!
//! x86 has a separate I/O address space (distinct from memory) used to
//! communicate with legacy hardware devices. Devices are accessed by reading
//! from or writing to specific port numbers using the `in` and `out` CPU
//! instructions.
//!
//! Common port assignments:
//! - 0x3F8: COM1 serial port (16550 UART)
//! - 0x60/0x64: PS/2 keyboard/mouse controller
//! - 0xCF8/0xCFC: PCI configuration space

use core::arch::asm;

/// Write a single byte to an x86 I/O port.
///
/// This executes the `out dx, al` instruction, which sends the byte in `al`
/// to the I/O port number in `dx`.
///
/// # Safety
///
/// Writing to an I/O port can have arbitrary side effects on hardware.
/// The caller must ensure the port number is valid and the write is
/// appropriate for the device at that port.
#[inline(always)]
pub unsafe fn outb(port: u16, value: u8) {
    // The `out` instruction sends a byte to an I/O port.
    // - "dx" register holds the port number
    // - "al" register holds the byte to send
    // - options(nomem, nostack, preserves_flags): tells the compiler this
    //   instruction doesn't touch memory, the stack, or CPU flags, allowing
    //   better optimization.
    unsafe {
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Read a single byte from an x86 I/O port.
///
/// This executes the `in al, dx` instruction, which reads a byte from the
/// I/O port number in `dx` into `al`.
///
/// # Safety
///
/// Reading from an I/O port can have side effects on hardware (e.g., clearing
/// an interrupt flag or advancing a FIFO). The caller must ensure the port
/// number is valid and the read is appropriate.
#[inline(always)]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // The `in` instruction reads a byte from an I/O port.
    // - "dx" register holds the port number
    // - "al" register receives the byte
    unsafe {
        asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// Halt the CPU until the next interrupt arrives.
///
/// This executes the `hlt` instruction, which puts the CPU into a low-power
/// state until an interrupt fires. Used in idle loops to avoid busy-waiting.
#[inline(always)]
pub fn halt() {
    // hlt is safe in the sense that it doesn't corrupt state — it just
    // pauses the CPU. But it does require being in ring 0 (kernel mode).
    unsafe {
        asm!("hlt", options(nomem, nostack, preserves_flags));
    }
}

/// Disable maskable CPU interrupts.
///
/// Executes the `CLI` (Clear Interrupt Flag) instruction, which clears the IF
/// flag in the RFLAGS register. While IF is clear, the CPU ignores all maskable
/// hardware interrupts (IRQs). Non-maskable interrupts (NMIs) and exceptions
/// are unaffected.
///
/// This is used to protect critical sections where an interrupt handler might
/// try to acquire a lock that the current code path already holds, which would
/// deadlock on a single-core system.
///
/// Always pair with a corresponding `sti()` call, or better yet, use
/// `InterruptMutex` which saves/restores the interrupt state automatically.
#[inline(always)]
pub fn cli() {
    // CLI clears the IF (Interrupt Flag) in RFLAGS. This is a privileged
    // instruction — it only works in ring 0 (kernel mode).
    // We do NOT mark this as preserves_flags because it modifies RFLAGS.IF.
    unsafe {
        asm!("cli", options(nomem, nostack));
    }
}

/// Enable maskable CPU interrupts.
///
/// Executes the `STI` (Set Interrupt Flag) instruction, which sets the IF flag
/// in RFLAGS. The CPU will begin responding to maskable interrupts again.
///
/// Note: the x86 architecture guarantees that the instruction immediately
/// following STI executes before any pending interrupt is delivered. This
/// one-instruction window allows patterns like `sti; hlt` to atomically
/// enable interrupts and halt (without risking an interrupt sneaking in
/// between the two instructions and causing `hlt` to sleep forever).
#[inline(always)]
pub fn sti() {
    // STI sets the IF (Interrupt Flag) in RFLAGS. Like CLI, this is a
    // privileged ring 0 instruction.
    unsafe {
        asm!("sti", options(nomem, nostack));
    }
}

// --- GDT/TSS support ---
//
// These structures and functions are used by the GDT module (gdt.rs) to load
// the Global Descriptor Table and Task State Segment. The GDT defines memory
// segments (code, data, TSS) and the TSS holds interrupt stack table entries
// for exception handlers that need dedicated stacks (like the double-fault handler).

/// GDT Register descriptor — the exact layout the `lgdt` instruction expects.
///
/// The CPU reads this 10-byte structure when `lgdt` executes. It contains the
/// size of the GDT (limit = size - 1) and the virtual address of the first entry.
/// Must be `packed` because the CPU expects the u64 base immediately after the
/// u16 limit with no padding between them.
#[repr(C, packed)]
pub struct GdtRegister {
    /// Size of the GDT in bytes minus 1. For a GDT with 5 entries of 8 bytes
    /// each, this would be 39 (= 5 * 8 - 1).
    pub limit: u16,
    /// Virtual address of the first GDT entry (the null descriptor).
    pub base: u64,
}

/// Load a new Global Descriptor Table.
///
/// Executes the `lgdt` instruction, which tells the CPU where the GDT is
/// and how large it is. This does NOT reload the segment registers — the old
/// segment selectors remain in effect until explicitly reloaded via
/// `reload_segments()`.
///
/// The GDTR structure is read immediately and its values are cached in the
/// CPU's internal GDT register, so the `GdtRegister` struct doesn't need
/// to outlive this call (but the GDT it points to must live forever).
///
/// # Safety
///
/// - The `GdtRegister` must describe a valid GDT at a stable address.
/// - The GDT must remain in memory for the lifetime of the kernel.
/// - Caller must reload segment registers afterward to activate the new GDT.
#[inline(always)]
pub unsafe fn lgdt(gdtr: &GdtRegister) {
    // lgdt reads 10 bytes from the address in the register operand:
    // 2 bytes for limit, then 8 bytes for base address.
    // No `nomem` — lgdt reads from the pointer, and the compiler must not
    // reorder the GDTR store past this instruction.
    unsafe {
        asm!(
            "lgdt [{}]",
            in(reg) gdtr as *const GdtRegister,
            options(nostack, preserves_flags)
        );
    }
}

/// Reload all segment registers to activate a new GDT.
///
/// CS (Code Segment) cannot be changed with a simple `mov` — the CPU requires
/// a control transfer (far jump, far call, or far return) to load a new CS.
/// We use `retfq` (far return): push the new CS and a return address onto the
/// stack, then far-return to "pop" both — the CPU loads CS from the stack and
/// jumps to the return address (which is the next instruction).
///
/// DS, ES, and SS are reloaded with simple `mov` instructions.
///
/// # Safety
///
/// - Both selectors must reference valid entries in the currently loaded GDT.
/// - `code_selector` must point to a 64-bit code segment.
/// - `data_selector` must point to a data segment.
/// - An invalid selector triggers a General Protection Fault (#GP).
#[inline(always)]
pub unsafe fn reload_segments(code_selector: u16, data_selector: u16) {
    unsafe {
        asm!(
            // --- Reload CS via far return ---
            //
            // retfq expects the stack to contain (from top):
            //   [RSP+0] = new RIP  (8 bytes)
            //   [RSP+8] = new CS   (8 bytes, only lower 16 bits used)
            //
            // We push them in reverse order (CS first, then RIP) because
            // the stack grows downward and retfq pops RIP first.
            "push {code_sel}",          // Push new CS selector
            "lea {tmp}, [rip + 2f]",    // Compute address of label '2'
            "push {tmp}",              // Push return address (new RIP)
            "retfq",                   // Far return: pop RIP, pop CS
            "2:",
            // Now running with the new CS selector.
            //
            // --- Reload data segment registers ---
            "mov ds, {data_sel:x}",    // Data Segment
            "mov es, {data_sel:x}",    // Extra Segment (conventional: same as DS)
            "mov ss, {data_sel:x}",    // Stack Segment
            code_sel = in(reg) code_selector as u64,
            data_sel = in(reg) data_selector,
            tmp = lateout(reg) _,
        );
    }
}

/// Load the Task Register with a TSS selector.
///
/// Executes the `ltr` instruction, which tells the CPU where to find the
/// Task State Segment. The TSS must be described by a valid 16-byte system
/// segment descriptor in the GDT. After loading, the CPU will use the TSS
/// for interrupt stack table (IST) lookups and privilege-level stack switches.
///
/// # Safety
///
/// - `selector` must point to a valid TSS descriptor in the loaded GDT.
/// - The TSS must remain at its current address for the lifetime of the kernel.
#[inline(always)]
pub unsafe fn ltr(selector: u16) {
    // ltr loads the 16-bit task register. The `:x` modifier gives us the
    // 16-bit form of the register (e.g., `ax` instead of `rax`), which is
    // what the `ltr` instruction expects as its operand.
    unsafe {
        asm!(
            "ltr {0:x}",
            in(reg) selector,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Check whether maskable CPU interrupts are currently enabled.
///
/// Reads the RFLAGS register and checks the IF (Interrupt Flag) at bit 9.
/// Returns `true` if interrupts are enabled (IF=1), `false` if disabled (IF=0).
///
/// Used by `InterruptMutex` to save the interrupt state before disabling
/// interrupts, so it can restore the exact same state when the lock is released.
/// This is important for nested critical sections — if interrupts were already
/// disabled by an outer lock, the inner lock should NOT re-enable them on release.
#[inline(always)]
pub fn interrupts_enabled() -> bool {
    let rflags: u64;
    // PUSHFQ pushes the full 64-bit RFLAGS register onto the stack, then
    // we POP it into a general-purpose register so Rust can inspect it.
    // We check bit 9 (IF — Interrupt Flag) to determine the interrupt state.
    unsafe {
        asm!(
            "pushfq",
            "pop {}",
            out(reg) rflags,
            // nomem: we don't access any Rust-visible memory (the stack push/pop
            // is invisible to the compiler's memory model).
            // No nostack: we do use the stack (push/pop), so the compiler must
            // account for stack usage.
            // No preserves_flags: pushfq reads flags, doesn't modify them, but
            // we omit the annotation to be conservative.
            options(nomem)
        );
    }
    rflags & (1 << 9) != 0
}
