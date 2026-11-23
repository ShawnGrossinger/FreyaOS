; ---------------------------------------------------------------------------
; ipc.asm — IPC test program: one role per run.
;
; RDI at entry = mode | value << 8. Every role exits with a result code the
; kernel's self-test checks (kernel/src/ipc.rs mirrors the modes). Slot 1
; (ENDPOINT) holds the endpoint capability the kernel gave it; slot 0 the
; console capability, for the roles that have one.
; ---------------------------------------------------------------------------
bits 64
%include "freya.inc"
org CODE_BASE

QUIT equ 0xFFFF

start:
    mov rbx, rdi
    shr rbx, 8                  ; value
    movzx eax, dil              ; mode
    cmp eax, (table_end - table) / 8
    jae bad_mode
    lea rcx, [rel table]
    jmp [rcx + rax*8]

table:
    dq echo_server              ; 0: answer `value` calls, then exit(count)
    dq client                   ; 1: one call; exit(badge the server saw)
    dq pinger                   ; 2: `value` calls in a row, checking each answer
    dq giver                    ; 3: send our console capability through slot 1
    dq taker                    ; 4: receive a capability, print through it
    dq wrong_caps               ; 5: misuse capabilities; exit(0) if all refused
    dq dying_server             ; 6: take one call and end without answering
    dq log_server               ; 7: print what clients send, for them
    dq log_client               ; 8: no console of its own; print via the server
    dq receiver                 ; 9: wait on slot 1 forever (to be killed)
    dq sender                   ; 10: one plain send; exit(result)
    dq collector                ; 11: receive `value` messages; exit(their badges, in order)
    dq patient_server           ; 12: take a call, wait for word on slot 2, then answer
    dq signalled                ; 13: receive two signals; exit(their bits)
table_end:

; --- 0: echo server ---------------------------------------------------------
; Each answer: words + 1, and the badge of the caller in word 3.
echo_server:
    xor r12d, r12d              ; calls answered
.next:
    cmp r12, rbx
    jae .done
    mov eax, SYS_RECV
    mov edi, ENDPOINT
    syscall
    test rax, rax
    jnz exit_rax
    inc rsi
    inc rdx
    inc r10
    mov r8, rdi                 ; the caller's badge
    mov r9, NO_CAP
    mov eax, SYS_REPLY
    syscall
    test rax, rax
    jnz exit_rax
    inc r12
    jmp .next
.done:
    mov rdi, r12
    jmp exit

; --- 1: client --------------------------------------------------------------
client:
    mov eax, SYS_CALL
    mov edi, ENDPOINT
    mov esi, 10
    mov edx, 20
    mov r10d, 30
    xor r8d, r8d
    mov r9, NO_CAP
    syscall
    test rax, rax
    jnz exit_rax                ; e.g. ERR_PEER_GONE
    cmp rsi, 11
    jne bad
    cmp rdx, 21
    jne bad
    cmp r10, 31
    jne bad
    mov rdi, r8                 ; the badge the server saw: ours
    jmp exit

; --- 2: pinger --------------------------------------------------------------
pinger:
    xor r12d, r12d
.next:
    cmp r12, rbx
    jae .done
    mov eax, SYS_CALL
    mov edi, ENDPOINT
    mov rsi, r12
    xor edx, edx
    xor r10d, r10d
    xor r8d, r8d
    mov r9, NO_CAP
    syscall
    test rax, rax
    jnz exit_rax
    lea rax, [r12 + 1]
    cmp rsi, rax                ; answered in order, each exactly once
    jne bad
    inc r12
    jmp .next
.done:
    mov rdi, r12
    jmp exit

; --- 3: giver ---------------------------------------------------------------
giver:
    mov eax, SYS_SEND
    mov edi, ENDPOINT
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    xor r8d, r8d
    mov r9d, CONSOLE            ; pass the console capability along
    syscall
    jmp exit_rax

; --- 4: taker ---------------------------------------------------------------
; Born without a console; prints once one arrives in a message.
taker:
    mov eax, SYS_WRITE          ; first, prove we can't print yet
    mov edi, CONSOLE
    lea rsi, [rel given_text]
    mov edx, given_length
    syscall
    cmp rax, ERR_NO_CAPABILITY
    jne bad
    mov eax, SYS_RECV
    mov edi, ENDPOINT
    syscall
    test rax, rax
    jnz exit_rax
    cmp r9, NO_CAP
    je bad
    mov rdi, r9                 ; the slot the capability landed in
    mov eax, SYS_WRITE
    lea rsi, [rel given_text]
    mov edx, given_length
    syscall
    jmp exit_rax                ; bytes written

