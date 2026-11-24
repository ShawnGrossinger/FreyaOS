//! heap.rs — the kernel heap.
//!
//! Backs Rust's `alloc` crate (Box, Vec, String, BTreeMap, ...) for kernel
//! code. It is the variable-size allocator that capability tables and
//! other kernel objects will be built on.
//!
//! Design: a first-fit free list, kept sorted by address so neighbours
//! can be merged (coalesced) on free. Free blocks store their own
//! bookkeeping in place:
//!
//!   free block:  [ size: usize | next: *mut FreeBlock | ...zeros... ]
//!
//! Every block start and size is a multiple of 16, so splitting a block
//! never leaves a sliver too small to hold that 16-byte header.
//!
//! The heap lives in its own virtual region and grows on demand: when no
//! free block fits, it takes frames from pmm and maps them (KERNEL_DATA:
//! writable, no-execute) at the top of the heap. It never shrinks yet.
//!
//! Sanitising, matching the frame allocator's Raw -> Sanitized rule:
//! freed memory is zeroed immediately, and new heap pages arrive zeroed
//! from pmm. So every free byte is zero except free-block headers, and the
//! allocator clears the one header an allocation can overlap. The result:
//! every allocation is returned zeroed, and nothing one owner wrote is
//! ever visible to the next.
//!
//! Misuse halts with a message instead of corrupting memory: freeing a
//! pointer outside the heap, freeing something already free (double free),
//! allocating or freeing inside a hardware interrupt handler, or
//! re-entering the allocator.
//!
//! Concurrency: the allocator is behind a lock (Entry 31; lock.rs) that
//! also turns interrupts off, so a task switch can never land in the
//! middle of a free-list update.

use core::alloc::{GlobalAlloc, Layout};

use crate::lock::{IrqGuard, IrqLock};

use crate::pmm::{self, Kind, Owner, PAGE_SIZE};
use crate::vmm::{AddressSpace, PageFlags};

/// The heap's virtual region: PML4 slot 385, right after the general
/// dynamic region (vmm::KERNEL_DYNAMIC_BASE, slot 384).
pub const HEAP_BASE: u64 = 0xFFFF_C080_0000_0000;
/// How far it may grow.
pub const HEAP_MAX: u64 = 1024 * 1024 * 1024; // 1 GiB
/// Grow by at least this much at a time, so small allocations don't each
/// cost a page-table walk.
const GROW_STEP: u64 = 64 * 1024;

/// Block granularity and minimum size: room for the free-block header.
const BLOCK: usize = 16;

#[repr(C)]
struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

struct Heap {
    head: *mut FreeBlock, // lowest-addressed free block
    top: u64,             // end of the mapped heap (exclusive)
    in_use: u64,          // bytes handed out and not yet freed
    allocations: u64,
    frees: u64,
}

// SAFETY: [static] `head` points into the heap's own mapping in the kernel
// half, the same from every task and every context, and is only followed
// with the lock below held.
unsafe impl Send for Heap {}

/// Held for the duration of every alloc/free, with interrupts disabled.
/// With one CPU, finding it already held can only mean re-entry (for
/// example an exception raised inside the allocator, whose handler then
/// allocates).
static HEAP: IrqLock<Heap> = IrqLock::new(Heap {
    head: core::ptr::null_mut(),
    top: HEAP_BASE,
    in_use: 0,
    allocations: 0,
    frees: 0,
});

fn fatal(msg: &[u8], value: u64) -> ! {
    crate::serial_write_str(b"[heap] FATAL: ");
    crate::serial_write_str(msg);
    crate::serial_write_hex(value);
    crate::serial_write_str(b"\n");
    crate::halt();
}

/// The heap, locked until the guard goes.
fn lock() -> IrqGuard<'static, Heap> {
    // Interrupt handlers must not allocate: a tick can arrive at any
    // instruction, including ones that hold this lock or that the
    // scheduler is in the middle of.
    if crate::idt::irq_depth() > 0 {
        fatal(b"heap used inside a hardware interrupt handler, depth ", crate::idt::irq_depth());
    }
    match HEAP.try_lock() {
        Some(heap) => heap,
        None => fatal(b"allocator re-entered (allocating inside an exception handler?) ", 0),
    }
}

