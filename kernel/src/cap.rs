//! cap.rs — capabilities: what a process may do, one slot at a time.
//!
//! Every process has a table of MAX_CAPS slots. A system call that acts on
//! anything outside the caller names a slot, and the kernel checks what is
//! in it; there is no other source of authority (Zero Ambient Authority).
//! A process starts with exactly the capabilities its creator put in its
//! table, and gains more only by being sent one in an IPC message, which
//! the sender may do only through an endpoint capability with GRANT.
//!
//! Kinds of capability:
//!   - Console: may write to the system log (log.rs), which the console
//!     driver shows, and leave notes on serial (system call note).
//!   - Endpoint: may send to and/or receive from an IPC endpoint (ipc.rs),
//!     with rights SEND, RECV, GRANT. The badge is fixed when the capability
//!     is made and is delivered with every message sent through it, so a
//!     server can tell its clients apart without trusting them.
//!   - IoPorts: may read and write the I/O ports [base, base + count): what
//!     a user-space driver needs to talk to its device.
//!   - Irq: may wait for (and acknowledge) one interrupt line.
//!   - Power: may restart or switch off the machine.
//!   - Memory: may map `pages` pages of physical memory from `base` into
//!     its own address space (system call map, mem.rs), with the access
//!     and caching `flags` say: a framebuffer, or pages the kernel shares
//!     read-only, such as the system log (Entry 20). The pages stay
//!     whoever's they were; the process only borrows them.
//!   - Factory: may create processes from the programs built into the
//!     kernel and new endpoints (system calls spawn and endpoint,
//!     factory.rs). Held by init (Entry 21), which builds the rest of the
//!     system with it. Since Entry 24 it has rights: which programs it may
//!     start (bit n: program n) and whether it may make endpoints, so init
//!     can hand the shell one that starts only a few test programs.
//!   - Process (Entry 24): names one process (its task slot, and which of
//!     the tasks that slot has held), and may ask how it is doing and stop
//!     it (system calls status and kill). spawn hands one back on request,
//!     so whoever starts a program, and nobody else, can stop it.
//!   - Info: may read the system's state: how long it has been up, how
//!     much memory is free, and every task with its state (system call
//!     info, info.rs). Read-only; what the shell's ps, uptime and mem show
//!     (Entry 23).
//!
//! Capabilities are values; copying one into another table (spawn, or a
//! transfer in a message) copies the authority. Endpoints are reference
//! counted by the capabilities that name them (ipc::retain / release).
//!
//! An endpoint capability can be copied with less authority (system call
//! mint, Entry 21): fewer rights, and a badge if it had none yet. That is
//! how init gives the console driver only RECV on its endpoint and the
//! keyboard driver only SEND, with a badge that names it. A Factory can be
//! copied with fewer rights too (Entry 24). And a process can let go of a
//! capability (system call drop).
//!
//! Revocation (Entry 25). Every capability in a table has a parent: the
//! capability it was copied from (by mint, copy, spawn's slot list, or a
//! message), or none if the kernel made it. revoke(slot) removes every
//! capability derived from the one in `slot`, directly or through any
//! number of copies, in any process, and leaves that one in place: whoever
//! hands out a copy can take it back, and everything made from it. A
//! capability that is dropped (or whose process ends) leaves what was made
//! from it to its own parent, so that stays revocable from above. A Memory
//! capability that is removed takes its mapping with it, so revoking
//! memory takes the access, not just the name. And a copy on its way (in a
//! message not yet received, or into a process being made) whose original
//! is removed first is never delivered.

pub const MAX_CAPS: usize = 32;

