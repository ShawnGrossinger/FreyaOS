; ---------------------------------------------------------------------------------------------
; FreyjaOS - BIOS Stage 1 Bootloader (MBR, 512 bytes)
;
; Loads Stage2 from disk (LBA 1 ..) into 0x0000:0x7E00 and jumps to it.
;
; Build (from build/):
;   nasm -f bin -D__STAGE2_SECTORS=<n> ../bootloader/BIOS/stage1.asm -o out/stage1.bin
;
; Notes:
; - Requires INT 13h Extensions (EDD). If unsupported, prints an error and halts.
; - Stage2 is assumed to immediately follow the MBR (LBA 1).
; ---------------------------------------------------------------------------------------------

[org 0x7C00]
BITS 16

%ifndef __STAGE2_SECTORS
    %define __STAGE2_SECTORS 32
%endif

STAGE2_LOAD_OFF  equ 0x7E00
STAGE2_LOAD_SEG  equ 0x0000
STAGE2_LBA_START equ 1

start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7C00

    mov [boot_drive], dl

    ; --- Debug message ---
    mov si, msg_stage1
    call print_string

    ; --- Check INT 13h extensions (EDD) ---
    mov ax, 0x4100
    mov bx, 0x55AA
    mov dl, [boot_drive]
    int 0x13
    jc disk_error
    cmp bx, 0xAA55
    jne disk_error
    test cx, 0x0001
    jz disk_error

    ; --- Prepare Disk Address Packet (DAP) for Stage2 read ---
    mov si, dap
    mov word [si + 2], __STAGE2_SECTORS       ; number of sectors
    mov word [si + 4], STAGE2_LOAD_OFF        ; destination offset
    mov word [si + 6], STAGE2_LOAD_SEG        ; destination segment
    mov dword [si + 8], STAGE2_LBA_START      ; starting LBA (low)
    mov dword [si + 12], 0                    ; starting LBA (high)

    ; --- Read Stage2 ---
    mov ah, 0x42
    mov dl, [boot_drive]
    int 0x13
    jc disk_error

    ; Ensure DL is the boot drive when we enter Stage2
    mov dl, [boot_drive]

    ; Jump to Stage2
    jmp STAGE2_LOAD_SEG:STAGE2_LOAD_OFF

; ---------------------------------------------------------------------------------------------
; Helpers
; ---------------------------------------------------------------------------------------------

; print_string: prints a 0-terminated string pointed to by DS:SI
print_string:
    pusha
.print_next:
    lodsb
    test al, al
    jz .done
    mov ah, 0x0E
    mov bh, 0x00
    mov bl, 0x07
    int 0x10
    jmp .print_next
.done:
    popa
    ret

; ---------------------------------------------------------------------------------------------
; Error handling
; ---------------------------------------------------------------------------------------------

disk_error:
    mov si, msg_error
    call print_string
.hang:
    cli
    hlt
    jmp .hang

; ---------------------------------------------------------------------------------------------
; Data
; ---------------------------------------------------------------------------------------------

boot_drive db 0

msg_stage1 db "Stage 1: OK", 0x0D, 0x0A, 0
msg_error  db "Stage1 disk error (no EDD or read failed)", 0

; Disk Address Packet (DAP) - 16 bytes
;  Offset  Size  Description
;  0       1     size (0x10)
;  1       1     reserved
;  2       2     sectors to read
;  4       2     destination offset
;  6       2     destination segment
;  8       8     starting LBA

dap:
    db 0x10, 0x00
    dw 0
    dw 0
    dw 0
    dq 0

; Pad to 512 bytes and add boot signature
times 510 - ($ - $$) db 0
dw 0xAA55
