//! paging.rs — kernel-owned page tables.
//!
//! Stage2 left a minimal 4-level hierarchy at 0x200000 that identity-maps
//! the first 8 MiB with four 2 MiB pages. This module builds the kernel's
//! own hierarchy and switches CR3 to it. The mapping stays the same 8 MiB
//! identity map (virtual == physical), with one change:
//!
//!   The first 2 MiB is mapped with 4 KiB pages instead of one 2 MiB page,
//!   so a single 4 KiB page can be left unmapped: the guard page directly
//!   below the kernel stack. A stack overflow now hits a non-present page
//!   and faults (escalating to #DF on its IST stack, see gdt.rs) instead of
//!   silently overwriting whatever lies below 0x70000.
//!
//! Mapping all usable RAM comes next; the frame allocator already hands out
//! frames above 8 MiB that nothing maps yet.
//!
//! Everything here relies on the identity map: the tables are statics whose
//! addresses are physical addresses. That stops being true the day the
//! kernel moves to a higher-half address.

use core::arch::asm;

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Set by stage2 (KERNEL_STACK_TOP). The stack is [0x70000, 0x80000).
pub const KERNEL_STACK_TOP: u64 = 0x0008_0000;
pub const KERNEL_STACK_SIZE: u64 = 0x0001_0000; // 64 KiB
pub const KERNEL_STACK_BOTTOM: u64 = KERNEL_STACK_TOP - KERNEL_STACK_SIZE;

/// The 4 KiB page directly below the stack, left unmapped.
pub const STACK_GUARD_PAGE: u64 = KERNEL_STACK_BOTTOM - PAGE_SIZE;

const PAGE_SIZE: u64 = 4096;
const HUGE_PAGE_SIZE: u64 = 2 * 1024 * 1024;

/// Same reach as stage2's map: 4 x 2 MiB.
const IDENTITY_MAP_BYTES: u64 = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------
//
// Entry flag bits used here (all levels share the low bits):
//   bit 0  P   present
//   bit 1  RW  writable
//   bit 7  PS  in a PD entry: this entry maps a 2 MiB page directly
// Bits 51:12 hold the physical address of the next table or the page.
// The NX bit (63) is reserved until EFER.NXE is enabled, so it stays 0.

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const HUGE: u64 = 1 << 7;

const ENTRIES: usize = 512;

/// One 4 KiB-aligned table of 512 entries. Every paging level uses this
/// same shape; what an entry means depends on the level it's in.
#[repr(C, align(4096))]
struct PageTable([u64; ENTRIES]);

const _: () = assert!(core::mem::size_of::<PageTable>() == 4096);

// Four tables, 16 KiB, in .bss (zeroed by _start, so every entry starts
// not present).
static mut PML4: PageTable = PageTable([0; ENTRIES]);
static mut PDPT: PageTable = PageTable([0; ENTRIES]);
static mut PD: PageTable = PageTable([0; ENTRIES]);
static mut PT_LOW: PageTable = PageTable([0; ENTRIES]); // first 2 MiB, 4 KiB pages

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

/// Build the tables and switch CR3. Returns the new CR3 value.
pub fn init() -> u64 {
    unsafe {
        let pml4 = &raw mut PML4;
        let pdpt = &raw mut PDPT;
        let pd = &raw mut PD;
        let pt_low = &raw mut PT_LOW;

        // --- PT_LOW: 0 .. 2 MiB in 4 KiB pages, except the guard page ---
        for i in 0..ENTRIES {
            let phys = i as u64 * PAGE_SIZE;
            (*pt_low).0[i] = if phys == STACK_GUARD_PAGE {
                0 // not present
            } else {
                phys | PRESENT | WRITABLE
            };
        }

        // --- PD: entry 0 -> PT_LOW, entries 1..3 -> 2 MiB pages ---
        (*pd).0[0] = (pt_low as u64) | PRESENT | WRITABLE;
        let huge_pages = (IDENTITY_MAP_BYTES / HUGE_PAGE_SIZE) as usize;
        for i in 1..huge_pages {
            (*pd).0[i] = (i as u64 * HUGE_PAGE_SIZE) | PRESENT | WRITABLE | HUGE;
        }

        // --- PDPT[0] -> PD (covers 0..1 GiB), PML4[0] -> PDPT (0..512 GiB) ---
        (*pdpt).0[0] = (pd as u64) | PRESENT | WRITABLE;
        (*pml4).0[0] = (pdpt as u64) | PRESENT | WRITABLE;

        // --- Switch ---
        // Writing CR3 also flushes every non-global TLB entry, so no stale
        // translation of the guard page survives. Code, stack and these
        // tables are all mapped identically in old and new hierarchies,
        // so execution continues seamlessly.
        let cr3 = pml4 as u64;
        asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags));
        cr3
    }
}

/// True if `addr` falls inside the kernel stack's guard page.
pub fn is_stack_guard(addr: u64) -> bool {
    (STACK_GUARD_PAGE..KERNEL_STACK_BOTTOM).contains(&addr)
}
