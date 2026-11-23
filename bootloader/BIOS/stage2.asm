; ---------------------------------------------------------------------------------------------
; FreyjaOS - BIOS Stage 2 Bootloader (real -> protected -> long mode)
;
; Responsibilities:
; - (Real mode) enable A20
; - (Real mode) load kernel from disk into 0x0010_0000 (1 MiB) using INT 13h extensions,
;   in 32 KiB chunks through a bounce buffer, copied up with unreal mode
; - (Real mode) collect the E820 memory map
; - (Real mode) set a VESA graphics mode with a linear framebuffer, if there is a good one,
;   and copy the VGA ROM's 8x16 font for the kernel to draw text with (Entry 18)
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

; BIOS-friendly bounce buffer (< 1 MiB). INT 13h can only write below
; 1 MiB, so each chunk of the kernel is read here, then copied up to its
; final address with a 32-bit copy in unreal mode (see enter_unreal).
KERNEL_TMP_ADDR   equ 0x00010000
KERNEL_TMP_SEG    equ 0x1000
KERNEL_TMP_OFF    equ 0x0000

; Sectors per INT 13h read: 32 KiB, well inside the 64 KiB bounce buffer
; and under the 127-sector limit some BIOSes put on one call.
LOAD_CHUNK_SECTORS equ 64

; Largest kernel image: 1 MiB (0x100000-0x200000). Its .bss follows the
; image and must stay below stage2's page tables at 0x200000, which are
; live until the kernel switches CR3; kernel.ld asserts that at link time.
KERNEL_MAX_SECTORS equ 2048

PML4_ADDR         equ 0x00200000
PDPT_ADDR         equ 0x00201000
PD_ADDR           equ 0x00202000

; Kernel stack: 64KB region ending just below the trampoline.
; Stack grows downward, so the top (initial RSP) is at 0x0008_0000).
; This gives 64KB of stack: [0x0007_0000, 0x0008_0000).
KERNEL_STACK_TOP equ 0x00080000

COM1_BASE         equ 0x03F8

; FLAGS as the last BIOS call left them, for the kernel's log (0x4F0-0x4FF
; is the BIOS data area's "inter-application communication area", kept
; free for programs like this one).
BIOS_FLAGS        equ 0x04F0

; Display hand-off for the kernel (kernel/src/boot.rs reads the same layout).
; 0x0900-0x093F and 0x1000-0x24FF lie between the E820 map (ends by 0x0804)
; and the stack (below 0x7C00), and nothing else uses them.
DISPLAY_INFO      equ 0x0900      ; 64 bytes, filled in by setup_display
FONT_ADDR         equ 0x1000      ; 4 KiB: 256 glyphs of 16 rows, 8 pixels wide
VBE_CTRL_INFO     equ 0x2000      ; 512 bytes: VBE controller information
VBE_MODE_INFO     equ 0x2200      ; 256 bytes: VBE mode information
EDID_BUFFER       equ 0x2400      ; 128 bytes: the display's EDID

DI_MAGIC          equ 0           ; dd 'FDSP'
DI_STATUS         equ 4           ; db, see DS_* below
DI_FONT           equ 5           ; db 1 = the font was copied to FONT_ADDR
DI_MODE           equ 6           ; dw VBE mode number
DI_WIDTH          equ 8           ; dw pixels
DI_HEIGHT         equ 10          ; dw pixels
DI_PITCH          equ 12          ; dw bytes from one line to the next
DI_BPP            equ 14          ; db bits per pixel (24 or 32)
DI_RED            equ 15          ; db size, db position (then green, then blue)
DI_FRAMEBUFFER    equ 24          ; dd physical address of the linear framebuffer
DI_NATIVE_W       equ 28          ; dw the display's own resolution, from EDID (0: unknown)
DI_NATIVE_H       equ 30          ; dw
DI_VBE_VERSION    equ 32          ; dw BCD, e.g. 0x0300
DI_MODES          equ 34          ; dw modes the card listed

