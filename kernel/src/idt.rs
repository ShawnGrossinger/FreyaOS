//! idt.rs — Interrupt Descriptor Table and CPU exception reporting.
//!
//! Pieces:
//!   - `IdtEntry`: one 16-byte 64-bit gate descriptor
//!   - `IDT`: 256 gates in a static (lives in .bss, zeroed by `_start`)
//!   - `init()`: points gates 0-31 at the asm stubs in entry.asm, loads IDTR
//!   - `interrupt_dispatch()`: called by `isr_common` with the saved CPU state
//!
//! Two more gates carry hardware interrupts from the local APIC: the timer
//! (apic::TIMER_VECTOR, 0x30) and spurious interrupts (0xFF).
//!
//! Every other vector from 32 to 255 is deliberately left as an all-zero
//! entry. If anything ever arrives on one, the CPU raises #GP (vector 13)
//! instead: type 0 fails the gate-type check, which comes before the present
//! check (a valid type with P=0 would give #NP instead). Either way the
//! error code has the IDT bit set and carries the original vector number,
//! and the report decodes it.
//!
//! The double fault gate (vector 8) uses IST1 (see gdt.rs), so #DF always
//! runs on its own stack, even when the kernel stack is the thing that broke.
//!
//! Interrupts stay disabled until pic::remap_and_mask() has moved the legacy
//! PICs off vectors 8-15 (where IRQ 0 would look like a double fault) and
//! apic::init() has the timer ready.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

const IDT_ENTRIES: usize = 256;
const EXCEPTION_VECTORS: usize = 32;

/// Gate type/attribute byte: P=1, DPL=0, type=0xE (64-bit interrupt gate).
/// An interrupt gate clears IF on entry; a trap gate (0xF) would not.
const GATE_INTERRUPT: u8 = 0x8E;

// ---------------------------------------------------------------------------
// Gate descriptor
// ---------------------------------------------------------------------------

/// One 64-bit IDT gate. The handler address is split across three fields
/// (a leftover from the 286 descriptor format).
#[derive(Clone, Copy)]
#[repr(C)]
struct IdtEntry {
    offset_low: u16,  // handler bits 15:0
    selector: u16,    // code segment selector the handler runs in
    ist: u8,          // bits 0-2: Interrupt Stack Table index (0 = current stack)
    type_attr: u8,    // P | DPL | 0 | gate type
    offset_mid: u16,  // handler bits 31:16
    offset_high: u32, // handler bits 63:32
    reserved: u32,    // must be zero
}

const _: () = assert!(core::mem::size_of::<IdtEntry>() == 16);