fn align_up(x: usize, align: usize) -> usize {
    (x + align - 1) & !(align - 1)
}

/// Bytes a layout actually occupies. Must give the same answer on alloc
/// and on free, since Rust hands the layout back to dealloc.
fn block_size(layout: Layout) -> usize {
    align_up(layout.size().max(BLOCK), BLOCK)
}

impl Heap {
    /// First fit: carve `size` bytes at `align` out of the first free block
    /// that can hold them. Leftover space in front of and behind the
    /// allocation stays on the list, in address order.
    ///
    /// # Safety
    /// The heap's lock is held (lock()). `size` is a multiple of BLOCK and
    /// `align` a power of two of at least BLOCK.
    unsafe fn take(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        // SAFETY: [heap] the free list's rules, which give and take keep: its
        // blocks are disjoint, in address order, BLOCK-aligned, at least BLOCK
        // bytes, inside the mapped heap [HEAP_BASE, top), and no allocation
        // overlaps one; each starts with its FreeBlock header. So every header read
        // or written here is mapped memory nobody else uses, and the lock
        // (# Safety) keeps it so. A front or tail left over is BLOCK-aligned and at
        // least BLOCK bytes, so it can carry a header of its own; an allocation
        // that starts where a header was gets those bytes zeroed.
        unsafe {
            let mut link: *mut *mut FreeBlock = &mut self.head;
            while !(*link).is_null() {
                let block = *link;
                let start = block as usize;
                let end = start + (*block).size;
                let alloc_start = align_up(start, align);
                let alloc_end = alloc_start + size;

                if alloc_end <= end {
                    let front = alloc_start - start; // a multiple of 16: 0 or a whole block
                    let back = end - alloc_end;
                    let mut rest = (*block).next;

                    if back >= BLOCK {
                        let tail = alloc_end as *mut FreeBlock;
                        (*tail).size = back;
                        (*tail).next = rest;
                        rest = tail;
                    }
                    if front >= BLOCK {
                        (*block).size = front;
                        (*block).next = rest;
                        rest = block;
                    } else {
                        // The allocation starts where this block's header was:
                        // clear it, so the caller gets all zeros.
                        core::ptr::write_bytes(start as *mut u8, 0, BLOCK);
                    }
                    *link = rest;
                    return Some(alloc_start as *mut u8);
                }
                link = &mut (*block).next;
            }
            None
        }
    }

    /// Put [start, start + size) on the free list, merging with adjacent
    /// free blocks. Halts if it overlaps a block that is already free.
    /// The range's bytes must already be zero.
    ///
    /// # Safety
    /// The heap's lock is held. [start, start + size) is mapped heap memory
    /// that no allocation is using, BLOCK-aligned, size a multiple of BLOCK
    /// and at least BLOCK, and all zeros.
    unsafe fn give(&mut self, start: usize, size: usize) {
        // SAFETY: [heap] the free list's rules, as in take(); the range is mapped,
        // unused, aligned and zeroed (# Safety). The walk reads only headers of
        // blocks on the list; a range overlapping one halts before anything is
        // written. Merging leaves one header where there were two, and the one
        // now inside a block is zeroed, so free memory stays all zeros.
        unsafe {
            let end = start + size;

            // Find the neighbours: `prev` ends at or before `start`, `next`
            // begins at or after `end`.
            let mut prev: *mut FreeBlock = core::ptr::null_mut();
            let mut next = self.head;
            while !next.is_null() && (next as usize) < start {
                prev = next;
                next = (*next).next;
            }
            if !prev.is_null() && prev as usize + (*prev).size > start {
                fatal(b"double free (inside an already-free block) at ", start as u64);
            }
            if !next.is_null() && (next as usize) < end {
                fatal(b"double free (overlaps an already-free block) at ", start as u64);
            }

            let block = start as *mut FreeBlock;
            (*block).size = size;
            (*block).next = next;

            // Merge with the following block.
            if !next.is_null() && end == next as usize {
                (*block).size += (*next).size;
                (*block).next = (*next).next;
                core::ptr::write_bytes(next as *mut u8, 0, BLOCK); // its header is now interior
            }

            // Link from, or merge into, the preceding block.
            if prev.is_null() {
                self.head = block;
            } else if prev as usize + (*prev).size == start {
                (*prev).size += (*block).size;
                (*prev).next = (*block).next;
                core::ptr::write_bytes(block as *mut u8, 0, BLOCK);
            } else {
                (*prev).next = block;
            }
        }
    }

