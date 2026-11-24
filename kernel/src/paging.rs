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
//! Second, `map_physical_memory()` maps all
//! RAM a second time at `PHYS_MAP_BASE` in the higher half (the "physical
//! map"): physical address P is reachable at virtual PHYS_MAP_BASE + P.
//! This is how the kernel touches any frame, whatever its address, without
//! growing the identity map. User space will own the lower half later, so
//! the identity map is the temporary part of this design; the physical map
//! is the permanent one. Physical-map pages are no-execute when the CPU
//! supports it: it exists for data, never code.
//!
//! The 8 MiB identity map relies on statics whose addresses are physical
//! addresses. That stops being true the day the kernel moves to a
//! higher-half address.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::boot::E820Entry;
use crate::pmm::EarlyFrames;

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
pub const IDENTITY_MAP_BYTES: u64 = 8 * 1024 * 1024;

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

// pub(crate): vmm.rs builds entries with the same bits.
pub(crate) const PRESENT: u64 = 1 << 0;
pub(crate) const WRITABLE: u64 = 1 << 1;
/// U/S: ring 3 may use the page. Only counts if every level on the walk
/// has it set, so a single clear bit anywhere keeps a whole subtree kernel-only.
pub(crate) const USER: u64 = 1 << 2;
pub(crate) const HUGE: u64 = 1 << 7;
pub(crate) const NO_EXECUTE: u64 = 1 << 63; // only legal once EFER.NXE = 1

/// Bits 51:12 of an entry: the physical address it points to.
pub(crate) const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

const ENTRIES: usize = 512;

/// CR0 bit 16.
pub(crate) const CR0_WRITE_PROTECT: u64 = 1 << 16;

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

/// Physical address of the kernel's PML4 (the root of its address space).
/// The statics sit in the identity map, so their address is physical.
pub(crate) fn kernel_root() -> u64 {
    &raw const PML4 as u64
}

/// True for the four tables above, which live in the kernel image and must
/// never be handed to the frame allocator, even when they become empty.
pub(crate) fn is_boot_table(phys: u64) -> bool {
    phys == &raw const PML4 as u64
        || phys == &raw const PDPT as u64
        || phys == &raw const PD as u64
        || phys == &raw const PT_LOW as u64
}

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

        // --- CR0.WP: make ring 0 honour read-only pages ---
        // With WP clear, the CPU only enforces read-only pages against user
        // mode; kernel writes go straight through. Stage2 leaves it clear,
        // so without this every read-only mapping is read-only in name only.
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        cr0 |= CR0_WRITE_PROTECT;
        asm!("mov cr0, {}", in(reg) cr0, options(nostack, preserves_flags));

        cr3
    }
}

/// True if `addr` falls inside the kernel stack's guard page.
pub fn is_stack_guard(addr: u64) -> bool {
    (STACK_GUARD_PAGE..KERNEL_STACK_BOTTOM).contains(&addr)
}

// ===========================================================================
// Physical map (higher half)
// ===========================================================================

/// Start of the physical map: the first address of the upper canonical half.
/// PML4 index 256; physical address P lives at PHYS_MAP_BASE + P.
pub const PHYS_MAP_BASE: u64 = 0xFFFF_8000_0000_0000;

/// One PML4 entry covers 512 GiB, which is all the physical map uses.
pub const PHYS_MAP_MAX: u64 = 512 * 1024 * 1024 * 1024;

static PHYS_MAP_READY: AtomicBool = AtomicBool::new(false);
static NX_ENABLED: AtomicBool = AtomicBool::new(false);

/// True once EFER.NXE is on, i.e. NO_EXECUTE may be set in an entry.
pub(crate) fn nx_enabled() -> bool {
    NX_ENABLED.load(Ordering::Acquire)
}

/// Kernel-virtual address for a physical address.
///
/// Before the physical map exists only the identity map is available, so
/// this returns `phys` unchanged and refuses anything above 8 MiB. Nothing
/// should call it in that window (the physical map's own tables come from
/// pmm::EarlyFrames, which zeroes them through the identity map directly),
/// so the refusal is a tripwire, not a code path.
pub fn phys_to_virt(phys: u64) -> *mut u8 {
    if PHYS_MAP_READY.load(Ordering::Acquire) {
        (PHYS_MAP_BASE + phys) as *mut u8
    } else if phys < IDENTITY_MAP_BYTES {
        phys as *mut u8
    } else {
        crate::serial_write_str(b"[paging] FATAL: phys_to_virt(");
        crate::serial_write_hex(phys);
        crate::serial_write_str(b") before the physical map exists\n");
        crate::halt();
    }
}

/// What map_physical_memory() did, for the boot log.
pub struct PhysMapStats {
    pub bytes_mapped: u64,
    pub huge_pages: u64,
    pub table_frames: u64,
    pub highest_addr: u64,
    pub nx: bool,
}

