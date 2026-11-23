/* user.ld — layout of a FreyaOS user program written in Rust (Entry 20).
 * The same places as every NASM program (kernel/src/process.rs):
 *
 *   0x4000_0000  code and constants, read + execute: what the kernel
 *                copies in (at most 1 MiB). _start comes first, with the
 *                program header the kernel reads (src/lib.rs).
 *   0x4010_0000  the start block: two pages the kernel fills before the
 *                program starts (empty for most programs)
 *   0x4010_2000  .bss: zeroed, read + write
 *
 * The kernel maps data pages zeroed and copies nothing into them except
 * the start block, so there can be no initialised writable data (.data):
 * a `static mut` must start as all zeros. */
OUTPUT_FORMAT("elf64-x86-64")
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

SECTIONS
{
    . = 0x40000000;
    .text : {
        KEEP(*(.text.start))
        *(.text .text.*)
    }
    .rodata : {
        *(.rodata .rodata.*)
        *(.data.rel.ro .data.rel.ro.*)
        *(.got .got.plt)   /* static linking: filled in by the linker, never written */
    }
    .data : {
        *(.data .data.*)
    }
    ASSERT(SIZEOF(.data) == 0, "a user program can't have initialised writable data: make it a zeroed static mut, or a constant")
    __image_end = .;
    ASSERT(__image_end <= 0x40100000, "a user program's code is limited to 1 MiB")

    . = 0x40102000;
    .bss (NOLOAD) : ALIGN(16) {
        *(.bss .bss.*)
        *(COMMON)
    }
    . = ALIGN(4096);
    __data_end = .;
    /* Pages from 0x4010_0000 (start block included) the kernel maps. */
    __data_pages = (__data_end - 0x40100000) / 4096;

    /DISCARD/ : {
        *(.comment)
        *(.eh_frame*)
        *(.note .note.*)
    }
}