    /// Map at least `min_bytes` more at the top of the heap and add it to
    /// the free list. False if the region is full or memory ran out; any
    /// pages mapped before the failure are released again.
    ///
    /// # Safety
    /// The heap's lock is held.
    unsafe fn grow(&mut self, min_bytes: u64) -> bool {
        let bytes = (min_bytes.max(GROW_STEP) + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        if self.top + bytes > HEAP_BASE + HEAP_MAX {
            return false;
        }

        let mut ks = AddressSpace::kernel();
        let mut mapped = 0;
        while mapped < bytes {
            let va = self.top + mapped;
            let ok = match pmm::alloc(Kind::KernelHeap, Owner::Kernel) {
                // pmm frames arrive zeroed, as give() requires.
                // SAFETY: [paging] the page just above the heap's top, in
                // its own region (checked against HEAP_MAX above), which
                // nothing maps or uses yet.
                Some(frame) => match unsafe { ks.map_frame(va, frame, PageFlags::KERNEL_DATA) } {
                    Ok(()) => true,
                    Err((_, frame)) => {
                        frame.free();
                        false
                    }
                },
                None => false,
            };
            if !ok {
                // Roll back this growth step.
                while mapped > 0 {
                    mapped -= PAGE_SIZE;
                    // SAFETY: [paging] a page this call just mapped above
                    // top, which no allocation has been given.
                    if let Ok(frame) = unsafe { ks.unmap_frame(self.top + mapped, Kind::KernelHeap.bit(), Owner::Kernel) } {
                        frame.free();
                    }
                }
                return false;
            }
            mapped += PAGE_SIZE;
        }

        let start = self.top as usize;
        self.top += bytes;
        // SAFETY: [heap] the pages just mapped: zeroed frames, BLOCK- (in
        // fact page-) aligned, a whole number of pages, used by nobody; the
        // lock is held (# Safety).
        unsafe { self.give(start, bytes as usize) };
        true
    }
}

// ===========================================================================
// GlobalAlloc: what Box, Vec, ... call
// ===========================================================================

pub struct KernelHeap;

// SAFETY: [heap] alloc returns null or a block of block_size(layout) >=
// layout.size() bytes at a multiple of max(layout.align(), BLOCK), taken off
// the free list, so no other live allocation overlaps it; dealloc puts back
// exactly that block (Rust passes the same layout, and block_size gives the
// same size). Both run under the heap's lock, with interrupts off, and halt
// rather than corrupt the list when handed something that isn't a block.
unsafe impl GlobalAlloc for KernelHeap {
    /// # Safety
    /// GlobalAlloc's: layout.size() is not zero.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut heap = lock();
        let size = block_size(layout);
        let align = layout.align().max(BLOCK);

        // SAFETY: [heap] the lock is held; size is block_size's (a multiple
        // of BLOCK) and align a power of two of at least BLOCK.
        let mut result = unsafe { heap.take(size, align) };
        // Grow by enough to fit even the worst-case alignment padding.
        // SAFETY: [heap] the lock is held, as grow and take require.
        if result.is_none() && unsafe { heap.grow((size + align) as u64) } {
            // SAFETY: [heap] as the first take.
            result = unsafe { heap.take(size, align) };
        }