/// Map every RAM-like E820 region (usable, ACPI reclaimable, ACPI NVS) at
/// PHYS_MAP_BASE + phys using 2 MiB pages, then switch phys_to_virt() over
/// to it. Region edges are rounded out to 2 MiB, so a few reserved bytes
/// sharing a 2 MiB page with RAM get mapped too; firmware's memory-type
/// ranges (MTRRs) keep device memory uncached regardless.
///
/// Page-table frames come from `early` (zeroed, identity-mapped): the real
/// frame allocator can't exist yet, because its bitmap is reached through
/// the physical map this function builds.
///
/// # Safety
/// Call once, before pmm::init(), with the map stage2 collected. `early`
/// must hold at least physmap_table_bound(e820) frames.
pub unsafe fn map_physical_memory(e820: &[E820Entry], early: &mut EarlyFrames) -> PhysMapStats {
    let nx = enable_no_execute();
    NX_ENABLED.store(nx, Ordering::Release);
    let leaf_flags = PRESENT | WRITABLE | HUGE | if nx { NO_EXECUTE } else { 0 };

    let mut stats = PhysMapStats {
        bytes_mapped: 0,
        huge_pages: 0,
        table_frames: 0,
        highest_addr: 0,
        nx,
    };

    // PML4[256] -> one PDPT covering the whole physical map.
    let pml4 = &raw mut PML4;
    let pdpt_phys = alloc_table(early, &mut stats);
    let pml4_index = ((PHYS_MAP_BASE >> 39) & 0x1FF) as usize;
    (*pml4).0[pml4_index] = pdpt_phys | PRESENT | WRITABLE;
    let pdpt = pdpt_phys as *mut u64; // identity view: table frames are < 8 MiB

    for entry in e820.iter() {
        // Copy out of the packed struct before using the fields.
        let (base, length, kind) = (entry.base, entry.length, entry.region_type);
        if length == 0 || !matches!(kind, 1 | 3 | 4) {
            continue;
        }

        let start = base & !(HUGE_PAGE_SIZE - 1);
        let end = (base + length + HUGE_PAGE_SIZE - 1) & !(HUGE_PAGE_SIZE - 1);
        let end = end.min(PHYS_MAP_MAX);

        let mut phys = start;
        while phys < end {
            // One PD per GiB of physical address space, created on demand.
            let pdpt_index = ((phys >> 30) & 0x1FF) as usize;
            let pd = if *pdpt.add(pdpt_index) & PRESENT == 0 {
                let pd_phys = alloc_table(early, &mut stats);
                *pdpt.add(pdpt_index) = pd_phys | PRESENT | WRITABLE;
                pd_phys as *mut u64
            } else {
                (*pdpt.add(pdpt_index) & ADDR_MASK) as *mut u64
            };

            let pd_index = ((phys >> 21) & 0x1FF) as usize;
            if *pd.add(pd_index) & PRESENT == 0 {
                *pd.add(pd_index) = phys | leaf_flags;
                stats.huge_pages += 1;
                stats.bytes_mapped += HUGE_PAGE_SIZE;
            }

            phys += HUGE_PAGE_SIZE;
        }

        stats.highest_addr = stats.highest_addr.max(end);
    }

    // Reload CR3 to start from a clean TLB. Strictly, only previously
    // non-present entries changed, which the TLB doesn't cache, but this is
    // cheap and removes the question.
    let cr3: u64;
    asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
    asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags));

    PHYS_MAP_READY.store(true, Ordering::Release);
    stats
}

/// Take a zeroed, identity-mapped frame from the early run for use as a
/// page table.
fn alloc_table(early: &mut EarlyFrames, stats: &mut PhysMapStats) -> u64 {
    match early.take() {
        Some(phys) => {
            stats.table_frames += 1;
            phys
        }
        None => {
            crate::serial_write_str(b"[paging] FATAL: early page-table frames used up\n");
            crate::halt();
        }
    }
}

/// Upper bound on the page-table frames map_physical_memory() will take:
/// one PDPT, plus one PD per GiB of physical address space up to the end
/// of the highest region it maps.
pub fn physmap_table_bound(e820: &[E820Entry]) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    let mut top = 0u64;
    for entry in e820.iter() {
        let (base, length, kind) = (entry.base, entry.length, entry.region_type);
        if length != 0 && matches!(kind, 1 | 3 | 4) {
            top = top.max(base.saturating_add(length));
        }
    }
    let top = top.min(PHYS_MAP_MAX);
    1 + top.div_ceil(GIB)
}

/// Turn on EFER.NXE if the CPU supports no-execute (CPUID 0x80000001,
/// EDX bit 20). Until NXE is set, bit 63 in a paging entry is reserved and
/// setting it causes a reserved-bit page fault.
fn enable_no_execute() -> bool {
    const IA32_EFER: u32 = 0xC000_0080;
    const EFER_NXE: u64 = 1 << 11;

    let (max_ext, _) = cpuid(0x8000_0000);
    if max_ext < 0x8000_0001 {
        return false;
    }
    let (_, edx) = cpuid(0x8000_0001);
    if edx & (1 << 20) == 0 {
        return false;
    }

    unsafe {
        let (lo, hi): (u32, u32);
        asm!("rdmsr", in("ecx") IA32_EFER, out("eax") lo, out("edx") hi,
             options(nomem, nostack, preserves_flags));
        let efer = ((hi as u64) << 32 | lo as u64) | EFER_NXE;
        asm!("wrmsr", in("ecx") IA32_EFER, in("eax") efer as u32, in("edx") (efer >> 32) as u32,
             options(nostack, preserves_flags));
    }
    true
}

