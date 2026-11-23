//! vmm.rs — virtual memory manager: mapping 4 KiB pages.
//!
//! paging.rs builds the fixed boot-time mappings. This module is the
//! general mechanism on top: map any frame at any virtual address,
//! unmap it, and ask what an address maps to. Missing page tables are
//! allocated from pmm on demand and reclaimed when they empty out.
//!
//! An `AddressSpace` is one 4-level hierarchy, named by its root (PML4)
//! frame. There is the kernel's, and one per user process (new_user). A
//! user address space shares the kernel's half with every other address
//! space and owns its user half:
//!
//!   - PML4 entries 256-511 are copies of the kernel's, pointing at the
//!     same PDPTs. Those PDPTs are never freed (see unmap), so a change
//!     the kernel makes inside them shows up everywhere at once. A NEW
//!     kernel PML4 entry would not, so creating one while any user address
//!     space exists halts (see map_locked). Every kernel region in use
//!     (physical map, dynamic region, heap, task stacks) has its entry
//!     before the first process starts.
//!   - PML4 entry 0 points at the process's own PDPT, whose entry 0 is a
//!     copy of the kernel's: the low 1 GiB, including the identity-mapped
//!     kernel image and stack, is the same in every address space and
//!     kernel-only (no U/S bit), because the kernel still runs there.
//!   - User space is [USER_BASE, USER_END): everything else in the lower
//!     half. Only pages with a USER_* preset go there, and only there.
//!
//! Mapping does not transfer ownership in the kernel's address space:
//! `unmap` hands back the physical address and the caller decides what
//! happens to that frame. A user address space is different: it owns the
//! frames mapped into it, and destroy() frees them with it.
//!
//! Virtual layout (kernel's view):
//!
//!   0x0000_0000_0000_0000  identity map, first 8 MiB (kernel image, stack,
//!                          VGA). Temporary: user space will own the lower
//!                          half.
//!   0xFFFF_8000_0000_0000  physical map, all RAM (PML4 256, up to 512 GiB)
//!   0xFFFF_C000_0000_0000  kernel dynamic mappings (PML4 384): self-tests
//!                          and anything built with map()
//!   0xFFFF_C040_0000_0000  device window (inside slot 384): map_mmio()
//!   0xFFFF_C080_0000_0000  kernel heap (PML4 385, see heap.rs)
//!
//! Every table is reached through the physical map, so this module only
//! works after paging::map_physical_memory().

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::paging::{self, ADDR_MASK, HUGE, NO_EXECUTE, PRESENT, USER, WRITABLE};
use crate::pmm::{self, PAGE_SIZE};

/// Start of the kernel's dynamic mapping region.
pub const KERNEL_DYNAMIC_BASE: u64 = 0xFFFF_C000_0000_0000;

/// Device registers are mapped here, 256 GiB into the dynamic region, well
/// clear of the pages the self-tests use.
pub const DEVICE_WINDOW_BASE: u64 = KERNEL_DYNAMIC_BASE + 0x40_0000_0000;

/// User space: from 1 GiB (below is the kernel's shared identity map) to
/// the last page below the canonical hole, which is left out on purpose:
/// a `syscall` in the very last bytes of user space would make SYSRET
/// return to a non-canonical address, which faults in ring 0 on Intel CPUs
/// with the user's stack pointer already loaded (CVE-2012-0217).
pub const USER_BASE: u64 = 0x0000_0000_4000_0000;
pub const USER_END: u64 = 0x0000_7FFF_FFFF_F000;

/// True if [virt, virt + len) lies inside user space (no wrap-around).
pub fn is_user_range(virt: u64, len: u64) -> bool {
    virt >= USER_BASE && virt < USER_END && len <= USER_END - virt
}

/// User address spaces that exist. While any do, the kernel must not create
/// a new kernel-half PML4 entry: their copies of the kernel half would never
/// see it.
static USER_SPACES: AtomicU64 = AtomicU64::new(0);

pub fn user_spaces() -> u64 {
    USER_SPACES.load(Ordering::Relaxed)
}

const ENTRIES: usize = 512;
const WRITE_THROUGH: u64 = 1 << 3; // PWT
const CACHE_DISABLE: u64 = 1 << 4; // PCD
/// In a 4 KiB leaf, bit 7 selects PAT entries 4-7. With PCD and PWT clear
/// it picks entry 4, which display.rs makes write-combining (Entry 18).
const PAT_4K: u64 = 1 << 7;

