/* Linker script for ThemeliOS userspace servers.
 *
 * Servers are loaded by the kernel as flat binaries (no ELF parsing in the
 * kernel — an ELF parser is itself attack surface). We link with LLD's
 * `--oformat binary` so the output file IS the raw memory image, and place
 * everything starting at a fixed virtual base. The kernel maps the binary's
 * bytes at that same address and jumps to its first byte (_start).
 *
 * Layout requirements:
 *   - _start must be the very first byte of the image (the kernel jumps to the
 *     load base), so the `.text.start` section is placed first.
 *   - All allocatable sections are contiguous from SERVER_BASE upward, keeping
 *     the flat binary compact (no huge zero-filled gaps).
 *   - .bss is NOLOAD; the kernel zeroes the pages it maps for the image plus a
 *     bss allowance, so uninitialised statics start zeroed without bloating the
 *     embedded binary.
 *
 * This base MUST match SERVER_CODE_VIRT in the kernel's server loader.
 */

ENTRY(_start)

SECTIONS {
    . = 0x200000;          /* 2 MiB — SERVER_CODE_VIRT in the kernel loader */

    .text : {
        *(.text.start)     /* _start first, so it lands at the load base */
        *(.text .text.*)
    }

    .rodata : {
        *(.rodata .rodata.*)
    }

    .data : {
        *(.data .data.*)
    }

    /* End of the loaded image (everything above is in the flat binary). */
    . = ALIGN(8);
    __bss_start = .;
    .bss (NOLOAD) : {
        *(.bss .bss.*)
        *(COMMON)
    }
    __bss_end = .;

    /* Drop metadata sections that would otherwise create gaps in the binary. */
    /DISCARD/ : {
        *(.eh_frame*)
        *(.comment)
        *(.note*)
    }
}
