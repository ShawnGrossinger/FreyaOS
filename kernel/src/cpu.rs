//! cpu.rs — interrupt-flag helpers.
//!
//! With one CPU, "nobody else touches this while I do" means "no interrupt
//! arrives while I do", because an interrupt is the only way another task
//! (via the scheduler) or a handler can run in the middle of our code.
//! `without_interrupts` is the kernel's lock for shared state on a single
//! core: the frame allocator, page tables, the heap and the scheduler all
//! use it. It nests safely: it restores whatever state it found.

use core::arch::asm;

/// True if RFLAGS.IF is set (maskable interrupts enabled).
pub fn interrupts_enabled() -> bool {
    let rflags: u64;
    // SAFETY: [cpu] pushfq/pop only reads RFLAGS into a register, through
    // the stack it is already on; nothing else changes.
    unsafe { asm!("pushfq", "pop {}", out(reg) rflags, options(nomem, preserves_flags)) };
    rflags & (1 << 9) != 0
}

/// Run `f` with interrupts disabled, then restore the previous state.
/// Halt with a message if interrupts are on. Every function that relies on
/// `without_interrupts` for its locking calls this first, so calling it
/// without the lock fails at once instead of racing once in a blue moon
/// (a stress test can't be relied on to hit a window two instructions
/// wide). Costs a pushfq.
#[inline]
pub fn assert_interrupts_off(who: &[u8]) {
    if interrupts_enabled() {
        crate::serial_write_str(b"[cpu] FATAL: ");
        crate::serial_write_str(who);
        crate::serial_write_str(b" ran with interrupts enabled (its lock is missing)\n");
        crate::halt();
    }
}

pub fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
    let were_enabled = interrupts_enabled();
    if were_enabled {
        // SAFETY: [cpu] cli only masks interrupts; they were on, and are
        // turned on again below once f returns.
        unsafe { asm!("cli", options(nomem, nostack)) };
    }
    let result = f();
    if were_enabled {
        // SAFETY: [cpu] sti restores the state found on entry (interrupts
        // were on), so no caller sees them turned on where it had them off.
        unsafe { asm!("sti", options(nomem, nostack)) };
    }
    result
}

/// Read a model-specific register.
///
/// # Safety
/// `msr` must exist on this CPU (otherwise #GP).
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: [cpu] reading an MSR changes nothing; the caller promises it
    // exists, so it doesn't fault (# Safety above).
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    (hi as u64) << 32 | lo as u64
}

/// Write a model-specific register.
///
/// # Safety
/// `msr` must exist, and the value must be one the kernel can live with.
pub unsafe fn wrmsr(msr: u32, value: u64) {
    // SAFETY: [cpu] the caller promises the MSR exists and that the value
    // is one the kernel keeps working with (# Safety above).
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32,
             options(nostack, preserves_flags));
    }
}

/// CPUID leaf `leaf`, subleaf 0: (eax, ebx, ecx, edx). RBX is reserved by
/// LLVM, so it is saved around the instruction by hand.
pub fn cpuid(leaf: u32) -> (u32, u32, u32, u32) {
    let (eax, ecx, edx): (u32, u32, u32);
    let ebx: u64;
    // SAFETY: [cpu] cpuid only reads the CPU's description into registers;
    // RBX, which LLVM reserves, is saved and restored around it.
    unsafe {
        asm!(
            "mov {tmp}, rbx",
            "cpuid",
            "xchg {tmp}, rbx",
            tmp = out(reg) ebx,
            inout("eax") leaf => eax,
            inout("ecx") 0u32 => ecx,
            out("edx") edx,
            options(nomem, nostack, preserves_flags),
        );
    }
    (eax, ebx as u32, ecx, edx)
}

/// Turn on SMEP (CR4 bit 20: the kernel may not execute user pages) and
/// SMAP (bit 21: nor read or write them) where the CPU has them. The kernel
/// never needs either: it reaches process memory only through the physical
/// map. Returns which ones are on. The T430's Ivy Bridge has SMEP only.
pub fn enable_smep_smap() -> (bool, bool) {
    const CR4_SMEP: u64 = 1 << 20;
    const CR4_SMAP: u64 = 1 << 21;
    if cpuid(0).0 < 7 {
        return (false, false);
    }
    let features = cpuid(7).1;
    let smep = features & (1 << 7) != 0;
    let smap = features & (1 << 20) != 0;
    // SAFETY: [cpu] sets only CR4.SMEP and CR4.SMAP, and only where CPUID
    // says the CPU has them (otherwise the write would #GP). Neither
    // changes what the kernel itself does: it never runs or touches user
    // pages directly (the physical map is how it reaches process memory).
    unsafe {
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        if smep {
            cr4 |= CR4_SMEP;
        }
        if smap {
            cr4 |= CR4_SMAP;
        }
        asm!("mov cr4, {}", in(reg) cr4, options(nostack, preserves_flags));
    }
    (smep, smap)
}

/// (SMEP, SMAP) as CR4 has them now.
pub fn smep_smap_enabled() -> (bool, bool) {
    let cr4: u64;
    // SAFETY: [cpu] reading CR4 changes nothing.
    unsafe { asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags)) };
    (cr4 & (1 << 20) != 0, cr4 & (1 << 21) != 0)
}

/// MXCSR as the kernel expects it: every SSE exception masked, round to
/// nearest (0x1F80; the low 6 bits are sticky exception flags and may be
/// set by the kernel's own arithmetic).
const KERNEL_MXCSR: u32 = 0x1F80;

/// Halt if MXCSR isn't the kernel's. Called on every entry from ring 3:
/// the entry stubs must have swapped in the kernel's settings, or a process
/// would control rounding and SSE exceptions for kernel code. A missing
/// swap is otherwise invisible in QEMU, which doesn't raise #XM.
pub fn assert_kernel_mxcsr(who: &[u8]) {
    let mut mxcsr: u32 = 0;
    // SAFETY: [cpu] stmxcsr writes 4 bytes, MXCSR, to `mxcsr`, a local u32.
    unsafe { asm!("stmxcsr [{}]", in(reg) &raw mut mxcsr, options(nostack, preserves_flags)) };
    if mxcsr & !0x3F != KERNEL_MXCSR {
        crate::serial_write_str(b"[cpu] FATAL: ");
        crate::serial_write_str(who);
        crate::serial_write_str(b" left the process's MXCSR in place: ");
        crate::serial_write_hex(mxcsr as u64);
        crate::serial_write_str(b"\n");
        crate::halt();
    }
}

/// Print "[kernel] FATAL: <msg><value>" and halt: a kernel invariant broke.
pub fn fatal(msg: &[u8], value: u64) -> ! {
    crate::serial_write_str(b"[kernel] FATAL: ");
    crate::serial_write_str(msg);
    crate::serial_write_hex(value);
    crate::serial_write_str(b"\n");
    crate::halt();
}
