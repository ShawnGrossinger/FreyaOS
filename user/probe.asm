; ---------------------------------------------------------------------------
; probe.asm — the kernel's test program: one thing to try per run.
;
; RDI at entry = mode | value << 8. Modes that end normally exit with a
; result code; the rest are supposed to be killed by a CPU exception.
; The modes are mirrored in kernel/src/process.rs.
; ---------------------------------------------------------------------------
bits 64
%include "freya.inc"
org CODE_BASE

start:
    ; The kernel promises a clean start: every register zero except RDI
    ; (the argument) and RSP. OR them all together and keep the result in
    ; the data page for the `ring` mode to report.
    or rax, rbx
    or rax, rcx
    or rax, rdx
    or rax, rsi
    or rax, rbp
    or rax, r8
    or rax, r9
    or rax, r10
    or rax, r11
    or rax, r12
    or rax, r13
    or rax, r14
    or rax, r15
    por xmm0, xmm1
    por xmm0, xmm2
    por xmm0, xmm3
    por xmm0, xmm4
    por xmm0, xmm5
    por xmm0, xmm6
    por xmm0, xmm7
    por xmm0, xmm8
    por xmm0, xmm9
    por xmm0, xmm10
    por xmm0, xmm11
    por xmm0, xmm12
    por xmm0, xmm13
    por xmm0, xmm14
    por xmm0, xmm15
    movq rcx, xmm0
    or rax, rcx
    punpckhqdq xmm0, xmm0
    movq rcx, xmm0
    or rax, rcx
    mov rcx, DATA_BASE + 8
    mov [rcx], rax

    mov rbx, rdi
    shr rbx, 8                  ; value
    movzx eax, dil              ; mode
    cmp eax, (table_end - table) / 8
    jae bad_mode
    lea rcx, [rel table]
    jmp [rcx + rax*8]

table:
    dq ring                     ;  0
    dq read_kernel              ;  1
    dq read_physmap             ;  2
    dq write_code               ;  3
    dq exec_data                ;  4
    dq do_cli                   ;  5
    dq int_timer_vector         ;  6
    dq port_io                  ;  7
    dq write_kernel_pointer     ;  8
    dq write_past_end           ;  9
    dq bad_syscall              ; 10
    dq spin                     ; 11
    dq isolate                  ; 12
    dq mxcsr                    ; 13
    dq console_scroll           ; 14
table_end:

ring:                           ; exit(CS & 3): 3 in ring 3; 0xD1D if we started dirty
    mov rax, DATA_BASE
    cmp qword [rax + 8], 0
    jne .dirty
    mov ax, cs
    movzx edi, ax
    and edi, 3
    jmp exit
.dirty:
    mov edi, 0xD1D
    jmp exit

read_kernel:                    ; the kernel image, identity-mapped (kernel-only)
    mov rax, [0x100000]
    jmp exit_zero

read_physmap:                   ; the kernel image again, through the physical map
    mov rax, 0xFFFF800000100000
    mov rax, [rax]
    jmp exit_zero

write_code:                     ; our own code is read-only
    mov byte [rel start], 0x90
    jmp exit_zero

exec_data:                      ; our data is no-execute
    mov rax, DATA_BASE
    jmp rax

do_cli:                         ; privileged
    cli
    jmp exit_zero

int_timer_vector:               ; the timer's gate is DPL 0: not ours to call
    int 0x30
    jmp exit_zero

port_io:                        ; no I/O privilege, no I/O permission bitmap
    mov dx, 0x3F8
    in al, dx
    jmp exit_zero

write_kernel_pointer:           ; ask the kernel to print its own memory
    mov eax, SYS_WRITE
    mov edi, CONSOLE
    mov esi, 0x100000
    mov edx, 16
    syscall
    mov rdi, rax
    jmp exit

write_past_end:                 ; a buffer that runs off the end of our data
    mov eax, SYS_WRITE
    mov edi, CONSOLE
    mov rsi, DATA_BASE + DATA_SIZE - 8
    mov edx, 64
    syscall
    mov rdi, rax
    jmp exit

bad_syscall:
    mov eax, 99
    syscall
    mov rdi, rax
    jmp exit

spin:                           ; never yields, never ends; with every SSE
    mov rax, DATA_BASE          ; exception unmasked, so each timer interrupt
    mov dword [rax], 0          ; arrives with a process MXCSR to put aside
    ldmxcsr [rax]
.forever:
    jmp .forever

isolate:                        ; keep `value` everywhere across 50 system calls
    mov rax, DATA_BASE
    mov [rax], rbx
    mov r12, rbx
    lea r13, [rbx + 1]
    lea r14, [rbx + 2]
    lea r15, [rbx + 3]
    lea rbp, [rbx + 4]
    movq xmm0, rbx
    mov r9d, 50
.again:
    mov eax, SYS_YIELD
    syscall
    dec r9d
    jnz .again
    mov rax, DATA_BASE
    cmp [rax], rbx
    jne .changed
    cmp r12, rbx
    jne .changed
    lea rax, [rbx + 1]
    cmp r13, rax
    jne .changed
    lea rax, [rbx + 2]
    cmp r14, rax
    jne .changed
    lea rax, [rbx + 3]
    cmp r15, rax
    jne .changed
    lea rax, [rbx + 4]
    cmp rbp, rax
    jne .changed
    movq rax, xmm0
    cmp rax, rbx
    jne .changed
    mov rdi, rbx
    jmp exit
.changed:
    mov edi, 0xBAD
    jmp exit

mxcsr:                          ; unmask every SSE exception, yield, read it back
    mov rax, DATA_BASE
    mov dword [rax], 0
    ldmxcsr [rax]
    mov r9d, 50
.again:
    mov eax, SYS_YIELD
    syscall
    dec r9d
    jnz .again
    mov rax, DATA_BASE
    stmxcsr [rax]
    mov edi, [rax]
    jmp exit

console_scroll:                 ; console_scroll(slot 0, action `value`): exit with the result
    mov eax, SYS_CONSOLE_SCROLL
    mov edi, CONSOLE
    mov rsi, rbx
    syscall
    mov rdi, rax
    jmp exit

bad_mode:
    mov edi, 0xBADBAD
    jmp exit

exit_zero:                      ; reaching here means the protection failed
    xor edi, edi
exit:
    mov eax, SYS_EXIT
    syscall
    ud2