/// Endpoint rights.
pub const SEND: u8 = 1 << 0;
pub const RECV: u8 = 1 << 1;
/// May pass a capability along with a message sent through this one.
pub const GRANT: u8 = 1 << 2;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    None,
    Console,
    Endpoint { ep: u8, badge: u64, rights: u8 },
    IoPorts { base: u16, count: u16 },
    Irq { line: u8 },
    /// May restart or switch off the machine (power.rs).
    Power,
    /// May map physical [base, base + pages * 4 KiB) (mem.rs).
    Memory { base: u64, pages: u32, flags: u8 },
    /// May create processes and endpoints (factory.rs): `rights` says
    /// which programs (bit n: program n) and whether endpoints
    /// (FACTORY_ENDPOINTS).
    Factory { rights: u32 },
    /// Names task slot `id`'s `generation`th task, a process; may ask how
    /// it is doing and stop it (Entry 24).
    Process { id: u8, generation: u32 },
    /// May read the system's state: uptime, memory, tasks (info.rs).
    Info,
    /// May have up to `pages` pages of fresh memory in use at once (Entry
    /// 26), counting its own heap and the heaps of everything copied from
    /// it; and so may every Budget it was copied from, each counting its
    /// own. Its holder grows a heap with it (mem.rs: heap).
    Budget { pages: u32 },
}

/// Memory capability flags.
pub const MEM_WRITE: u8 = 1 << 0;
/// Write-combining: for a framebuffer.
pub const MEM_WRITE_COMBINING: u8 = 1 << 1;
/// Uncached: for device memory that must see every access (VGA text).
pub const MEM_UNCACHED: u8 = 1 << 2;

/// Factory rights (Entry 24): bits 0 to 15 are programs (factory.rs:
/// PROGRAMS), this one making endpoints.
pub const FACTORY_ENDPOINTS: u32 = 1 << 16;
/// Every program and endpoints: the Factory the kernel gives init.
pub const FACTORY_ALL: u32 = 0xFFFF | FACTORY_ENDPOINTS;

pub type Caps = [Cap; MAX_CAPS];
pub const NO_CAPS: Caps = [Cap::None; MAX_CAPS];

/// A table holding `list` in slots 0, 1, 2, ... and nothing else.
pub const fn table(list: &[Cap]) -> Caps {
    let mut caps = NO_CAPS;
    let mut i = 0;
    while i < list.len() && i < MAX_CAPS {
        caps[i] = list[i];
        i += 1;
    }
    caps
}

/// The console capability in slot 0 (user/freya.inc: CONSOLE equ 0).
pub const CONSOLE_ONLY: Caps = table(&[Cap::Console]);

/// The first empty slot, if any.
pub fn free_slot(caps: &Caps) -> Option<usize> {
    caps.iter().position(|&c| c == Cap::None)
}

/// A capability is being stored in a table: count the reference (and, for
/// one with RECV, that someone can receive there).
pub fn retain(cap: Cap) {
    if let Cap::Endpoint { ep, rights, .. } = cap {
        crate::ipc::retain(ep);
        if rights & RECV != 0 {
            crate::ipc::retain_receiver(ep);
        }
    }
}

/// A capability is leaving a table.
pub fn release(cap: Cap) {
    if let Cap::Endpoint { ep, rights, .. } = cap {
        if rights & RECV != 0 {
            crate::ipc::release_receiver(ep);
        }
        crate::ipc::release(ep);
    }
}

