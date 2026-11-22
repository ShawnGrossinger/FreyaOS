OUTPUT_FORMAT("elf64-x86-64")
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

SECTIONS
{
    . = 0x0000000000100000;

    /* .entry contains the naked _start function via #[link_section = ".entry"].
     * This guarantees _start is the very first byte at 0x100000. */
    .entry : {
        KEEP(*(.entry))
    }

    .text : {
        *(.text .text.*)
    }

    .rodata : {
        *(.rodata .rodata.*)
    }

    .data : {
        *(.data .data.*)
    }

    /* .bss is NOBITS: objcopy -O binary leaves it out of kernel.bin, so
     * stage2 never loads it. _start zeroes [__bss_start, __bss_end). */
    .bss : ALIGN(16) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    /DISCARD/ : {
        *(.comment)
        *(.eh_frame)
        *(.note .note.*)
    }
}
