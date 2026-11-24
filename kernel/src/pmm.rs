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
//! Memory model (the manifesto's Section III; Entry 29): every frame has a
//! record in the frame table, beside the bitmap, giving its state, its kind
//! and its owner. A frame moves Raw (free) -> Sanitized (zeroed) -> Typed
//! (given a kind and an owner), and back to Raw when it is freed; in the
//! kernel's Rust a held frame is a Frame<Raw>, Frame<Sanitized> or
//! Frame<Typed>, so the compiler keeps the order (see "Frame identity").
//!
//! Concurrency: alloc, free and reclaim run with interrupts disabled
//! (cpu::without_interrupts), so a task switch can never land half-way
//! through a bitmap update. That is sufficient on one CPU; SMP will need a
//! real lock.

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
// Frame identity (Entry 29; the manifesto's Section III)
// ===========================================================================
//
// Every frame has a record in the frame table, beside the bitmap: where it
// is in its life (its State), what it is used for (its Kind), and who it
// belongs to (its Owner). The bitmap still finds a free frame quickly; the
// table says what every other frame is.
//
// In the kernel's Rust, a frame the kernel holds is a Frame<S>, and S says
// where it is in its life. The only way to a Frame<Sanitized> is to zero a
// Frame<Raw> (sanitize), and the only way to a Frame<Typed> is to give a
// Frame<Sanitized> a kind and an owner (assign): the compiler enforces
// Raw -> Sanitized -> Typed. A Frame can't be copied, and dropping one
// halts, so a frame is never lost by forgetting it: it is stored (into_phys:
// a page table holds it by address from then on) or freed. A frame stored
// by address comes back only through reclaim(), which halts unless the
// table says it is Typed, of the kind expected, belonging to the owner
// expected: a double free, a free by the wrong owner, or a free of the
// kernel's own image is caught there (the bitmap alone couldn't tell the
// last one from an allocated frame).

use core::marker::PhantomData;

/// Where a frame is in its life.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    /// Free, in the allocator's pool. It may still hold its last owner's
    /// data: nobody can reach it until it is sanitized.
    Raw = 0,
    /// Zeroed, on its way to an owner (only ever seen inside alloc()).
    Sanitized = 1,
    /// Given a kind and an owner.
    Typed = 2,
    /// Never the allocator's to hand out: low memory and whatever the
    /// firmware keeps, the kernel image, the allocator's own bitmap and
    /// table, the page tables of the physical map.
    Reserved = 3,
}

impl State {
    fn from_byte(b: u8) -> State {
        match b {
            0 => State::Raw,
            1 => State::Sanitized,
            2 => State::Typed,
            _ => State::Reserved,
        }
    }

    pub fn name(self) -> &'static [u8] {
        match self {
            State::Raw => b"free (raw)",
            State::Sanitized => b"sanitized",
            State::Typed => b"typed",
            State::Reserved => b"reserved",
        }
    }
}

/// What a frame is used for.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// A free frame has no kind.
    None = 0,
    /// Reserved: low memory, RAM the firmware keeps, and addresses in the
    /// table's range that aren't RAM at all (the PCI hole below 4 GiB).
    Firmware = 1,
    /// Reserved: the kernel's code, data and .bss.
    KernelImage = 2,
    /// Reserved: the frame allocator's bitmap and this table.
    Allocator = 3,
    PageTable = 4,
    KernelStack = 5,
    KernelHeap = 6,
    ProcessCode = 7,
    ProcessData = 8,
    ProcessStack = 9,
    ProcessHeap = 10,
    /// A self-test's own frame.
    Test = 11,
}

/// How many kinds there are.
pub const KINDS: usize = 12;

const ALL_KINDS: [Kind; KINDS] = [
    Kind::None,
    Kind::Firmware,
    Kind::KernelImage,
    Kind::Allocator,
    Kind::PageTable,
    Kind::KernelStack,
    Kind::KernelHeap,
    Kind::ProcessCode,
    Kind::ProcessData,
    Kind::ProcessStack,
    Kind::ProcessHeap,
    Kind::Test,
];

