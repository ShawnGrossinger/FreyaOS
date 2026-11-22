; ---------------------------------------------------------------------------
; entry.asm — 64-bit kernel entry stub + CPU exception stubs for FreyaOS
;
; By the time we reach _start, stage2's trampoline has already:
;   - switched to 64-bit long mode (CS=0x18)
;   - reloaded DS/ES/SS/FS/GS with selector 0x20
;   - enabled SSE (CR4.OSFXSR + OSXMMEXCPT, CR0.EM=0 CR0.MP=1)
;   - set RSP = KERNEL_STACK_TOP (0x80000)
;
; _start zeroes .bss (which is NOT in kernel.bin — objcopy drops NOBITS
; sections) and hands off to Rust.
;
; The second half of this file holds the exception entry stubs that the IDT
; points at (see src/idt.rs).
;
; Built as ELF object by build.rs:  nasm -f elf64 entry.asm -o entry.o
; ---------------------------------------------------------------------------

BITS 64

section .entry progbits alloc exec nowrite align=16

global _start
extern rust_start
extern __bss_start
extern __bss_end

_start:
    ; Interrupts off before anything else, whatever the loader left: until
    ; the PICs are remapped they put the timer on vector 8, a CPU exception
    ; vector (Entry 16). RFLAGS as found is kept in r15, which the .bss
    ; clearing below doesn't touch, and saved for the self-test after it.
    pushfq
    cli
    pop r15

    ; Zero .bss. Stage2 only copies the bytes in kernel.bin, and .bss is
    ; not in kernel.bin, so without this every zero-initialised static
    ; (including the IDT) starts as whatever was left in RAM. QEMU's RAM
    ; happens to start zeroed; real hardware's does not.
    ; Symbols come from kernel.ld.
    cld
    lea rdi, [rel __bss_start]
    lea rcx, [rel __bss_end]
    sub rcx, rdi                ; byte count
    xor eax, eax
    rep stosb
    mov [rel entry_rflags], r15

    ; Align stack to 16 bytes for the SysV ABI.
    and rsp, -16

    call rust_start

.hang:
    cli
    hlt
    jmp .hang

; ---------------------------------------------------------------------------
; Entry stubs: one for every vector, 0 to 255
;
; On an exception or interrupt in 64-bit mode the CPU:
;   1. aligns RSP down to 16 bytes
;   2. pushes SS, RSP, RFLAGS, CS, RIP
;   3. for some exceptions only, pushes an error code
;
; Every stub makes the frame uniform: vectors without a CPU error code push
; a dummy 0 first. Then every stub pushes its vector number and jumps to
; isr_common. The resulting layout is mirrored by InterruptFrame in idt.rs.
;
; The CPU pushes an error code only for an EXCEPTION on vectors 8, 10-14,
; 17, 21, 29 and 30. An interrupt that arrives on one of those vectors
; (from a device or a local APIC entry the firmware left set up) brings no
; error code, and a stub that assumed one would leave every later push 8
; bytes off: isr_common's FXSAVE then faults with #GP, which is exactly
; what the T430 showed in Entry 15. So those stubs check: after the CPU's
; pushes, RSP is 16-aligned with an error code and 8 off without one. With
; none, the stub pushes NOT_AN_EXCEPTION in its place, and idt.rs reports
; an unexpected interrupt instead of a CPU exception.
; ---------------------------------------------------------------------------

section .text

extern interrupt_dispatch

; Vector where the CPU does NOT push an error code.
%macro ISR_NOERR 1
isr_stub_%1:
    push qword 0                ; dummy error code
    push qword %1               ; vector number
    jmp isr_common
%endmacro

; Vector where a CPU exception DOES push an error code (it is already on
; the stack) - unless this is an interrupt, not an exception (see above).
NOT_AN_EXCEPTION equ -1         ; no CPU error code is ever all ones
%macro ISR_ERR 1
isr_stub_%1:
    test rsp, 8                 ; 0: the CPU pushed an error code
    jnz %%no_error_code
    push qword %1               ; vector number
    jmp isr_common
%%no_error_code:
    push qword NOT_AN_EXCEPTION
    push qword %1
    jmp isr_common
%endmacro

