; ---------------------------------------------------------------------------
; hello.asm — the first FreyaOS user program.
;
; Prints one line through the console capability in slot 0, then exits with
; whatever write returned: the byte count, or a negative error if it was
; started without the capability.
; ---------------------------------------------------------------------------
bits 64
%include "freya.inc"
org CODE_BASE

    mov eax, SYS_WRITE
    mov edi, CONSOLE
    lea rsi, [rel message]
    mov edx, message_length
    syscall

    mov rdi, rax                ; exit code: what write returned
    mov eax, SYS_EXIT
    syscall
    ud2                         ; not reached: exit never returns

message:        db "Hello from ring 3!", 10
message_length  equ $ - message
