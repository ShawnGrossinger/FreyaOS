//! pmm.rs — physical memory manager: the frame allocator.
//!
//! One bit per 4 KiB frame (0 = free, 1 = used). The bitmap is sized from
//! the E820 map at boot, so it covers every usable frame however much RAM
//! the machine has and however the firmware laid it out. On a 16 GiB
//! machine, RAM displaced by the PCI hole sits above 4 GiB, so physical
//! addresses run to 17 GiB; the bitmap covers all of it.
//!
//! The bitmap lives in ordinary RAM, wherever the first fit is, and the
//! kernel reaches it through the physical map. That creates a bootstrap
//! loop: the physical map needs frames for its page tables before the
//! allocator exists. It is broken in three steps:
//!
//!   1. `EarlyFrames` hands out a handful of frames directly after the
//!      kernel image, inside the identity map, for those page tables.
//!   2. `paging::map_physical_memory()` builds the physical map with them.
//!   3. `init()` places the bitmap, then reserves the low 1 MiB, the kernel
//!      image, the early frames actually used, and the bitmap itself.
//!
//! Memory model: an allocated frame moves Raw -> Sanitized (zeroed before
//! it is handed out). Freeing returns it to Raw. Typed allocation
//! (Sanitized -> Typed) arrives with the capability layer.
//!
//! Single CPU, interrupts off: no locking yet. That changes with SMP or
//! once interrupt handlers allocate.

use crate::boot::E820Entry;
use crate::paging;

pub const PAGE_SIZE: u64 = 4096;

/// Everything below 1 MiB stays reserved: IVT, BIOS data area, the E820
/// buffer at 0x500, stage2, the kernel stack (0x70000-0x80000), the EBDA,
/// VGA memory and the BIOS ROM.
const LOW_MEMORY_END: u64 = 0x10_0000;

extern "C" {
    // From kernel.ld: first byte of the image and the first byte after
    // its .bss, page-aligned.
    static __kernel_start: u8;
    static __kernel_end: u8;
}

/// Physical [start, end) of the kernel image including .bss.
pub fn kernel_image() -> (u64, u64) {
    let start = &raw const __kernel_start as u64;
    let end = &raw const __kernel_end as u64;
    (start, end)
}

fn align_up(x: u64, align: u64) -> u64 {
    (x + align - 1) & !(align - 1)
}

fn align_down(x: u64, align: u64) -> u64 {
    x & !(align - 1)
}

fn fatal(msg: &[u8], value: u64) -> ! {
    crate::serial_write_str(b"[pmm] FATAL: ");
    crate::serial_write_str(msg);
    crate::serial_write_hex(value);
    crate::serial_write_str(b"\n");
    crate::halt();
}

/// Copy the fields out of a packed E820 entry: (base, end, type).
fn region(entry: &E820Entry) -> (u64, u64, u32) {
    let (base, length, kind) = (entry.base, entry.length, entry.region_type);
    (base, base.saturating_add(length), kind)
}

/// (base, end) of a usable region, or None for any other type.
fn usable(entry: &E820Entry) -> Option<(u64, u64)> {
    let (base, end, _) = region(entry);
    entry.is_usable().then_some((base, end))
}

/// True if [start, end) lies entirely inside one usable (type 1) region.
fn inside_usable(e820: &[E820Entry], start: u64, end: u64) -> bool {
    e820.iter().any(|e| {
        let (base, top, _) = region(e);
        e.is_usable() && base <= start && end <= top
    })
}

// ===========================================================================
// Step 1: early frames for the physical map's page tables
// ===========================================================================

/// A bump allocator over a small run of frames right after the kernel.
/// Its frames are identity-mapped, so they can be zeroed and written as
/// page tables before the physical map exists.
pub struct EarlyFrames {
    start: u64,
    next: u64,
    end: u64,
}

impl EarlyFrames {
    /// Reserve room for `count` frames directly after the kernel image.
    /// Halts if that run isn't usable RAM inside the identity map: nothing
    /// else could hold the physical map's tables at this point.
    pub fn after_kernel(e820: &[E820Entry], count: u64) -> Self {
        let (_, kernel_end) = kernel_image();
        let start = align_up(kernel_end, PAGE_SIZE);
        let end = start + count * PAGE_SIZE;

        if end > paging::IDENTITY_MAP_BYTES {
            fatal(b"early page-table frames would end past the identity map at ", end);
        }
        if !inside_usable(e820, start, end) {
            fatal(b"no usable RAM for early page-table frames at ", start);
        }

        EarlyFrames { start, next: start, end }
    }

    /// Next zeroed frame, or None when the run is used up.
    pub fn take(&mut self) -> Option<u64> {
        if self.next >= self.end {
            return None;
        }
        let frame = self.next;
        self.next += PAGE_SIZE;
        // Identity-mapped (checked in after_kernel).
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE as usize) };
        Some(frame)
    }

    /// [start, end) of the frames actually handed out. Only these stay
    /// reserved; the rest of the run goes back to the allocator.
    pub fn used(&self) -> (u64, u64) {
        (self.start, self.next)
    }
}