// ===========================================================================
// Flags
// ===========================================================================

/// Leaf permissions. PRESENT is implied. The presets never combine writable
/// with executable, so W^X holds unless a caller builds one by hand.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PageFlags(u64);

impl PageFlags {
    /// Read + execute. For code.
    pub const KERNEL_CODE: PageFlags = PageFlags(0);
    /// Read only, no execute. For constants.
    pub const KERNEL_RODATA: PageFlags = PageFlags(NO_EXECUTE);
    /// Read + write, no execute. The default for kernel memory.
    pub const KERNEL_DATA: PageFlags = PageFlags(WRITABLE | NO_EXECUTE);
    /// Read + write, no execute, uncached. For device registers, which
    /// must see every access in order and never be served from cache.
    pub const MMIO: PageFlags = PageFlags(WRITABLE | NO_EXECUTE | CACHE_DISABLE | WRITE_THROUGH);
    /// Read + write, no execute, PAT entry 4. Write-combining once
    /// display.rs has set that entry: for a framebuffer, which is only
    /// ever written, in long runs. (Uncached would be several times slower.)
    pub const FRAMEBUFFER: PageFlags = PageFlags(WRITABLE | NO_EXECUTE | PAT_4K);
    /// Ring 3: read + execute. For a program's code.
    pub const USER_CODE: PageFlags = PageFlags(USER);
    /// Ring 3: read + write, no execute. Data and stacks.
    pub const USER_DATA: PageFlags = PageFlags(USER | WRITABLE | NO_EXECUTE);

    fn is_user(self) -> bool {
        self.0 & USER != 0
    }

    /// The bits that go into the leaf entry. NO_EXECUTE is only legal once
    /// EFER.NXE is on; without NX support the page is executable (and the
    /// boot log already said "NX unsupported").
    fn entry_bits(self) -> u64 {
        let bits = PRESENT | self.0;
        if paging::nx_enabled() {
            bits
        } else {
            bits & !NO_EXECUTE
        }
    }
}

// ===========================================================================
// Errors
// ===========================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// Virtual or physical address not 4 KiB aligned.
    Misaligned,
    /// Virtual address bits 63:48 don't copy bit 47 (a hole in x86-64's
    /// address space that no page table can reach).
    NonCanonical,
    /// Something is already mapped there. Unmap it first: silently
    /// replacing a mapping is how use-after-free bugs hide.
    AlreadyMapped,
    /// A 2 MiB or 1 GiB page already covers the address. Splitting it would
    /// change the permissions or meaning of the other 511 pages, so it is
    /// refused rather than done implicitly.
    HugePageInTheWay,
    /// No frame left for an intermediate page table.
    OutOfMemory,
    /// A user page outside user space or in the kernel's address space, or
    /// a kernel page inside user space.
    WrongRegion,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UnmapError {
    Misaligned,
    NonCanonical,
    NotMapped,
    /// The address is inside a 2 MiB or 1 GiB page; only 4 KiB pages can be
    /// unmapped one at a time.
    HugePage,
    /// Outside user space, through a user address space (the kernel half
    /// is shared and only changes through the kernel's address space).
    WrongRegion,
}

impl MapError {
    pub fn name(self) -> &'static [u8] {
        match self {
            MapError::Misaligned => b"misaligned",
            MapError::NonCanonical => b"non-canonical",
            MapError::AlreadyMapped => b"already mapped",
            MapError::HugePageInTheWay => b"huge page in the way",
            MapError::OutOfMemory => b"out of memory",
            MapError::WrongRegion => b"wrong region",
        }
    }
}

impl UnmapError {
    pub fn name(self) -> &'static [u8] {
        match self {
            UnmapError::Misaligned => b"misaligned",
            UnmapError::NonCanonical => b"non-canonical",
            UnmapError::NotMapped => b"not mapped",
            UnmapError::HugePage => b"huge page",
            UnmapError::WrongRegion => b"wrong region",
        }
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

fn is_canonical(virt: u64) -> bool {
    // Sign-extend from bit 47 and compare.
    ((virt << 16) as i64 >> 16) as u64 == virt
}

/// Table index at a level: 4 = PML4 (bits 47:39) ... 1 = PT (bits 20:12).
fn index(virt: u64, level: u32) -> usize {
    ((virt >> (12 + 9 * (level - 1))) & 0x1FF) as usize
}

/// A page table, reached through the physical map.
fn table(phys: u64) -> *mut u64 {
    paging::phys_to_virt(phys) as *mut u64
}

unsafe fn is_empty(table_phys: u64) -> bool {
    let t = table(table_phys);
    (0..ENTRIES).all(|i| *t.add(i) == 0)
}

fn invlpg(virt: u64) {
    unsafe { asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags)) };
}