/// CPUID leaf -> (eax, edx). rbx is reserved by LLVM, so it is saved and
/// restored around the instruction by hand.
fn cpuid(leaf: u32) -> (u32, u32) {
    let (eax, edx): (u32, u32);
    unsafe {
        asm!(
            "mov {tmp}, rbx",
            "cpuid",
            "mov rbx, {tmp}",
            tmp = out(reg) _,
            inout("eax") leaf => eax,
            inout("ecx") 0u32 => _,
            out("edx") edx,
            options(nostack, preserves_flags),
        );
    }
    (eax, edx)
}

// ===========================================================================
// Self-test
// ===========================================================================

/// Exercise the physical map end to end. Prints one line per check.
///
/// 1. Alias: a byte written through the physical map is visible through the
///    identity map at the same physical address.
/// 2. Allocator: keep allocating until a frame above 8 MiB comes back. Each
///    must arrive zeroed. Before this change, zeroing that frame faulted.
///    The frames are chained through their own first 8 bytes, then freed.
/// 3. Top of RAM: the last usable frame reads back what was written.
///
/// # Safety
/// Call after map_physical_memory() and pmm::init(). Temporarily
/// allocates every free frame below 8 MiB (about 7 MiB).
pub unsafe fn selftest(e820: &[E820Entry]) -> bool {
    use crate::pmm::{self, Kind, Owner};
    use crate::{serial_write_hex as hex, serial_write_str as s, serial_write_u64 as dec};

    let mut ok = true;

    // --- 1. alias ---
    let alias_ok = match pmm::alloc(Kind::Test, Owner::Kernel) {
        Some(held) => {
            let frame = held.phys();
            phys_to_virt(frame).write_volatile(0x5A);
            let seen = (frame as *const u8).read_volatile(); // identity view
            held.free();
            seen == 0x5A
        }
        None => false,
    };
    s(b"[paging] self-test alias (physmap write, identity read)");
    ok &= crate::selftest::verdict(alias_ok);

    // --- 2. allocate past the old 8 MiB limit ---
    let mut chain: u64 = 0; // physical address of the last frame, 0 = end
    let mut count: u64 = 0;
    let mut all_zeroed = true;
    let mut high_frame: u64 = 0;
    // Each frame is stored by address in the one before (into_phys), and
    // taken back by address at the end (reclaim).
    while let Some(held) = pmm::alloc(Kind::Test, Owner::Kernel) {
        let frame = held.into_phys();
        let words = phys_to_virt(frame) as *mut u64;
        for i in 0..(PAGE_SIZE as usize / 8) {
            if words.add(i).read_volatile() != 0 {
                all_zeroed = false;
            }
        }
        words.write_volatile(chain); // link to the previous frame
        chain = frame;
        count += 1;
        if frame >= IDENTITY_MAP_BYTES {
            high_frame = frame;
            break;
        }
    }
    while chain != 0 {
        let next = (phys_to_virt(chain) as *const u64).read_volatile();
        pmm::Frame::reclaim(chain, Kind::Test.bit(), Owner::Kernel).free();
        chain = next;
    }
    let alloc_ok = high_frame != 0 && all_zeroed;
    s(b"[paging] self-test allocator: ");
    dec(count);
    s(b" frames, first above 8 MiB at ");
    hex(high_frame);
    s(if all_zeroed { b", all zeroed" } else { b", NOT ZEROED" });
    ok &= crate::selftest::verdict(alloc_ok);

    // --- 3. last usable frame ---
    let mut top: u64 = 0;
    for entry in e820.iter() {
        let (base, length, kind) = (entry.base, entry.length, entry.region_type);
        if kind == 1 && length >= PAGE_SIZE {
            top = top.max((base + length) & !(PAGE_SIZE - 1));
        }
    }
    let top_ok = if top >= PAGE_SIZE && top <= PHYS_MAP_MAX {
        let last = top - PAGE_SIZE;
        let p = phys_to_virt(last) as *mut u64;
        let saved = p.read_volatile();
        p.write_volatile(0xF4E7_A05F_4E7A_05F4);
        let seen = p.read_volatile();
        p.write_volatile(saved);
        s(b"[paging] self-test top of RAM: frame ");
        hex(last);
        seen == 0xF4E7_A05F_4E7A_05F4
    } else {
        s(b"[paging] self-test top of RAM: no usable region found");
        false
    };
    ok &= crate::selftest::verdict(top_ok);

    ok
}
