//! sync.rs — a mutex for kernel code that runs in task context.
//!
//! cpu::without_interrupts is the right lock for a handful of instructions
//! (and the only one usable inside an interrupt handler), but holding it
//! for long stops the clock. A Mutex is for longer critical sections: a
//! task that finds it taken is put to sleep (sched::block) until the owner
//! releases it, and ownership then passes straight to whoever has waited
//! longest. Interrupts stay on throughout, and the holder can even be
//! preempted.
//!
//! Rules, each checked (and fatal if broken): never from an interrupt
//! handler; never locked twice by the same task (that would wait forever);
//! released only by the task that holds it. The guard releases it.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU64, Ordering};

pub struct Mutex<T> {
    /// Task id of the holder, or sched::FREE.
    owner: AtomicU64,
    /// Times a task had to wait for it.
    contended: AtomicU64,
    data: UnsafeCell<T>,
}

// SAFETY: [static] the mutex hands out access to `data` to one task at a
// time: a MutexGuard exists only between mutex_acquire, which returns once
// `owner` is this task, and mutex_release in its Drop, which passes it on.
// T: Send because the value is reached from whichever task holds it.
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Mutex { owner: AtomicU64::new(crate::sched::FREE), contended: AtomicU64::new(0), data: UnsafeCell::new(value) }
    }

    /// Wait for the mutex (asleep, not spinning) and take it.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        crate::sched::mutex_acquire(self.key(), &self.owner, &self.contended);
        MutexGuard { mutex: self }
    }

    /// Times a task found it taken and had to wait.
    pub fn contended(&self) -> u64 {
        self.contended.load(Ordering::Relaxed)
    }

    fn key(&self) -> usize {
        self as *const Self as usize
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: [static] this guard's task holds the mutex until the guard
        // drops, and the reference can't outlive the guard.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: [static] as deref; &mut self means no other reference made
        // through this guard is alive, and no other guard exists.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        crate::sched::mutex_release(self.mutex.key(), &self.mutex.owner);
    }
}

// ===========================================================================
// Self-test
// ===========================================================================

const WORKERS: u64 = 3;
const ROUNDS: u64 = 100;

static COUNTER: Mutex<u64> = Mutex::new(0);

/// Read, give up the CPU, write back: without the mutex, the other tasks
/// run in between and their increments are lost every time.
fn increment_task(_: u64) {
    for _ in 0..ROUNDS {
        let mut counter = COUNTER.lock();
        let seen = *counter;
        crate::sched::yield_now();
        *counter = seen + 1;
    }
}

pub fn selftest() -> bool {
    let mut ids = [None; WORKERS as usize];
    for slot in ids.iter_mut() {
        *slot = crate::sched::spawn("mutex", increment_task, 0).ok();
    }
    for id in ids.iter().flatten() {
        crate::sched::join_timeout(*id, 500);
    }
    let total = *COUNTER.lock();
    let waits = COUNTER.contended();
    crate::serial_write_str(b"[sync] self-test 3 tasks, 100 increments each, yielding inside the lock: total ");
    crate::serial_write_u64(total);
    crate::serial_write_str(b", ");
    crate::serial_write_u64(waits);
    crate::serial_write_str(b" waits");
    crate::selftest::verdict(ids.iter().all(|i| i.is_some()) && total == WORKERS * ROUNDS && waits > 0)
}
