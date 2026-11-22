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
SYS_EXIT     equ 0                  ; exit(code): never returns
SYS_YIELD    equ 1                  ; yield(): give up the rest of this time slice
SYS_WRITE    equ 2                  ; write(cap, ptr, len): bytes written
SYS_SEND     equ 3                  ; send(ep, w0, w1, w2, w3, xfer)
SYS_RECV     equ 4                  ; recv(ep): badge in RDI, words in RSI RDX R10 R8, cap slot in R9
SYS_CALL     equ 5                  ; call(ep, w0, w1, w2, w3, xfer): reply like recv
SYS_REPLY    equ 6                  ; reply(-, w0, w1, w2, w3, xfer)
SYS_PORT_IN  equ 7                  ; port_in(ports, port, width): value
SYS_PORT_OUT equ 8                  ; port_out(ports, port, width, value)
SYS_IRQ_WAIT equ 9                  ; irq_wait(irq): returns once the line has fired
SYS_IRQ_ACK  equ 10                 ; irq_ack(irq)
SYS_POWER    equ 11                 ; power(cap, action): never returns if allowed

POWER_RESTART  equ 0
POWER_SHUTDOWN equ 1

; Message words go in RSI, RDX, R10, R8; R9 = slot of a capability to pass
; along, or NO_CAP.
NO_CAP       equ -1

ERR_BAD_SYSCALL      equ -1
ERR_NO_CAPABILITY    equ -2
ERR_BAD_ADDRESS      equ -3
ERR_WRONG_CAPABILITY equ -4
ERR_NO_GRANT         equ -5
ERR_PEER_GONE        equ -6
ERR_INTERRUPTED      equ -7
ERR_NO_REPLY         equ -8
ERR_BAD_ARGUMENT     equ -9

; Capability slots. A process gets only what its creator hands it; the
; kernel's tests use these slots.
CONSOLE     equ 0                   ; the console capability, when given
ENDPOINT    equ 1                   ; an endpoint capability, when given (ipc.asm)
PORTS       equ 0                   ; I/O ports (driver.asm)
IRQ         equ 1                   ; an interrupt line (driver.asm)