fn read_cr3() -> u64 {
    let cr3: u64;
    unsafe { asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags)) };
    cr3
}

/// The root of the address space the CPU is using.
pub fn active_root() -> u64 {
    read_cr3() & ADDR_MASK
}

/// Switch to another address space (flushes every non-global TLB entry).
///
/// # Safety
/// `root` must be a complete hierarchy that maps the running code, its
/// stack and the kernel half: one from AddressSpace::kernel() or new_user().
pub unsafe fn switch_to(root: u64) {
    asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
}

fn fatal(msg: &[u8], value: u64) -> ! {
    crate::serial_write_str(b"[vmm] FATAL: ");
    crate::serial_write_str(msg);
    crate::serial_write_hex(value);
    crate::serial_write_str(b"\n");
    crate::halt();
}

// ===========================================================================
// AddressSpace
// ===========================================================================

pub struct AddressSpace {
    root: u64, // physical address of the PML4
}

impl AddressSpace {
    /// The kernel's address space (the one paging::init() switched to).
    pub fn kernel() -> AddressSpace {
        AddressSpace { root: paging::kernel_root() }
    }

    /// The address space whose PML4 is at `root`.
    ///
    /// # Safety
    /// `root` must have come from root() of a live address space.
    pub unsafe fn from_root(root: u64) -> AddressSpace {
        AddressSpace { root }
    }

    pub fn root(&self) -> u64 {
        self.root
    }

    fn is_kernel(&self) -> bool {
        self.root == paging::kernel_root()
    }

    /// A new, empty user address space: the kernel half and the low 1 GiB
    /// shared with the kernel's (see the top of this file), nothing in user
    /// space yet. Costs two frames (its PML4 and its lower-half PDPT).
    pub fn new_user() -> Result<AddressSpace, MapError> {
        crate::cpu::without_interrupts(|| unsafe {
            let pml4 = pmm::alloc_frame().ok_or(MapError::OutOfMemory)? as u64;
            let Some(pdpt) = pmm::alloc_frame() else {
                pmm::free_frame(pml4 as usize);
                return Err(MapError::OutOfMemory);
            };
            let pdpt = pdpt as u64;
            let kernel = table(paging::kernel_root());
            let (new_pml4, new_pdpt) = (table(pml4), table(pdpt));
            for i in 256..ENTRIES {
                *new_pml4.add(i) = *kernel.add(i);
            }
            // Low 1 GiB: the kernel's own entry, so U/S stays clear.
            let kernel_pdpt = *kernel & ADDR_MASK;
            *new_pdpt = *table(kernel_pdpt);
            // U/S set here so user pages further down can be reached; the
            // copied entry above still keeps the low 1 GiB kernel-only.
            *new_pml4 = pdpt | PRESENT | WRITABLE | USER;
            USER_SPACES.fetch_add(1, Ordering::Relaxed);
            Ok(AddressSpace { root: pml4 })
        })
    }

    /// Free every user page, every table of the user half and the PML4.
    /// Leaves the shared parts alone: the kernel half and the low 1 GiB.
    ///
    /// # Safety
    /// Must be a user address space that no task will use again. It must
    /// not be the active one (halts if it is).
    pub unsafe fn destroy(self) {
        if self.is_kernel() || self.root == active_root() {
            fatal(b"destroy of the kernel's or the active address space, root ", self.root);
        }
        crate::cpu::without_interrupts(|| {
            let pml4 = table(self.root);
            for i in 0..256 {
                let e4 = *pml4.add(i);
                if e4 & PRESENT == 0 {
                    continue;
                }
                let pdpt = e4 & ADDR_MASK;
                for j in 0..ENTRIES {
                    let e3 = *table(pdpt).add(j);
                    if (i == 0 && j == 0) || e3 & PRESENT == 0 {
                        continue; // the shared low 1 GiB, or nothing
                    }
                    let pd = e3 & ADDR_MASK;
                    for k in 0..ENTRIES {
                        let e2 = *table(pd).add(k);
                        if e2 & PRESENT == 0 {
                            continue;
                        }
                        let pt = e2 & ADDR_MASK;
                        for l in 0..ENTRIES {
                            let e1 = *table(pt).add(l);
                            if e1 & PRESENT != 0 {
                                pmm::free_frame((e1 & ADDR_MASK) as usize);
                            }
                        }
                        pmm::free_frame(pt as usize);
                    }
                    pmm::free_frame(pd as usize);
                }
                pmm::free_frame(pdpt as usize);
            }
            pmm::free_frame(self.root as usize);
            USER_SPACES.fetch_sub(1, Ordering::Relaxed);
        });
    }