impl Kind {
    /// This kind as one bit of a set of kinds (for reclaim()).
    pub const fn bit(self) -> u32 {
        1 << self as u32
    }

    fn from_byte(b: u8) -> Kind {
        ALL_KINDS.get(b as usize).copied().unwrap_or(Kind::None)
    }

    /// The kind numbered `n` (Info lists them in this order).
    pub fn nth(n: usize) -> Kind {
        Kind::from_byte(n as u8)
    }

    pub fn name(self) -> &'static [u8] {
        match self {
            Kind::None => b"none",
            Kind::Firmware => b"firmware",
            Kind::KernelImage => b"kernel image",
            Kind::Allocator => b"allocator",
            Kind::PageTable => b"page table",
            Kind::KernelStack => b"kernel stack",
            Kind::KernelHeap => b"kernel heap",
            Kind::ProcessCode => b"process code",
            Kind::ProcessData => b"process data",
            Kind::ProcessStack => b"process stack",
            Kind::ProcessHeap => b"process heap",
            Kind::Test => b"self-test",
        }
    }
}

/// The kinds of page a process's own address space holds, and frees when
/// the process ends.
pub const PROCESS_PAGES: u32 =
    Kind::ProcessCode.bit() | Kind::ProcessData.bit() | Kind::ProcessStack.bit() | Kind::ProcessHeap.bit();

/// Who a frame belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// Free and reserved frames belong to nobody.
    Nobody,
    Kernel,
    /// A process still being built: its address space exists, its task
    /// doesn't yet. sched::spawn hands every such frame to the task when
    /// it has an id (vmm::AddressSpace::adopt).
    Unassigned,
    /// A task, named by its slot and that slot's generation (Entry 24),
    /// so a frame can't be taken as belonging to a later task that happens
    /// to reuse the slot.
    Task { id: u8, generation: u32 },
}

const OWNER_NOBODY: u8 = 0;
const OWNER_UNASSIGNED: u8 = 0xFE;
const OWNER_KERNEL: u8 = 0xFF;

impl Owner {
    fn byte(self) -> u8 {
        match self {
            Owner::Nobody => OWNER_NOBODY,
            Owner::Kernel => OWNER_KERNEL,
            Owner::Unassigned => OWNER_UNASSIGNED,
            Owner::Task { id, .. } => id + 1,
        }
    }

    fn generation(self) -> u32 {
        match self {
            Owner::Task { generation, .. } => generation,
            _ => 0,
        }
    }

    fn from_parts(byte: u8, generation: u32) -> Owner {
        match byte {
            OWNER_NOBODY => Owner::Nobody,
            OWNER_KERNEL => Owner::Kernel,
            OWNER_UNASSIGNED => Owner::Unassigned,
            slot => Owner::Task { id: slot - 1, generation },
        }
    }

    /// "the kernel", "task 7 (generation 2)", ... for the log.
    pub fn print(self) {
        use crate::{serial_write_str as s, serial_write_u64 as dec};
        match self {
            Owner::Nobody => s(b"nobody"),
            Owner::Kernel => s(b"the kernel"),
            Owner::Unassigned => s(b"a process being built"),
            Owner::Task { id, generation } => {
                s(b"task ");
                dec(id as u64);
                s(b" (generation ");
                dec(generation as u64);
                s(b")");
            }
        }
    }
}

/// A frame's record: bits 0-7 its state, 8-15 its kind, 16-23 its owner
/// (0 nobody, 0xFE unassigned, 0xFF the kernel, else a task's slot plus
/// one, since slot 0 is the boot thread's), 32-63
/// the owning task's generation. Eight bytes a frame: 0.2% of memory.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Record(u64);

impl Record {
    const fn new(state: State, kind: Kind, owner_byte: u8, generation: u32) -> Record {
        Record(state as u64 | (kind as u64) << 8 | (owner_byte as u64) << 16 | (generation as u64) << 32)
    }