        let ptr = match result {
            Some(p) => {
                heap.in_use += size as u64;
                heap.allocations += 1;
                p
            }
            None => core::ptr::null_mut(), // Rust's alloc error path takes over
        };
        ptr
    }

    /// # Safety
    /// GlobalAlloc's: ptr came from alloc with this same layout, and is
    /// freed once.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let mut heap = lock();
        let size = block_size(layout);
        let start = ptr as u64;

        if start < HEAP_BASE || start + size as u64 > heap.top || start % BLOCK as u64 != 0 {
            fatal(b"free of a pointer that is not a heap block: ", start);
        }

        // Sanitise before the memory goes back on the list.
        // SAFETY: [heap] the block alloc gave for this layout (# Safety),
        // inside the heap (checked above): size bytes, now unused. Zeroed,
        // then given back with the lock held, as give requires; give halts
        // if it overlaps a free block (a double free).
        unsafe {
            core::ptr::write_bytes(ptr, 0, size);
            heap.give(ptr as usize, size);
        }
        heap.in_use -= size as u64;
        heap.frees += 1;
    }
}

#[global_allocator]
pub(crate) static GLOBAL: KernelHeap = KernelHeap;

// ===========================================================================
// Stats
// ===========================================================================

pub struct HeapStats {
    pub mapped: u64,
    pub in_use: u64,
    pub free_blocks: u64,
    pub largest_free: u64,
    pub allocations: u64,
    pub frees: u64,
}

pub fn stats() -> HeapStats {
    let heap = lock();
    let mut free_blocks = 0;
    let mut largest_free = 0;
    let mut block = heap.head;
    while !block.is_null() {
        // SAFETY: [heap] a free block's header, on the list the lock keeps
        // still: mapped, and used by no allocation.
        unsafe {
            free_blocks += 1;
            largest_free = largest_free.max((*block).size as u64);
            block = (*block).next;
        }
    }
    let stats = HeapStats {
        mapped: heap.top - HEAP_BASE,
        in_use: heap.in_use,
        free_blocks,
        largest_free,
        allocations: heap.allocations,
        frees: heap.frees,
    };
    stats
}

// ===========================================================================
// Self-test
// ===========================================================================