; --- 5: wrong capabilities --------------------------------------------------
; Slot 1 is send-only; slot 0 is the console. Each call must be refused.
wrong_caps:
    mov eax, SYS_RECV           ; receive with a send-only capability
    mov edi, ENDPOINT
    syscall
    cmp rax, ERR_WRONG_CAPABILITY
    jne bad
    mov eax, SYS_SEND           ; send through something that isn't an endpoint
    mov edi, CONSOLE
    mov r9, NO_CAP
    syscall
    cmp rax, ERR_WRONG_CAPABILITY
    jne bad
    mov eax, SYS_SEND           ; send through an empty slot
    mov edi, 7
    mov r9, NO_CAP
    syscall
    cmp rax, ERR_WRONG_CAPABILITY
    jne bad
    mov eax, SYS_REPLY          ; answer a call nobody made
    mov r9, NO_CAP
    syscall
    cmp rax, ERR_NO_REPLY
    jne bad
    xor edi, edi
    jmp exit

; --- 6: dying server --------------------------------------------------------
dying_server:
    mov eax, SYS_RECV
    mov edi, ENDPOINT
    syscall
    xor edi, edi
    jmp exit                    ; the caller is still waiting for an answer

; --- 7: log server ----------------------------------------------------------
; Prints the 32 bytes each client sends (4 words), answers, repeats `value`
; times.
log_server:
    xor r12d, r12d
.next:
    cmp r12, rbx
    jae .done
    mov eax, SYS_RECV
    mov edi, ENDPOINT
    syscall
    test rax, rax
    jnz exit_rax
    mov rax, DATA_BASE
    mov [rax], rsi
    mov [rax + 8], rdx
    mov [rax + 16], r10
    mov [rax + 24], r8
    mov eax, SYS_WRITE
    mov edi, CONSOLE
    mov rsi, DATA_BASE
    mov edx, 32
    syscall
    mov rsi, rax                ; answer: what write returned
    mov eax, SYS_REPLY
    mov r9, NO_CAP
    syscall
    inc r12
    jmp .next
.done:
    mov rdi, r12
    jmp exit

; --- 8: log client ----------------------------------------------------------
log_client:
    mov eax, SYS_WRITE          ; no console of our own
    mov edi, CONSOLE
    lea rsi, [rel log_text]
    mov edx, 32
    syscall
    cmp rax, ERR_NO_CAPABILITY
    jne bad
    mov eax, SYS_CALL           ; so ask the server, which has one
    mov edi, ENDPOINT
    mov rsi, [rel log_text]
    mov rdx, [rel log_text + 8]
    mov r10, [rel log_text + 16]
    mov r8, [rel log_text + 24]
    mov r9, NO_CAP
    syscall
    test rax, rax
    jnz exit_rax
    mov rdi, rsi                ; bytes the server printed for us
    jmp exit

; --- 9: receiver ------------------------------------------------------------
receiver:
    mov eax, SYS_RECV
    mov edi, ENDPOINT
    syscall
    jmp exit_rax                ; only reached if killed... which never returns

; --- 10: sender -------------------------------------------------------------
sender:
    mov eax, SYS_SEND
    mov edi, ENDPOINT
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    xor r8d, r8d
    mov r9, NO_CAP
    syscall
    jmp exit_rax

; --- 11: collector ----------------------------------------------------------
; Receives `value` messages and exits with their badges' low bytes packed
; first-to-last: badges 1, 2, 3 in that order give 0x010203.
collector:
    xor r12d, r12d              ; messages so far
    xor r13d, r13d              ; packed badges
.next:
    cmp r12, rbx
    jae .done
    mov eax, SYS_RECV
    mov edi, ENDPOINT
    syscall
    test rax, rax
    jnz exit_rax
    shl r13, 8
    movzx edi, dil
    or r13, rdi
    inc r12
    jmp .next
.done:
    mov rdi, r13
    jmp exit

; --- 12: patient server ----------------------------------------------------
; Takes a call on slot 1, then waits for a plain message on slot 2 before
; answering, so the kernel can kill the caller in between. exit(reply result).
patient_server:
    mov eax, SYS_RECV
    mov edi, ENDPOINT
    syscall
    test rax, rax
    jnz exit_rax
    mov eax, SYS_RECV
    mov edi, 2
    syscall
    test rax, rax
    jnz exit_rax
    mov eax, SYS_REPLY
    mov r9, NO_CAP
    syscall
    jmp exit_rax                ; ERR_PEER_GONE if the caller is gone

; --- 13: signalled (Entry 20) ------------------------------------------------
; Two receives on slot 1, both of which must be signals from the kernel
; (badge -1); exits with the first one's bits | the second one's << 8.
signalled:
    call .one
    mov r12, rsi
    call .one
    shl rsi, 8
    or r12, rsi
    mov rdi, r12
    jmp exit
.one:
    mov eax, SYS_RECV
    mov edi, ENDPOINT
    syscall
    test rax, rax
    jnz exit_rax
    cmp rdi, -1
    jne bad
    ret

bad_mode:
bad:
    mov edi, 0xBAD
    jmp exit
exit_rax:
    mov rdi, rax
exit:
    mov eax, SYS_EXIT
    syscall
    ud2

given_text:     db "I was given the console in a message", 10
given_length    equ $ - given_text
log_text:       db "printed for me by the log server"     ; exactly 32 bytes