    fn of(state: State, kind: Kind, owner: Owner) -> Record {
        Record::new(state, kind, owner.byte(), owner.generation())
    }

    pub fn state(self) -> State {
        State::from_byte(self.0 as u8)
    }

    pub fn kind(self) -> Kind {
        Kind::from_byte((self.0 >> 8) as u8)
    }

    pub fn owner(self) -> Owner {
        Owner::from_parts((self.0 >> 16) as u8, (self.0 >> 32) as u32)
    }

    /// "typed process heap of task 7 (generation 2)" for the log.
    pub fn print(self) {
        use crate::serial_write_str as s;
        s(self.state().name());
        if self.kind() != Kind::None {
            s(b" ");
            s(self.kind().name());
        }
        if self.owner() != Owner::Nobody {
            s(b" of ");
            self.owner().print();
        }
    }
}

const RAW: Record = Record::new(State::Raw, Kind::None, OWNER_NOBODY, 0);

/// Type-state markers for Frame<S> (never values, only types).
pub enum Raw {}
pub enum Sanitized {}
pub enum Typed {}

/// A frame the kernel holds, in state S. Not Copy, not Clone: one holder.
pub struct Frame<S> {
    phys: u64,
    state: PhantomData<S>,
}

impl<S> Frame<S> {
    /// Its physical address.
    pub fn phys(&self) -> u64 {
        self.phys
    }

    /// The same frame, in another state (only pmm moves a frame along).
    fn become_<T>(self) -> Frame<T> {
        let phys = self.phys;
        core::mem::forget(self);
        Frame { phys, state: PhantomData }
    }
}

impl<S> Drop for Frame<S> {
    /// A frame let go of without being stored or freed is memory lost:
    /// halt and say where, rather than leak it quietly.
    fn drop(&mut self) {
        crate::serial_write_str(b"[pmm] FATAL: a frame was dropped while still held, neither stored nor freed: ");
        crate::serial_write_hex(self.phys);
        crate::serial_write_str(b"\n");
        crate::halt();
    }
}

impl Frame<Raw> {
    /// Raw -> Sanitized: zero it. The only way to a Sanitized frame.
    pub fn sanitize(self) -> Frame<Sanitized> {
        unsafe {
            core::ptr::write_bytes(paging::phys_to_virt(self.phys), 0, PAGE_SIZE as usize);
            (*(&raw mut PMM)).set(self.phys, Record::of(State::Sanitized, Kind::None, Owner::Nobody));
        }
        self.become_()
    }
}

impl Frame<Sanitized> {
    /// Sanitized -> Typed: give it a kind and an owner. The only way to a
    /// Typed frame (but for reclaim(), which takes back one already Typed).
    pub fn assign(self, kind: Kind, owner: Owner) -> Frame<Typed> {
        unsafe { (*(&raw mut PMM)).set(self.phys, Record::of(State::Typed, kind, owner)) };
        self.become_()
    }
}

impl Frame<Typed> {
    pub fn kind(&self) -> Kind {
        record(self.phys).kind()
    }

    pub fn owner(&self) -> Owner {
        record(self.phys).owner()
    }

    /// Store it by address (in a page table, a list): the table keeps its
    /// identity, and reclaim() is the way back.
    pub fn into_phys(self) -> u64 {
        let phys = self.phys;
        core::mem::forget(self);
        phys
    }

    /// Take back the frame at `phys` that was stored by address. Halts
    /// unless the table says it is Typed, of one of `kinds` (Kind::bit()s),
    /// and belongs to `owner`.
    ///
    /// # Safety
    /// Whatever held `phys` (a page-table entry, a list) must no longer use
    /// it: from here, the returned Frame is its only holder.
    pub unsafe fn reclaim(phys: u64, kinds: u32, owner: Owner) -> Frame<Typed> {
        match Self::try_reclaim(phys, kinds, owner) {
            Ok(frame) => frame,
            Err(found) => {
                use crate::serial_write_str as s;
                s(b"[pmm] FATAL: frame ");
                crate::serial_write_hex(phys);
                s(b" given back as ");
                let mut first = true;
                for k in ALL_KINDS {
                    if kinds & k.bit() != 0 {
                        s(if first { b"" } else { b" or " });
                        s(k.name());
                        first = false;
                    }
                }
                s(b" of ");
                owner.print();
                s(b", but it is ");
                found.print();
                s(b"\n");
                crate::halt();
            }
        }
    }