DS_GRAPHICS       equ 0           ; graphics mode set
DS_BUILT_TEXT     equ 1           ; text: built with TEXT_MODE=1
DS_SHIFT          equ 2           ; text: Shift held while booting
DS_NO_VBE         equ 3           ; text: no VBE 2.0 (no linear framebuffer)
DS_NO_MODE        equ 4           ; text: no usable mode in the card's list
DS_SET_FAILED     equ 5           ; text: the card refused the chosen mode
DS_NO_FONT        equ 6           ; text: the ROM font couldn't be copied (nothing to draw with)

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

    cmp ax, KERNEL_MAX_SECTORS
    jbe .kernel_size_ok
    mov si, msg_kernel_too_big
    call print_string
    mov si, msg_kernel_too_big_serial
    call serial_write_string
    jmp hang
.kernel_size_ok:

    ; --- Load the kernel to 0x0010_0000, one chunk at a time ---
    ; Each pass: INT 13h AH=42h reads up to LOAD_CHUNK_SECTORS into the
    ; bounce buffer, then a 32-bit copy (unreal mode) moves them up to
    ; load_dest, above 1 MiB.
    mov dword [load_lba], __KERNEL_LBA
    mov word [load_left], __KERNEL_SECTORS
    mov dword [load_dest], KERNEL_ENTRY

.load_chunk:
    mov cx, [load_left]
    test cx, cx
    jz .read_ok
    cmp cx, LOAD_CHUNK_SECTORS
    jbe .chunk_size_ok
    mov cx, LOAD_CHUNK_SECTORS
.chunk_size_ok:
    mov [load_count], cx

    mov si, kernel_dap
    mov [si + 2], cx
    mov word [si + 4], KERNEL_TMP_OFF
    mov word [si + 6], KERNEL_TMP_SEG
    mov eax, [load_lba]
    mov [si + 8], eax
    mov dword [si + 12], 0

    mov ah, 0x42
    mov dl, [boot_drive]
    int 0x13
    jc .read_error

    ; The BIOS may have reloaded segment limits, so (re)enter unreal mode
    ; before every copy rather than once.
    call enter_unreal
    movzx ecx, word [load_count]
    shl ecx, 7                    ; sectors * 512 / 4 = dwords
    mov esi, KERNEL_TMP_ADDR
    mov edi, [load_dest]
    cld
    a32 rep movsd                 ; DS:ESI -> ES:EDI, 32-bit addresses

    movzx eax, word [load_count]
    add [load_lba], eax
    sub [load_left], ax
    shl eax, 9                    ; sectors -> bytes
    add [load_dest], eax
    jmp .load_chunk

.read_error:
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

    ; --- What the BIOS left the interrupt flag as ---
    ; INT 13h and INT 15h return with RETF 2 (to keep their carry flag),
    ; which also keeps whatever IF the BIOS handler left: the CLI at the top
    ; of stage 2 doesn't survive them. SeaBIOS (QEMU) returns with IF clear;
    ; the T430's BIOS returns with it set, so the kernel used to start with
    ; interrupts on and the PIC still putting the timer on vector 8, one of
    ; the CPU's exception vectors (Entry 16). Recorded here, for the kernel's
    ; log, after the disk and memory-map calls: setup_display turns
    ; interrupts on and off itself (to see Shift), so recorded after it
    ; this would always say off (Entry 19).
%ifdef __BIOS_LEAVES_IF_ON
    sti                           ; test build: behave like the T430's BIOS
%endif
    pushf
    pop ax
    mov [BIOS_FLAGS], ax

    ; --- Display: VESA graphics if possible (the last BIOS call) ---
    ; After a graphics mode is set the BIOS's text output is gone, so this
    ; comes after every print_string, and before interrupts go off for good.
    call setup_display

    ; --- Interrupts off, for good ---
    ; From here until the kernel loads its own IDT there is nothing that
    ; could take an interrupt.
%ifdef __BIOS_LEAVES_IF_ON
    sti                           ; test build: as a video BIOS might leave it
%endif
    cli

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

    ; (The kernel is already at KERNEL_ENTRY: the real-mode loader copied
    ; each chunk up as it went.)

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

