//! # Global Descriptor Table (GDT) and Task State Segment (TSS)
//!
//! The GDT is an x86 legacy structure that defines memory segments. In 64-bit
//! long mode, segmentation is mostly vestigial — the CPU ignores segment base
//! and limit for code and data segments (everything is flat 0-based). However,
//! the GDT is still required for:
//!
//! - **Ring transitions**: The CS (Code Segment) selector determines the current
//!   privilege level (ring 0 = kernel, ring 3 = user). Switching between rings
//!   requires valid GDT entries.
//! - **TSS**: The Task State Segment descriptor lives in the GDT. The CPU uses
//!   the TSS to find interrupt stack table (IST) entries and the kernel stack
//!   pointer (RSP0) for privilege transitions.
//!
//! ## GDT layout
//!
//! Our GDT has 5 logical entries (occupying 5 u64 slots):
//!
//! | Index | Selector | Description |
//! |-------|----------|-------------|
//! | 0     | 0x00     | Null descriptor (required by CPU) |
//! | 1     | 0x08     | Kernel code segment (64-bit, ring 0) |
//! | 2     | 0x10     | Kernel data segment (ring 0) |
//! | 3-4   | 0x18     | TSS descriptor (16 bytes, spans two slots) |
//!
//! The selector value = index * 8 (each GDT entry is 8 bytes).
//!
//! ## TSS purpose
//!
//! In long mode, the TSS is not used for hardware task switching (that's a
//! 32-bit feature). Instead, it provides:
//!
//! - **RSP0-RSP2**: Stack pointers loaded on ring transitions. RSP0 is loaded
//!   when transitioning from ring 3 to ring 0 (e.g., syscall). Not used in
//!   Phase 1 since we have no userspace yet.
//! - **IST1-IST7**: Interrupt Stack Table entries. When an IDT gate specifies
//!   an IST index (1-7), the CPU loads the corresponding stack pointer from the
//!   TSS before invoking the handler. IST index 0 means "don't use IST" (use
//!   the current stack). We use IST1 for the double-fault handler so it gets
//!   a known-good stack even if the kernel stack is corrupted.
//!
//! ## Why the TSS descriptor is 16 bytes
//!
//! Most GDT entries are 8 bytes, but the TSS descriptor is a "system segment
//! descriptor" that needs to hold a full 64-bit base address. In 32-bit mode,
//! the 32-bit base fits in one 8-byte entry. In 64-bit mode, the upper 32 bits
//! of the base spill into a second 8-byte slot, making the TSS descriptor 16
//! bytes total. This is a notorious gotcha — if you only allocate 8 bytes for
//! the TSS entry, the next GDT entry gets misinterpreted as the upper half of
//! the TSS descriptor.

use core::mem;

use super::cpu;
use crate::println;

// --- Segment selectors ---
//
// A segment selector is a 16-bit value that identifies a GDT entry.
// Format: [index:13 bits][TI:1 bit][RPL:2 bits]
// - index: GDT entry index (byte offset / 8)
// - TI: Table Indicator (0 = GDT, 1 = LDT — we always use GDT)
// - RPL: Requested Privilege Level (0 = kernel, 3 = user)
//
// For kernel segments at GDT index N with RPL=0:
//   selector = N * 8

/// Segment selector for the kernel code segment (GDT index 1, RPL 0).
#[allow(clippy::identity_op)]
pub const KERNEL_CODE_SELECTOR: u16 = 1 * 8;

/// Segment selector for the kernel data segment (GDT index 2, RPL 0).
pub const KERNEL_DATA_SELECTOR: u16 = 2 * 8;

/// Segment selector for the TSS (GDT index 3, RPL 0).
const TSS_SELECTOR: u16 = 3 * 8;

/// IST index for the double-fault handler (used in the IDT entry).
///
/// The IDT's IST field is 1-indexed: 0 means "don't use IST", 1 means
/// use IST entry 1 (which is `ist[0]` in our TSS struct). We put the
/// double-fault stack in IST1 so the handler always gets a clean stack,
/// even if the exception was caused by a corrupted or exhausted kernel stack.
pub const DOUBLE_FAULT_IST_INDEX: u8 = 1;