// ===========================================================================
// Step 3: the bitmap allocator
// ===========================================================================

struct FrameAllocator {
    bitmap: *mut u64,  // physical-map address of the first bitmap word
    words: usize,      // bitmap length in u64 words
    frames: u64,       // frames covered (physical [0, frames * 4 KiB))
    hint: usize,       // no free frame exists in any word before this one
    free: u64,         // free-frame counter
}

static mut PMM: FrameAllocator = FrameAllocator {
    bitmap: core::ptr::null_mut(),
    words: 0,
    frames: 0,
    hint: 0,
    free: 0,
};

impl FrameAllocator {
    /// Mark frames [first, last) used or free. Whole 64-frame words are
    /// written in one store; only the ragged ends go bit by bit.
    unsafe fn mark(&mut self, first: u64, last: u64, used: bool) {
        let last = last.min(self.frames);
        let mut frame = first;
        while frame < last {
            let word = self.bitmap.add((frame / 64) as usize);
            if frame % 64 == 0 && last - frame >= 64 {
                *word = if used { u64::MAX } else { 0 };
                frame += 64;
            } else {
                let bit = 1u64 << (frame % 64);
                *word = if used { *word | bit } else { *word & !bit };
                frame += 1;
            }
        }
    }

    unsafe fn mark_bytes(&mut self, start: u64, end: u64, used: bool) {
        if used {
            // Reserving: cover every frame the range touches.
            self.mark(start / PAGE_SIZE, align_up(end, PAGE_SIZE) / PAGE_SIZE, true);
        } else {
            // Freeing: only frames the range covers completely.
            self.mark(align_up(start, PAGE_SIZE) / PAGE_SIZE, end / PAGE_SIZE, false);
        }
    }

    /// Count free frames the slow way, straight from the bitmap.
    unsafe fn count_free(&self) -> u64 {
        let mut free = 0;
        for i in 0..self.words {
            free += (*self.bitmap.add(i)).count_zeros() as u64;
        }
        free
    }

    unsafe fn is_free(&self, frame: u64) -> bool {
        frame < self.frames && *self.bitmap.add((frame / 64) as usize) & (1 << (frame % 64)) == 0
    }
}

/// What init() set up, for the boot log.
pub struct PmmStats {
    pub frames: u64,
    pub free_frames: u64,
    pub usable_bytes: u64,
    pub bitmap_phys: u64,
    pub bitmap_bytes: u64,
}

/// Size and place the bitmap, then build it from the E820 map.
///
/// # Safety
/// Call once, after paging::map_physical_memory(), passing the range the
/// early frames used.
pub unsafe fn init(e820: &[E820Entry], early_used: (u64, u64)) -> PmmStats {
    // --- Size: every frame up to the end of the highest usable region ---
    let mut top = 0u64;
    let mut usable_bytes = 0u64;
    for (base, end) in e820.iter().filter_map(usable) {
        top = top.max(end);
        usable_bytes += end - base;
    }
    // The bitmap can only track what the physical map can reach.
    let top = align_down(top.min(paging::PHYS_MAP_MAX), PAGE_SIZE);
    let frames = top / PAGE_SIZE;
    let words = frames.div_ceil(64) as usize;
    let bitmap_bytes = align_up(words as u64 * 8, PAGE_SIZE);

    // --- Place: first usable run at or above the early frames ---
    let floor = align_up(early_used.1.max(LOW_MEMORY_END), PAGE_SIZE);
    let mut bitmap_phys = 0u64;
    for (base, end) in e820.iter().filter_map(usable) {
        let start = align_up(base.max(floor), PAGE_SIZE);
        if start + bitmap_bytes <= end {
            bitmap_phys = start;
            break;
        }
    }
    if bitmap_phys == 0 {
        fatal(b"no usable RAM large enough for the frame bitmap, bytes needed: ", bitmap_bytes);
    }

    let pmm = &mut *(&raw mut PMM);
    *pmm = FrameAllocator {
        bitmap: paging::phys_to_virt(bitmap_phys) as *mut u64,
        words,
        frames,
        hint: 0,
        free: 0,
    };

    // --- Build: deny everything, open usable RAM, then close what we use ---
    // Bits past `frames` in the last word stay 1 forever, so the allocator
    // can never hand out a frame the bitmap doesn't really cover.
    for i in 0..words {
        *pmm.bitmap.add(i) = u64::MAX;
    }
    for (base, end) in e820.iter().filter_map(usable) {
        pmm.mark_bytes(base, end, false);
    }
    let (kernel_start, kernel_end) = kernel_image();
    pmm.mark_bytes(0, LOW_MEMORY_END, true);
    pmm.mark_bytes(kernel_start, kernel_end, true);
    pmm.mark_bytes(early_used.0, early_used.1, true);
    pmm.mark_bytes(bitmap_phys, bitmap_phys + bitmap_bytes, true);

    pmm.free = pmm.count_free();

    PmmStats {
        frames,
        free_frames: pmm.free,
        usable_bytes,
        bitmap_phys,
        bitmap_bytes,
    }
}