ISR_NOERR 0     ; #DE  divide error
ISR_NOERR 1     ; #DB  debug
ISR_NOERR 2     ;      NMI
ISR_NOERR 3     ; #BP  breakpoint (int3)
ISR_NOERR 4     ; #OF  overflow
ISR_NOERR 5     ; #BR  bound range
ISR_NOERR 6     ; #UD  invalid opcode
ISR_NOERR 7     ; #NM  device not available
ISR_ERR   8     ; #DF  double fault (error code always 0)
ISR_NOERR 9     ;      coprocessor segment overrun (legacy)
ISR_ERR   10    ; #TS  invalid TSS
ISR_ERR   11    ; #NP  segment / gate not present
ISR_ERR   12    ; #SS  stack-segment fault
ISR_ERR   13    ; #GP  general protection
ISR_ERR   14    ; #PF  page fault
ISR_NOERR 15    ;      reserved
ISR_NOERR 16    ; #MF  x87 floating-point
ISR_ERR   17    ; #AC  alignment check (error code always 0)
ISR_NOERR 18    ; #MC  machine check
ISR_NOERR 19    ; #XM  SIMD floating-point
ISR_NOERR 20    ; #VE  virtualization
ISR_ERR   21    ; #CP  control protection
ISR_NOERR 22    ;      reserved
ISR_NOERR 23    ;      reserved
ISR_NOERR 24    ;      reserved
ISR_NOERR 25    ;      reserved
ISR_NOERR 26    ;      reserved
ISR_NOERR 27    ;      reserved
ISR_NOERR 28    ; #HV  hypervisor injection (AMD)
ISR_ERR   29    ; #VC  VMM communication (AMD)
ISR_ERR   30    ; #SX  security exception (AMD)
ISR_NOERR 31    ;      reserved

; Vectors 32-255: interrupts. idt.rs decides what each one means: the APIC
; timer (0x30), driver lines (0x40-0x50), the APIC spurious vector (0xFF);
; anything else is reported as unexpected instead of faulting for want of
; a gate.
%assign v 32
%rep 224
ISR_NOERR v
%assign v v+1
%endrep

; ---------------------------------------------------------------------------
; isr_common — shared path for every exception and hardware interrupt.
;
; Stack on arrival (lowest address first):
;   vector, error_code, RIP, CS, RFLAGS, RSP, SS      = 7 qwords
; We push 15 GPRs                                      = 15 qwords
; 22 qwords = 176 bytes = 11 * 16, and the CPU 16-aligned RSP before its
; pushes, so RSP is 16-aligned again after the GPR pushes.
;
; Then 512 bytes for FXSAVE (x87/SSE state). The kernel is compiled with
; SSE, so the Rust handler may use XMM registers; saving them means an
; exception we return from (int3 today, IRQs later) can't corrupt the
; interrupted code's XMM state. 512 is a multiple of 16, so RSP stays
; aligned for both FXSAVE (needs 16) and the SysV call (needs 16).
; ---------------------------------------------------------------------------
isr_common:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15                    ; last push = lowest address = first field

    sub rsp, 512
    fxsave [rsp]

    ; From ring 3 (CS RPL = 3, InterruptFrame.cs is the 19th qword): the
    ; saved state is the process's, and so are the live MXCSR and x87
    ; control word. Give the kernel its own defaults, or a process could
    ; unmask SSE exceptions or change rounding for kernel code (including
    ; other tasks the scheduler switches to from here). fxrstor below
    ; puts the process's back.
    test qword [rsp + 512 + 18*8], 3
    jz .from_kernel
    fninit
    ldmxcsr [rel mxcsr_default]
.from_kernel:

    cld                         ; SysV requires DF=0 at function entry
    lea rdi, [rsp + 512]        ; arg0 = &InterruptFrame (points at saved r15)
    call interrupt_dispatch     ; returns for #BP and hardware interrupts

    fxrstor [rsp]
    add rsp, 512

    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax

    add rsp, 16                 ; drop vector + error code
    iretq

; ---------------------------------------------------------------------------
; switch_context(save_rsp: *mut u64 [rdi], load_rsp: u64 [rsi])
;
; The whole of a task switch. Push the callee-saved registers the SysV ABI
; says a function call must preserve, store RSP into *save_rsp, load RSP
; from load_rsp, pop the other task's callee-saved registers and `ret` into
; wherever that task last called switch_context from. Everything else
; (caller-saved registers, SSE state) is either already saved by the
; caller per the ABI, or, for a task preempted by the timer, sits in the
; interrupt frame isr_common built on that task's own stack.
;
; A new task's stack is prepared (sched.rs) to look as if it had called
; switch_context from task_entry: six zero registers and task_entry's
; address as the return address.
; ---------------------------------------------------------------------------
global switch_context
switch_context:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