// --- Segment descriptor constants ---
//
// GDT entries are 8 bytes (u64). In 64-bit mode, base and limit are ignored
// for code/data segments (everything is flat), but the access byte and flags
// still matter.
//
// Access byte layout (bits 40-47 of the descriptor):
//   bit 7: Present (P) — must be 1 for valid segments
//   bits 5-6: Descriptor Privilege Level (DPL) — ring level (0 = kernel)
//   bit 4: Descriptor type (S) — 1 for code/data, 0 for system (TSS)
//   bit 3: Executable (E) — 1 for code, 0 for data
//   bit 2: Direction/Conforming (DC) — 0 for normal segments
//   bit 1: Readable/Writable (RW) — 1 to allow reads (code) or writes (data)
//   bit 0: Accessed (A) — set by CPU on first access, we set to 0
//
// Flags nibble (bits 52-55 of the descriptor):
//   bit 55: Granularity (G) — 0 for byte granularity
//   bit 54: Size (D/B) — 0 for 64-bit code segments
//   bit 53: Long mode (L) — 1 for 64-bit code segments (must be 0 for data)
//   bit 52: Available (AVL) — 0, not used by hardware

/// Kernel code segment descriptor (64-bit, ring 0).
///
/// Access byte = 0x9A: Present=1, DPL=00, Type=1, Exec=1, Conform=0, Read=1, Access=0
/// Flags = 0x2: Long=1 (64-bit mode), Size=0, Granularity=0
/// Base and limit = 0 (ignored in 64-bit mode).
const KERNEL_CODE_64: u64 = 0x00209A0000000000;

/// Kernel data segment descriptor (ring 0).
///
/// Access byte = 0x92: Present=1, DPL=00, Type=1, Exec=0, Direction=0, Write=1, Access=0
/// Flags = 0x0: Long=0 (not applicable to data), Size=0, Granularity=0
/// Base and limit = 0 (ignored in 64-bit mode).
const KERNEL_DATA_64: u64 = 0x0000920000000000;

// --- Static structures ---
//
// These are `static mut` because they must live at fixed addresses for the
// entire lifetime of the kernel (the CPU holds pointers to them in the GDTR
// and TR registers). They are only written once during early boot (before
// interrupts are enabled) and read-only thereafter — the CPU reads them
// on every interrupt and privilege transition.

/// Number of u64 entries in the GDT.
/// 5 entries: null + kernel code + kernel data + TSS low + TSS high.
const GDT_ENTRY_COUNT: usize = 5;

/// The raw GDT table. Filled in during `init()`, then loaded with `lgdt`.
static mut GDT: [u64; GDT_ENTRY_COUNT] = [0; GDT_ENTRY_COUNT];

/// The Task State Segment. IST entries are filled in during `init()`,
/// then referenced by the TSS descriptor in the GDT.
static mut TSS: Tss = Tss::new();

/// Size of the double-fault handler's dedicated stack (5 pages = 20 KiB).
/// Generous for a handler that just prints diagnostics and halts.
const DOUBLE_FAULT_STACK_SIZE: usize = 4096 * 5;

/// Static memory for the double-fault stack. Zeroed at boot.
/// The IST entry in the TSS points to the TOP of this array (stacks grow
/// downward on x86_64, so the initial RSP is the highest address).
static mut DOUBLE_FAULT_STACK: [u8; DOUBLE_FAULT_STACK_SIZE] = [0; DOUBLE_FAULT_STACK_SIZE];

// --- TSS structure ---