impl IdtEntry {
    /// All zeros: invalid gate type, P=0. Delivering through it raises #GP
    /// with the vector number in the error code.
    const fn missing() -> Self {
        IdtEntry {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    fn interrupt_gate(handler: u64, selector: u16) -> Self {
        IdtEntry {
            offset_low: handler as u16,
            selector,
            ist: 0,
            type_attr: GATE_INTERRUPT,
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }
}

#[repr(C, align(16))]
struct Idt([IdtEntry; IDT_ENTRIES]);

/// The table itself. Must outlive everything: the CPU reads it on every
/// exception, long after `init()` returns.
static mut IDT: Idt = Idt([IdtEntry::missing(); IDT_ENTRIES]);

/// The 10-byte operand of `lidt`. `packed` is required: without it the
/// compiler inserts 6 bytes of padding after `limit` to align `base`, and
/// the CPU would read garbage for the base address.
#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

const _: () = assert!(core::mem::size_of::<Idtr>() == 10);

// ---------------------------------------------------------------------------
// Saved CPU state
// ---------------------------------------------------------------------------

/// Stack layout built by the stubs in entry.asm, lowest address first.
/// Field order is the REVERSE of push order: `isr_common` pushes r15 last,
/// so r15 sits at the lowest address and comes first here.
#[repr(C)]
pub struct InterruptFrame {
    // Pushed by isr_common.
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    // Pushed by the per-vector stub.
    pub vector: u64,
    pub error_code: u64, // from the CPU, or the stub's dummy 0
    // Pushed by the CPU.
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

const _: () = assert!(core::mem::size_of::<InterruptFrame>() == 22 * 8);

extern "C" {
    /// Addresses of isr_stub_0 .. isr_stub_31 (entry.asm, .rodata).
    static isr_stub_table: [u64; EXCEPTION_VECTORS];
    /// Hardware interrupt stubs (entry.asm).
    fn isr_stub_48();
    fn isr_stub_255();
}

static BREAKPOINT_HITS: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

/// Fill the IDT and load IDTR. Returns the code selector written into the
/// gates (read from CS rather than hard-coded). Call after gdt::init(), so
/// CS is the kernel's own code selector and the TSS for IST1 is loaded.
pub fn init() -> u16 {
    let cs = read_cs();

    unsafe {
        let idt = &raw mut IDT;

        // Write every entry explicitly, so correctness never depends on
        // what was in RAM before.
        for v in 0..IDT_ENTRIES {
            (*idt).0[v] = IdtEntry::missing();
        }
        for v in 0..EXCEPTION_VECTORS {
            (*idt).0[v] = IdtEntry::interrupt_gate(isr_stub_table[v], cs);
        }

        // #DF switches to the dedicated stack in TSS.IST1 no matter what RSP
        // held. Without this, a kernel stack overflow escalates #PF -> #DF
        // -> triple fault, because #DF would try to push onto the same
        // dead stack.
        (*idt).0[8].ist = crate::gdt::DOUBLE_FAULT_IST;

        // Hardware interrupts from the local APIC.
        (*idt).0[crate::apic::TIMER_VECTOR as usize] =
            IdtEntry::interrupt_gate(isr_stub_48 as *const () as u64, cs);
        (*idt).0[crate::apic::SPURIOUS_VECTOR as usize] =
            IdtEntry::interrupt_gate(isr_stub_255 as *const () as u64, cs);

        // lidt copies base+limit into the IDTR register, so the Idtr value
        // itself can live on the stack. The table it points at cannot.
        let idtr = Idtr {
            limit: (core::mem::size_of::<Idt>() - 1) as u16,
            base: idt as u64,
        };
        asm!("lidt [{}]", in(reg) &idtr, options(readonly, nostack, preserves_flags));
    }

    cs
}

/// Fire `int3` with known values in registers the handler path is free to
/// overwrite, and check they come back intact. rdi is the strongest check:
/// isr_common itself loads rdi with the frame pointer before calling Rust,
/// so it only survives if the stub's push/pop pairing is right. xmm0 checks
/// the fxsave/fxrstor path.
pub fn selftest_breakpoint() -> bool {
    const RDI_MAGIC: u64 = 0x1122_3344_5566_7788;
    const RSI_MAGIC: u64 = 0x8877_6655_4433_2211;
    const R11_MAGIC: u64 = 0x0F0E_0D0C_0B0A_0908;
    const XMM0_MAGIC: f64 = 1.5;

    let before = BREAKPOINT_HITS.load(Ordering::SeqCst);
    let rdi_out: u64;
    let rsi_out: u64;
    let r11_out: u64;
    let xmm0_out: f64;

    unsafe {
        asm!(
            "int3",
            inout("rdi") RDI_MAGIC => rdi_out,
            inout("rsi") RSI_MAGIC => rsi_out,
            inout("r11") R11_MAGIC => r11_out,
            inout("xmm0") XMM0_MAGIC => xmm0_out,
        );
    }

    let after = BREAKPOINT_HITS.load(Ordering::SeqCst);

    after == before + 1
        && rdi_out == RDI_MAGIC
        && rsi_out == RSI_MAGIC
        && r11_out == R11_MAGIC
        && xmm0_out.to_bits() == XMM0_MAGIC.to_bits()
}

// ---------------------------------------------------------------------------
// Dispatch (called from isr_common in entry.asm)
// ---------------------------------------------------------------------------

#[no_mangle]
extern "C" fn interrupt_dispatch(frame: &mut InterruptFrame) {
    // Read CR2 first. It only means something for #PF, and anything later
    // in this function could in principle fault and overwrite it.
    let cr2 = read_cr2();

    match frame.vector {
        // #BP is a trap: RIP already points past the int3, so returning
        // simply resumes the interrupted code.
        3 => {
            BREAKPOINT_HITS.fetch_add(1, Ordering::SeqCst);
            crate::serial_write_str(b"[idt] #BP breakpoint, resuming at rip=");
            crate::serial_write_hex(frame.rip);
            crate::serial_write_str(b"\n");
        }

        // Hardware interrupts: handle and return to the interrupted code.
        v if v == crate::apic::TIMER_VECTOR as u64 => crate::apic::on_timer(),
        v if v == crate::apic::SPURIOUS_VECTOR as u64 => crate::apic::on_spurious(),

        // Everything else is fatal for now. Returning from a fault would
        // re-execute the faulting instruction forever.
        _ => {
            report_serial(frame, cr2);
            report_vga(frame, cr2);
            crate::halt();
        }
    }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

static EXCEPTION_NAMES: [&[u8]; EXCEPTION_VECTORS] = [
    b"#DE Divide Error",
    b"#DB Debug",
    b"NMI",
    b"#BP Breakpoint",
    b"#OF Overflow",
    b"#BR Bound Range Exceeded",
    b"#UD Invalid Opcode",
    b"#NM Device Not Available",
    b"#DF Double Fault",
    b"Coprocessor Segment Overrun",
    b"#TS Invalid TSS",
    b"#NP Segment Not Present",
    b"#SS Stack-Segment Fault",
    b"#GP General Protection",
    b"#PF Page Fault",
    b"Reserved",
    b"#MF x87 Floating-Point",
    b"#AC Alignment Check",
    b"#MC Machine Check",
    b"#XM SIMD Floating-Point",
    b"#VE Virtualization",
    b"#CP Control Protection",
    b"Reserved",
    b"Reserved",
    b"Reserved",
    b"Reserved",
    b"Reserved",
    b"Reserved",
    b"#HV Hypervisor Injection",
    b"#VC VMM Communication",
    b"#SX Security",
    b"Reserved",
];

fn exception_name(vector: u64) -> &'static [u8] {
    EXCEPTION_NAMES
        .get(vector as usize)
        .copied()
        .unwrap_or(b"Unknown")
}

// The serial functions are #[inline(never)] in main.rs, so each call below
// is a plain call rather than a copy of the UART polling loop.
use crate::{serial_write_hex as hex, serial_write_str as s, serial_write_u64 as dec};

fn report_serial(f: &InterruptFrame, cr2: u64) {
    s(b"\n[EXCEPTION] vector ");
    dec(f.vector);
    s(b" ");
    s(exception_name(f.vector));
    s(b"\n  error=");
    hex(f.error_code);
    s(b"  rip=");
    hex(f.rip);
    s(b"  cs=");
    hex(f.cs);
    s(b"\n  rflags=");
    hex(f.rflags);
    s(b"  rsp=");
    hex(f.rsp);
    s(b"  ss=");
    hex(f.ss);
    s(b"\n");

    if f.vector == 14 {
        s(b"  cr2=");
        hex(cr2);
        s(b"  (faulting address)\n  cause: ");
        decode_page_fault(f.error_code);
        s(b"\n");
    }

    // #DF hides its cause, but CR2 still holds the last page-fault address.
    // If that's the guard page, the kernel stack overflowed: the #PF frame
    // couldn't be pushed, which escalated to #DF on the IST1 stack.
    if f.vector == 8 {
        s(b"  cr2=");
        hex(cr2);
        s(b"  (last page-fault address)\n");
    }
    if matches!(f.vector, 8 | 14) && crate::paging::is_stack_guard(cr2) {
        s(b"  diagnosis: KERNEL STACK OVERFLOW (hit guard page ");
        hex(crate::paging::STACK_GUARD_PAGE);
        s(b")\n");
    }

    // #TS, #NP, #SS, #GP carry a selector-style error code.
    if matches!(f.vector, 10..=13) && f.error_code != 0 {
        s(b"  cause: ");
        decode_selector_error(f.error_code);
        s(b"\n");
    }

    s(b"  rax=");
    hex(f.rax);
    s(b" rbx=");
    hex(f.rbx);
    s(b" rcx=");
    hex(f.rcx);
    s(b"\n  rdx=");
    hex(f.rdx);
    s(b" rsi=");
    hex(f.rsi);
    s(b" rdi=");
    hex(f.rdi);
    s(b"\n  rbp=");
    hex(f.rbp);
    s(b" r8 =");
    hex(f.r8);
    s(b" r9 =");
    hex(f.r9);
    s(b"\n  r10=");
    hex(f.r10);
    s(b" r11=");
    hex(f.r11);
    s(b" r12=");
    hex(f.r12);
    s(b"\n  r13=");
    hex(f.r13);
    s(b" r14=");
    hex(f.r14);
    s(b" r15=");
    hex(f.r15);
    s(b"\n[EXCEPTION] halted\n");
}

/// #PF error code: bit0 P, bit1 W/R, bit2 U/S, bit3 RSVD, bit4 I/D.
fn decode_page_fault(err: u64) {
    if err & (1 << 3) != 0 {
        s(b"reserved bit set in a paging entry, ");
    } else if err & 1 == 0 {
        s(b"page not present, ");
    } else {
        s(b"protection violation, ");
    }
    if err & (1 << 4) != 0 {
        s(b"instruction fetch, ");
    } else if err & (1 << 1) != 0 {
        s(b"write, ");
    } else {
        s(b"read, ");
    }
    if err & (1 << 2) != 0 {
        s(b"user mode");
    } else {
        s(b"supervisor mode");
    }
}

/// Selector error code: bit0 EXT, bit1 IDT, bit2 TI (LDT), bits 15:3 index.
fn decode_selector_error(err: u64) {
    let index = (err >> 3) & 0x1FFF;
    if err & (1 << 1) != 0 {
        s(b"IDT vector ");
        dec(index);
        s(b" (gate not present or invalid)");
    } else {
        s(if err & (1 << 2) != 0 { b"LDT selector " } else { b"GDT selector " });
        hex(index << 3);
    }
    if err & 1 != 0 {
        s(b", external event");
    }
}

/// Short report on screen, for machines with no serial port attached
/// (the T430 has none). Rows 20-22, white on red.
fn report_vga(f: &InterruptFrame, cr2: u64) {
    const ATTR: u8 = 0x4F;

    let mut line = Line::new();
    line.str(b"!! CPU EXCEPTION ").dec(f.vector).str(b": ").str(exception_name(f.vector));
    crate::vga_print(20, line.as_bytes(), ATTR);

    let mut line = Line::new();
    line.str(b"RIP=").hex(f.rip).str(b"  ERR=").hex(f.error_code);
    crate::vga_print(21, line.as_bytes(), ATTR);

    let mut line = Line::new();
    if matches!(f.vector, 8 | 14) && crate::paging::is_stack_guard(cr2) {
        line.str(b"KERNEL STACK OVERFLOW  CR2=").hex(cr2);
    } else if f.vector == 14 {
        line.str(b"CR2=").hex(cr2).str(b"  (full report on COM1)");
    } else {
        line.str(b"Full register dump on COM1");
    }
    crate::vga_print(22, line.as_bytes(), ATTR);
}

/// Fixed 80-column text line builder (no allocator, no core::fmt).
struct Line {
    buf: [u8; 80],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Line { buf: [b' '; 80], len: 0 }
    }

    fn byte(&mut self, b: u8) {
        if self.len < self.buf.len() {
            self.buf[self.len] = b;
            self.len += 1;
        }
    }

    fn str(&mut self, s: &[u8]) -> &mut Self {
        for &b in s {
            self.byte(b);
        }
        self
    }

    fn hex(&mut self, n: u64) -> &mut Self {
        self.str(b"0x");
        for shift in (0..16).rev() {
            let nibble = ((n >> (shift * 4)) & 0xF) as u8;
            self.byte(if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 });
        }
        self
    }

    fn dec(&mut self, mut n: u64) -> &mut Self {
        // u64::MAX has 20 digits. Collect them least-significant first,
        // then emit in reverse. Iterators instead of index arithmetic keep
        // the compiler from emitting a bounds-check panic, which would pull
        // in core::fmt.
        let mut digits = [0u8; 20];
        let mut count = 0;
        for slot in digits.iter_mut() {
            *slot = b'0' + (n % 10) as u8;
            count += 1;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        for &d in digits.iter().take(count).rev() {
            self.byte(d);
        }
        self
    }

    fn as_bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or(&self.buf)
    }
}

// ---------------------------------------------------------------------------
// Register reads
// ---------------------------------------------------------------------------

fn read_cs() -> u16 {
    let cs: u16;
    unsafe {
        asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack, preserves_flags));
    }
    cs
}

fn read_cr2() -> u64 {
    let value: u64;
    unsafe {
        asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}