/// mint(slot, rights, badge): a copy of the caller's endpoint capability in
/// `slot`, with `rights` (which it must already have; at least one) and
/// `badge`, in the caller's first free slot. A capability without a badge
/// (0) can be given one; one with a badge keeps it, and asking for another
/// is refused. No capability ever gets the signal badge. Returns the slot.
///
/// For a Factory (Entry 24), `rights` are its rights (FACTORY_ENDPOINTS and
/// program bits), again a subset of what it has and at least one, and
/// there is no badge (0).
pub fn mint(slot: u64, rights: u64, badge: u64) -> i64 {
    use crate::syscall::{ERR_BAD_ARGUMENT, ERR_WRONG_CAPABILITY};
    // The copy records where it came from (Entry 25): revoking the
    // original takes it too.
    let (source, origin) = current_with_origin(slot);
    let copy = match source {
        Cap::Endpoint { ep, badge: old, rights: have } => {
            let all = (SEND | RECV | GRANT) as u64;
            if rights == 0 || rights & !all != 0 || rights & !(have as u64) != 0 {
                return ERR_BAD_ARGUMENT; // none, unknown, or more than it has
            }
            if badge == crate::ipc::SIGNAL_BADGE || (old != 0 && badge != old) {
                return ERR_BAD_ARGUMENT;
            }
            Cap::Endpoint { ep, badge, rights: rights as u8 }
        }
        Cap::Factory { rights: have } => {
            if rights == 0 || rights & !(have as u64) != 0 || badge != 0 {
                return ERR_BAD_ARGUMENT; // none, more than it has, or a badge
            }
            Cap::Factory { rights: rights as u32 }
        }
        // Entry 26: `rights` is how many pages; its heap counts against the
        // original's too.
        Cap::Budget { pages: have } => {
            if rights == 0 || rights > have as u64 || badge != 0 {
                return ERR_BAD_ARGUMENT; // none, more than it has, or a badge
            }
            Cap::Budget { pages: rights as u32 }
        }
        _ => return ERR_WRONG_CAPABILITY,
    };
    match put_current(copy, origin) {
        Ok(new) => new as i64,
        Err(e) => e,
    }
}

/// drop(slot): let go of the capability in the caller's `slot` (Entry 21).
/// What it held is released as if the process had ended: an endpoint loses
/// a reference, and with it maybe its last possible receiver, which wakes
/// whoever waits to send there with ERR_PEER_GONE. Init uses it when it
/// stops starting a driver again, so that nobody waits for that driver.
/// Since Entry 25: what was copied from it now counts as copied from its
/// parent, and a mapping made with it is undone.
pub fn drop_cap(slot: u64) -> i64 {
    let cap = without_interrupts(|| remove(crate::sched::current(), slot.min(MAX_CAPS as u64) as usize, true));
    match cap {
        Cap::None => crate::syscall::ERR_NO_CAPABILITY,
        _ => 0,
    }
}

// ===========================================================================
// Derivation and revocation (Entry 25)
// ===========================================================================
//
// A position is a slot in a task's table: task * MAX_CAPS + slot. For each
// one the kernel keeps, besides the capability (in sched's task table):
//   serials  a number no other capability has had (0: the slot is empty), so
//            a copy in flight can tell whether its original is still there
//   parents  the position of the capability it was copied from, plus one
//            (0: none). A parent always exists: when one is removed, its
//            children are handed to its own parent.
//   mappings where a Memory capability is mapped in its task (0: nowhere)
//   heaps    how many pages long a Budget capability's heap is (Entry 26);
//            where it is follows from the slot (mem.rs: heap_base)
// All of it under one lock (Entry 31; lock.rs). Each operation takes it
// once and hands it to the helpers below, and lets go of it before
// anything that takes it again (put, remove).

use crate::cpu::without_interrupts;
use crate::lock::{IrqGuard, IrqLock};
use crate::sched::TaskId;

const POSITIONS: usize = crate::sched::MAX_TASKS * MAX_CAPS;
const _: () = assert!(POSITIONS < u16::MAX as usize);

struct Derivation {
    serials: [u32; POSITIONS],
    parents: [u16; POSITIONS],
    mappings: [u64; POSITIONS],
    heaps: [u32; POSITIONS],
    /// The serial number given out last (0: none yet). Kept as the last
    /// rather than the next so that the whole table starts as zeros and
    /// stays out of the kernel image (.bss).
    last_serial: u32,
}

static DERIVATION: IrqLock<Derivation> = IrqLock::new(Derivation {
    serials: [0; POSITIONS],
    parents: [0; POSITIONS],
    mappings: [0; POSITIONS],
    heaps: [0; POSITIONS],
    last_serial: 0,
});