/// Exercise the heap through the same types kernel code will use.
///
/// The raw-allocation checks call GLOBAL (this allocator) directly rather
/// than alloc::alloc::alloc. LLVM treats the latter as a known allocation
/// function and may delete an allocation that is freed without being used,
/// which would let a check pass without ever reaching this code.
pub fn selftest() -> bool {
    use alloc::boxed::Box;
    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec::Vec;
    use crate::selftest::check;

    let mut ok = true;
    let in_heap = |p: u64| (HEAP_BASE..HEAP_BASE + HEAP_MAX).contains(&p);

    // 1. Box: a value lands in the heap region and reads back.
    {
        let b = Box::new(0xF4E7_A0u64);
        let addr = &*b as *const u64 as u64;
        ok &= check(b"heap", b"Box<u64> lives in the heap region and reads back", in_heap(addr) && *b == 0xF4E7_A0);
    }

    // 2. Fresh allocations are zeroed (the sanitising invariant).
    // SAFETY: [heap] a non-zero layout; the 256 bytes are read only if alloc
    // gave them, and freed once with the same layout.
    unsafe {
        let layout = Layout::from_size_align(256, 16).unwrap();
        let p = GLOBAL.alloc(layout);
        let zeroed = !p.is_null() && (0..256).all(|i| p.add(i).read_volatile() == 0);
        if !p.is_null() {
            GLOBAL.dealloc(p, layout);
        }
        ok &= check(b"heap", b"a fresh 256-byte allocation is all zeros", zeroed);
    }

    // 3. Over-aligned allocation.
    // SAFETY: [heap] as test 2: one alloc, one dealloc, same layout.
    unsafe {
        let layout = Layout::from_size_align(64, 4096).unwrap();
        let p = GLOBAL.alloc(layout);
        let aligned = !p.is_null() && p as u64 % 4096 == 0;
        if !p.is_null() {
            GLOBAL.dealloc(p, layout);
        }
        ok &= check(b"heap", b"a 4096-aligned allocation is 4096-aligned", aligned);
    }

    // 4. Vec growth past one growth step: forces heap growth and realloc.
    {
        let mut v: Vec<u64> = Vec::new();
        for i in 0..20_000u64 {
            v.push(i);
        }
        let sum: u64 = v.iter().sum();
        ok &= check(b"heap", b"Vec<u64> of 20,000 (160 KiB, grows the heap) sums correctly",
                    v.len() == 20_000 && sum == 20_000 * 19_999 / 2);
    }

    // 5. String.
    {
        let mut text = String::new();
        for _ in 0..100 {
            text.push_str("FreyaOS ");
        }
        ok &= check(b"heap", b"String built from 100 pieces has the right length and content",
                    text.len() == 800 && text.starts_with("FreyaOS FreyaOS") && text.ends_with("FreyaOS "));
    }

    // 6. BTreeMap: many small allocations and frees in mixed order.
    {
        let mut map = BTreeMap::new();
        for k in 0..2_000u32 {
            map.insert(k, k * 3);
        }
        let lookups_ok = (0..2_000u32).all(|k| map.get(&k) == Some(&(k * 3)));
        map.retain(|k, _| k % 2 == 1);
        ok &= check(b"heap", b"BTreeMap with 2,000 entries: lookups, then removing half",
                    lookups_ok && map.len() == 1_000 && map.get(&2).is_none() && map.get(&3) == Some(&9));
    }

    // 7. Freed memory is zeroed straight away (past the 16-byte header the
    //    free list keeps in place).
    // SAFETY: [heap] writes only the 128 bytes alloc gave. After the free
    // it reads them again: still mapped (the heap never unmaps), and the
    // test runs alone at boot, so nothing has been given them meanwhile.
    // Only this test may read freed memory.
    unsafe {
        let layout = Layout::from_size_align(128, 16).unwrap();
        let p = GLOBAL.alloc(layout);
        let cleared = if p.is_null() {
            false
        } else {
            core::ptr::write_bytes(p, 0xAA, 128);
            GLOBAL.dealloc(p, layout);
            // Still mapped: the heap never unmaps. Reading it here is the
            // test, not something kernel code may do.
            (BLOCK..128).all(|i| core::ptr::read_volatile(p.add(i)) == 0)
        };
        ok &= check(b"heap", b"freed memory is zeroed immediately", cleared);
    }

    // 8. Everything above was dropped: nothing in use, and every free
    //    block merged back into one.
    let st = stats();
    ok &= check(b"heap", b"after all tests: 0 bytes in use, free space merged into 1 block",
                st.in_use == 0 && st.free_blocks == 1 && st.largest_free == st.mapped);

    crate::serial_write_str(b"[heap] region ");
    crate::serial_write_hex(HEAP_BASE);
    crate::serial_write_str(b": ");
    crate::serial_write_u64(st.mapped / 1024);
    crate::serial_write_str(b" KiB mapped, ");
    crate::serial_write_u64(st.allocations);
    crate::serial_write_str(b" allocations, ");
    crate::serial_write_u64(st.frees);
    crate::serial_write_str(b" frees\n");

    ok
}

/// Demo (halts): free the same allocation twice. Calls GLOBAL directly:
/// through alloc::alloc, LLVM may delete this unused alloc/free pair
/// entirely, and the double free never reaches the allocator.
pub fn demo_double_free() {
    // SAFETY: [fault] the second dealloc breaks GlobalAlloc's contract on
    // purpose: give() finds the block already free and halts the kernel
    // before anything is changed.
    unsafe {
        let layout = Layout::from_size_align(64, 16).unwrap();
        let p = GLOBAL.alloc(layout);
        GLOBAL.dealloc(p, layout);
        GLOBAL.dealloc(p, layout);
    }
}