    /// The frame behind user address `virt`, if ring 3 could read it (or
    /// write it, with `write`): present, U/S at every level, and writable
    /// if asked. This is how the kernel checks a pointer a process hands it.
    pub fn user_page(&self, virt: u64, write: bool) -> Option<u64> {
        if !is_user_range(virt, 1) {
            return None;
        }
        unsafe {
            let mut table_phys = self.root;
            for level in [4u32, 3, 2, 1] {
                let entry = *table(table_phys).add(index(virt, level));
                if entry & (PRESENT | USER) != PRESENT | USER
                    || (write && entry & WRITABLE == 0)
                    || (level < 4 && level > 1 && entry & HUGE != 0)
                {
                    return None;
                }
                table_phys = entry & ADDR_MASK;
            }
            Some(table_phys)
        }
    }

    /// True if the CPU is using this hierarchy right now. TLB entries only
    /// exist for the active one, so only then does a change need invlpg.
    fn is_active(&self) -> bool {
        read_cr3() & ADDR_MASK == self.root
    }

    /// Map the 4 KiB frame at `phys` at virtual address `virt`.
    ///
    /// # Safety
    /// The caller decides what `phys` is. Mapping a frame someone else owns,
    /// or device memory with KERNEL_DATA's caching, is not caught here.
    pub unsafe fn map(&mut self, virt: u64, phys: u64, flags: PageFlags) -> Result<(), MapError> {
        // Interrupts off: a task switch mid-walk could let another task
        // build a conflicting table under the same entry.
        crate::cpu::without_interrupts(|| self.map_locked(virt, phys, flags))
    }

    unsafe fn map_locked(&mut self, virt: u64, phys: u64, flags: PageFlags) -> Result<(), MapError> {
        crate::cpu::assert_interrupts_off(b"vmm map");
        if virt % PAGE_SIZE != 0 || phys % PAGE_SIZE != 0 {
            return Err(MapError::Misaligned);
        }
        if !is_canonical(virt) {
            return Err(MapError::NonCanonical);
        }
        // User pages only in user space of a user address space; nothing
        // else in user space; nothing outside it through a user address
        // space (the kernel half is shared: change it via the kernel's).
        let in_user_space = is_user_range(virt, PAGE_SIZE);
        if flags.is_user() != in_user_space || (flags.is_user() && self.is_kernel())
            || (!self.is_kernel() && !in_user_space)
        {
            return Err(MapError::WrongRegion);
        }

        // Walk PML4 -> PDPT -> PD, creating any missing table on the way.
        // Intermediate entries are permissive; the leaf decides. For a user
        // page they also need U/S, or ring 3 can't get past them.
        let table_bits = PRESENT | WRITABLE | if flags.is_user() { USER } else { 0 };
        let mut table_phys = self.root;
        for level in [4u32, 3, 2] {
            let entry = table(table_phys).add(index(virt, level));

            if *entry & PRESENT == 0 {
                if level == 4 && index(virt, 4) >= 256 && user_spaces() > 0 {
                    // User address spaces copied the kernel's PML4 entries
                    // when they were made, and would never see this one.
                    fatal(b"new kernel-half PML4 entry after user address spaces copied the kernel half, address ", virt);
                }
                let new = match pmm::alloc_frame() {
                    Some(frame) => frame as u64, // already zeroed
                    None => return Err(MapError::OutOfMemory),
                };
                *entry = new | table_bits;
            } else if level < 4 && *entry & HUGE != 0 {
                return Err(MapError::HugePageInTheWay);
            } else {
                *entry |= table_bits & USER;
            }

            table_phys = *entry & ADDR_MASK;
        }

        let leaf = table(table_phys).add(index(virt, 1));
        if *leaf & PRESENT != 0 {
            return Err(MapError::AlreadyMapped);
        }
        *leaf = phys | flags.entry_bits();

        // Not-present -> present needs no TLB flush: the CPU never caches a
        // translation for a page that wasn't present.
        Ok(())
    }