/// Task State Segment for 64-bit long mode (104 bytes).
///
/// The layout is dictated by the CPU — every field must be at its exact
/// byte offset, hence `repr(C, packed)`. See Intel SDM Vol. 3A, Section 7.7.
///
/// Field offsets:
/// - 0x00: reserved
/// - 0x04: RSP0, RSP1, RSP2 (ring transition stacks)
/// - 0x1C: reserved
/// - 0x24: IST1 through IST7 (interrupt stack table)
/// - 0x5C: reserved
/// - 0x64: reserved
/// - 0x66: I/O Map Base Address
#[repr(C, packed)]
struct Tss {
    _reserved0: u32,

    /// Stack pointers for ring transitions.
    /// RSP0 is loaded when transitioning from ring 3 (user) to ring 0 (kernel).
    /// RSP1 and RSP2 are for rings 1 and 2 (unused in our design).
    rsp: [u64; 3],

    _reserved1: u64,

    /// Interrupt Stack Table entries (IST1-IST7).
    /// When an IDT gate specifies IST index N (1-7), the CPU loads ist[N-1]
    /// as the new RSP before invoking the handler. This ensures critical
    /// handlers (like double fault) always have a known-good stack.
    ist: [u64; 7],

    _reserved2: u64,
    _reserved3: u16,

    /// Offset of the I/O permission bitmap from the TSS base.
    /// Set to sizeof(Tss) to indicate "no I/O bitmap" (offset points past
    /// the end of the TSS, so all I/O ports are treated as privileged).
    iomap_base: u16,
}

impl Tss {
    /// Create a zeroed TSS with the I/O map base set past the end.
    const fn new() -> Self {
        Self {
            _reserved0: 0,
            rsp: [0; 3],
            _reserved1: 0,
            ist: [0; 7],
            _reserved2: 0,
            _reserved3: 0,
            // Setting iomap_base to the TSS size means "no I/O permission bitmap".
            // Any I/O port access from ring > 0 will cause a #GP.
            iomap_base: mem::size_of::<Self>() as u16,
        }
    }
}

// --- TSS descriptor builder ---

/// Build a 16-byte TSS system segment descriptor.
///
/// Returns `(lower_u64, upper_u64)` — the two GDT entries that together form
/// the TSS descriptor. The lower 8 bytes follow the standard segment descriptor
/// format; the upper 8 bytes hold the high 32 bits of the 64-bit TSS base address.
///
/// ## Descriptor layout (lower 8 bytes)
///
/// ```text
/// Bits 0-15:  Limit[0:15]    (TSS size - 1)
/// Bits 16-31: Base[0:15]     (lower 16 bits of TSS address)
/// Bits 32-39: Base[16:23]    (next 8 bits of TSS address)
/// Bits 40-47: Access byte    (0x89 = present, DPL=0, 64-bit TSS available)
/// Bits 48-51: Limit[16:19]   (upper 4 bits of limit)
/// Bits 52-55: Flags          (0x0 = byte granularity, no AVL/L/DB)
/// Bits 56-63: Base[24:31]    (next 8 bits of TSS address)
/// ```
///
/// ## Access byte (0x89)
///
/// ```text
/// Bit 7:    Present = 1
/// Bits 5-6: DPL = 00 (kernel)
/// Bit 4:    System = 0 (this is a system segment, not code/data)
/// Bits 0-3: Type = 0b1001 (available 64-bit TSS)
/// ```
fn make_tss_descriptor(base: u64, limit: u64) -> (u64, u64) {
    // Access byte: 0x89 = Present(1), DPL(00), System(0), Type(1001 = 64-bit TSS available)
    const TSS_ACCESS: u64 = 0x89;

    let lower =
        (limit & 0xFFFF)                          // bits 0-15: limit low
        | ((base & 0xFFFF) << 16)                 // bits 16-31: base[0:15]
        | (((base >> 16) & 0xFF) << 32)           // bits 32-39: base[16:23]
        | (TSS_ACCESS << 40)                      // bits 40-47: access byte
        | (((limit >> 16) & 0xF) << 48)           // bits 48-51: limit[16:19]
        | (((base >> 24) & 0xFF) << 56);          // bits 56-63: base[24:31]

    // Upper 8 bytes: base[32:63] in bits 0-31, reserved zeros in bits 32-63
    let upper = base >> 32;

    (lower, upper)
}

