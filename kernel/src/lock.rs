//! lock.rs — kernel state shared between tasks and interrupt handlers,
//! reached only through types whose soundness doesn't rest on the callers
//! (Entry 31).
//!
//! Until Entry 31 such state lived in `static mut`s, reached through
//! functions that handed out `&'static mut`. What made that sound was a
//! rule written in comments: one CPU, interrupts off, one reference at a
//! time, none kept across a task switch. Entry 30's list showed it was the
//! largest group of unsafe code, and that only part of the rule was ever
//! checked.
//!
//! IrqLock<T>: `lock()` turns interrupts off and marks the lock held; the
//! guard it returns is the only way to the value, and the compiler keeps
//! every reference made through it inside the guard's life. Dropping the
//! guard lets the lock go and puts interrupts back as they were. Taking a
//! lock that is already held, by the same code again or by a task the
//! holder switched away to, halts the kernel: a second reference is never
//! made. So what used to be a rule for callers is now a property of the
//! type, and breaking the rule is a halt that says which lock and which
//! line, not memory corruption. (Interrupts off is what keeps a handler
//! from ever finding a lock held; the held flag is what makes it sound if
//! one did.)
//!
//! BootCell<T>: set once, at boot; after that anyone may read it, as a
//! `&'static T`. Setting it twice halts.
//!
//! sync::Mutex is the third kind: a lock a task sleeps on, for long work.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Halt, saying where: the lock's address (its symbol is in kernel.elf)
/// and the line that asked for it. The locks carry no name: a name would
/// move every one of them, with its value, out of .bss and into the
/// kernel image.
#[track_caller]
fn fatal(what: &[u8], at: u64) -> ! {
    let caller = core::panic::Location::caller();
    crate::serial_write_str(b"[lock] FATAL: the lock at ");
    crate::serial_write_hex(at);
    crate::serial_write_str(what);
    crate::serial_write_str(b" (asked for at ");
    crate::serial_write_str(caller.file().as_bytes());
    crate::serial_write_str(b":");
    crate::serial_write_u64(caller.line() as u64);
    crate::serial_write_str(b")\n");
    crate::halt();
}

/// Interrupts off; returns whether they were on.
fn interrupts_off() -> bool {
    let were_on = crate::cpu::interrupts_enabled();
    // SAFETY: [cpu] cli only masks interrupts; the guard (or try_lock's
    // failure path) turns them on again if they were on.
    unsafe { core::arch::asm!("cli", options(nomem, nostack)) };
    were_on
}

/// Interrupts on again, if `were_on`.
fn restore(were_on: bool) {
    if were_on {
        // SAFETY: [cpu] they were on when this lock's lock() turned them
        // off, and the lock has just been let go. If guards are dropped out
        // of order, interrupts can come back while another lock is held;
        // that lock's held flag still keeps its value to one reference.
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    }
}

// ===========================================================================
// IrqLock
// ===========================================================================

/// The value comes first: a lock whose value is lent to a process by
/// physical address (the log, the screen's cells, the console's
/// description) has the value's address as its own, page-aligned when the
/// value is (the kernel image is identity-mapped, so that address is
/// physical).
#[repr(C)]
pub struct IrqLock<T> {
    value: UnsafeCell<T>,
    held: AtomicBool,
}

// SAFETY: [static] at most one guard exists at a time: lock() and
// try_lock() make one only after swapping `held` from false to true, and
// only the guard's drop sets it false again. Every reference to the value
// is made through a guard and can't outlive it, so the value is reached by
// one holder at a time. T: Send, because whichever task holds the lock
// reaches the value.
unsafe impl<T: Send> Sync for IrqLock<T> {}

impl<T> IrqLock<T> {
    pub const fn new(value: T) -> Self {
        IrqLock { value: UnsafeCell::new(value), held: AtomicBool::new(false) }
    }

    /// Interrupts off, and the value. Halts if the lock is already held.
    #[track_caller]
    pub fn lock(&self) -> IrqGuard<'_, T> {
        let were_on = interrupts_off();
        if self.held.swap(true, Ordering::Acquire) {
            fatal(b" was taken while already held: by this same code, or by a task that switched away holding it", self.addr());
        }
        IrqGuard { lock: self, were_on, not_send: PhantomData }
    }

    /// lock(), or None if the lock is held. For output on a fatal path,
    /// which must not halt again for a lock the failure left held.
    pub fn try_lock(&self) -> Option<IrqGuard<'_, T>> {
        let were_on = interrupts_off();
        if self.held.swap(true, Ordering::Acquire) {
            restore(were_on);
            return None;
        }
        Some(IrqGuard { lock: self, were_on, not_send: PhantomData })
    }

    /// The value's address, for memory lent by physical address. Nothing
    /// is reached through it here.
    pub fn addr(&self) -> u64 {
        self.value.get() as u64
    }

    /// A raw pointer to the value, for the one place it must be reached
    /// across a task switch (sched::schedule's saved stack pointers), where
    /// no guard can be held. Halts if the lock is held; what is done
    /// through the pointer is the caller's to justify.
    #[track_caller]
    pub fn unheld(&self) -> *mut T {
        if self.held.load(Ordering::Acquire) {
            fatal(b" was held across a task switch", self.addr());
        }
        self.value.get()
    }
}