; ---------------------------------------------------------------------------------------------
; enter_unreal - give DS and ES 4 GiB limits while staying in real mode
;
; Switch to protected mode just long enough to load DS and ES from the flat
; data descriptor (0x10), then switch back. Real-mode segment loads only
; change a segment's base, not its cached limit, so after returning DS=ES=0
; keep the 4 GiB limit and 32-bit addresses (a32 prefix) reach above 1 MiB.
; CS is never reloaded, so code keeps running as ordinary 16-bit code.
; ---------------------------------------------------------------------------------------------
enter_unreal:
    pushfd
    push eax
    push bx
    cli
    lgdt [gdt_descriptor]
    mov eax, cr0
    or al, 1
    mov cr0, eax                  ; protected mode (CS still a real-mode segment)
    mov bx, 0x10                  ; flat 32-bit data descriptor
    mov ds, bx
    mov es, bx
    and al, 0xFE
    mov cr0, eax                  ; back to real mode
    xor bx, bx
    mov ds, bx                    ; base 0 again; limit stays 4 GiB
    mov es, bx
    pop bx
    pop eax
    popfd
    ret

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

; serial_putc: write AL to COM1. Does nothing if serial_init found no UART
; (the T430 has none), and never waits forever for one that doesn't answer.
; Keeps every register (Entry 18: it used to change AH, so a disk error
; code printed by serial_write_hex_byte had its low digit wrong).
serial_putc:
    cmp byte [serial_present], 0
    je .absent
    push dx
    push cx
    push ax
    mov ah, al
    mov cx, 0xFFFF
.wait:
    mov dx, COM1_BASE + 5
    in al, dx
    test al, 0x20
    jnz .ready
    loop .wait
.ready:
    mov dx, COM1_BASE
    mov al, ah
    out dx, al
    pop ax
    pop cx
    pop dx
.absent:
    ret

serial_present db 0

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
    ; Is there a UART at all? Its scratch register (base + 7) keeps
    ; whatever is written to it; an empty port reads back 0xFF (or 0x00).
    mov dx, COM1_BASE + 7
    mov al, 0xA5
    out dx, al
    in al, dx
    cmp al, 0xA5
    jne .none
    mov al, 0x5A
    out dx, al
    in al, dx
    cmp al, 0x5A
    jne .none
    mov byte [serial_present], 1

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

.none:
    popa
    ret

; ---------------------------------------------------------------------------------------------
; setup_display
; Copy the VGA ROM's 8x16 font to FONT_ADDR, then choose and set a VESA mode with a
; linear framebuffer, and describe the result at DISPLAY_INFO. Stays in text mode (and
; says why) if the build asks for it, Shift is held, the card has no VBE 2.0, no mode
; fits, or setting it fails: the kernel draws either way.
;
; Choice: graphics modes with a linear framebuffer, direct colour, 24 or 32 bits per
; pixel, between 640x480 and 1920x1280 (the kernel's largest text grid is 240x80 cells
; of 8x16). If the display's EDID gives its own resolution, nothing larger than that;
; among the rest, the most pixels, then 32 bits over 24.
; ---------------------------------------------------------------------------------------------
setup_display:
    pushad
    push es

    cld
    xor ax, ax
    mov es, ax
    mov di, DISPLAY_INFO
    mov cx, 32
    rep stosw                          ; zero the 64-byte block
    mov dword [DISPLAY_INFO + DI_MAGIC], 'FDSP'

    ; --- The font: INT 10h AX=1130h BH=06h returns ES:BP -> 8x16 glyphs, CX = 16 ---
    push bp
    mov ax, 0x1130
    mov bh, 0x06
    int 0x10
    cmp cx, 16
    jne .font_done
    push ds
    mov ax, es
    mov ds, ax
    mov si, bp
    xor ax, ax
    mov es, ax
    mov di, FONT_ADDR
    mov cx, 2048
    cld
    rep movsw                          ; 4096 bytes
    pop ds
    mov byte [DISPLAY_INFO + DI_FONT], 1
.font_done:
    pop bp
    xor ax, ax
    mov es, ax

%ifdef __TEXT_MODE
    mov byte [DISPLAY_INFO + DI_STATUS], DS_BUILT_TEXT
    jmp .report