#[track_caller]
fn derivation() -> IrqGuard<'static, Derivation> {
    DERIVATION.lock()
}

/// Where a copy comes from: the original's position and serial number.
pub type Origin = Option<(usize, u32)>;

pub fn position(task: TaskId, slot: usize) -> usize {
    task * MAX_CAPS + slot
}

/// The capability in `task`'s `slot`, and the origin a copy of it records.
/// Interrupts must be off.
pub fn with_origin(task: TaskId, slot: usize) -> (Cap, Origin) {
    if slot >= MAX_CAPS {
        return (Cap::None, None);
    }
    let p = position(task, slot);
    let cap = crate::sched::task(task).caps[slot];
    let serial = derivation().serials[p];
    (cap, (cap != Cap::None).then_some((p, serial)))
}

/// with_origin for the current task, with interrupts off for it.
pub fn current_with_origin(slot: u64) -> (Cap, Origin) {
    without_interrupts(|| {
        if slot >= MAX_CAPS as u64 {
            return (Cap::None, None);
        }
        with_origin(crate::sched::current(), slot as usize)
    })
}

/// Put `cap` in `task`'s empty `slot`, copied from `from` (None: the
/// kernel made it). If `from` has been removed meanwhile, the copy goes
/// with it: nothing is stored, and the answer is false. Interrupts off.
pub fn put(task: TaskId, slot: usize, cap: Cap, from: Origin) -> bool {
    if cap == Cap::None || slot >= MAX_CAPS {
        return false;
    }
    let p = position(task, slot);
    crate::cpu::assert_interrupts_off(b"cap put");
    {
        let mut d = derivation();
        // Is the capability `from` names still where it was?
        let parent = match from {
            Some(origin) if d.serials[origin.0] == origin.1 => origin.0 as u16 + 1,
            Some(_) => return false,
            None => 0,
        };
        let serial = d.last_serial.wrapping_add(1).max(1);
        d.last_serial = serial;
        d.serials[p] = serial;
        d.parents[p] = parent;
        d.mappings[p] = 0;
        d.heaps[p] = 0;
    }
    crate::sched::task(task).caps[slot] = cap;
    retain(cap); // ipc's lock, and none of these
    true
}

/// put() in the current task's first free slot. Ok(slot); Err(NO_SLOT) if
/// every slot is taken, Err(NO_CAPABILITY) if `from` is gone.
pub fn put_current(cap: Cap, from: Origin) -> Result<usize, i64> {
    without_interrupts(|| {
        let me = crate::sched::current();
        let slot = free_slot(&crate::sched::task(me).caps).ok_or(crate::syscall::ERR_NO_SLOT)?;
        if put(me, slot, cap, from) {
            Ok(slot)
        } else {
            Err(crate::syscall::ERR_NO_CAPABILITY)
        }
    })
}

/// Take the capability out of `task`'s `slot`: what was copied from it is
/// handed to its parent, a mapping made with it is undone and a heap grown
/// with it freed (unless `unmap` is false: the whole address space is
/// going, and takes them with it), and its references are released.
/// Returns what was there. Interrupts off.
pub fn remove(task: TaskId, slot: usize, unmap: bool) -> Cap {
    if slot >= MAX_CAPS {
        return Cap::None;
    }
    let p = position(task, slot);
    let cap = core::mem::replace(&mut crate::sched::task(task).caps[slot], Cap::None);
    if cap == Cap::None {
        return Cap::None;
    }
    {
        let mut d = derivation();
        let up = d.parents[p];
        for q in d.parents.iter_mut() {
            if *q == p as u16 + 1 {
                *q = up;
            }
        }
    }
    if unmap {
        unmap_mapping(task, p, cap);
    }
    {
        let mut d = derivation();
        d.serials[p] = 0;
        d.parents[p] = 0;
        d.mappings[p] = 0;
        d.heaps[p] = 0;
    }
    release(cap);
    cap
}

