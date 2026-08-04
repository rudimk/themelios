/*
 * ThemeliOS aarch64 Linker Script
 *
 * Mirrors linker-x86_64.ld for the ARM64 kernel: a higher-half kernel loaded by
 * Limine at 0xffffffff80000000. Only the output format and (implicitly) the
 * relocation model differ from x86_64 — the segment layout and the Limine request
 * markers are identical, because the Limine boot protocol is architecture-neutral.
 */

OUTPUT_FORMAT(elf64-littleaarch64)
ENTRY(kmain)

PHDRS
{
    text    PT_LOAD;
    rodata  PT_LOAD;
    data    PT_LOAD;
}

SECTIONS
{
    /* Higher half — same base as x86_64; Limine maps the kernel here. */
    . = 0xffffffff80000000;

    .text : {
        *(.text .text.*)
    } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .data : {
        *(.data .data.*)

        /* Limine boot protocol request sections (scanned by the bootloader). */
        KEEP(*(.requests_start_marker))
        KEEP(*(.requests))
        KEEP(*(.requests_end_marker))
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
    }
}
