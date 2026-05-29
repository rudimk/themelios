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

/// Write a 32-bit (double word) value to an x86 I/O port.
///
/// This executes the `out dx, eax` instruction, which sends a 32-bit value
/// to the I/O port number in `dx`. Used for devices with 32-bit I/O port
/// interfaces, such as QEMU's `isa-debug-exit` device.
///
/// # Safety
///
/// Writing to an I/O port can have arbitrary side effects on hardware.
/// The caller must ensure the port number is valid and the write is
/// appropriate for the device at that port.
#[inline(always)]
pub unsafe fn outl(port: u16, value: u32) {
    unsafe {
        asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Read a 32-bit (double word) value from an x86 I/O port.
///
/// This executes the `in eax, dx` instruction, which reads a 32-bit value
/// from the I/O port number in `dx` into `eax`. Used for devices with 32-bit
/// I/O port interfaces — notably the PCI configuration space data port
/// (0xCFC), which always returns a full 32-bit register.
///
/// # Safety
///
/// Reading from an I/O port can have side effects on hardware. The caller
/// must ensure the port number is valid and the read is appropriate for the
/// device at that port.
#[inline(always)]
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe {
        asm!(
            "in eax, dx",
            in("dx") port,
            out("eax") value,
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

/// IDT Register descriptor — the exact layout the `lidt` instruction expects.
///
/// Identical format to `GdtRegister` (both use the same CPU descriptor table
/// pointer format), but a separate type for clarity.
#[repr(C, packed)]
pub struct IdtRegister {
    /// Size of the IDT in bytes minus 1. For a 256-entry IDT with 16-byte
    /// entries: 256 * 16 - 1 = 4095.
    pub limit: u16,
    /// Virtual address of the first IDT entry.
    pub base: u64,
}

/// Load a new Interrupt Descriptor Table.
///
/// Executes the `lidt` instruction to point the CPU at the IDT. After this,
/// any interrupt or exception will be dispatched through the new IDT entries.
///
/// # Safety
///
/// - The `IdtRegister` must describe a valid IDT at a stable address.
/// - The IDT must contain valid gate descriptors for all expected vectors.
/// - The IDT must remain in memory for the lifetime of the kernel.
#[inline(always)]
pub unsafe fn lidt(idtr: &IdtRegister) {
    unsafe {
        asm!(
            "lidt [{}]",
            in(reg) idtr as *const IdtRegister,
            options(nostack, preserves_flags)
        );
    }
}

/// Execute a software breakpoint (INT3 instruction).
///
/// Triggers exception vector 3 (Breakpoint, #BP). Used to test that the
/// IDT and exception handlers are working correctly. The breakpoint handler
/// should print diagnostics and return, allowing execution to continue.
#[inline(always)]
pub fn int3() {
    unsafe {
        asm!("int3", options(nomem, nostack));
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

// --- Page table / virtual memory support ---
//
// These functions provide the raw CPU operations for manipulating page tables:
// reading/writing the CR3 register (which holds the physical address of the
// active PML4 page table) and invalidating individual TLB entries.

/// Read the current value of the CR3 register.
///
/// CR3 holds the physical address of the top-level page table (PML4) for the
/// currently active address space. On x86_64, the lower 12 bits of CR3 contain
/// flags (PCID, PWT, PCD) but the physical address portion is bits 12-51
/// (the PML4 must be page-aligned, so the low 12 bits of the address are
/// implicitly zero).
///
/// Returns the raw CR3 value including flags. Mask with `!0xFFF` to get
/// just the PML4 physical address.
#[inline(always)]
pub fn read_cr3() -> u64 {
    let value: u64;
    // SAFETY: reading CR3 is a privileged but non-destructive operation.
    // It simply returns the current page table base address.
    unsafe {
        asm!(
            "mov {}, cr3",
            out(reg) value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// Write a new value to the CR3 register, switching the active page table.
///
/// This causes the CPU to start using the PML4 at `value` (physical address)
/// for all address translations. Writing CR3 also flushes all non-global TLB
/// entries, ensuring stale translations from the old page table don't linger.
///
/// # Safety
///
/// - `value` must be the physical address of a valid, page-aligned PML4 table.
/// - The new page table must correctly map the currently executing code and
///   stack, or execution will immediately fault.
/// - The new page table must map the kernel's HHDM and code regions identically
///   to the current table (for the kernel upper-half, this is guaranteed by
///   copying PML4 entries 256-511).
#[inline(always)]
pub unsafe fn write_cr3(value: u64) {
    // SAFETY: caller guarantees the page table is valid and maps
    // currently executing code correctly. The `volatile` nature of CR3
    // writes means we must not let the compiler reorder or eliminate this.
    unsafe {
        asm!(
            "mov cr3, {}",
            in(reg) value,
            // No nomem: writing CR3 changes the memory mapping, which affects
            // how all subsequent memory accesses resolve. The compiler must not
            // reorder loads/stores across this point.
            options(nostack, preserves_flags)
        );
    }
}

/// Invalidate the TLB entry for a single virtual address.
///
/// Executes the `invlpg` instruction, which removes the cached translation
/// for the page containing `addr` from the TLB. This must be called after
/// unmapping or modifying a page table entry to ensure the CPU uses the
/// updated mapping.
///
/// On a multi-core system, `invlpg` only affects the current core's TLB —
/// a TLB shootdown IPI would be needed for other cores. On our single-core
/// Phase 2 system, this is sufficient.
///
/// # Safety
///
/// The address must be a valid virtual address. Invalidating a TLB entry for
/// a mapped page is safe (the next access will re-walk the page table), but
/// invalidating kernel code pages in a hot loop could cause performance issues.
#[inline(always)]
pub unsafe fn invlpg(addr: u64) {
    // SAFETY: invlpg invalidates a single TLB entry. The CPU will re-walk
    // the page tables on the next access to this address. This is safe
    // but must be called at the right time (after modifying PTEs).
    unsafe {
        asm!(
            "invlpg [{}]",
            in(reg) addr,
            // No nomem: the whole point is to change how memory is accessed.
            options(nostack, preserves_flags)
        );
    }
}

// --- Model-Specific Registers (MSRs) ---
//
// MSRs are CPU configuration registers accessed via `rdmsr` and `wrmsr` instructions.
// Each MSR has a 32-bit address (specified in ECX) and a 64-bit value (split across
// EDX:EAX for historical reasons — high 32 bits in EDX, low 32 bits in EAX).
//
// Key MSRs for syscall/sysret:
// - IA32_EFER (0xC0000080): Extended Feature Enable Register (SCE bit enables syscall)
// - IA32_STAR (0xC0000081): Segment selectors for syscall/sysret transitions
// - IA32_LSTAR (0xC0000082): RIP target for syscall entry (64-bit mode)
// - IA32_FMASK (0xC0000084): RFLAGS mask applied on syscall entry
// - IA32_GS_BASE (0xC0000101): Current GS segment base (userspace GS)
// - IA32_KERNEL_GS_BASE (0xC0000102): Kernel GS base (swapped in by swapgs)

/// Read a Model-Specific Register (MSR).
///
/// Returns the 64-bit value of the MSR at the given address. The `rdmsr`
/// instruction puts the low 32 bits in EAX and the high 32 bits in EDX;
/// we combine them into a single u64.
///
/// # Safety
///
/// Reading an unimplemented or reserved MSR causes a #GP exception.
/// The caller must ensure the MSR address is valid.
#[inline(always)]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags)
        );
    }
    ((high as u64) << 32) | (low as u64)
}

/// Write a value to a Model-Specific Register (MSR).
///
/// Writes a 64-bit value to the MSR at the given address. The `wrmsr`
/// instruction takes the low 32 bits from EAX and the high 32 bits from EDX.
///
/// # Safety
///
/// - Writing to an unimplemented or reserved MSR causes a #GP exception.
/// - Writing incorrect values to MSRs can crash the system or cause undefined
///   behavior (e.g., a bad IA32_LSTAR causes syscall to jump to garbage).
/// - The caller must ensure the MSR address and value are correct.
#[inline(always)]
pub unsafe fn wrmsr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") low,
            in("edx") high,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Execute the `swapgs` instruction.
///
/// Atomically swaps the values of `IA32_GS_BASE` (MSR 0xC0000101) and
/// `IA32_KERNEL_GS_BASE` (MSR 0xC0000102). Used at syscall entry (swap
/// user GS for kernel GS, giving access to the PerCpu struct) and syscall
/// exit (swap kernel GS back to user GS before returning to ring 3).
///
/// After `swapgs` at syscall entry, `gs:0` points to the PerCpu struct
/// containing the current task's kernel stack pointer.
///
/// # Safety
///
/// Must only be called at ring transitions (syscall entry/exit). Calling
/// swapgs twice without an intervening ring transition leaves the GS bases
/// in the wrong state (user GS in the kernel slot and vice versa).
#[inline(always)]
pub unsafe fn swapgs() {
    unsafe {
        asm!(
            "swapgs",
            options(nomem, nostack, preserves_flags)
        );
    }
}

// --- QEMU test infrastructure ---

/// I/O port for the QEMU `isa-debug-exit` device.
///
/// When QEMU is launched with `-device isa-debug-exit,iobase=0xf4,iosize=0x04`,
/// writing to this port causes QEMU to exit with exit code `(value << 1) | 1`.
const QEMU_EXIT_PORT: u16 = 0xf4;

/// Exit QEMU with a mapped exit code.
///
/// The `isa-debug-exit` device transforms the written value into QEMU's
/// process exit code as: `exit_code = (value << 1) | 1`. We use:
/// - `exit_qemu(0x01)` → QEMU exits with code `3` → **test success**
/// - `exit_qemu(0x00)` → QEMU exits with code `1` → **test failure**
///
/// This function does not return. After writing to the exit port, the
/// CPU enters an infinite halt loop as a safety net (the QEMU process
/// should terminate before the halt is ever reached).
pub fn exit_qemu(value: u32) -> ! {
    // SAFETY: Writing to the isa-debug-exit port causes QEMU to terminate.
    // This is only used in test mode — in normal operation this device
    // isn't configured, and the write is harmless (ignored by hardware).
    unsafe {
        outb(QEMU_EXIT_PORT, value as u8);
    }

    // Unreachable — QEMU should have exited. Halt just in case.
    loop {
        halt();
    }
}