/// Undo the mapping made with the Memory capability at position `p`, if
/// there is one. The pages were only lent: none is freed.
///
/// For a Budget capability (Entry 26): its heap's pages are unmapped and
/// freed. They were the heap's own; nobody else maps them. The task whose
/// heap it is isn't running (one CPU), or is the caller, in which case
/// unmap flushes the TLB entry; any other's are flushed when its address
/// space is next loaded (no PCIDs, no global user pages).
fn unmap_mapping(task: TaskId, p: usize, cap: Cap) {
    let root = crate::sched::space_of(task);
    if root == 0 {
        return;
    }
    // SAFETY: [paging] space_of gives the root of the task's own address
    // space, which lives until the task is reaped; it is still in the task
    // table, so it hasn't been.
    let mut space = unsafe { crate::vmm::AddressSpace::from_root(root) };
    match cap {
        Cap::Memory { pages, .. } => {
            let at = derivation().mappings[p];
            if at == 0 {
                return;
            }
            for i in 0..pages as u64 {
                // SAFETY: [paging] pages in the task's user half that
                // mem::map put there for this capability (MAPPINGS says
                // where). The kernel keeps no reference into them; if the
                // process touches them again it faults, which is the
                // capability's revocation taking effect.
                let _ = unsafe { space.unmap(at + i * crate::pmm::PAGE_SIZE) };
            }
        }
        Cap::Budget { .. } => {
            let base = crate::mem::heap_base((p % MAX_CAPS) as u64);
            let owner = space.owner();
            let pages = derivation().heaps[p] as u64;
            for i in 0..pages {
                let page = base + i * crate::pmm::PAGE_SIZE;
                // SAFETY: [paging] the heap's pages, which only the process
                // uses (the kernel keeps no reference into a heap), taken
                // back as ProcessHeap frames of the space's owner.
                if let Ok(frame) = unsafe { space.unmap_frame(page, crate::pmm::Kind::ProcessHeap.bit(), owner) } {
                    frame.free();
                }
            }
            derivation().heaps[p] = 0;
        }
        _ => {}
    }
}

// ===========================================================================
// Budgets (Entry 26): what a heap may grow to
// ===========================================================================
//
// A Budget's limit covers its own heap and the heaps of everything copied
// from it, so the derivation tree is the hierarchy of budgets: the shell's
// budget, minted from init's, holds every budget the shell lends, and a
// program can't grow past what its lender has left. Nothing is reserved:
// pages are taken from the frame allocator as a heap grows, and the
// budgets only say how far each may.

/// The Budget capability in the current task's `slot`, its serial number
/// and how long its heap is. Interrupts off.
pub fn heap_state(slot: u64) -> Result<(u32, u64), i64> {
    let me = crate::sched::current();
    if slot >= MAX_CAPS as u64 {
        return Err(crate::syscall::ERR_NO_CAPABILITY);
    }
    let p = position(me, slot as usize);
    let cap = crate::sched::task(me).caps[slot as usize];
    match cap {
        Cap::Budget { .. } => {
            let d = derivation();
            Ok((d.serials[p], d.heaps[p] as u64))
        }
        Cap::None => Err(crate::syscall::ERR_NO_CAPABILITY),
        _ => Err(crate::syscall::ERR_WRONG_CAPABILITY),
    }
}

/// Record that the heap of the Budget in the current task's `slot` is now
/// `pages` long. Interrupts off.
pub fn set_heap(slot: u64, pages: u64) {
    let p = position(crate::sched::current(), slot as usize);
    derivation().heaps[p] = pages as u32;
}

/// How many heap pages position `p` and everything copied from it have.
fn in_use(d: &Derivation, p: usize) -> u64 {
    (0..POSITIONS)
        .filter(|&q| d.heaps[q] != 0 && (q == p || derived_from(d, q, p)))
        .map(|q| d.heaps[q] as u64)
        .sum()
}

