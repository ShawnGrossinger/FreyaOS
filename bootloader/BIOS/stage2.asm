; ---------------------------------------------------------------------------------------------
; FreyjaOS - BIOS Stage 2 Bootloader (real -> protected -> long mode)
;
; Responsibilities:
; - (Real mode) enable A20
; - (Real mode) load kernel from disk into 0x0010_0000 (1 MiB) using INT 13h extensions
; - enter 32-bit protected mode
; - set up minimal IA-32e paging (identity-map first 2 MiB using a single 2 MiB page)
; - enable long mode and jump to the kernel entry at 0x0010_0000
;
; Build (from build/):
;   nasm -f bin -D__KERNEL_LBA=<lba> -D__KERNEL_SECTORS=<n> ../bootloader/BIOS/stage2.asm -o out/stage2.bin
;
; Notes:
; - This stage assumes Stage1 already verified INT 13h extensions exist.
; - Kernel is a flat binary linked for 0x0010_0000 (see kernel/kernel.ld).
; ---------------------------------------------------------------------------------------------

[org 0x7E00]
BITS 16

%ifndef __KERNEL_LBA
    %define __KERNEL_LBA 0
%endif
%ifndef __KERNEL_SECTORS
    %define __KERNEL_SECTORS 0
%endif

KERNEL_ENTRY      equ 0x00100000

; BIOS-friendly bounce buffer (< 1 MiB)
; We read the kernel here in real mode, then copy it to 0x0010_0000 in 32-bit PM.
KERNEL_TMP_ADDR   equ 0x00010000
KERNEL_TMP_SEG    equ 0x1000
KERNEL_TMP_OFF    equ 0x0000

PML4_ADDR         equ 0x00200000
PDPT_ADDR         equ 0x00201000
PD_ADDR           equ 0x00202000

; Kernel stack: 64KB region ending just below the trampoline.
; Stack grows downward, so the top (initial RSP) is at 0x0008_0000).
; This gives 64KB of stack: [0x0007_0000, 0x0008_0000).
KERNEL_STACK_TOP equ 0x00080000

COM1_BASE         equ 0x03F8

; --- Stage 2 entry ---
start:

    cli

    ; Init serial early (QEMU: -serial stdio)
    call serial_init

    ; Keep DS=0 so our labels (org 0x7E00) are direct physical addresses.
    xor ax, ax
    mov ds, ax
    mov es, ax

    mov [boot_drive], dl

    ; --- Debug ---
    mov si, msg_stage2
    call print_string
    mov si, msg_stage2_serial
    call serial_write_string

    ; ------------------
    ; --- Enable A20 ---
    ; ------------------

    ; --- Method 1 - Ask BIOS (INT 15h AX=2401h)
    mov ax, 0x2401
    int 0x15
    jnc .a20_check  ; carry clear = BIOS said OK. Still verify below

    ; --- Method 2 - Enable A20 (fast A20 via port 0x92) ---
    in   al, 0x92
    or   al, 0x02
    and  al, 0xFE   ; bit 0 = reset line, keep it clear
    out  0x92, al

.a20_check:
    ; Verify A20 is actually on:
    ; Write 0xAA to 0x0000:0x0500, write 0xBB to 0xFFFF:0x0510
    ; (wraps to 0x0000:0x0500 if A20 is off).
    mov ax, 0xFFFF
    mov es, ax
    mov word [ds:0x0500], 0x00AA
    mov word [es:0x0510], 0x00BB
    cmp word [ds:0x0500], 0x00BB  ; did the write wrap?
    jne .a20_ok                   ; no wrap -> A20 is on
    mov si, msg_a20_fail
    call print_string
    jmp hang
.a20_ok:
    xor ax, ax                    ; restore ES = 0
    mov es, ax

    ; --- Sanity: kernel constants must be patched ---
    ; (Runtime check instead of a NASM preprocessor comparison.)
    mov ax, __KERNEL_SECTORS
    test ax, ax
    jnz .kernel_constants_ok
    mov si, msg_kernel_not_patched
    call print_string
    mov si, msg_kernel_not_patched_serial
    call serial_write_string
    jmp hang