    /// reclaim(), but a mismatch is answered with what the table says
    /// instead of halting (for the self-tests).
    ///
    /// # Safety
    /// As for reclaim().
    pub unsafe fn try_reclaim(phys: u64, kinds: u32, owner: Owner) -> Result<Frame<Typed>, Record> {
        crate::cpu::without_interrupts(|| {
            let pmm = &*(&raw const PMM);
            if phys % PAGE_SIZE != 0 || phys / PAGE_SIZE >= pmm.frames {
                return Err(Record::of(State::Reserved, Kind::Firmware, Owner::Nobody));
            }
            let r = pmm.get(phys);
            if r.state() != State::Typed || kinds & r.kind().bit() == 0 || r.owner() != owner {
                return Err(r);
            }
            Ok(Frame { phys, state: PhantomData })
        })
    }

    /// Typed -> Raw: back to the pool. It is zeroed again before anyone
    /// else can have it.
    pub fn free(self) {
        let phys = self.into_phys();
        crate::cpu::without_interrupts(|| unsafe {
            let pmm = &mut *(&raw mut PMM);
            pmm.set(phys, RAW);
            pmm.release(phys / PAGE_SIZE);
        });
    }
}

/// A new frame of `kind` for `owner`: taken from the pool, zeroed, given
/// its kind and owner (Raw -> Sanitized -> Typed). None when memory is
/// exhausted.
pub fn alloc(kind: Kind, owner: Owner) -> Option<Frame<Typed>> {
    crate::cpu::without_interrupts(|| take().map(|raw| raw.sanitize().assign(kind, owner)))
}

/// A frame's record (Raw for anything outside the table).
pub fn record(phys: u64) -> Record {
    crate::cpu::without_interrupts(|| unsafe {
        let pmm = &*(&raw const PMM);
        if phys / PAGE_SIZE < pmm.frames { pmm.get(phys) } else { Record::of(State::Reserved, Kind::Firmware, Owner::Nobody) }
    })
}

/// Hand a frame of a process being built (Owner::Unassigned) to the task
/// that now runs it. Halts if it isn't such a frame.
///
/// # Safety
/// `phys` must be a frame of that process's address space.
pub(crate) unsafe fn adopt(phys: u64, owner: Owner) {
    crate::cpu::without_interrupts(|| {
        let pmm = &mut *(&raw mut PMM);
        let r = if phys / PAGE_SIZE < pmm.frames { pmm.get(phys) } else { RAW };
        if r.state() != State::Typed || r.owner() != Owner::Unassigned {
            crate::serial_write_str(b"[pmm] FATAL: frame ");
            crate::serial_write_hex(phys);
            crate::serial_write_str(b" handed to a new process, but it is ");
            r.print();
            crate::serial_write_str(b"\n");
            crate::halt();
        }
        pmm.set(phys, Record::of(State::Typed, r.kind(), owner));
    })
}

/// Frames of `kind` right now (Kind::None: the free ones).
pub fn frames_of(kind: Kind) -> u64 {
    crate::cpu::without_interrupts(|| unsafe {
        let pmm = &*(&raw const PMM);
        if kind == Kind::None { pmm.free } else { pmm.kinds[kind as usize] }
    })
}

/// Frames task slot `id` owns right now (whatever their generation: a slot
/// holds one task at a time).
pub fn frames_owned_by(id: u8) -> u64 {
    let byte = Owner::Task { id, generation: 0 }.byte();
    crate::cpu::without_interrupts(|| unsafe { (*(&raw const PMM)).owned[byte as usize] as u64 })
}

