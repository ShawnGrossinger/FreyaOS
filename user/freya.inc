; ---------------------------------------------------------------------------
; freya.inc — what a FreyaOS user program needs to know.
; Mirrors kernel/src/process.rs (layout, capability slots) and
; kernel/src/syscall.rs (system calls and errors).
; ---------------------------------------------------------------------------

; Where every process's memory is.
CODE_BASE   equ 0x40000000          ; code, read + execute (org of every program)
DATA_BASE   equ 0x40100000          ; data, read + write, zeroed
DATA_SIZE   equ 4096
STACK_TOP   equ 0x00007FFF00000000  ; RSP at entry (16 KiB of stack below)

; System calls: number in RAX, arguments in RDI, RSI, RDX, result in RAX.
; RCX and R11 are destroyed; every other register and the SSE state are kept.
SYS_EXIT    equ 0                   ; exit(code): never returns
SYS_YIELD   equ 1                   ; yield(): give up the rest of this time slice
SYS_WRITE   equ 2                   ; write(cap, ptr, len): bytes written

ERR_BAD_SYSCALL   equ -1
ERR_NO_CAPABILITY equ -2
ERR_BAD_ADDRESS   equ -3

; Capability slots. A process gets only what its creator hands it.
CONSOLE     equ 0                   ; the console capability, when given