; ---------------------------------------------------------------------------
; syscall_entry — where the SYSCALL instruction lands (IA32_LSTAR).
;
; SYSCALL does very little: RCX = the user's RIP, R11 = the user's RFLAGS,
; CS/SS = kernel selectors from IA32_STAR, RFLAGS &= ~IA32_FMASK (which
; clears IF, so no interrupt can arrive until we say so). It does NOT
; switch stacks: RSP is still the process's, and nothing here may touch
; it. So first switch to the task's kernel stack (one CPU, interrupts off:
; a scratch variable is enough), then save everything the process had.
;
; System-call ABI: number in RAX, arguments in RDI, RSI, RDX, R10, R8, R9,
; result in RAX. RCX and R11 are destroyed (SYSCALL uses them); every
; other register, and all x87/SSE state, comes back unchanged.
;
; Frame built on the kernel stack, mirrored by SyscallFrame in syscall.rs:
;   user RSP, RFLAGS, RIP (3 qwords), the 15 GPRs (15 qwords): 144 bytes,
;   a multiple of 16, so RSP is aligned for FXSAVE and the call.
; ---------------------------------------------------------------------------
extern syscall_dispatch

global syscall_entry
syscall_entry:
    mov [rel syscall_user_rsp], rsp
    mov rsp, [rel syscall_kernel_rsp]
    push qword [rel syscall_user_rsp]
    push r11                    ; user RFLAGS
    push rcx                    ; user RIP
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15

    sub rsp, 512
    fxsave [rsp]
    fninit
    ldmxcsr [rel mxcsr_default]
    cld

    lea rdi, [rsp + 512]        ; &SyscallFrame
    sti                         ; system calls are preemptible
    call syscall_dispatch       ; exit() and kills never come back
    cli

    fxrstor [rsp]
    add rsp, 512
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    pop rcx                     ; user RIP
    pop r11                     ; user RFLAGS (IF set: sysret re-enables)
    pop rsp                     ; user RSP: nothing may touch the stack now
    o64 sysret

; ---------------------------------------------------------------------------
; enter_user(rip [rdi], rsp [rsi], arg [rdx]) -> never returns
;
; A process's first entry into ring 3. iretq pops RIP, CS, RFLAGS, RSP, SS
; from a frame we build: user code and stack selectors (RPL 3), interrupts
; enabled in the process. Every register the process could read is cleared
; first, so no kernel value leaks into it; the argument arrives in RDI.
; ---------------------------------------------------------------------------
global enter_user
enter_user:
    cli
    fninit
    ldmxcsr [rel mxcsr_default]
    pxor xmm0, xmm0
    pxor xmm1, xmm1
    pxor xmm2, xmm2
    pxor xmm3, xmm3
    pxor xmm4, xmm4
    pxor xmm5, xmm5
    pxor xmm6, xmm6
    pxor xmm7, xmm7
    pxor xmm8, xmm8
    pxor xmm9, xmm9
    pxor xmm10, xmm10
    pxor xmm11, xmm11
    pxor xmm12, xmm12
    pxor xmm13, xmm13
    pxor xmm14, xmm14
    pxor xmm15, xmm15

    push qword 0x1B             ; SS: user data, RPL 3 (gdt.rs USER_DATA_SEL)
    push rsi                    ; RSP
    push qword 0x202            ; RFLAGS: IF, and the always-one bit 1
    push qword 0x23             ; CS: user code, RPL 3 (gdt.rs USER_CODE_SEL)
    push rdi                    ; RIP

    mov rdi, rdx                ; the argument
    xor eax, eax
    xor ebx, ebx
    xor ecx, ecx
    xor edx, edx
    xor esi, esi
    xor ebp, ebp
    xor r8d, r8d
    xor r9d, r9d
    xor r10d, r10d
    xor r11d, r11d
    xor r12d, r12d
    xor r13d, r13d
    xor r14d, r14d
    xor r15d, r15d
    iretq

; The power-on MXCSR: every SSE exception masked, round to nearest.
align 4
mxcsr_default:
    dd 0x1F80

section .bss
align 8
global syscall_kernel_rsp
syscall_kernel_rsp: resq 1      ; = TSS.RSP0, set by gdt::set_kernel_stack
global entry_rflags
entry_rflags:       resq 1      ; RFLAGS when _start was entered
syscall_user_rsp:   resq 1      ; scratch while switching stacks

; ---------------------------------------------------------------------------
; isr_stub_table — addresses of all 256 stubs, read by idt.rs to fill gates.
; ---------------------------------------------------------------------------
section .rodata

global isr_stub_table
align 8
isr_stub_table:
%assign i 0
%rep 256
    dq isr_stub_%+i
%assign i i+1
%endrep