.kernel_constants_ok:

    ; Real-mode BIOS DAP buffer uses a 16:16 pointer. Starting at 0x0010_0000
    ; (0xFFFF:0x0010), we can only address ~64KiB-16 bytes in real mode.
    ; Keep this revival loader simple: cap the kernel to 127 sectors (~63.5KiB).
    cmp ax, 127
    jbe .kernel_size_ok
    mov si, msg_kernel_too_big
    call print_string
    mov si, msg_kernel_too_big_serial
    call serial_write_string
    jmp hang
.kernel_size_ok:

    ; --- Load kernel to 0x0010_0000 using INT 13h AH=42h ---
    mov si, kernel_dap
    mov word [si + 2], __KERNEL_SECTORS
    mov word [si + 4], KERNEL_TMP_OFF
    mov word [si + 6], KERNEL_TMP_SEG
    mov dword [si + 8], __KERNEL_LBA
    mov dword [si + 12], 0

    mov ah, 0x42
    mov dl, [boot_drive]
    int 0x13
    jnc .read_ok

    ; Print AH error code over serial before halting
    push ax
    mov si, msg_read_err_code
    call serial_write_string
    pop ax
    mov al, ah    ; AH - BIOS error code
    call serial_write_hex_byte
    mov al, 0x0A
    call serial_putc
    jmp hang

.read_ok:

    ; --- Optional debug ---
    mov si, msg_kernel_loaded
    call print_string
    mov si, msg_kernel_loaded_serial
    call serial_write_string

    ; --- Collect E820 memory map (must be done in real mode) ---
    ; Entries stared at physical 0x504 (24 bytes each, max 32).
    ; Entry count stored at 0x0500 as a 16-bit word.
    call get_e820_map

    ; --- Set up GDT and enter protected mode ---
    lgdt [gdt_descriptor]

    mov eax, cr0
    or  eax, 0x1   ; CR0.PE
    mov cr0, eax

    jmp 0x08:pm_entry     ; far jump to flush pipeline

