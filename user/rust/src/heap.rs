//! heap.rs — Box, Vec and String for user programs (Entry 26).
//!
//! The memory comes from a Budget capability. A program names the slot
//! once, with use_budget, and from then on the allocator grows that
//! capability's heap (system call 28) as it needs more: GROW pages at a
//! time, or just what one allocation needs if the budget won't allow GROW,
//! counting a free block at the heap's end, so a budget can be used to its
//! last page.
//! Never past what the budget allows: an allocation that can't be satisfied
//! returns null, which Vec::try_reserve reports as an error and Vec::push or
//! Box::new turn into a panic, ending the program. A program that never
//! calls use_budget has no heap, and every allocation fails.
//!
//! First fit over a list of free blocks kept in address order, inside the
//! free blocks themselves; a freed block is merged with free neighbours.
//! Every block is a multiple of UNIT bytes and UNIT-aligned. Freed memory
//! stays in the heap for the next allocation: nothing is handed back to the
//! budget until the program ends, or calls release with nothing in use.
//!
//! One thread: a program has no other, and the kernel never runs a
//! program's code on its behalf, so nothing here needs a lock.
//!
//! If the Budget is revoked, the pages go with it (the kernel frees them),
//! and the next touch of anything on the heap is a page fault.

use crate::{cap_info, KIND_BUDGET, PAGE};
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::null_mut;

/// Smallest block and alignment: room for a free block's header.
const UNIT: usize = 16;
/// Pages to grow by when the free blocks can't satisfy an allocation.
const GROW: u64 = 16;

/// A free block's header, at its start.
struct Free {
    size: usize,
    next: *mut Free,
}

struct State {
    /// The Budget's slot plus one; 0 before use_budget. (All of State
    /// starts as zeros: user.ld allows no initialised writable data.)
    slot_plus_one: u64,
    /// Where the heap starts, and how many pages long it is.
    base: usize,
    pages: u64,
    /// Free blocks, in address order.
    free: *mut Free,
    /// Bytes handed out and not yet freed (rounded up to UNIT).
    in_use: usize,
}

pub struct Heap(UnsafeCell<State>);

// One thread (see above).
unsafe impl Sync for Heap {}

#[global_allocator]
static HEAP: Heap = Heap(UnsafeCell::new(State { slot_plus_one: 0, base: 0, pages: 0, free: null_mut(), in_use: 0 }));

fn state() -> &'static mut State {
    unsafe { &mut *HEAP.0.get() }
}

/// Take memory from the Budget capability in `slot` from now on. Err with
/// the kernel's answer if `slot` holds no Budget. A heap the capability
/// already has is kept as it is, and not used: the allocator can't know
/// what is in it.
pub fn use_budget(slot: u64) -> Result<(), i64> {
    let (kind, _, _, own) = cap_info(slot)?;
    if kind != KIND_BUDGET {
        return Err(crate::ERR_WRONG_CAPABILITY);
    }
    let at = crate::heap(slot, own); // the same length: only asks where
    if at < 0 {
        return Err(at);
    }
    let s = state();
    *s = State { slot_plus_one: slot + 1, base: at as usize, pages: own, free: null_mut(), in_use: 0 };
    Ok(())
}

/// Give the whole heap back to the budget, if nothing on it is in use:
/// true if it was (or there was no heap). The next allocation grows it
/// again.
pub fn release() -> bool {
    let s = state();
    if s.slot_plus_one == 0 || s.pages == 0 {
        return true;
    }
    if s.in_use != 0 || crate::heap(s.slot_plus_one - 1, 0) < 0 {
        return false;
    }
    s.pages = 0;
    s.free = null_mut();
    true
}

/// What the heap holds now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stats {
    /// The heap's length in bytes (its pages).
    pub bytes: u64,
    /// Bytes handed out and not yet freed.
    pub in_use: u64,
    /// Bytes in free blocks, and how many blocks.
    pub free: u64,
    pub free_blocks: u64,
}

pub fn stats() -> Stats {
    let s = state();
    let (mut free, mut blocks) = (0, 0);
    let mut b = s.free;
    while !b.is_null() {
        unsafe {
            free += (*b).size as u64;
            blocks += 1;
            b = (*b).next;
        }
    }
    Stats { bytes: s.pages * PAGE, in_use: s.in_use as u64, free, free_blocks: blocks }
}

fn round(n: usize) -> usize {
    (n.max(UNIT) + UNIT - 1) & !(UNIT - 1)
}

impl State {
    /// First fit: unlink a free block with room for `size` bytes at an
    /// `align`ed address, and give back what is left on either side.
    unsafe fn take(&mut self, size: usize, align: usize) -> Option<usize> {
        let mut link: *mut *mut Free = &mut self.free;
        while !(*link).is_null() {
            let block = *link;
            let (at, len) = (block as usize, (*block).size);
            let start = (at + align - 1) & !(align - 1);
            if start + size <= at + len {
                *link = (*block).next;
                if start > at {
                    self.give(at, start - at);
                }
                if start + size < at + len {
                    self.give(start + size, at + len - (start + size));
                }
                return Some(start);
            }
            link = &mut (*block).next;
        }
        None
    }

    /// Put [at, at + size) on the free list, in address order, merged with
    /// a free block just before it and one just after it.
    unsafe fn give(&mut self, at: usize, size: usize) {
        let mut before: *mut Free = null_mut();
        let mut after = self.free;
        while !after.is_null() && (after as usize) < at {
            before = after;
            after = (*after).next;
        }
        let block = at as *mut Free;
        (*block).size = size;
        (*block).next = after;
        if !after.is_null() && at + size == after as usize {
            (*block).size += (*after).size;
            (*block).next = (*after).next;
        }
        if before.is_null() {
            self.free = block;
        } else if before as usize + (*before).size == at {
            (*before).size += (*block).size;
            (*before).next = (*block).next;
        } else {
            (*before).next = block;
        }
    }

    /// How many more pages an allocation of `size` bytes at `align` needs:
    /// from the start of a free block that reaches the heap's end, if there
    /// is one, else from the end.
    unsafe fn pages_for(&self, size: usize, align: usize) -> u64 {
        let end = self.base + (self.pages * PAGE) as usize;
        let mut from = end;
        let mut b = self.free;
        while !b.is_null() {
            if b as usize + (*b).size == end {
                from = b as usize;
            }
            b = (*b).next;
        }
        let start = (from + align - 1) & !(align - 1);
        ((start + size).saturating_sub(end) as u64).div_ceil(PAGE).max(1)
    }

    /// Make the heap `more` pages longer, and free the new pages.
    unsafe fn grow(&mut self, more: u64) -> Result<(), i64> {
        let at = crate::heap(self.slot_plus_one - 1, self.pages + more);
        if at < 0 {
            return Err(at);
        }
        let start = self.base + (self.pages * PAGE) as usize;
        self.pages += more;
        self.give(start, (more * PAGE) as usize);
        Ok(())
    }
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let s = state();
        if s.slot_plus_one == 0 {
            return null_mut(); // no budget, no heap
        }
        let size = round(layout.size());
        let align = layout.align().max(UNIT);
        let found = s.take(size, align).or_else(|| {
            // GROW pages if the budget allows, else just what this needs.
            let need = s.pages_for(size, align);
            if s.grow(need.max(GROW)).is_err() && (need >= GROW || s.grow(need).is_err()) {
                return None;
            }
            s.take(size, align)
        });
        match found {
            Some(at) => {
                s.in_use += size;
                at as *mut u8
            }
            None => null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let s = state();
        let size = round(layout.size());
        s.in_use -= size;
        s.give(ptr as usize, size);
    }
}