/// May the heap of the Budget in the current task's `slot` grow by `more`
/// pages? Only if that keeps it, and every Budget it was copied from,
/// within its limit. Interrupts off.
pub fn budget_allows(slot: u64, more: u64) -> bool {
    let mut p = position(crate::sched::current(), slot as usize);
    let d = derivation();
    // Up the tree: a parent always exists while its children do, and is
    // older, so this ends; the bound is only a guard.
    for _ in 0..POSITIONS {
        let cap = crate::sched::task(p / MAX_CAPS).caps[p % MAX_CAPS];
        if let Cap::Budget { pages: limit } = cap {
            if in_use(&d, p) + more > limit as u64 {
                return false;
            }
        }
        match d.parents[p] {
            0 => return true,
            up => p = up as usize - 1,
        }
    }
    false
}

/// For cap_info: a Budget's pages in use (its heap and everything copied
/// from it) and its own heap's.
pub fn budget_numbers(task: TaskId, slot: usize) -> (u64, u64) {
    let p = position(task, slot);
    let d = derivation();
    (in_use(&d, p), d.heaps[p] as u64)
}

/// True if position `q` was copied, directly or not, from position `p`.
fn derived_from(d: &Derivation, q: usize, p: usize) -> bool {
    let mut up = d.parents[q];
    // A parent always exists and is older than its children, so there is
    // no cycle; the bound is only a guard.
    for _ in 0..POSITIONS {
        if up == 0 {
            return false;
        }
        if up as usize - 1 == p {
            return true;
        }
        up = d.parents[up as usize - 1];
    }
    false
}

/// Remove every capability derived from `task`'s `slot`, wherever it is;
/// Ok(how many). Err if the slot is empty. Interrupts off.
pub fn revoke_in(task: TaskId, slot: usize) -> Result<usize, i64> {
    if slot >= MAX_CAPS || crate::sched::task(task).caps[slot] == Cap::None {
        return Err(crate::syscall::ERR_NO_CAPABILITY);
    }
    let p = position(task, slot);
    let mut doomed = [0u64; POSITIONS / 64];
    {
        let d = derivation();
        for q in 0..POSITIONS {
            if q != p && d.serials[q] != 0 && derived_from(&d, q, p) {
                doomed[q / 64] |= 1 << (q % 64);
            }
        }
    }
    let mut removed = 0;
    for q in 0..POSITIONS {
        if doomed[q / 64] & (1 << (q % 64)) != 0 {
            remove(q / MAX_CAPS, q % MAX_CAPS, true);
            removed += 1;
        }
    }
    Ok(removed)
}

/// How many capabilities are derived from `task`'s `slot`. Interrupts off.
pub fn derived_count(task: TaskId, slot: usize) -> usize {
    if slot >= MAX_CAPS || crate::sched::task(task).caps[slot] == Cap::None {
        return 0;
    }
    let p = position(task, slot);
    let d = derivation();
    let n = (0..POSITIONS).filter(|&q| q != p && d.serials[q] != 0 && derived_from(&d, q, p)).count();
    n
}

/// Where the current task's capability in `slot` is mapped (0: nowhere),
/// and its serial number, to record a new mapping against.
pub fn mapping(slot: usize) -> (u64, u32) {
    let p = position(crate::sched::current(), slot.min(MAX_CAPS - 1));
    let d = derivation();
    (d.mappings[p], d.serials[p])
}

/// Record that the current task mapped its capability in `slot` at `at`,
/// if it is still the one with `serial`; false if it was removed while it
/// was being mapped (the caller then undoes the mapping).
pub fn record_mapping(slot: usize, serial: u32, at: u64) -> bool {
    let p = position(crate::sched::current(), slot.min(MAX_CAPS - 1));
    let mut d = derivation();
    if serial == 0 || d.serials[p] != serial {
        return false;
    }
    d.mappings[p] = at;
    true
}

