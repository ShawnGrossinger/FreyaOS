; ---------------------------------------------------------------------------
; entry.asm — 64-bit kernel entry stub + CPU exception stubs for FreyaOS
;
; By the time we reach _start, stage2's trampoline has already:
;   - switched to 64-bit long mode (CS=0x18)
;   - reloaded DS/ES/SS/FS/GS with selector 0x20
;   - enabled SSE (CR4.OSFXSR + OSXMMEXCPT, CR0.EM=0 CR0.MP=1)
;   - set RSP = KERNEL_STACK_TOP (0x80000)
;
; _start zeroes .bss (which is NOT in kernel.bin — objcopy drops NOBITS
; sections) and hands off to Rust.
;
; The second half of this file holds the exception entry stubs that the IDT
; points at (see src/idt.rs).
;
; Built as ELF object by build.rs:  nasm -f elf64 entry.asm -o entry.o
; ---------------------------------------------------------------------------

BITS 64

section .entry progbits alloc exec nowrite align=16

global _start
extern rust_start
extern __bss_start
extern __bss_end

_start:
    ; Zero .bss. Stage2 only copies the bytes in kernel.bin, and .bss is
    ; not in kernel.bin, so without this every zero-initialised static
    ; (including the IDT) starts as whatever was left in RAM. QEMU's RAM
    ; happens to start zeroed; real hardware's does not.
    ; Symbols come from kernel.ld.
    cld
    lea rdi, [rel __bss_start]
    lea rcx, [rel __bss_end]
    sub rcx, rdi                ; byte count
    xor eax, eax
    rep stosb

    ; Align stack to 16 bytes for the SysV ABI.
    and rsp, -16

    call rust_start

.hang:
    cli
    hlt
    jmp .hang

; ---------------------------------------------------------------------------
; CPU exception entry stubs (vectors 0-31)
;
; On an exception in 64-bit mode the CPU:
;   1. aligns RSP down to 16 bytes
;   2. pushes SS, RSP, RFLAGS, CS, RIP
;   3. for some vectors only, pushes an error code
;
; Every stub makes the frame uniform: vectors without a CPU error code push
; a dummy 0 first. Then every stub pushes its vector number and jumps to
; isr_common. The resulting layout is mirrored by InterruptFrame in idt.rs.
; ---------------------------------------------------------------------------

section .text

extern exception_dispatch

; Vector where the CPU does NOT push an error code.
%macro ISR_NOERR 1
isr_stub_%1:
    push qword 0                ; dummy error code
    push qword %1               ; vector number
    jmp isr_common
%endmacro

; Vector where the CPU DOES push an error code (it is already on the stack).
%macro ISR_ERR 1
isr_stub_%1:
    push qword %1               ; vector number
    jmp isr_common
%endmacro

ISR_NOERR 0     ; #DE  divide error
ISR_NOERR 1     ; #DB  debug
ISR_NOERR 2     ;      NMI
ISR_NOERR 3     ; #BP  breakpoint (int3)
ISR_NOERR 4     ; #OF  overflow
ISR_NOERR 5     ; #BR  bound range
ISR_NOERR 6     ; #UD  invalid opcode
ISR_NOERR 7     ; #NM  device not available
ISR_ERR   8     ; #DF  double fault (error code always 0)
ISR_NOERR 9     ;      coprocessor segment overrun (legacy)
ISR_ERR   10    ; #TS  invalid TSS
ISR_ERR   11    ; #NP  segment / gate not present
ISR_ERR   12    ; #SS  stack-segment fault
ISR_ERR   13    ; #GP  general protection
ISR_ERR   14    ; #PF  page fault
ISR_NOERR 15    ;      reserved
ISR_NOERR 16    ; #MF  x87 floating-point
ISR_ERR   17    ; #AC  alignment check (error code always 0)
ISR_NOERR 18    ; #MC  machine check
ISR_NOERR 19    ; #XM  SIMD floating-point
ISR_NOERR 20    ; #VE  virtualization
ISR_ERR   21    ; #CP  control protection
ISR_NOERR 22    ;      reserved
ISR_NOERR 23    ;      reserved
ISR_NOERR 24    ;      reserved
ISR_NOERR 25    ;      reserved
ISR_NOERR 26    ;      reserved
ISR_NOERR 27    ;      reserved
ISR_NOERR 28    ; #HV  hypervisor injection (AMD)
ISR_ERR   29    ; #VC  VMM communication (AMD)
ISR_ERR   30    ; #SX  security exception (AMD)
ISR_NOERR 31    ;      reserved

; ---------------------------------------------------------------------------
; isr_common — shared path for every exception.
;
; Stack on arrival (lowest address first):
;   vector, error_code, RIP, CS, RFLAGS, RSP, SS      = 7 qwords
; We push 15 GPRs                                      = 15 qwords
; 22 qwords = 176 bytes = 11 * 16, and the CPU 16-aligned RSP before its
; pushes, so RSP is 16-aligned again after the GPR pushes.
;
; Then 512 bytes for FXSAVE (x87/SSE state). The kernel is compiled with
; SSE, so the Rust handler may use XMM registers; saving them means an
; exception we return from (int3 today, IRQs later) can't corrupt the
; interrupted code's XMM state. 512 is a multiple of 16, so RSP stays
; aligned for both FXSAVE (needs 16) and the SysV call (needs 16).
; ---------------------------------------------------------------------------
isr_common:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15                    ; last push = lowest address = first field

    sub rsp, 512
    fxsave [rsp]

    cld                         ; SysV requires DF=0 at function entry
    lea rdi, [rsp + 512]        ; arg0 = &InterruptFrame (points at saved r15)
    call exception_dispatch     ; returns only for recoverable vectors (#BP)

    fxrstor [rsp]
    add rsp, 512

    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax

    add rsp, 16                 ; drop vector + error code
    iretq

; ---------------------------------------------------------------------------
; isr_stub_table — addresses of the 32 stubs, read by idt.rs to fill gates.
; ---------------------------------------------------------------------------
section .rodata

global isr_stub_table
align 8
isr_stub_table:
%assign i 0
%rep 32
    dq isr_stub_%+i
%assign i i+1
%endrep