    /// Remove the 4 KiB mapping at `virt` and return the frame it pointed
    /// to. The frame is NOT freed: whoever owns it decides that.
    ///
    /// Page tables that become empty are freed, except the PML4 and the
    /// PDPTs directly under it. Those stay, so that kernel-half PML4
    /// entries can later be shared by every address space without one of
    /// them pulling a table out from under the others.
    ///
    /// # Safety
    /// Nothing may still be using the page through `virt`.
    pub unsafe fn unmap(&mut self, virt: u64) -> Result<u64, UnmapError> {
        crate::cpu::without_interrupts(|| self.unmap_locked(virt))
    }

    unsafe fn unmap_locked(&mut self, virt: u64) -> Result<u64, UnmapError> {
        crate::cpu::assert_interrupts_off(b"vmm unmap");
        if virt % PAGE_SIZE != 0 {
            return Err(UnmapError::Misaligned);
        }
        if !is_canonical(virt) {
            return Err(UnmapError::NonCanonical);
        }
        if !self.is_kernel() && !is_user_range(virt, PAGE_SIZE) {
            return Err(UnmapError::WrongRegion);
        }

        // Walk down, remembering the entry that points at each table so an
        // empty table can be unlinked on the way back up.
        let mut parents: [*mut u64; 4] = [core::ptr::null_mut(); 4];
        let mut table_phys = self.root;
        for level in [4u32, 3, 2] {
            let entry = table(table_phys).add(index(virt, level));
            if *entry & PRESENT == 0 {
                return Err(UnmapError::NotMapped);
            }
            if level < 4 && *entry & HUGE != 0 {
                return Err(UnmapError::HugePage);
            }
            parents[(level - 1) as usize] = entry; // entry pointing at the level-(n-1) table
            table_phys = *entry & ADDR_MASK;
        }

        let leaf = table(table_phys).add(index(virt, 1));
        if *leaf & PRESENT == 0 {
            return Err(UnmapError::NotMapped);
        }
        let phys = *leaf & ADDR_MASK;
        *leaf = 0;

        // Reclaim the PT, then the PD, if this left them empty. Stop at the
        // PDPT (see above) and never touch a boot-time static table.
        let mut child = table_phys;
        for level in [1u32, 2] {
            let parent = parents[level as usize]; // entry in the level+1 table
            if paging::is_boot_table(child) || !is_empty(child) {
                break;
            }
            *parent = 0;
            pmm::free_frame(child as usize);
            child = *parents[(level + 1) as usize] & ADDR_MASK;
        }

        // One invlpg drops both the TLB entry for `virt` and any cached
        // paging-structure entries on its path, including ones pointing at
        // tables just freed.
        if self.is_active() {
            invlpg(virt);
        }
        Ok(phys)
    }

    /// The physical address `virt` maps to, for any page size, or None.
    pub fn translate(&self, virt: u64) -> Option<u64> {
        if !is_canonical(virt) {
            return None;
        }
        unsafe {
            let mut table_phys = self.root;
            for level in [4u32, 3, 2, 1] {
                let entry = *table(table_phys).add(index(virt, level));
                if entry & PRESENT == 0 {
                    return None;
                }
                let page_bits = 12 + 9 * (level - 1); // 1 GiB: 30, 2 MiB: 21, 4 KiB: 12
                if level == 1 || (level < 4 && entry & HUGE != 0) {
                    // Huge leaves keep bit 12 for PAT, so mask with the
                    // page size, not ADDR_MASK.
                    let base = entry & ADDR_MASK & !((1u64 << page_bits) - 1);
                    return Some(base + (virt & ((1u64 << page_bits) - 1)));
                }
                table_phys = entry & ADDR_MASK;
            }
            None
        }
    }

    /// Raw leaf entry for a 4 KiB mapping (for the self-test's flag checks).
    unsafe fn leaf_entry(&self, virt: u64) -> Option<u64> {
        let mut table_phys = self.root;
        for level in [4u32, 3, 2] {
            let entry = *table(table_phys).add(index(virt, level));
            if entry & PRESENT == 0 || (level < 4 && entry & HUGE != 0) {
                return None;
            }
            table_phys = entry & ADDR_MASK;
        }
        let leaf = *table(table_phys).add(index(virt, 1));
        (leaf & PRESENT != 0).then_some(leaf)
    }
}

