; ---------------------------------------------------------------------------
; entry.asm — 64-bit kernel entry stub for FreyaOS
;
; By the time we reach here, stage2's trampoline has already:
;   - switched to 64-bit long mode (CS=0x18)
;   - reloaded DS/ES/SS/FS/GS with selector 0x20
;   - enabled SSE (CR4.OSFXSR + OSXMMEXCPT, CR0.EM=0 CR0.MP=1)
;   - set RSP = KERNEL_STACK_TOP (0x80000)
;
; All we do here is a VGA probe (to confirm we reached the kernel)
; and hand off to Rust.
;
; Built as ELF object by build.rs:  nasm -f elf64 entry.asm -o entry.o
; ---------------------------------------------------------------------------

BITS 64

section .entry progbits alloc exec nowrite align=16

global _start
extern rust_start

_start:
    ; VGA probe: 'KK' white-on-red at row 1, col 0 (0xb8000 + 80*2)
    ; Stage2 wrote 'S2 J' at row 0.  If we see KK on row 1, the
    ; entire boot chain from BIOS to kernel entry is proven.
    mov rax, 0xb80a0
    mov byte [rax],     'K'
    mov byte [rax + 1], 0x4F
    mov byte [rax + 2], 'K'
    mov byte [rax + 3], 0x4F

    ; Align stack to 16 bytes for the SysV ABI.
    and rsp, -16

    call rust_start

.hang:
    cli
    hlt
    jmp .hang