/*
 * ThemeliOS x86_64 Linker Script
 *
 * This script controls how the kernel binary is laid out in memory.
 * The Limine bootloader loads the kernel ELF and maps it at the virtual
 * address specified here (higher half: 0xffffffff80000000).
 *
 * Key concepts:
 *   - ENTRY(kmain): tells the ELF loader which function is the entry point
 *   - PHDRS: defines ELF program headers (segments) with their permissions
 *   - SECTIONS: maps input sections from .o files to output sections in the ELF
 *   - KEEP(): prevents the linker from garbage-collecting a section even if
 *     nothing references it (important for Limine request markers)
 *
 * Memory layout (virtual addresses):
 *   0xffffffff80000000  .text     (kernel code, read + execute)
 *   ...                 .rodata   (read-only data)
 *   ...                 .data     (initialized mutable data + Limine requests)
 *   ...                 .bss      (zero-initialized data, not stored in ELF)
 */

/* The ELF entry point — the bootloader jumps here after loading the kernel */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(kmain)

/*
 * Program headers (PHDRS) define segments — contiguous regions of memory with
 * the same permissions. The OS/bootloader loads these segments into memory.
 *
 * PT_LOAD means "load this segment into memory."
 * We don't specify FLAGS here; the linker infers them from section attributes.
 */
PHDRS
{
    text    PT_LOAD;
    rodata  PT_LOAD;
    data    PT_LOAD;
}

SECTIONS
{
    /*
     * Place the kernel in the higher half of virtual memory.
     * Limine maps the kernel here and sets up page tables accordingly.
     * The topmost 2 GiB (starting at 0xffffffff80000000) is the standard
     * location for higher-half kernels in the Limine protocol.
     */
    . = 0xffffffff80000000;

    /* Executable code */
    .text : {
        *(.text .text.*)
    } :text

    /* Align to page boundary between segments with different permissions */
    . = ALIGN(CONSTANT(MAXPAGESIZE));

    /* Read-only data (string literals, constants, vtables) */
    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    /*
     * Initialized mutable data.
     *
     * The Limine request sections are placed here. These are special data
     * structures that the bootloader scans for at load time. The start/end
     * markers delimit the region where Limine looks for requests.
     *
     * KEEP() prevents the linker from discarding these sections during
     * garbage collection, since nothing in our code references them directly
     * (the bootloader does).
     */
    .data : {
        *(.data .data.*)

        /* Limine boot protocol request sections */
        KEEP(*(.requests_start_marker))
        KEEP(*(.requests))
        KEEP(*(.requests_end_marker))
    } :data

    /*
     * Zero-initialized data. Must be the last section in the :data segment
     * so the ELF file doesn't waste space storing zeros — the loader just
     * allocates this much extra memory and zeros it.
     */
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /* Discard sections we don't need — saves space in the final binary */
    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
    }
}