; ---------------------------------------------------------------------------------------------
; 32-bit Protected Mode
; ---------------------------------------------------------------------------------------------
[BITS 32]
pm_entry:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax

    mov esp, 0x00090000   ; simple stack below 1 MiB

    ; Copy kernel from bounce buffer to final 1 MiB destination
    ; (We do this before enabling paging/long mode.)
    cld
    mov esi, KERNEL_TMP_ADDR
    mov edi, KERNEL_ENTRY
    mov ecx, (__KERNEL_SECTORS * 512) / 4
    rep movsd

    ; --- Build minimal page tables for long mode ---
    ; Identity-map first 2 MiB using a single 2 MiB page (PDE.PS=1)

    ; Zero 3 pages (PML4, PDPT, PD)
    mov edi, PML4_ADDR
    xor eax, eax
    mov ecx, (3*4096)/4
    rep stosd

    ; PML4[0] -> PDPT
    mov dword [PML4_ADDR + 0], PDPT_ADDR | 0x03
    mov dword [PML4_ADDR + 4], 0

    ; PDPT[0] -> PD
    mov dword [PDPT_ADDR + 0], PD_ADDR | 0x03
    mov dword [PDPT_ADDR + 4], 0

    ; PD[0..3] -> identity-map first 8 MiB using 2 MiB pages (PDE.PS=1)
    ; This covers: kernel@1MB, bitmap@3MB, metadata@4MB+
    ; Flags: P=1, RW=1, PS=1 => 0x83 (each entry adds 0x200000 to base)
    mov dword [PD_ADDR + 0], 0x00000083   ; [0..2MB)
    mov dword [PD_ADDR + 4], 0

    mov dword [PD_ADDR + 8], 0x00200083   ; [2..4MB)
    mov dword [PD_ADDR + 12], 0

    mov dword [PD_ADDR + 16], 0x00400083  ; [4..6MB)
    mov dword [PD_ADDR +20], 0

    mov dword [PD_ADDR + 24], 0x00600083  ; [6..8MB)
    mov dword [PD_ADDR + 28], 0

    ; Point CR3 at the PML4. Must happen before CR0.PG is set: the very
    ; next instruction fetch after enabling paging walks from CR3.
    ; (mov cr3 only accepts a register, not an immediate.)
    mov eax, PML4_ADDR
    mov cr3, eax

    ; Enable PAE + SSE support in CR4.
    ;   PAE        (bit 5)  — required for IA-32e paging
    ;   OSFXSR     (bit 9)  — required for SSE (FXSAVE/FXRSTOR)
    ;   OSXMMEXCPT (bit 10) — required for unmasked SIMD exceptions (#XM)
    ; Done here in 32-bit PM so SSE is available from the very first
    ; 64-bit instruction.  The Rust target spec declares +sse,+sse2,
    ; so LLVM freely emits movaps/xorps/etc.; without OSFXSR those
    ; raise #UD → triple fault with no IDT.
    mov eax, cr4
    or  eax, (1 << 5) | (1 << 9) | (1 << 10)
    mov cr4, eax

    ; Enable Long Mode (IA32_EFER.LME)
    mov ecx, 0xC0000080   ; IA32_EFER
    rdmsr
    or  eax, (1 << 8)
    wrmsr

    ; Kernel stack: top sits at KERNEL_STACK_TOP, grows downward.
    ; The PD above identity-maps [0, 8 MiB) with four 2 MiB pages, so
    ; the entire stack region [0x0007_0000, 0x0008_0000) is covered.
    ; If you ever shrink the identity map or move KERNEL_STACK_TOP
    ; above 8 MiB, you must extend the PD entries to match.
    KERNEL_STACK_TOP equ 0x00080000
    mov esp, KERNEL_STACK_TOP

    ; Enable paging + configure CR0 for SSE.
    ;   PG (bit 31) — enable paging; activates LMA since LME is set
    ;   MP (bit 1)  — monitor coprocessor (recommended with SSE)
    ;   EM (bit 2)  — must be CLEAR for SSE
    ; PE + ET are already set from entering protected mode.
    mov eax, cr0
    and eax, 0xFFFFFFFB        ; clear EM (bit 2)
    or  eax, (1 << 1) | (1 << 31)  ; set MP + PG
    mov cr0, eax

    ; Now in compatibility mode (32-bit code, but LMA is active).

    ; Far jump into 64-bit code. Loading CS with the 64-bit selector is
    ; what actually leaves compatibility mode. We land on a short 64-bit
    ; trampoline (below) rather than the kernel itself so the data
    ; segments and the full 64-bit RSP are set before any kernel code
    ; runs.
    jmp 0x18:trampoline_64

; ---------------------------------------------------------------------------
; 64-bit trampoline — entered immediately after the far jump.
; Runs in stage2's low-memory address range (~0x8xxx), well within the
; identity-mapped [0, 8 MiB) window. Reloads data segments, sets the
; 64-bit stack pointer, and does a normal near jump to the kernel.
; ---------------------------------------------------------------------------
[BITS 64]
trampoline_64:
    ; Reload data segments with the 64-bit data selector (0x20).
    ; After the far jump, CS=0x18 (64-bit code) is correct, but
    ; DS/ES/SS still hold the 32-bit selector 0x10 from pm_entry.
    mov ax, 0x20
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax

    ; Set 64-bit stack pointer (ESP was set above, but RSP's upper
    ; 32 bits may contain garbage after the mode switch).
    mov rsp, KERNEL_STACK_TOP

    ; Near jump to the kernel entry point.  This starts a clean
    ; translation block at 0x100000 with no mode-transition baggage.
    jmp KERNEL_ENTRY

; ---------------------------------------------------------------------------------------------
; Real-mode helpers / errors
; ---------------------------------------------------------------------------------------------
[BITS 16]

kernel_read_error:
    mov si, msg_kernel_read_error
    call print_string
    mov si, msg_kernel_read_error_serial
    call serial_write_string
    jmp hang

hang:
    cli
.hlt:
    hlt
    jmp .hlt

; print_string: prints a 0-terminated string pointed to by DS:SI (real mode only)
print_string:
    pusha
.next:
    lodsb
    test al, al
    jz .done
    mov ah, 0x0E
    mov bh, 0x00
    mov bl, 0x07
    int 0x10
    jmp .next
.done:
    popa
    ret

; serial_putc: write AL to COM1
serial_putc:
    push dx
    mov ah, al
.wait:
    mov dx, COM1_BASE + 5
    in al, dx
    test al, 0x20
    jz .wait
    mov dx, COM1_BASE
    mov al, ah
    out dx, al
    pop dx
    ret

; serial_write_string: prints 0-terminated string DS:SI
serial_write_string:
    pusha
.next:
    lodsb
    test al, al
    jz .done
    cmp al, 0x0A
    jne .emit
    ; LF -> CRLF for nicer terminal output
    mov al, 0x0D
    call serial_putc
    mov al, 0x0A
.emit:
    call serial_putc
    jmp .next
.done:
    popa
    ret

; serial_write_hex_byte: write AL as two hex digits to COM1
serial_write_hex_byte:
    push ax
    mov  ah, al
    shr  al, 4
    and  al, 0x0F
    add  al, '0'
    cmp  al, '9'
    jbe  .hi_ok
    add  al, 7
.hi_ok:
    call serial_putc
    mov  al, ah
    and  al, 0x0F
    add  al, '0'
    cmp  al, '9'
    jbe  .lo_ok
    add  al, 7
.lo_ok:
    call serial_putc
    pop  ax
    ret

; -------------------------
; Serial helpers (COM1)
; -------------------------

; serial_init: configure COM1 for 38400 8N1
serial_init:
    pusha
    mov dx, COM1_BASE + 1
    mov al, 0x00
    out dx, al

    mov dx, COM1_BASE + 3
    mov al, 0x80
    out dx, al

    mov dx, COM1_BASE + 0
    mov al, 0x03          ; divisor low (115200/3 + 38400)
    out dx, al

    mov dx, COM1_BASE + 1
    mov al, 0x00          ; divisor high
    out dx, al

    mov dx, COM1_BASE + 3
    mov al, 0x03          ; 8 bits, no parity, 1 stop
    out dx, al

    mov dx, COM1_BASE + 2
    mov al, 0xC7          ; enable FIFO, clear, 14-byte threshold
    out dx, al

    mov dx, COM1_BASE + 4
    mov al, 0x0B          ; IRQs enabled, RTS/DSR   
    out dx, al

    popa
    ret

; ---------------------------------------------------------------------------------------------
; get_e820_map
; Collect BIOS E820 memory map into physical 0x0504.
; 0x0500 (dword) = entry count on return.
; Each entry is 24 bytes: base(8) + length(8) + type(4) + ACPI(4).
; Max 32 entries. Stops early if BIOS returns carry or count exceeds limit.
; ---------------------------------------------------------------------------------------------
E820_BUFFER equ 0x0504
E820_MAX    equ 32
E820_MAGIC  equ 0x534D4150  ; "SMAP"

get_e820_map:
    pusha
    mov di,  E820_BUFFER     ; ES:DI -> buffer (ES is already 0)
    xor ebx, ebx             ; continuation = 0 (start of list)
    xor bp,  bp              ; entry count

.next_entry:
    mov eax, 0xE820          ; function code in EAX - this is what the BIOS reads
    mov edx, E820_MAGIC      ; "SMAP" signature in EDX
    mov ecx, 24
    int 0x15
    jc  .done                ; carry set -> list exhausted or unsupported

    cmp eax, E820_MAGIC      ; BIOS must return "SMAP" in EAX
    jne .fail

    test ecx, ecx            ; entry length 0 -> skip
    jz   .skip

    inc bp
    add di, 24

.skip:
    test ebx, ebx            ; EBX=0 -> last entry
    jz   .done
    cmp  bp, E820_MAX
    jb   .next_entry         ; room for more

.done:
    mov  [0x0500], bp        ; store count at 0x0500
    cmp  bp, 0
    je   .no_e820
    mov  si, msg_e820_ok
    call print_string
    mov  si, msg_e820_ok_serial
    call serial_write_string
    popa
    ret

.no_e820:
.fail:
    mov  word [0x0500], 0    ; zero count signals failure to kernel
    mov  si, msg_e820_fail
    call print_string
    mov  si, msg_e820_fail_serial
    call serial_write_string
    popa
    ret

; ---------------------------------------------------------------------------------------------


; ---------------------------------------------------------------------------------------------
; Data
; ---------------------------------------------------------------------------------------------

boot_drive db 0

msg_stage2 db "Stage 2: OK", 0x0D, 0x0A, 0
msg_kernel_loaded db "Kernel loaded", 0x0D, 0x0A, 0
msg_kernel_not_patched db "Kernel constants not patched", 0x0D, 0x0A, 0
msg_kernel_read_error db "Kernel read failed", 0x0D, 0x0A, 0
msg_kernel_too_big db "Kernel too large (max 127 sectors)", 0x0D, 0x0A, 0
msg_a20_fail db "A20 enable failed - halting", 0x0D, 0x0A, 0
msg_e820_ok db "E820 map collected", 0x0D, 0x0A, 0
msg_e820_fail db "E820 not supported", 0x0D, 0x0A, 0

; Serial versions (LF is 0x0A; serial_write_string canverts it to CRLF)
msg_stage2_serial db "[stage2] Stage 2: OK", 0x0A, 0
msg_kernel_loaded_serial db "[stage2] Kernel loaded", 0x0A, 0
msg_kernel_not_patched_serial db "[stage2] ERROR: kernel constants not patched", 0x0A, 0
msg_kernel_read_error_serial db "[stage2] ERROR: kernel read failed", 0x0A, 0
msg_kernel_too_big_serial db "[stage2] ERROR: kernel too large (max 127 sectors)", 0x0A, 0
msg_a20_fail_serial db "[stage2] ERROR: kernel too large (max 127 sectors)", 0x0A, 0
msg_e820_ok_serial db "[stage2] E820 map collected", 0x0A, 0
msg_e820_fail_serial db "[stage2] ERROR: E820 not supported", 0x0A, 0
msg_read_err_code db "[stage2] INT13 error code: 0x", 0



; Disk Address Packet (DAP) for kernel load
kernel_dap:
    db 0x10, 0x00
    dw 0              ; sectors (filled at runtime)
    dw 0              ; offset (filled)
    dw 0              ; segment (filled)
    dq 0              ; LBA (filled)

; ---------------------------------------------------------------------------------------------
; GDT
; ---------------------------------------------------------------------------------------------

align 8
gdt_start:
    dq 0x0000000000000000    ; null
    dq 0x00CF9A000000FFFF    ; 0x08: 32-bit code (ring 0, G=1, D=1, P=1, S=1, X=1, R=1)
    dq 0x00CF92000000FFFF    ; 0x10: 32-bit data (ring 0, G=1, B=1, P=1, S=1, W=1)
    dq 0x00AF9A000000FFFF    ; 0x18: 64-bit code (ring 0, G=1, L=1, P=1, S=1, X=1, R=1)
    dq 0x00CF92000000FFFF    ; 0x20: 64-bit data (ring 0, G=1, L=1, P=1, S=1, W=1)
gdt_end:

gdt_descriptor:
    dw gdt_end - gdt_start - 1
    dd gdt_start

; ---------------------------------------------------------------------------------------------
; Pad stage2 to a whole number of 512-byte sectors.
; The Makefile computes kernel_lba = 1 + stage2_sectors, which is only true
; if stage2 occupies full sectors on disk.
; ---------------------------------------------------------------------------------------------
align 512, db 0