/// copy(slot) (system call 25): a copy of the capability in the caller's
/// `slot`, derived from it, in its first free slot. Any kind: what can't
/// be narrowed can still be lent, and taken back.
pub fn copy(slot: u64) -> i64 {
    let (cap, origin) = current_with_origin(slot);
    if cap == Cap::None {
        return crate::syscall::ERR_NO_CAPABILITY;
    }
    match put_current(cap, origin) {
        Ok(new) => new as i64,
        Err(e) => e,
    }
}

/// revoke(slot) (system call 26): see revoke_in. Returns how many went.
pub fn revoke(slot: u64) -> i64 {
    without_interrupts(|| match revoke_in(crate::sched::current(), slot.min(MAX_CAPS as u64) as usize) {
        Ok(n) => n as i64,
        Err(e) => e,
    })
}

// ===========================================================================
// Self-test (Entry 25): user/rust/src/bin/captest.rs does the asking
// ===========================================================================

const CAPTEST_REVOKE: u64 = 7;
const CAPTEST_REVOKE_ALL: u64 = 0x3F;
const CAPTEST_RECEIVE: u64 = 8;
const CAPTEST_GIVE: u64 = 9;
const CAPTEST_GIVE_LATE: u64 = 10;
const CAPTEST_RECEIVE_ONE: u64 = 11;
const WAIT_TICKS: u64 = 500;
/// What captest writes through its mapping (captest.rs: WRITTEN).
const WRITTEN: u64 = 0x1234_5678;