/// Frames still with a process being built (Owner::Unassigned): none once
/// every process has its task (Entry 29).
pub fn frames_being_built() -> u64 {
    crate::cpu::without_interrupts(|| unsafe { (*(&raw const PMM)).owned[OWNER_UNASSIGNED as usize] as u64 })
}

/// Frames that name as their owner a task that no longer runs: `live(id,
/// generation)` says which do. Reads the whole table (for the self-tests).
pub fn orphans(live: impl Fn(u8, u32) -> bool) -> u64 {
    let frames = unsafe { (*(&raw const PMM)).frames };
    let mut n = 0;
    for f in 0..frames {
        let r = record(f * PAGE_SIZE);
        if let (State::Typed, Owner::Task { id, generation }) = (r.state(), r.owner()) {
            if !live(id, generation) {
                n += 1;
            }
        }
    }
    n
}

// ===========================================================================
// Step 3: the bitmap and the table
// ===========================================================================

struct FrameAllocator {
    bitmap: *mut u64,  // physical-map address of the first bitmap word
    table: *mut u64,   // physical-map address of the first frame record
    words: usize,      // bitmap length in u64 words
    frames: u64,       // frames covered (physical [0, frames * 4 KiB))
    hint: usize,       // no free frame exists in any word before this one
    free: u64,         // free-frame counter
    kinds: [u64; KINDS], // frames of each kind that aren't free
    owned: [u32; 256], // frames owned, by owner byte (a task's slot, 0xFE, 0xFF)
}