/// The holder of an IrqLock: dereferences to the value; dropping it lets
/// the lock go. Not Send: it stays with the task that took the lock.
pub struct IrqGuard<'a, T> {
    lock: &'a IrqLock<T>,
    were_on: bool,
    not_send: PhantomData<*const ()>,
}

impl<T> Deref for IrqGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: [static] this guard is the lock's only holder (see the
        // Sync impl), and the reference borrows the guard.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for IrqGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: [static] as deref; &mut self means no other reference made
        // through this guard is alive.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for IrqGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
        restore(self.were_on);
    }
}

impl<'a, T> IrqGuard<'a, T> {
    /// The guard, narrowed to part of the value (one task of the table,
    /// say): still holding the whole lock until it is dropped.
    pub fn map<U>(self, f: impl FnOnce(&mut T) -> &mut U) -> IrqMapped<'a, U> {
        let (lock, were_on) = (self.lock, self.were_on);
        core::mem::forget(self); // the IrqMapped lets the lock go instead
        // SAFETY: [static] the guard just given up was the lock's only
        // holder, and its hold passes to the IrqMapped made here, which
        // borrows the lock for 'a and lets it go when dropped.
        let value: &'a mut T = unsafe { &mut *lock.value.get() };
        IrqMapped { value: f(value), held: &lock.held, were_on, not_send: PhantomData }
    }
}

/// An IrqGuard narrowed by map(): the lock is held until this is dropped.
pub struct IrqMapped<'a, U> {
    value: &'a mut U,
    held: &'a AtomicBool,
    were_on: bool,
    not_send: PhantomData<*const ()>,
}

impl<U> Deref for IrqMapped<'_, U> {
    type Target = U;
    fn deref(&self) -> &U {
        self.value
    }
}

impl<U> DerefMut for IrqMapped<'_, U> {
    fn deref_mut(&mut self) -> &mut U {
        self.value
    }
}

impl<U> Drop for IrqMapped<'_, U> {
    fn drop(&mut self) {
        self.held.store(false, Ordering::Release);
        restore(self.were_on);
    }
}

// ===========================================================================
// BootCell
// ===========================================================================

const EMPTY: u8 = 0;
const SETTING: u8 = 1;
const SET: u8 = 2;

/// A value set once, at boot, then only read.
pub struct BootCell<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: [static] the value is written only by the one set() that moved
// `state` from EMPTY to SETTING, before it stores SET (Release); get() hands
// out shared references only after reading SET (Acquire), and nothing writes
// the value after that. T: Sync, since every task may read it; T: Send,
// since the task that sets it needn't be the one that reads it.
unsafe impl<T: Sync + Send> Sync for BootCell<T> {}

impl<T> BootCell<T> {
    pub const fn new() -> Self {
        BootCell { state: AtomicU8::new(EMPTY), value: UnsafeCell::new(MaybeUninit::uninit()) }
    }

    /// Set it, and return it. Halts if it was set already.
    #[track_caller]
    pub fn set(&self, value: T) -> &T {
        let at = self as *const Self as u64;
        if self.state.compare_exchange(EMPTY, SETTING, Ordering::Acquire, Ordering::Relaxed).is_err() {
            fatal(b" (a BootCell) was set a second time", at);
        }
        // SAFETY: [static] the exchange above made this call the only
        // writer, and no reader can see the value before SET is stored.
        unsafe { (*self.value.get()).write(value) };
        self.state.store(SET, Ordering::Release);
        match self.get() {
            Some(v) => v,
            None => fatal(b" (a BootCell) lost its value", at),
        }
    }

    /// The value, once set.
    pub fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) != SET {
            return None;
        }
        // SAFETY: [static] SET means set() wrote it, and nothing writes it
        // again; references handed out are shared ones.
        Some(unsafe { (*self.value.get()).assume_init_ref() })
    }
}

// ===========================================================================
// Self-test
// ===========================================================================

/// What can be checked without halting: a held lock refuses try_lock and
/// is free again after the guard goes, interrupts come back as they were,
/// and a BootCell is empty until set and the same after.
pub fn selftest() -> bool {
    static PROBE: IrqLock<u64> = IrqLock::new(0);
    static CELL: BootCell<u64> = BootCell::new();
    let were_on = crate::cpu::interrupts_enabled();
    let held_refused = {
        let mut g = PROBE.lock();
        *g += 1;
        let off = !crate::cpu::interrupts_enabled();
        off && PROBE.try_lock().is_none()
    };
    let free_again = PROBE.try_lock().map(|g| *g == 1).unwrap_or(false);
    let restored = crate::cpu::interrupts_enabled() == were_on;
    let empty = CELL.get().is_none();
    let set = *CELL.set(7) == 7 && CELL.get() == Some(&7);
    crate::selftest::check(
        b"lock",
        b"a held IrqLock refuses a second holder, interrupts are off while it is held and back as they were after, and it is free again; a BootCell is empty until set, then the same value",
        held_refused && free_again && restored && empty && set,
    )
}

/// Demo (halts): a lock taken while its holder still holds it.
pub fn demo_lock_twice() {
    static TWICE: IrqLock<u64> = IrqLock::new(0);
    let first = TWICE.lock();
    let second = TWICE.lock();
    core::hint::black_box((&first, &second));
}
