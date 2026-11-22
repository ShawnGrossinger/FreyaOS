//! gdt.rs — the kernel's own GDT and TSS.
//!
//! Until now the kernel ran on stage2's GDT (in low memory at ~0x82F8, with
//! 64-bit code at 0x18 and data at 0x20). This module replaces it with a GDT
//! the kernel owns, and adds a Task State Segment.
//!
//! In long mode the TSS no longer does hardware task switching. It holds
//! stack pointers the CPU loads on its own:
//!   - RSP0: the stack used when an interrupt arrives from ring 3 (later)
//!   - IST1-7: "Interrupt Stack Table" entries. A gate with a nonzero IST
//!     index ALWAYS switches to that stack, whatever RSP was before.
//!
//! The double fault gate uses IST1. If the kernel stack overflows into its
//! guard page, the CPU can't push a #PF frame onto it, which escalates to
//! #DF. Without IST, #DF would try the same dead stack and triple-fault.
//! With IST, #DF runs on a known-good stack and gets reported.

use core::arch::asm;

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------
//
// Order matters for later: SYSCALL/SYSRET derive selectors from fixed
// offsets in the STAR MSR. SYSCALL needs kernel data = kernel code + 8;
// 64-bit SYSRET needs user data = base + 8 and user code = base + 16. This
// layout satisfies both, so user mode can be added without reshuffling.

pub const KERNEL_CODE_SEL: u16 = 0x08;
pub const KERNEL_DATA_SEL: u16 = 0x10;
#[allow(dead_code)] // used once ring 3 exists
pub const USER_DATA_SEL: u16 = 0x18 | 3; // RPL 3
#[allow(dead_code)]
pub const USER_CODE_SEL: u16 = 0x20 | 3;
pub const TSS_SEL: u16 = 0x28;

/// IST slot for the double fault handler (1-based, as stored in the gate).
pub const DOUBLE_FAULT_IST: u8 = 1;

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------
//
// Code/data descriptor bits that matter in long mode:
//   bit 47     P    present
//   bits 45:46 DPL  privilege level
//   bit 44     S    1 = code/data (0 = system, e.g. TSS)
//   bit 43     E    1 = code
//   bit 41     R/W  readable (code) / writable (data)
//   bit 53     L    64-bit code (code segments only)
// Base and limit are ignored for 64-bit code/data; they're set to the
// usual flat values so the descriptors also read sensibly in a debugger.

const KERNEL_CODE_64: u64 = 0x00AF_9A00_0000_FFFF; // P, DPL0, code, R, L=1
const KERNEL_DATA: u64 = 0x00CF_9200_0000_FFFF; // P, DPL0, data, W
const USER_DATA: u64 = 0x00CF_F200_0000_FFFF; // P, DPL3, data, W
const USER_CODE_64: u64 = 0x00AF_FA00_0000_FFFF; // P, DPL3, code, R, L=1

/// 7 slots: null, 4 segments, and the TSS descriptor, which is 16 bytes in
/// long mode and so takes two slots (5 and 6).
const GDT_ENTRIES: usize = 7;

#[repr(C, align(16))]
struct Gdt([u64; GDT_ENTRIES]);

/// Not `const`-complete: the TSS descriptor needs the TSS's address, which
/// is only known at run time, so init() fills slots 5-6. Must be writable:
/// `ltr` sets the TSS descriptor's busy bit in place.
static mut GDT: Gdt = Gdt([
    0, // null descriptor (selector 0)
    KERNEL_CODE_64,
    KERNEL_DATA,
    USER_DATA,
    USER_CODE_64,
    0, // TSS low  (filled in init)
    0, // TSS high (filled in init)
]);

/// Operand of `lgdt`: same packed limit+base shape as the IDTR.
#[repr(C, packed)]
struct Gdtr {
    limit: u16,
    base: u64,
}

// ---------------------------------------------------------------------------
// TSS
// ---------------------------------------------------------------------------

/// 64-bit TSS (Intel SDM Vol. 3, "Task Management in 64-bit Mode").
/// `packed` because rsp0 sits at offset 4: the hardware layout isn't
/// naturally aligned.
#[repr(C, packed)]
struct TaskStateSegment {
    reserved0: u32,
    rsp: [u64; 3], // RSP0-2: stacks for ring 3/2/1 -> ring 0 transitions
    reserved1: u64,
    ist: [u64; 7], // IST1-7
    reserved2: u64,
    reserved3: u16,
    /// Offset of the I/O permission bitmap. Pointing it at the end of the
    /// TSS means "no bitmap": every port is denied to ring 3.
    iomap_base: u16,
}