static mut PMM: FrameAllocator = FrameAllocator {
    bitmap: core::ptr::null_mut(),
    table: core::ptr::null_mut(),
    words: 0,
    frames: 0,
    hint: 0,
    free: 0,
    kinds: [0; KINDS],
    owned: [0; 256],
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

    /// Record `r` for every frame [start, end) touches (init only: the
    /// counters are taken afterwards).
    unsafe fn record_bytes(&mut self, start: u64, end: u64, r: Record) {
        let last = (align_up(end, PAGE_SIZE) / PAGE_SIZE).min(self.frames);
        for frame in start / PAGE_SIZE..last {
            *self.table.add(frame as usize) = r.0;
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

    unsafe fn get(&self, phys: u64) -> Record {
        Record(*self.table.add((phys / PAGE_SIZE) as usize))
    }

    /// Change a frame's record, keeping the counters of each kind and of
    /// each owner in step.
    unsafe fn set(&mut self, phys: u64, new: Record) {
        let slot = self.table.add((phys / PAGE_SIZE) as usize);
        let old = Record(*slot);
        if old.state() != State::Raw {
            self.kinds[old.kind() as usize] -= 1;
        }
        if old.state() == State::Typed {
            self.owned[old.owner().byte() as usize] -= 1;
        }
        if new.state() != State::Raw {
            self.kinds[new.kind() as usize] += 1;
        }
        if new.state() == State::Typed {
            self.owned[new.owner().byte() as usize] += 1;
        }
        *slot = new.0;
    }

    /// Put frame number `frame` back in the bitmap's pool.
    unsafe fn release(&mut self, frame: u64) {
        let w = (frame / 64) as usize;
        let bit = 1u64 << (frame % 64);
        let word = self.bitmap.add(w);
        if *word & bit == 0 {
            fatal(b"a frame freed that the bitmap already had free ", frame * PAGE_SIZE);
        }
        *word &= !bit;
        self.free += 1;
        if w < self.hint {
            self.hint = w;
        }
    }
}

/// What init() set up, for the boot log.
pub struct PmmStats {
    pub frames: u64,
    pub free_frames: u64,
    pub usable_bytes: u64,
    pub bitmap_phys: u64,
    pub bitmap_bytes: u64,
    pub table_bytes: u64,
}

/// Size and place the bitmap and the frame table, then build them from the
/// E820 map.
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
    // The table: one 8-byte record per frame (Entry 29), right after the
    // bitmap, in the same run of RAM.
    let table_bytes = align_up(frames * 8, PAGE_SIZE);
    let meta_bytes = bitmap_bytes + table_bytes;

    // --- Place: first usable run at or above the early frames ---
    // Above the identity map's 8 MiB if there is room there (Entry 29):
    // the table is large on a large machine (35 MiB at 16 GiB), and at the
    // bottom it would leave no free frame the identity map reaches.
    let floor = align_up(early_used.1.max(LOW_MEMORY_END), PAGE_SIZE);
    let first_fit = |floor: u64| {
        e820.iter().filter_map(usable).find_map(|(base, end)| {
            let start = align_up(base.max(floor), PAGE_SIZE);
            (start + meta_bytes <= end).then_some(start)
        })
    };
    let bitmap_phys = first_fit(floor.max(paging::IDENTITY_MAP_BYTES)).or_else(|| first_fit(floor)).unwrap_or(0);
    if bitmap_phys == 0 {
        fatal(b"no usable RAM large enough for the frame bitmap and table, bytes needed: ", meta_bytes);
    }

    let pmm = &mut *(&raw mut PMM);
    *pmm = FrameAllocator {
        bitmap: paging::phys_to_virt(bitmap_phys) as *mut u64,
        table: paging::phys_to_virt(bitmap_phys + bitmap_bytes) as *mut u64,
        words,
        frames,
        hint: 0,
        free: 0,
        kinds: [0; KINDS],
        owned: [0; 256],
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
    pmm.mark_bytes(bitmap_phys, bitmap_phys + meta_bytes, true);

    // --- The table: free where the bitmap is, reserved everywhere else,
    // and the reserved frames the kernel knows about named ---
    let firmware = Record::of(State::Reserved, Kind::Firmware, Owner::Nobody);
    for frame in 0..frames {
        let r = if pmm.is_free(frame) { RAW } else { firmware };
        *pmm.table.add(frame as usize) = r.0;
    }
    pmm.record_bytes(kernel_start, kernel_end, Record::of(State::Reserved, Kind::KernelImage, Owner::Kernel));
    pmm.record_bytes(early_used.0, early_used.1, Record::of(State::Reserved, Kind::PageTable, Owner::Kernel));
    pmm.record_bytes(bitmap_phys, bitmap_phys + meta_bytes, Record::of(State::Reserved, Kind::Allocator, Owner::Kernel));
    for frame in 0..frames {
        let r = Record(*pmm.table.add(frame as usize));
        if r.state() != State::Raw {
            pmm.kinds[r.kind() as usize] += 1;
        }
    }

    pmm.free = pmm.count_free();
    USABLE_FRAMES.store(usable_bytes / PAGE_SIZE, core::sync::atomic::Ordering::Relaxed);

    PmmStats {
        frames,
        free_frames: pmm.free,
        usable_bytes,
        bitmap_phys,
        bitmap_bytes,
        table_bytes,
    }
}

/// Take a free frame out of the pool: Raw, not yet zeroed. Only alloc()
/// does this, and sanitizes it at once.
///
/// Scans 64 frames per step, starting at the hint instead of frame 0.
fn take() -> Option<Frame<Raw>> {
    crate::cpu::assert_interrupts_off(b"pmm take");
    let pmm = unsafe { &mut *(&raw mut PMM) };
    if pmm.bitmap.is_null() {
        fatal(b"a frame asked for before pmm::init, caller near ", 0);
    }
    for w in pmm.hint..pmm.words {
        let word = unsafe { *pmm.bitmap.add(w) };
        if word != u64::MAX {
            let bit = (!word).trailing_zeros() as u64;
            unsafe { *pmm.bitmap.add(w) = word | (1 << bit) };
            pmm.hint = w;
            pmm.free -= 1;
            return Some(Frame { phys: (w as u64 * 64 + bit) * PAGE_SIZE, state: PhantomData });
        }
    }
    pmm.hint = pmm.words;
    None
}

/// Is the frame at `phys` free? (For self-tests: false outside the bitmap.)
pub fn is_frame_free(phys: u64) -> bool {
    crate::cpu::without_interrupts(|| unsafe { (*(&raw const PMM)).is_free(phys / PAGE_SIZE) })
}

/// Free frames right now.
pub fn free_frames() -> u64 {
    unsafe { (*(&raw const PMM)).free }
}

static USABLE_FRAMES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Frames of RAM the firmware's map calls usable (Entry 23: the shell's
/// mem shows free frames out of these).
pub fn usable_frames() -> u64 {
    USABLE_FRAMES.load(core::sync::atomic::Ordering::Relaxed)
}

// ===========================================================================
// Self-test
// ===========================================================================

/// 1. The free counter agrees with a full recount of the bitmap.
/// 2. The last usable frame is tracked and free (the old fixed-size bitmap
///    silently dropped everything above 16 GiB of physical address).
/// 3. alloc -> free -> alloc returns the same frame (the hint moves back).
/// 4. The counter goes down by one and back up across alloc/free.
/// 5. The table agrees with the bitmap frame by frame, and its counters
///    with a recount (Entry 29).
/// 6. A frame's life: alloc records it Typed, with its kind and owner,
///    zeroed; free makes it Raw again (Entry 29).
/// 7. reclaim refuses a frame given back by the wrong owner or as the
///    wrong kind, a free frame, and the kernel's own image (Entry 29).
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
    let round_trip_ok = match alloc(Kind::Test, Owner::Kernel) {
        Some(a) => {
            let during = free_frames();
            let pa = a.phys();
            a.free();
            let after_free = free_frames();
            match alloc(Kind::Test, Owner::Kernel) {
                Some(b) => {
                    let pb = b.phys();
                    b.free();
                    pa == pb && during == before - 1 && after_free == before && free_frames() == before
                }
                None => false,
            }
        }
        None => false,
    };
    s(b"[pmm] self-test alloc/free/alloc returns the same frame, counter tracks it");
    crate::selftest::verdict(round_trip_ok);

    // --- 5. the table against the bitmap, and its counters ---
    let mut kinds = [0u64; KINDS];
    let mut disagree = 0u64;
    for frame in 0..pmm.frames {
        let r = pmm.get(frame * PAGE_SIZE);
        if pmm.is_free(frame) != (r.state() == State::Raw) {
            disagree += 1;
        }
        if r.state() != State::Raw {
            kinds[r.kind() as usize] += 1;
        }
    }
    let table_ok = disagree == 0 && kinds == pmm.kinds && kinds[Kind::KernelImage as usize] > 0 && kinds[Kind::Allocator as usize] > 0;
    s(b"[pmm] self-test frame table: ");
    dec(pmm.frames);
    s(b" records (");
    dec(align_up(pmm.frames * 8, PAGE_SIZE) / 1024);
    s(b" KiB), ");
    dec(disagree);
    s(b" disagreeing with the bitmap; reserved: ");
    dec(kinds[Kind::Firmware as usize]);
    s(b" firmware, ");
    dec(kinds[Kind::KernelImage as usize]);
    s(b" kernel image, ");
    dec(kinds[Kind::Allocator as usize]);
    s(b" allocator; ");
    dec(kinds[Kind::PageTable as usize]);
    s(b" page tables so far; the counters match a recount");
    crate::selftest::verdict(table_ok);

    // --- 6. a frame's life ---
    let life_ok = match alloc(Kind::Test, Owner::Kernel) {
        Some(f) => {
            let phys = f.phys();
            let typed = record(phys) == Record::of(State::Typed, Kind::Test, Owner::Kernel)
                && f.kind() == Kind::Test
                && f.owner() == Owner::Kernel;
            let words = paging::phys_to_virt(phys) as *const u64;
            let zeroed = (0..PAGE_SIZE as usize / 8).all(|i| words.add(i).read_volatile() == 0);
            let counted = frames_of(Kind::Test) == 1 && unsafe { (*(&raw const PMM)).owned[OWNER_KERNEL as usize] } > 0;
            // Dirty it, free it, and see it come back clean.
            (paging::phys_to_virt(phys) as *mut u64).write_volatile(0xD1D1_D1D1_D1D1_D1D1);
            f.free();
            let raw = record(phys) == RAW && frames_of(Kind::Test) == 0;
            let again = match alloc(Kind::Test, Owner::Kernel) {
                Some(g) => {
                    let clean = g.phys() != phys || words.read_volatile() == 0;
                    g.free();
                    clean
                }
                None => false,
            };
            typed && zeroed && counted && raw && again
        }
        None => false,
    };
    s(b"[pmm] self-test a frame's life: allocated, it is typed (self-test, the kernel's) and zeroed, and counted; freed, raw again, and zeroed again before reuse");
    crate::selftest::verdict(life_ok);

    // --- 7. reclaim refuses the wrong owner, kind, state ---
    let task = Owner::Task { id: 7, generation: 1 };
    let refusals_ok = match alloc(Kind::ProcessHeap, task) {
        Some(f) => {
            let phys = f.into_phys();
            let wrong_owner = Frame::try_reclaim(phys, Kind::ProcessHeap.bit(), Owner::Task { id: 7, generation: 2 }).is_err();
            let wrong_kind = Frame::try_reclaim(phys, Kind::KernelStack.bit(), task).is_err();
            let right = match Frame::try_reclaim(phys, PROCESS_PAGES, task) {
                Ok(f) => {
                    f.free();
                    true
                }
                Err(_) => false,
            };
            let free_again = Frame::try_reclaim(phys, PROCESS_PAGES, task).is_err();
            let (kernel_start, _) = kernel_image();
            let image = matches!(
                Frame::try_reclaim(kernel_start, u32::MAX, Owner::Kernel),
                Err(r) if r.state() == State::Reserved && r.kind() == Kind::KernelImage
            );
            wrong_owner && wrong_kind && right && free_again && image && frames_owned_by(7) == 0
        }
        None => false,
    };
    s(b"[pmm] self-test given back by the wrong owner (another generation of the same slot) or as the wrong kind, already free, or the kernel's own image: refused; by its owner, as its kind: taken");
    crate::selftest::verdict(refusals_ok);

    count_ok && top_ok && round_trip_ok && table_ok && life_ok && refusals_ok
}

/// After the self-tests' processes have ended (Entry 29): no frame names
/// as its owner a task that no longer holds its slot (`live` says which
/// do), and no self-test frame is left.
pub fn selftest_owners(live: impl Fn(u8, u32) -> bool) -> bool {
    use crate::{serial_write_str as s, serial_write_u64 as dec};
    let orphaned = orphans(live);
    let tests = frames_of(Kind::Test);
    s(b"[pmm] self-test after every test process: ");
    dec(orphaned);
    s(b" frames owned by a task that has gone, ");
    dec(tests);
    s(b" self-test frames left");
    crate::selftest::verdict(orphaned == 0 && tests == 0)
}

/// What the frames are doing, for the boot log (Entry 29).
pub fn print_kinds() {
    use crate::{serial_write_str as s, serial_write_u64 as dec};
    s(b"[pmm] frames by kind:");
    for k in ALL_KINDS {
        let n = frames_of(k);
        if n > 0 && k != Kind::None {
            s(b" ");
            s(k.name());
            s(b" ");
            dec(n);
            s(b",");
        }
    }
    s(b" free ");
    dec(free_frames());
    s(b"\n");
}

/// Demos (Entry 29), each of which must halt: a frame held and then
/// dropped, and a frame of the kernel's own image given back as if it
/// were the kernel heap's.
pub fn demo_lost_frame() {
    let f = alloc(Kind::Test, Owner::Kernel);
    crate::serial_write_str(b"[demo] a frame allocated and dropped without being freed\n");
    drop(f);
}

pub fn demo_free_kernel_image() {
    let (kernel_start, _) = kernel_image();
    crate::serial_write_str(b"[demo] the kernel image's first frame given back as kernel heap\n");
    unsafe { Frame::reclaim(kernel_start, Kind::KernelHeap.bit(), Owner::Kernel).free() };
}
