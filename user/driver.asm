; ---------------------------------------------------------------------------
; driver.asm — a user-space driver, in miniature: one job per run.
;
; RDI at entry = mode. Slot 0 (PORTS) holds an I/O-port capability and
; slot 1 (IRQ) an interrupt-line capability, for the roles given them.
; kernel/src/irq.rs's self-test mirrors the modes.
; ---------------------------------------------------------------------------
bits 64
%include "freya.inc"
org CODE_BASE

CMOS_INDEX  equ 0x70
CMOS_DATA   equ 0x71
RTC_SECONDS equ 0x00
RTC_STATUS_A equ 0x0A
KEYBOARD    equ 0x60

start:
    movzx eax, dil
    cmp eax, (table_end - table) / 8
    jae bad
    lea rcx, [rel table]
    jmp [rcx + rax*8]

table:
    dq read_clock               ; 0: read the CMOS clock's seconds
    dq outside_range            ; 1: a port the capability doesn't cover
    dq irq_loop                 ; 2: handle 10 interrupts on the line in slot 1
    dq misuse                   ; 3: wrong capability kinds, bad widths
    dq power_unknown            ; 4: the power switch with an action that doesn't exist
table_end:

POWER_NO_SUCH_ACTION equ 2      ; never valid, so a broken check can't switch anything off

; --- 0: the real-time clock -------------------------------------------------
; Wait while the clock is mid-update (status A bit 7), then read seconds.
read_clock:
    mov r12d, 100000            ; give up eventually
.wait:
    mov edx, RTC_STATUS_A
    call cmos_read
    test eax, 0x80
    jz .ready
    dec r12d
    jnz .wait
.ready:
    mov edx, RTC_SECONDS
    call cmos_read
    mov rdi, rax
    jmp exit

; cmos_read: register EDX -> value in RAX (exits on a refused port access)
cmos_read:
    mov r13, rdx
    mov eax, SYS_PORT_OUT
    mov edi, PORTS
    mov esi, CMOS_INDEX
    mov edx, 1
    mov r10, r13
    syscall
    test rax, rax
    jnz exit_rax
    mov eax, SYS_PORT_IN
    mov edi, PORTS
    mov esi, CMOS_DATA
    mov edx, 1
    syscall
    test rax, rax
    js exit_rax
    ret

; --- 1: outside the range ---------------------------------------------------
outside_range:
    mov eax, SYS_PORT_IN
    mov edi, PORTS
    mov esi, KEYBOARD
    mov edx, 1
    syscall
    jmp exit_rax                ; expect ERR_WRONG_CAPABILITY

; --- 2: interrupt loop ------------------------------------------------------
irq_loop:
    xor r12d, r12d
.next:
    cmp r12d, 10
    jae .done
    mov eax, SYS_IRQ_WAIT
    mov edi, IRQ
    syscall
    test rax, rax
    jnz exit_rax
    mov eax, SYS_IRQ_ACK
    mov edi, IRQ
    syscall
    inc r12d
    jmp .next
.done:
    mov edi, r12d
    jmp exit

; --- 3: misuse --------------------------------------------------------------
misuse:
    mov eax, SYS_IRQ_WAIT       ; wait on a port capability
    mov edi, PORTS
    syscall
    cmp rax, ERR_WRONG_CAPABILITY
    jne bad
    mov eax, SYS_PORT_IN        ; port I/O through an interrupt capability
    mov edi, IRQ
    mov esi, CMOS_DATA
    mov edx, 1
    syscall
    cmp rax, ERR_WRONG_CAPABILITY
    jne bad
    mov eax, SYS_PORT_IN        ; a 3-byte access
    mov edi, PORTS
    mov esi, CMOS_DATA
    mov edx, 3
    syscall
    cmp rax, ERR_BAD_ARGUMENT
    jne bad
    mov eax, SYS_PORT_IN        ; 2 bytes from the last port: runs past the range
    mov edi, PORTS
    mov esi, CMOS_DATA
    mov edx, 2
    syscall
    cmp rax, ERR_WRONG_CAPABILITY
    jne bad
    mov eax, SYS_POWER          ; the power switch through a port capability
    mov edi, PORTS
    mov esi, POWER_NO_SUCH_ACTION
    syscall
    cmp rax, ERR_WRONG_CAPABILITY
    jne bad
    mov eax, SYS_POWER          ; ... and through an empty slot
    mov edi, 9
    mov esi, POWER_NO_SUCH_ACTION
    syscall
    cmp rax, ERR_WRONG_CAPABILITY
    jne bad
    xor edi, edi
    jmp exit

; --- 4: an unknown power action -----------------------------------------------
; Slot 0 holds the Power capability here. The action doesn't exist, so the
; kernel must refuse it as a bad argument (not as a missing capability).
power_unknown:
    mov eax, SYS_POWER
    xor edi, edi
    mov esi, POWER_NO_SUCH_ACTION
    syscall
    jmp exit_rax                ; expect ERR_BAD_ARGUMENT

bad:
    mov edi, 0xBAD
    jmp exit
exit_rax:
    mov rdi, rax
exit:
    mov eax, SYS_EXIT
    syscall
    ud2
