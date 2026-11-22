OUTPUT_FORMAT("elf64-x86-64")
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

SECTIONS
{
    . = 0x0000000000100000;
    __kernel_start = .;   /* pmm reserves [__kernel_start, __kernel_end) */

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

    . = ALIGN(4096);
    __kernel_end = .;

    /* _start zeroes .bss before the kernel has its own page tables, while
     * stage2's tables at 0x200000 are still live: the image and .bss must
     * end below them. This is also stage2's 2048-sector (1 MiB) limit. */
    ASSERT(__kernel_end <= 0x200000, "kernel image + .bss would overlap stage2's page tables at 0x200000")

    /DISCARD/ : {
        *(.comment)
        *(.eh_frame)
        *(.note .note.*)
    }
}
