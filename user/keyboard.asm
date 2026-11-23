; ---------------------------------------------------------------------------
; keyboard.asm — the PS/2 keyboard driver, as a user process.
;
; Capabilities init gives it (and all it can touch; the kernel gave them
; until Entry 21):
;   slot 0  Console        to print what is typed
;   slot 1  Irq line 1     the keyboard interrupt
;   slot 2  IoPorts 0x60   the controller's data port
;   slot 3  IoPorts 0x64   the controller's status port
;   slot 4  Power          to restart or switch off
;   slot 5  Endpoint       the console driver's, to send: PageUp and PageDown
;                          are calls to it (Entry 20; before, a system call)
;
; The firmware has already set the controller up (keyboard on, interrupt
; on, scancode set 1 through translation), as every PC BIOS does, so this
; mostly only reads. The one command it sends switches the mouse port off
; (0xA7): nothing handles the TrackPoint or touchpad yet, and the controller
; has a single output buffer, so an unread mouse byte would sit there and
; hold back every keystroke behind it. Each interrupt: read every byte
; waiting, then ack the line.
;
; The keyboard's line is edge-triggered: the controller raises it when a
; byte arrives and keeps it up until the byte is read. An edge that comes
; while the line is masked is lost, and the raised line then never makes
; another. So the line is armed (irq_ack) before the controller is emptied,
; never after: a key held down through boot sends a byte every tenth of a
; second, and one landing between the two left the keyboard dead (Entry 19).
;
;   Ctrl+Alt+Del   restart
;   Ctrl+Alt+End   switch off
;   PageUp/Down    scroll the screen's log back / forward a page
;   typing + Enter prints the line ("typed: ..."), back at the newest line
; ---------------------------------------------------------------------------
bits 64
%include "freya.inc"
org CODE_BASE

KEYBOARD_IRQ equ 1              ; capability slots
DATA_CAP     equ 2
STATUS_CAP   equ 3
POWER        equ 4
CONSOLE_EP   equ 5

DATA_PORT    equ 0x60
STATUS_PORT  equ 0x64
OUTPUT_FULL  equ 0x01           ; status: a byte is waiting at the data port
INPUT_FULL   equ 0x02           ; status: the controller hasn't taken the last command yet
DISABLE_MOUSE equ 0xA7          ; controller command: switch the second (mouse) port off
FROM_MOUSE   equ 0x20           ; status: that byte came from the mouse

CTRL         equ 1              ; R15 flags
ALT          equ 2
SHIFT        equ 4
EXTENDED     equ 8              ; the last byte was the 0xE0 prefix

PREFIX_LEN   equ 7              ; "typed: "
MAX_LINE     equ 64
LINE         equ DATA_BASE      ; "typed: " + the line being typed

start:
    xor r15d, r15d              ; modifier flags
    xor r14d, r14d              ; characters in the line
    mov rax, DATA_BASE
    mov rcx, 'typed: '
    mov [rax], rcx

    mov r12d, 100000            ; wait (bounded) until the controller takes commands
.idle:
    call read_status
    test al, INPUT_FULL
    jz .send
    dec r12d
    jnz .idle
.send:
    mov eax, SYS_PORT_OUT       ; port 0x64 written is the command port
    mov edi, STATUS_CAP
    mov esi, STATUS_PORT
    mov edx, 1
    mov r10d, DISABLE_MOUSE
    syscall
    test rax, rax
    jnz fail

    mov eax, SYS_IRQ_ACK        ; arm the line first (see the top)
    mov edi, KEYBOARD_IRQ
    syscall
    test rax, rax
    jnz fail

    mov r12d, 64                ; then throw away anything left from the firmware
.flush:
    call read_status
    test al, OUTPUT_FULL
    jz .flushed
    call read_data
    dec r12d
    jnz .flush
.flushed:
    mov eax, SYS_WRITE
    mov edi, CONSOLE
    lea rsi, [rel ready_text]
    mov edx, ready_length
    syscall

wait_for_key:
    mov eax, SYS_IRQ_WAIT
    mov edi, KEYBOARD_IRQ
    syscall
    test rax, rax
    jnz fail
.next_byte:
    call read_status
    test al, OUTPUT_FULL
    jz .done
    mov bl, al
    call read_data
    test bl, FROM_MOUSE
    jnz .next_byte              ; not ours
    movzx edi, al
    call handle_scancode
    jmp .next_byte
.done:
    mov eax, SYS_IRQ_ACK
    mov edi, KEYBOARD_IRQ
    syscall
    jmp wait_for_key

; --- console_scroll: ESI = action. A call to the console driver; if there
; is none (it ended, or never started) the call fails at once with
; peer-gone (ipc.rs) and nothing happens. Changes RDI, RSI, RDX, R8-R10.
console_scroll:
    mov edx, esi                ; words: the request, then the action
    mov esi, CONSOLE_SCROLL
    xor r10d, r10d
    xor r8d, r8d
    mov r9, NO_CAP
    mov eax, SYS_CALL
    mov edi, CONSOLE_EP
    syscall
    ret