// ===========================================================================
// Device window
// ===========================================================================

static NEXT_DEVICE_VA: AtomicU64 = AtomicU64::new(DEVICE_WINDOW_BASE);

/// Map `bytes` of device registers starting at physical `phys` into the
/// device window, uncached (PageFlags::MMIO), and return the virtual
/// address that corresponds to `phys`. Device memory is not RAM, so it is
/// not in the physical map; drivers reach it only through this.
///
/// A simple bump allocator: mappings are never removed. If a page fails to
/// map, the pages already mapped for this call stay mapped.
///
/// # Safety
/// `phys` must be device memory the caller is entitled to drive.
pub unsafe fn map_mmio(phys: u64, bytes: u64) -> Result<u64, MapError> {
    map_device(phys, bytes, PageFlags::MMIO)
}

/// map_mmio with other flags (PageFlags::FRAMEBUFFER).
///
/// # Safety
/// As map_mmio.
pub unsafe fn map_device(phys: u64, bytes: u64, flags: PageFlags) -> Result<u64, MapError> {
    let first = phys & !(PAGE_SIZE - 1);
    let end = (phys + bytes + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let pages = (end - first) / PAGE_SIZE;
    let va = NEXT_DEVICE_VA.fetch_add(pages * PAGE_SIZE, Ordering::Relaxed);

    let mut ks = AddressSpace::kernel();
    for i in 0..pages {
        ks.map(va + i * PAGE_SIZE, first + i * PAGE_SIZE, flags)?;
    }
    Ok(va + (phys - first))
}

/// The raw leaf entry of a 4 KiB kernel mapping, for self-tests that check
/// its flags.
pub fn kernel_leaf_entry(virt: u64) -> Option<u64> {
    unsafe { AddressSpace::kernel().leaf_entry(virt) }
}

/// PAT bit and the two cache bits of a leaf entry: (PAT, PCD, PWT).
pub fn cache_bits(entry: u64) -> (bool, bool, bool) {
    (entry & PAT_4K != 0, entry & CACHE_DISABLE != 0, entry & WRITE_THROUGH != 0)
}

// ===========================================================================
// Self-test
// ===========================================================================

/// Exercise the API against the kernel's live address space. Every check
/// prints one line.
///
/// # Safety
/// Call after pmm::init(). Uses (and leaves clean) the first pages of the
/// dynamic region.
pub unsafe fn selftest() -> bool {
    use crate::{serial_write_hex as hex, serial_write_str as s};

    let mut ks = AddressSpace::kernel();
    let va = KERNEL_DYNAMIC_BASE;
    let mut ok = true;

    fn check(ok: &mut bool, name: &[u8], pass: bool) {
        *ok &= crate::selftest::check(b"vmm", name, pass);
    }

    // --- 1. map, translate, alias ---
    let frame = match pmm::alloc_frame() {
        Some(f) => f as u64,
        None => {
            check(&mut ok, b"(no frame to test with)", false);
            return false;
        }
    };
    let mapped = ks.map(va, frame, PageFlags::KERNEL_DATA).is_ok();
    let translated = ks.translate(va) == Some(frame) && ks.translate(va + 0x123) == Some(frame + 0x123);
    (va as *mut u64).write_volatile(0xF4E7_A0C0_DE00_0001);
    let alias = (paging::phys_to_virt(frame) as *const u64).read_volatile() == 0xF4E7_A0C0_DE00_0001;
    check(&mut ok, b"map + translate + write via new address, read via physical map", mapped && translated && alias);

    // --- 2. leaf permissions are what the preset says ---
    let leaf = ks.leaf_entry(va).unwrap_or(0);
    let nx_ok = !paging::nx_enabled() || leaf & NO_EXECUTE != 0;
    check(&mut ok, b"KERNEL_DATA leaf is present, writable, no-execute", leaf & PRESENT != 0 && leaf & WRITABLE != 0 && nx_ok);

    // --- 2b. ring 0 actually honours read-only pages ---
    let cr0: u64;
    asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
    check(&mut ok, b"CR0.WP is on (kernel writes to read-only pages fault)", cr0 & paging::CR0_WRITE_PROTECT != 0);

    // --- 3. every refusal, by name ---
    let map_result = |r: Result<(), MapError>| -> &'static [u8] {
        match r {
            Ok(()) => b"mapped",
            Err(e) => e.name(),
        }
    };
    let refusals: [(&[u8], Result<(), MapError>, MapError); 4] = [
        (b"second mapping at the same address -> ",
         ks.map(va, frame, PageFlags::KERNEL_DATA), MapError::AlreadyMapped),
        (b"unaligned address -> ",
         ks.map(va + 8, frame, PageFlags::KERNEL_DATA), MapError::Misaligned),
        (b"non-canonical 0x0000800000000000 -> ",
         ks.map(0x0000_8000_0000_0000, frame, PageFlags::KERNEL_DATA), MapError::NonCanonical),
        (b"4 KiB at 0x400000, inside a 2 MiB page -> ",
         ks.map(0x0040_0000, frame, PageFlags::KERNEL_DATA), MapError::HugePageInTheWay),
    ];
    for (what, got, want) in refusals {
        s(b"[vmm] self-test ");
        s(what);
        s(map_result(got));
        ok &= crate::selftest::verdict(got == Err(want));
    }
    let got = ks.unmap(va + PAGE_SIZE);
    s(b"[vmm] self-test unmap of a page never mapped -> ");
    s(match got {
        Ok(_) => b"unmapped",
        Err(e) => e.name(),
    });
    ok &= crate::selftest::verdict(got == Err(UnmapError::NotMapped));

    // --- 4. unmap hands back the frame and the address goes away ---
    let returned = ks.unmap(va);
    check(&mut ok, b"unmap returns the frame, translate then finds nothing",
          returned == Ok(frame) && ks.translate(va).is_none());
    pmm::free_frame(frame as usize);

    // --- 5. a full cycle leaves nothing behind ---
    // The first cycle created the PDPT for the dynamic region, which stays
    // by design. From here on, map + unmap must return every page-table
    // frame it took.
    let before = pmm::free_frames();
    let residue_ok = match pmm::alloc_frame() {
        Some(f) => {
            let f = f as u64;
            let far = va + 0x4000_0000; // different GiB: needs a new PD and PT
            let cycle = ks.map(far, f, PageFlags::KERNEL_DATA).is_ok() && ks.unmap(far) == Ok(f);
            pmm::free_frame(f as usize);
            cycle && pmm::free_frames() == before
        }
        None => false,
    };
    check(&mut ok, b"map + unmap in a fresh GiB returns every page-table frame", residue_ok);

    // --- 6. executable mapping: run code through a KERNEL_CODE page ---
    // Two views of one frame: write the bytes through the physical map
    // (writable, no-execute), run them through a KERNEL_CODE mapping
    // (read-only, executable). Neither view is both writable and executable.
    let code_ok = match pmm::alloc_frame() {
        Some(f) => {
            let f = f as u64;
            let code: [u8; 6] = [0xB8, 42, 0, 0, 0, 0xC3]; // mov eax, 42 ; ret
            for (i, byte) in code.iter().enumerate() {
                paging::phys_to_virt(f).add(i).write_volatile(*byte);
            }
            let code_va = va + 0x3000;
            let mapped = ks.map(code_va, f, PageFlags::KERNEL_CODE).is_ok();
            let entry = ks.leaf_entry(code_va).unwrap_or(0);
            let shape_ok = entry & WRITABLE == 0 && entry & NO_EXECUTE == 0;
            let result = if mapped && shape_ok {
                let func: extern "C" fn() -> u32 = core::mem::transmute(code_va);
                func()
            } else {
                0
            };
            let unmapped = ks.unmap(code_va) == Ok(f);
            pmm::free_frame(f as usize);
            mapped && shape_ok && result == 42 && unmapped
        }
        None => false,
    };
    check(&mut ok, b"KERNEL_CODE page is read-only and runs (mov eax, 42; ret -> 42)", code_ok);

    // --- 7. device memory: the screen, uncached ---
    // Write 4 bytes of screen memory through a new MMIO mapping and read
    // them back through the screen's own view: same physical bytes. In text
    // mode that is the VGA buffer (the bottom-right two cells); in graphics
    // mode the framebuffer's first page (VGA memory isn't plain memory then).
    let (screen_phys, screen_view, offset, value): (u64, u64, usize, u32) =
        match crate::display::mmio_test_target() {
            Some((phys, view)) => (phys, view, 0xFFC, 0x00FF_00FF),
            None => (0xB8000, 0xB8000, (24 * 80 + 78) * 2, 0x0F46_0F46),
        };
    let test_va = va + 0x2000;
    let mmio_ok = ks.map(test_va, screen_phys, PageFlags::MMIO).is_ok() && {
        let new_view = (test_va as *mut u8).add(offset) as *mut u32;
        let old_view = (screen_view as *const u8).add(offset) as *const u32;
        let saved = old_view.read_volatile();
        new_view.write_volatile(value);
        let seen = old_view.read_volatile();
        new_view.write_volatile(saved);
        let entry = ks.leaf_entry(test_va).unwrap_or(0);
        let uncached = entry & CACHE_DISABLE != 0;
        ks.unmap(test_va) == Ok(screen_phys) && seen == value && uncached
    };
    check(&mut ok, b"MMIO mapping of the screen's memory is uncached and aliases it", mmio_ok);

    // --- User address spaces ---
    // Where user pages may go: only user space, only in a user address
    // space; and nothing but user pages there.
    let before = pmm::free_frames();
    let regions_ok = match (AddressSpace::new_user(), pmm::alloc_frame()) {
        (Ok(mut us), Some(frame)) => {
            let frame = frame as u64;
            let refused = ks.map(USER_BASE, frame, PageFlags::USER_DATA) == Err(MapError::WrongRegion)
                && us.map(KERNEL_DYNAMIC_BASE, frame, PageFlags::USER_DATA) == Err(MapError::WrongRegion)
                && us.map(0x10_0000, frame, PageFlags::USER_DATA) == Err(MapError::WrongRegion)
                && us.map(USER_END, frame, PageFlags::USER_DATA) == Err(MapError::WrongRegion)
                && us.map(USER_BASE, frame, PageFlags::KERNEL_DATA) == Err(MapError::WrongRegion)
                && us.unmap(KERNEL_DYNAMIC_BASE) == Err(UnmapError::WrongRegion);
            let accepted = us.map(USER_BASE, frame, PageFlags::USER_CODE).is_ok()
                && us.user_page(USER_BASE, false) == Some(frame)
                && us.user_page(USER_BASE, true).is_none(); // code is read-only
            // The kernel half and the low 1 GiB are there, the low 1 GiB
            // without U/S; destroy gives back the page, every table and the
            // PML4.
            let pdpt = *table(us.root) & ADDR_MASK;
            let shared = us.translate(paging::PHYS_MAP_BASE + frame) == Some(frame)
                && us.translate(0x10_0000) == Some(0x10_0000)
                && *table(pdpt) & PRESENT != 0
                && *table(pdpt) & USER == 0;
            let spaces = user_spaces();
            us.destroy();
            refused && accepted && shared && spaces == 1
        }
        (Ok(us), None) => {
            us.destroy();
            false
        }
        (Err(_), frame) => {
            if let Some(f) = frame {
                pmm::free_frame(f);
            }
            false
        }
    };
    check(&mut ok, b"user pages only in user space, only in a user address space; kernel pages never there", regions_ok);
    check(
        &mut ok,
        b"user address space shares the kernel half, keeps the low 1 GiB kernel-only, gives every frame back",
        regions_ok && pmm::free_frames() == before && user_spaces() == 0,
    );

    s(b"[vmm] dynamic region at ");
    hex(KERNEL_DYNAMIC_BASE);
    s(b" clean after tests\n");
    ok
}

// ===========================================================================
// Demos (each halts with an exception report)
// ===========================================================================

/// Use after unmap: the page is used, unmapped, then used again. The CPU
/// cached the translation during the first use; the second use faults
/// (not present) only because unmap() flushed that TLB entry with invlpg.
pub unsafe fn demo_use_after_unmap() {
    let mut ks = AddressSpace::kernel();
    let va = KERNEL_DYNAMIC_BASE + 0x10_0000;
    if let Some(f) = pmm::alloc_frame() {
        let _ = ks.map(va, f as u64, PageFlags::KERNEL_DATA);
        core::ptr::write_volatile(va as *mut u8, 1); // TLB now holds va
        let _ = ks.unmap(va);
        core::ptr::read_volatile(va as *const u8);
    }
}

/// Write to a read-only mapping: protection-violation #PF on a write.
pub unsafe fn demo_write_read_only() {
    let mut ks = AddressSpace::kernel();
    let va = KERNEL_DYNAMIC_BASE + 0x10_0000;
    if let Some(f) = pmm::alloc_frame() {
        let _ = ks.map(va, f as u64, PageFlags::KERNEL_RODATA);
        core::ptr::write_volatile(va as *mut u8, 1);
    }
}