const _: () = assert!(core::mem::size_of::<TaskStateSegment>() == 104);

static mut TSS: TaskStateSegment = TaskStateSegment {
    reserved0: 0,
    rsp: [0; 3],
    reserved1: 0,
    ist: [0; 7],
    reserved2: 0,
    reserved3: 0,
    iomap_base: 0,
};

// ---------------------------------------------------------------------------
// The double fault stack
// ---------------------------------------------------------------------------

const DOUBLE_FAULT_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct Stack([u8; DOUBLE_FAULT_STACK_SIZE]);

/// Lives in .bss (zeroed by _start). Its own overflow is not guarded: the
/// #DF handler only prints and halts, which uses well under 1 KiB.
static mut DOUBLE_FAULT_STACK: Stack = Stack([0; DOUBLE_FAULT_STACK_SIZE]);

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

/// Build the TSS, load the GDT, reload every segment register, load TR.
/// Must run before idt::init(): the IDT reads CS to fill its gates, and a
/// gate with an IST index is only usable once TR points at a valid TSS.
pub fn init() {
    unsafe {
        // --- TSS: point IST1 at the top of the double fault stack ---
        // Stacks grow down, so the TSS holds the address one past the end.
        let tss = &raw mut TSS;
        let df_stack_top =
            (&raw const DOUBLE_FAULT_STACK) as u64 + DOUBLE_FAULT_STACK_SIZE as u64;
        (*tss).ist[(DOUBLE_FAULT_IST - 1) as usize] = df_stack_top;
        (*tss).iomap_base = core::mem::size_of::<TaskStateSegment>() as u16;

        // --- TSS descriptor (16-byte system descriptor) ---
        let gdt = &raw mut GDT;
        let (low, high) = tss_descriptor(tss as u64);
        (*gdt).0[5] = low;
        (*gdt).0[6] = high;

        // --- Load GDTR ---
        // Limit from the entry count, not size_of::<Gdt>(): align(16) pads
        // the struct from 56 to 64 bytes, and the padding is not a slot.
        let gdtr = Gdtr {
            limit: (GDT_ENTRIES * 8 - 1) as u16,
            base: gdt as u64,
        };
        asm!("lgdt [{}]", in(reg) &gdtr, options(readonly, nostack, preserves_flags));

        // --- Reload CS ---
        // There is no `jmp far imm` in 64-bit mode. Instead, push the new
        // CS and a return address, then do a far return, which pops both.
        asm!(
            "push {sel}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            sel = in(reg) KERNEL_CODE_SEL as u64,
            tmp = lateout(reg) _,
            options(preserves_flags),
        );

        // --- Reload data segments ---
        // Their hidden descriptor caches still hold stage2's entries until
        // reloaded. SS must be a valid writable data segment (or null) at
        // CPL 0.
        asm!(
            "mov ds, {0:x}",
            "mov es, {0:x}",
            "mov fs, {0:x}",
            "mov gs, {0:x}",
            "mov ss, {0:x}",
            in(reg) KERNEL_DATA_SEL,
            options(nostack, preserves_flags),
        );

        // --- Load the Task Register ---
        // Marks the TSS descriptor busy; the CPU now reads IST/RSP0 from it.
        asm!("ltr {0:x}", in(reg) TSS_SEL, options(nostack, preserves_flags));
    }
}

/// Encode a 64-bit available-TSS descriptor.
///
/// Low 8 bytes:  limit 15:0 | base 23:0 | type=0x9 (64-bit TSS, available),
///               S=0, DPL=0, P=1 | limit 19:16 | flags | base 31:24
/// High 8 bytes: base 63:32 | reserved
fn tss_descriptor(base: u64) -> (u64, u64) {
    let limit = (core::mem::size_of::<TaskStateSegment>() - 1) as u64;

    let mut low = 0u64;
    low |= limit & 0xFFFF; // limit 15:0
    low |= (base & 0xFF_FFFF) << 16; // base 23:0
    low |= 0x89 << 40; // P=1, DPL=0, S=0, type=0x9
    low |= ((limit >> 16) & 0xF) << 48; // limit 19:16
    low |= ((base >> 24) & 0xFF) << 56; // base 31:24

    let high = base >> 32; // base 63:32

    (low, high)
}

/// Read back CS and TR so the boot log shows what the CPU actually has.
pub fn current_selectors() -> (u16, u16) {
    let cs: u16;
    let tr: u16;
    unsafe {
        asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack, preserves_flags));
        asm!("str {0:x}", out(reg) tr, options(nomem, nostack, preserves_flags));
    }
    (cs, tr)
}