; --- port access (each a system call through a port capability) ---------
read_status:
    mov eax, SYS_PORT_IN
    mov edi, STATUS_CAP
    mov esi, STATUS_PORT
    mov edx, 1
    syscall
    test rax, rax
    js fail
    ret

read_data:
    mov eax, SYS_PORT_IN
    mov edi, DATA_CAP
    mov esi, DATA_PORT
    mov edx, 1
    syscall
    test rax, rax
    js fail
    ret

; --- handle_scancode: EDI = one byte of scancode set 1 --------------------
handle_scancode:
    cmp edi, 0xE0
    jne .not_prefix
    or r15d, EXTENDED
    ret
.not_prefix:
    mov eax, edi
    and eax, 0x7F               ; the key
    mov ecx, edi
    and ecx, 0x80               ; set on release

    cmp eax, 0x1D               ; Ctrl (left, or right with E0)
    jne .not_ctrl
    mov edx, CTRL
    jmp .modifier
.not_ctrl:
    cmp eax, 0x38               ; Alt (left, or right with E0)
    jne .not_alt
    mov edx, ALT
    jmp .modifier
.not_alt:
    test r15d, EXTENDED         ; E0 2A / E0 36 are fake shifts (Print Screen)
    jnz .not_shift
    cmp eax, 0x2A
    je .shift
    cmp eax, 0x36
    jne .not_shift
.shift:
    mov edx, SHIFT
.modifier:
    test ecx, ecx
    jnz .modifier_up
    or r15d, edx
    jmp .handled
.modifier_up:
    not edx
    and r15d, edx
    jmp .handled

.not_shift:
    test ecx, ecx
    jnz .handled                ; other releases don't matter

    mov edx, r15d
    and edx, CTRL | ALT
    cmp edx, CTRL | ALT
    jne .plain
    cmp eax, 0x53               ; Del (the Delete key or the keypad's)
    je .restart
    cmp eax, 0x4F               ; End
    je .shut_down
.plain:
    cmp eax, 0x49               ; PageUp (E0 49; 49 alone is the keypad's 9)
    je .page_up
    cmp eax, 0x51               ; PageDown (E0 51; the keypad's 3)
    je .page_down
    test r15d, EXTENDED
    jnz .handled                ; arrows and the like: nothing to print
    cmp eax, 0x1C
    je .enter
    cmp eax, 0x0E
    je .backspace
    cmp eax, (keys_end - keys)
    jae .handled
    lea rdx, [rel keys]
    test r15d, SHIFT
    jz .unshifted
    lea rdx, [rel shifted_keys]
.unshifted:
    movzx edx, byte [rdx + rax]
    test edx, edx
    jz .handled
    cmp r14d, MAX_LINE
    jae .handled
    mov rcx, LINE + PREFIX_LEN
    mov [rcx + r14], dl
    inc r14d
    jmp .handled

.backspace:
    test r14d, r14d
    jz .handled
    dec r14d
    jmp .handled

.enter:
    test r14d, r14d
    jz .handled
    mov esi, SCROLL_TO_END      ; back to the newest line, where this lands
    call console_scroll
    mov eax, SYS_WRITE
    mov edi, CONSOLE
    mov rsi, LINE
    lea edx, [r14d + PREFIX_LEN]
    syscall
    xor r14d, r14d
    jmp .handled

.page_up:
    mov esi, SCROLL_BACK
    call console_scroll
    jmp .handled
.page_down:
    mov esi, SCROLL_FORWARD
    call console_scroll
    jmp .handled

.restart:
    mov esi, POWER_RESTART
    jmp .power
.shut_down:
    mov esi, POWER_SHUTDOWN
.power:
    mov eax, SYS_POWER
    mov edi, POWER
    syscall                     ; only returns if refused
    mov eax, SYS_WRITE
    mov edi, CONSOLE
    lea rsi, [rel refused_text]
    mov edx, refused_length
    syscall

.handled:
    and r15d, ~EXTENDED
    ret

fail:
    mov rdi, rax
    mov eax, SYS_EXIT
    syscall
    ud2

ready_text:     db "keyboard ready: Ctrl+Alt+Del restart, Ctrl+Alt+End switch off, PgUp/PgDn scroll", 10
ready_length    equ $ - ready_text
refused_text:   db "power request refused", 10
refused_length  equ $ - refused_text

; Scancode set 1 to ASCII, US layout, keys 0x00-0x39 (0 = nothing to print).
keys:
    db 0, 0, '1234567890-=', 0, 0
    db 'qwertyuiop[]', 0, 0
    db "asdfghjkl;'`", 0, '\'
    db 'zxcvbnm,./', 0, '*', 0, ' '
keys_end:
shifted_keys:
    db 0, 0, '!@#$%^&*()_+', 0, 0
    db 'QWERTYUIOP{}', 0, 0
    db 'ASDFGHJKL:"~', 0, '|'
    db 'ZXCVBNM<>?', 0, '*', 0, ' '