/// Allocate a frame, zero it (Raw -> Sanitized), return its physical
/// address. None when memory is exhausted.
///
/// Scans 64 frames per step, starting at the hint instead of frame 0.
#[inline(never)]
pub unsafe fn alloc_frame() -> Option<usize> {
    let pmm = &mut *(&raw mut PMM);
    if pmm.bitmap.is_null() {
        fatal(b"alloc_frame called before pmm::init, caller near ", 0);
    }

    for w in pmm.hint..pmm.words {
        let word = *pmm.bitmap.add(w);
        if word != u64::MAX {
            let bit = (!word).trailing_zeros() as u64;
            *pmm.bitmap.add(w) = word | (1 << bit);
            pmm.hint = w;
            pmm.free -= 1;

            let phys = (w as u64 * 64 + bit) * PAGE_SIZE;
            core::ptr::write_bytes(paging::phys_to_virt(phys), 0, PAGE_SIZE as usize);
            return Some(phys as usize);
        }
    }

    pmm.hint = pmm.words;
    None
}

/// Return a frame (-> Raw). It is zeroed again on its next allocation.
/// Halts on a misaligned address, a frame outside the bitmap, or a double
/// free: each one means some kernel code has lost track of what it owns.
///
/// # Safety
/// The caller must own `phys` (it came from alloc_frame) and must not use
/// it afterwards. The bitmap can't tell an allocated frame from a reserved
/// one (kernel image, bitmap, low memory), so freeing one of those is not
/// caught and corrupts the allocator.
pub unsafe fn free_frame(phys: usize) {
    let pmm = &mut *(&raw mut PMM);
    let phys = phys as u64;
    let frame = phys / PAGE_SIZE;

    if phys % PAGE_SIZE != 0 {
        fatal(b"free_frame of a misaligned address ", phys);
    }
    if frame >= pmm.frames {
        fatal(b"free_frame of a frame outside the bitmap ", phys);
    }

    let w = (frame / 64) as usize;
    let bit = 1u64 << (frame % 64);
    let word = pmm.bitmap.add(w);
    if *word & bit == 0 {
        fatal(b"double free of frame ", phys);
    }

    *word &= !bit;
    pmm.free += 1;
    if w < pmm.hint {
        pmm.hint = w;
    }
}

/// Free frames right now.
pub fn free_frames() -> u64 {
    unsafe { (*(&raw const PMM)).free }
}

// ===========================================================================
// Self-test
// ===========================================================================

/// 1. The free counter agrees with a full recount of the bitmap.
/// 2. The last usable frame is tracked and free (the old fixed-size bitmap
///    silently dropped everything above 16 GiB of physical address).
/// 3. alloc -> free -> alloc returns the same frame (the hint moves back).
/// 4. The counter goes down by one and back up across alloc/free.
///
/// # Safety
/// Call after init().
pub unsafe fn selftest(e820: &[E820Entry]) -> bool {
    use crate::{serial_write_hex as hex, serial_write_str as s, serial_write_u64 as dec};

    let pmm = &*(&raw const PMM);

    let counted = pmm.count_free();
    let count_ok = counted == pmm.free;
    s(b"[pmm] self-test free counter (");
    dec(pmm.free);
    s(b") matches bitmap recount (");
    dec(counted);
    s(b")");
    crate::selftest::verdict(count_ok);

    let mut last = 0u64;
    for (base, end) in e820.iter().filter_map(usable) {
        if end >= base + PAGE_SIZE {
            last = last.max(align_down(end, PAGE_SIZE) - PAGE_SIZE);
        }
    }
    let last = last.min(paging::PHYS_MAP_MAX - PAGE_SIZE);
    let top_ok = pmm.is_free(last / PAGE_SIZE);
    s(b"[pmm] self-test last usable frame ");
    hex(last);
    s(b" is tracked and free");
    crate::selftest::verdict(top_ok);

    let before = free_frames();
    let round_trip_ok = match alloc_frame() {
        Some(a) => {
            let during = free_frames();
            free_frame(a);
            let after_free = free_frames();
            match alloc_frame() {
                Some(b) => {
                    free_frame(b);
                    a == b && during == before - 1 && after_free == before && free_frames() == before
                }
                None => false,
            }
        }
        None => false,
    };
    s(b"[pmm] self-test alloc/free/alloc returns the same frame, counter tracks it");
    crate::selftest::verdict(round_trip_ok);

    count_ok && top_ok && round_trip_ok
}