// --- Initialization ---

/// Initialize the GDT and TSS, then load them into the CPU.
///
/// This sets up:
/// 1. The double-fault IST stack in the TSS
/// 2. The GDT entries (null, kernel code, kernel data, TSS descriptor)
/// 3. Loads the GDT via `lgdt`
/// 4. Reloads CS/DS/ES/SS to use the new kernel segments
/// 5. Loads the TSS via `ltr`
///
/// Must be called early in boot, before the IDT is set up (since the IDT
/// references GDT segment selectors and the TSS IST entries).
///
/// # Safety note
///
/// This function accesses `static mut` variables. It is safe to call because:
/// - It runs once during early single-threaded boot
/// - Interrupts are not yet enabled (no concurrent access)
/// - After this function, the statics are effectively read-only (only the CPU reads them)
pub fn init() {
    // SAFETY: Single-threaded early boot, no interrupts enabled yet.
    // These statics are written here and never modified again.
    unsafe {
        // --- Step 1: Configure the TSS ---
        //
        // Set IST entry 1 to the top of the double-fault stack.
        // x86 stacks grow downward: the initial RSP points to the highest
        // address in the stack allocation, and grows toward lower addresses.
        let stack_bottom = &raw const DOUBLE_FAULT_STACK as *const u8;
        let stack_top = stack_bottom.add(DOUBLE_FAULT_STACK_SIZE) as u64;

        let tss = &raw mut TSS;
        (*tss).ist[0] = stack_top;

        // --- Step 2: Build the GDT entries ---
        let tss_addr = tss as u64;
        let tss_limit = (mem::size_of::<Tss>() - 1) as u64;
        let (tss_low, tss_high) = make_tss_descriptor(tss_addr, tss_limit);

        let gdt = &raw mut GDT;

        // Entry 0: Null descriptor — required by the CPU. The first GDT entry
        // is never used (selector 0x00 triggers a #GP if loaded into a segment
        // register), serving as a guard against uninitialized selectors.
        (*gdt)[0] = 0;

        // Entry 1: Kernel code segment — the segment we execute kernel code in.
        (*gdt)[1] = KERNEL_CODE_64;

        // Entry 2: Kernel data segment — used for DS, ES, SS.
        (*gdt)[2] = KERNEL_DATA_64;

        // Entries 3-4: TSS descriptor — spans two slots because the 64-bit
        // base address doesn't fit in a single 8-byte descriptor.
        (*gdt)[3] = tss_low;
        (*gdt)[4] = tss_high;

        // --- Step 3: Load the GDT ---
        //
        // lgdt takes a 10-byte descriptor: 2-byte limit (size - 1) + 8-byte base.
        // The CPU caches these values internally, so the GdtRegister struct
        // doesn't need to persist after this call (but the GDT itself must).
        let gdtr = cpu::GdtRegister {
            limit: (mem::size_of_val(&*gdt) - 1) as u16,
            base: gdt as u64,
        };
        cpu::lgdt(&gdtr);

        // --- Step 4: Reload segment registers ---
        //
        // After lgdt, the old (Limine's) segment selectors are still loaded in
        // the CPU registers. We must reload them to point at our new GDT entries.
        // This is what makes the new GDT actually take effect.
        cpu::reload_segments(KERNEL_CODE_SELECTOR, KERNEL_DATA_SELECTOR);

        // --- Step 5: Load the TSS ---
        //
        // ltr tells the CPU where the TSS is. After this, the CPU will
        // use our TSS for IST lookups on interrupts and RSP0 on ring transitions.
        cpu::ltr(TSS_SELECTOR);
    }

    println!("GDT loaded (kernel code=0x{:02X}, data=0x{:02X}, TSS=0x{:02X})",
        KERNEL_CODE_SELECTOR, KERNEL_DATA_SELECTOR, TSS_SELECTOR);
}
