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
        unsafe { asm!("cli", options(nomem, nostack)) };
    }
    let result = f();
    if were_enabled {
        unsafe { asm!("sti", options(nomem, nostack)) };
    }
    result
}