%endif

    ; --- Shift held: stay in text mode ---
    ; Interrupts have been off since stage 1 (but inside some BIOS calls),
    ; so the BIOS may not have seen the keyboard. Give it a quarter of a
    ; second with them on: its keyboard handler records Shift in the shift
    ; flags (BIOS data area 0x417), and a held key repeats. The flags are
    ; then read straight from there: INT 16h would do, but SeaBIOS's also
    ; fetches waiting keys itself, which would hide a broken wait from the
    ; test (test.sh's shift case). Bounded even if the timer never ticks.
    sti
    mov ebx, [0x046C]                   ; BIOS timer ticks, 18.2 a second
    mov ecx, 0x00200000                 ; ~2 s of port 0x80 delays at most
.shift_wait:
    mov eax, [0x046C]
    sub eax, ebx
    cmp eax, 5
    jae .shift_check
    out 0x80, al                        ; about 1 us (the POST code port)
    dec ecx
    jnz .shift_wait
.shift_check:
    cli
    test byte [0x0417], 0x03            ; bit 0 right Shift, bit 1 left Shift
    jz .vbe
    mov byte [DISPLAY_INFO + DI_STATUS], DS_SHIFT
    jmp .report

.vbe:
    ; --- Without the font the kernel couldn't draw a single character ---
    cmp byte [DISPLAY_INFO + DI_FONT], 1
    je .vbe_info
    mov byte [DISPLAY_INFO + DI_STATUS], DS_NO_FONT
    jmp .report

.vbe_info:
    ; --- Controller information: ask for VBE 2.0+ fields ---
    mov dword [VBE_CTRL_INFO], 'VBE2'
    mov di, VBE_CTRL_INFO
    mov ax, 0x4F00
    int 0x10
    xor bx, bx
    mov es, bx
    cmp ax, 0x004F
    jne .no_vbe
    cmp dword [VBE_CTRL_INFO], 'VESA'
    jne .no_vbe
    mov ax, [VBE_CTRL_INFO + 4]        ; version, BCD
    mov [DISPLAY_INFO + DI_VBE_VERSION], ax
    cmp ax, 0x0200
    jb .no_vbe

    ; --- The display's own resolution, from EDID (VBE/DDC 4F15h, block 0) ---
    mov ax, 0x4F15
    mov bl, 0x01
    xor cx, cx
    xor dx, dx
    mov di, EDID_BUFFER
    int 0x10
    xor bx, bx
    mov es, bx
    cmp ax, 0x004F
    jne .modes
    cmp dword [EDID_BUFFER], 0xFFFFFF00     ; header 00 FF FF FF FF FF FF 00
    jne .modes
    cmp dword [EDID_BUFFER + 4], 0x00FFFFFF
    jne .modes
    mov si, EDID_BUFFER
    mov cx, 128
    xor bl, bl
.edid_sum:
    add bl, [si]
    inc si
    loop .edid_sum
    test bl, bl                         ; all 128 bytes sum to 0
    jnz .modes
    cmp word [EDID_BUFFER + 54], 0      ; first detailed timing: pixel clock 0 = not a timing
    je .modes
    movzx ax, byte [EDID_BUFFER + 58]   ; horizontal active: byte 56, high nibble of 58
    shr ax, 4
    shl ax, 8
    mov al, [EDID_BUFFER + 56]
    mov [DISPLAY_INFO + DI_NATIVE_W], ax
    movzx ax, byte [EDID_BUFFER + 61]   ; vertical active: byte 59, high nibble of 61
    shr ax, 4
    shl ax, 8
    mov al, [EDID_BUFFER + 59]
    mov [DISPLAY_INFO + DI_NATIVE_H], ax

.modes:
    ; --- Walk the card's mode list (far pointer at offset 14), at most 256 entries ---
    mov ax, [VBE_CTRL_INFO + 14]
    mov [vbe_list_off], ax
    mov ax, [VBE_CTRL_INFO + 16]
    mov [vbe_list_seg], ax
    mov word [vbe_best], 0xFFFF
    mov dword [vbe_best_area], 0
    mov byte [vbe_best_bpp], 0
.next_mode:
    cmp word [DISPLAY_INFO + DI_MODES], 256
    jae .chosen
    push ds
    mov si, [vbe_list_off]
    mov ds, [vbe_list_seg]
    mov cx, [si]                        ; the mode number
    pop ds
    cmp cx, 0xFFFF
    je .chosen
    add word [vbe_list_off], 2
    inc word [DISPLAY_INFO + DI_MODES]
    mov [vbe_mode], cx

    mov ax, 0x4F01
    mov di, VBE_MODE_INFO
    int 0x10
    xor bx, bx
    mov es, bx
    cmp ax, 0x004F
    jne .next_mode
    mov ax, [VBE_MODE_INFO + 0]         ; attributes: supported (0), graphics (4), linear (7)
    and ax, 0x0091
    cmp ax, 0x0091
    jne .next_mode
    cmp byte [VBE_MODE_INFO + 27], 6    ; memory model: direct colour
    jne .next_mode
    mov bl, [VBE_MODE_INFO + 25]        ; bits per pixel
    cmp bl, 32
    je .bpp_ok
    cmp bl, 24
    jne .next_mode
.bpp_ok:
    mov ax, [VBE_MODE_INFO + 18]        ; width
    mov dx, [VBE_MODE_INFO + 20]        ; height
    cmp ax, 640
    jb .next_mode
    cmp ax, 1920
    ja .next_mode
    cmp dx, 480
    jb .next_mode
    cmp dx, 1280
    ja .next_mode
    ; No bigger than the display's own resolution; with no EDID, no bigger
    ; than 1024x768, which every VESA display shows (a mode the panel can't
    ; show would leave the screen dark).
    mov cx, [DISPLAY_INFO + DI_NATIVE_W]
    mov si, [DISPLAY_INFO + DI_NATIVE_H]
    test cx, cx
    jz .no_edid
    test si, si
    jnz .limit
.no_edid:
    mov cx, 1024
    mov si, 768
.limit:
    cmp ax, cx
    ja .next_mode
    cmp dx, si
    ja .next_mode
    ; Everything the kernel needs to draw: a framebuffer address, and lines
    ; at least as long as a row of pixels. Checked here, before the mode is
    ; set: once it is, there is no going back to text mode.
    cmp dword [VBE_MODE_INFO + 40], 0
    je .next_mode
    push ax
    push dx
    movzx cx, bl
    shr cx, 3                           ; bytes per pixel
    mul cx                              ; AX = bytes in a row of pixels
    mov cx, ax
    call mode_pitch
    cmp ax, cx
    pop dx
    pop ax
    jb .next_mode
    movzx eax, ax
    movzx edx, dx
    imul eax, edx                       ; area
    cmp eax, [vbe_best_area]
    ja .better
    jb .next_mode
    cmp bl, [vbe_best_bpp]              ; same area: more bits per pixel wins
    jbe .next_mode
.better:
    mov [vbe_best_area], eax
    mov [vbe_best_bpp], bl
    mov ax, [vbe_mode]
    mov [vbe_best], ax
    jmp .next_mode

.chosen:
    mov cx, [vbe_best]
    cmp cx, 0xFFFF
    jne .set
    mov byte [DISPLAY_INFO + DI_STATUS], DS_NO_MODE
    jmp .report
.set:
    mov ax, 0x4F01                      ; its information again, into the buffer
    mov di, VBE_MODE_INFO
    int 0x10
    xor bx, bx
    mov es, bx
    cmp ax, 0x004F
    jne .set_failed
    mov bx, [vbe_best]
    or bx, 0x4000                       ; linear framebuffer
    mov ax, 0x4F02
    int 0x10
    xor cx, cx
    mov es, cx
    cmp ax, 0x004F
    jne .set_failed

    ; --- Describe the mode for the kernel ---
    mov ax, [vbe_best]
    mov [DISPLAY_INFO + DI_MODE], ax
    mov ax, [VBE_MODE_INFO + 18]
    mov [DISPLAY_INFO + DI_WIDTH], ax
    mov ax, [VBE_MODE_INFO + 20]
    mov [DISPLAY_INFO + DI_HEIGHT], ax
    call mode_pitch
    mov [DISPLAY_INFO + DI_PITCH], ax
    mov al, [VBE_MODE_INFO + 25]
    mov [DISPLAY_INFO + DI_BPP], al
    mov si, VBE_MODE_INFO + 31          ; red size, red position, green..., blue...
    mov di, DISPLAY_INFO + DI_RED
    mov cx, 6
    cld
    rep movsb
    mov eax, [VBE_MODE_INFO + 40]
    mov [DISPLAY_INFO + DI_FRAMEBUFFER], eax
    mov byte [DISPLAY_INFO + DI_STATUS], DS_GRAPHICS
    jmp .report

.set_failed:
    mov byte [DISPLAY_INFO + DI_STATUS], DS_SET_FAILED
    jmp .report
.no_vbe:
    mov byte [DISPLAY_INFO + DI_STATUS], DS_NO_VBE

.report:
    ; Serial only: after a mode set, BIOS text output is gone.
    mov si, msg_display_serial
    call serial_write_string
    mov al, [DISPLAY_INFO + DI_STATUS]
    call serial_write_hex_byte
    mov si, msg_display_mode_serial
    call serial_write_string
    mov al, [DISPLAY_INFO + DI_MODE + 1]
    call serial_write_hex_byte
    mov al, [DISPLAY_INFO + DI_MODE]
    call serial_write_hex_byte
    mov al, 0x0A
    call serial_putc

    pop es
    popad
    ret

vbe_list_off   dw 0
vbe_list_seg   dw 0
vbe_mode       dw 0
vbe_best       dw 0
vbe_best_area  dd 0
vbe_best_bpp   db 0

; AX = bytes from one line to the next of the mode in VBE_MODE_INFO: the
; VBE 3 linear-mode field (offset 50) when there is one, else offset 16.
mode_pitch:
    mov ax, [VBE_MODE_INFO + 16]
    cmp word [DISPLAY_INFO + DI_VBE_VERSION], 0x0300
    jb .done
    cmp word [VBE_MODE_INFO + 50], 0
    je .done
    mov ax, [VBE_MODE_INFO + 50]
.done:
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
msg_kernel_too_big db "Kernel too large (max 2048 sectors)", 0x0D, 0x0A, 0
msg_a20_fail db "A20 enable failed - halting", 0x0D, 0x0A, 0
msg_e820_ok db "E820 map collected", 0x0D, 0x0A, 0
msg_e820_fail db "E820 not supported", 0x0D, 0x0A, 0

; Serial versions (LF is 0x0A; serial_write_string canverts it to CRLF)
msg_stage2_serial db "[stage2] Stage 2: OK", 0x0A, 0
msg_kernel_loaded_serial db "[stage2] Kernel loaded", 0x0A, 0
msg_kernel_not_patched_serial db "[stage2] ERROR: kernel constants not patched", 0x0A, 0
msg_kernel_read_error_serial db "[stage2] ERROR: kernel read failed", 0x0A, 0
msg_kernel_too_big_serial db "[stage2] ERROR: kernel too large (max 2048 sectors)", 0x0A, 0
msg_a20_fail_serial db "[stage2] ERROR: kernel too large (max 127 sectors)", 0x0A, 0
msg_e820_ok_serial db "[stage2] E820 map collected", 0x0A, 0
msg_e820_fail_serial db "[stage2] ERROR: E820 not supported", 0x0A, 0
msg_read_err_code db "[stage2] INT13 error code: 0x", 0
msg_display_serial db "[stage2] display: status 0x", 0
msg_display_mode_serial db ", VBE mode 0x", 0



; Disk Address Packet (DAP) for kernel load
kernel_dap:
    db 0x10, 0x00
    dw 0              ; sectors (filled at runtime)
    dw 0              ; offset (filled)
    dw 0              ; segment (filled)
    dq 0              ; LBA (filled)

; Chunked-load state
load_lba   dd 0       ; next LBA to read
load_dest  dd 0       ; where the next chunk goes (physical, >= 1 MiB)
load_left  dw 0       ; sectors still to read
load_count dw 0       ; sectors in the current chunk

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