pub fn selftest() -> bool {
    use crate::process::{self, CAPTEST};
    use crate::sched::{self, ExitStatus};
    use crate::selftest::check;
    let code = |v: u64| Some(ExitStatus::Code(v));
    sched::reap();
    let baseline = sched::free_frames_and_heap();
    let endpoints = crate::ipc::in_use();

    // 1. Copies, revoke and drop, across processes, and memory: captest's
    //    REVOKE test, with a page of the kernel's to map.
    let page = crate::pmm::alloc(crate::pmm::Kind::Test, crate::pmm::Owner::Kernel);
    let (revoked, written) = match (crate::ipc::create(), page) {
        (Some(ep), Some(page)) => {
            let frame = page.phys();
            let caps = table(&[
                Cap::Console,
                Cap::Factory { rights: FACTORY_ALL },
                Cap::Endpoint { ep, badge: 0, rights: SEND | RECV },
                Cap::Memory { base: frame, pages: 1, flags: MEM_WRITE },
            ]);
            let tester = process::spawn(&CAPTEST, CAPTEST_REVOKE, caps);
            crate::ipc::release_created(ep);
            let status = tester.ok().and_then(|id| sched::join_timeout(id, WAIT_TICKS));
            sched::reap(); // its mappings gone with it, before the page is freed
            // SAFETY: [phys] the test's own frame (`page` still holds it),
            // through the physical map; captest, which had it mapped, has
            // been reaped.
            let word = unsafe { (crate::paging::phys_to_virt(frame) as *const u64).read_volatile() };
            page.free();
            (status == code(CAPTEST_REVOKE_ALL), word == WRITTEN)
        }
        (ep, frame) => {
            if let Some(ep) = ep {
                crate::ipc::release_created(ep);
            }
            if let Some(page) = frame {
                page.free();
            }
            (false, false)
        }
    };
    let mut ok = check(
        b"cap",
        b"copy, and revoke: both copies gone, the original kept; a dropped copy's copy revoked from above; across processes: relay's console and its ticker's in one revoke, and both end; memory: a revoked or dropped capability's mapping undone, one mapping per capability; minted copies derived too",
        revoked && written,
    );

    // 2. In a message: revoked after it arrived, the receiver's copy goes;
    //    revoked while the message waited, it never arrives.
    let pair = |ep: u8, giver: u64, receiver: u64, between: &dyn Fn(TaskId) -> bool| {
        let give = process::spawn(&CAPTEST, giver, table(&[Cap::Console, Cap::Endpoint { ep, badge: 0, rights: SEND | GRANT }]));
        let mut middle = true;
        if giver == CAPTEST_GIVE_LATE {
            // Wait (bounded) until it waits to send, then take its copy.
            let deadline = crate::apic::ticks() + 100;
            while give.as_ref().is_ok_and(|&id| !sched::blocked_in_ipc(id)) && crate::apic::ticks() < deadline {
                sched::sleep_ticks(1);
            }
            middle = give.as_ref().is_ok_and(|&id| between(id));
        }
        let take = process::spawn(&CAPTEST, receiver, table(&[Cap::None, Cap::Endpoint { ep, badge: 0, rights: RECV }]));
        crate::ipc::release_created(ep);
        let g = give.ok().and_then(|id| sched::join_timeout(id, WAIT_TICKS));
        let t = take.ok().and_then(|id| sched::join_timeout(id, WAIT_TICKS));
        (g, t, middle)
    };
    let after = crate::ipc::create().map(|ep| pair(ep, CAPTEST_GIVE, CAPTEST_RECEIVE, &|_| true));
    let late = crate::ipc::create().map(|ep| {
        pair(ep, CAPTEST_GIVE_LATE, CAPTEST_RECEIVE_ONE, &|id| {
            without_interrupts(|| revoke_in(id, 0)) == Ok(1) // its console's one copy, in the message
        })
    });
    let passed = after == Some((code(1), code(KIND_CONSOLE_CODE), true)) && late == Some((code(0), code(0x10), true));
    if !passed {
        // What each process ended with, for whoever reads the log.
        use crate::{serial_write_str as s, serial_write_u64 as dec};
        let show = |what: &[u8], status: Option<ExitStatus>| {
            s(what);
            match status {
                Some(ExitStatus::Code(c)) => {
                    s(b"exit code ");
                    dec(c);
                }
                Some(ExitStatus::Fault(v)) => {
                    s(b"fault ");
                    dec(v as u64);
                }
                Some(ExitStatus::Killed) => s(b"killed"),
                None => s(b"did not finish"),
            }
        };
        for (name, pair) in [(&b"[cap] after it arrived: giver "[..], after), (&b"[cap] while it waited: giver "[..], late)] {
            match pair {
                Some((g, t, middle)) => {
                    show(name, g);
                    show(b", receiver ", t);
                    s(if middle { b"\n" } else { b", and the revoke in the middle failed\n" });
                }
                None => {
                    s(name);
                    s(b"not started: no endpoint\n");
                }
            }
        }
    }
    ok &= check(
        b"cap",
        b"a capability passed in a message goes when its original is revoked; one revoked while its message waited is not delivered",
        passed,
    );

    // 3. Nothing left: relay's ticker, which nobody joins, ends by itself
    //    once its console is gone.
    let deadline = crate::apic::ticks() + WAIT_TICKS;
    while sched::live_tasks() > 0 && crate::apic::ticks() < deadline {
        sched::sleep_ticks(1);
        sched::reap();
    }
    sched::reap();
    let leaked = baseline as i64 - sched::free_frames_and_heap() as i64;
    ok &= check(
        b"cap",
        b"all reaped: no tasks (relay's ticker ended with its console), no endpoints and no frames left over",
        sched::live_tasks() == 0 && crate::ipc::in_use() == endpoints && leaked == 0,
    );
    ok
}

/// What RECEIVE exits with: the console's kind (info.rs: KIND_CONSOLE)
/// from the first message, nothing (0) << 8 after the second.
const KIND_CONSOLE_CODE: u64 = crate::info::KIND_CONSOLE as u64;
