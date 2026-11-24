//! mem.rs — memory capabilities: lending a process physical memory.
//!
//! A Memory capability (cap.rs) names a run of physical pages and how they
//! may be used: written or only read, and cached normally, write-combining
//! (a framebuffer) or uncached (device memory). Its holder maps the whole
//! run into its own address space with one system call:
//!
//!   13 map(memory, address) -> 0
//!
//! The pages are borrowed, not given: the process's address space doesn't
//! own them, so when the process ends they are unmapped and left alone
//! (vmm's BORROWED bit), never handed back to the frame allocator. That is
//! how the console driver reaches the framebuffer, and reads the system log
//! and the kernel's status rows (Entry 20), without the kernel deciding
//! what it draws.
//!
//! Where: [MAP_BASE, MAP_END), page-aligned, clear of everything else. The
//! window starts at 4 GiB, far above a program's code and data (1 GiB up)
//! and below the heaps (Entry 26) and its stack's region, so a mapping
//! can't land on any of them.
//!
//! One mapping per capability (Entry 25): the kernel remembers where each
//! is mapped, and when the capability goes (revoked, dropped), so does the
//! mapping. To map the same pages twice, copy the capability first.
//!
//! Heaps (Entry 26): memory of a process's own, from a Budget capability.
//!
//!   28 heap(budget, pages) -> the heap's address
//!
//! makes the heap that the Budget in that slot provides `pages` pages long:
//! growing maps fresh pages (zeroed, the process's own, freed when it ends),
//! as far as the budget and every budget it was copied from allow;
//! shrinking unmaps and frees from the top, and 0 gives the heap up. Each
//! slot's heap has a fixed place, HEAP_BASE + slot * HEAP_SPAN; asking for
//! the length it has already (cap_info tells it) answers where without
//! changing anything. When the Budget is revoked or dropped, its heap goes
//! with it: the pages are freed, and touching them is a page fault.

use crate::cap::{Cap, MAX_CAPS, MEM_UNCACHED, MEM_WRITE, MEM_WRITE_COMBINING};
use crate::cpu::without_interrupts;
use crate::pmm::{self, Kind, PAGE_SIZE};
use crate::sched;
use crate::syscall::{ERR_BAD_ADDRESS, ERR_BAD_ARGUMENT, ERR_NO_CAPABILITY, ERR_NO_MEMORY, ERR_OVER_BUDGET};
use crate::vmm::{AddressSpace, MapError, PageFlags};

pub const MAP_BASE: u64 = 0x0000_0001_0000_0000;
/// Lowered from 0x7F00_0000_0000 in Entry 26, to leave room for heaps.
pub const MAP_END: u64 = 0x0000_4000_0000_0000;
/// Heaps (Entry 26): slot n's at HEAP_BASE + n * HEAP_SPAN, up to 1 TiB.
pub const HEAP_BASE: u64 = MAP_END;
pub const HEAP_SPAN: u64 = 0x0000_0100_0000_0000;
const _: () = assert!(HEAP_BASE + MAX_CAPS as u64 * HEAP_SPAN <= crate::process::USER_STACK_TOP - 0x1_0000_0000);
/// Pages mapped or freed with interrupts off at a time: a revoke can come
/// in between, and is seen at the next batch.
const HEAP_BATCH: u64 = 64;

pub fn heap_base(slot: u64) -> u64 {
    HEAP_BASE + slot * HEAP_SPAN
}

/// map(slot, address): map the Memory capability in `slot` at `address`.
pub fn map(slot: u64, address: u64) -> i64 {
    let Cap::Memory { base, pages, flags } = sched::current_cap(slot) else {
        return ERR_NO_CAPABILITY;
    };
    let bytes = pages as u64 * PAGE_SIZE;
    let fits = address % PAGE_SIZE == 0
        && address >= MAP_BASE
        && address.checked_add(bytes).is_some_and(|end| end <= MAP_END);
    if !fits || pages == 0 {
        return ERR_BAD_ADDRESS;
    }
    let (mapped_at, serial) = crate::cap::mapping(slot as usize);
    if mapped_at != 0 {
        return ERR_BAD_ARGUMENT; // this capability is mapped already
    }
    let page_flags = PageFlags::user_borrowed(
        flags & MEM_WRITE != 0,
        flags & MEM_WRITE_COMBINING != 0,
        flags & MEM_UNCACHED != 0,
    );
    // SAFETY: [paging] the running task's own address space, live while it
    // runs.
    let mut space = unsafe { AddressSpace::from_root(sched::current_space()) };
    for i in 0..pages as u64 {
        // SAFETY: [paging] the memory the capability names, which only the
        // kernel creates, for memory it means to lend (the screen, the log,
        // the console's description, a self-test's frame); mapped in the
        // caller's user half (checked: [MAP_BASE, MAP_END)), as borrowed
        // pages, which tearing the space down never frees.
        let result = unsafe { space.map(address + i * PAGE_SIZE, base + i * PAGE_SIZE, page_flags) };
        if let Err(e) = result {
            // Undo what was mapped; the frames were only borrowed, so
            // nothing is freed.
            for j in 0..i {
                // SAFETY: [paging] pages this call just mapped; the process
                // hasn't been told they exist, and the kernel never used them.
                let _ = unsafe { space.unmap(address + j * PAGE_SIZE) };
            }
            return if e == MapError::OutOfMemory { ERR_NO_MEMORY } else { ERR_BAD_ADDRESS };
        }
    }
    // Remembered against this capability, so that removing it unmaps it
    // (Entry 25). If it was revoked while the pages were being mapped, the
    // mapping goes now.
    if !crate::cap::record_mapping(slot as usize, serial, address) {
        for i in 0..pages as u64 {
            // SAFETY: [paging] as above: only just mapped, and not reported.
            let _ = unsafe { space.unmap(address + i * PAGE_SIZE) };
        }
        return ERR_NO_CAPABILITY;
    }
    crate::console::on_map(base); // the screen: this process draws now (Entry 21)
    0
}

/// heap(slot, pages): make the heap of the Budget capability in `slot`
/// `pages` long, and return where it starts. If it can't be done (over
/// budget, out of memory, the capability gone meanwhile), the heap is put
/// back as it was, if the capability is still there, and the error
/// returned.
pub fn heap(slot: u64, pages: u64) -> i64 {
    if slot >= MAX_CAPS as u64 {
        return ERR_NO_CAPABILITY;
    }
    if pages > HEAP_SPAN / PAGE_SIZE {
        return ERR_BAD_ARGUMENT;
    }
    let (serial, before) = match without_interrupts(|| crate::cap::heap_state(slot)) {
        Ok(state) => state,
        Err(e) => return e,
    };
    // Refused before anything is mapped if it can't fit now; each batch is
    // checked again, as the budgets above may be used meanwhile.
    if pages > before && !without_interrupts(|| crate::cap::budget_allows(slot, pages - before)) {
        return ERR_OVER_BUDGET;
    }
    if let Err(e) = resize(slot, serial, pages) {
        let _ = resize(slot, serial, before);
        return e;
    }
    heap_base(slot) as i64
}

/// Grow or shrink, HEAP_BATCH pages at a time, until the heap is `target`
/// pages long. Each batch checks the capability is still the one it was.
fn resize(slot: u64, serial: u32, target: u64) -> Result<(), i64> {
    let base = heap_base(slot);
    loop {
        let done = without_interrupts(|| -> Result<bool, i64> {
            let (now, have) = crate::cap::heap_state(slot)?;
            if now != serial {
                return Err(ERR_NO_CAPABILITY); // revoked, and the slot reused
            }
            // SAFETY: [paging] the running task's own address space.
            let mut space = unsafe { AddressSpace::from_root(sched::current_space()) };
            if have < target {
                let n = (target - have).min(HEAP_BATCH);
                if !crate::cap::budget_allows(slot, n) {
                    return Err(ERR_OVER_BUDGET);
                }
                // Each page the process's own (Entry 29), as its space is.
                let owner = space.owner();
                for i in have..have + n {
                    let frame = pmm::alloc(Kind::ProcessHeap, owner).ok_or(ERR_NO_MEMORY)?; // zeroed
                    // SAFETY: [paging] a frame just allocated for this
                    // process, zeroed, at the next page of its heap, which
                    // nothing maps yet (the heap is `have` pages long).
                    if let Err((_, frame)) = unsafe { space.map_frame(base + i * PAGE_SIZE, frame, PageFlags::USER_DATA) } {
                        frame.free();
                        return Err(ERR_NO_MEMORY); // no frame for a page table
                    }
                    crate::cap::set_heap(slot, i + 1);
                }
            } else if have > target {
                let n = (have - target).min(HEAP_BATCH);
                let owner = space.owner();
                for i in (have - n..have).rev() {
                    // SAFETY: [paging] the heap's last page, which only the
                    // process uses; shrinking was its own request.
                    if let Ok(frame) = unsafe { space.unmap_frame(base + i * PAGE_SIZE, Kind::ProcessHeap.bit(), owner) } {
                        frame.free();
                    }
                    crate::cap::set_heap(slot, i);
                }
            }
            Ok(have == target)
        })?;
        if done {
            return Ok(());
        }
    }
}

// ===========================================================================
// Self-test (Entry 26): user/rust/src/bin/captest.rs does the asking
// ===========================================================================

const CAPTEST_HEAP: u64 = 12;
const CAPTEST_HEAP_ALL: u64 = 0x1F;
/// The Budget captest is given: 1 MiB.
const CAPTEST_BUDGET: u32 = 256;
const WAIT_TICKS: u64 = 1000;

pub fn selftest() -> bool {
    use crate::cap::{table, FACTORY_ALL, RECV, SEND};
    use crate::process::{self, CAPTEST};
    use crate::sched::ExitStatus;
    use crate::selftest::check;
    sched::reap();
    let baseline = sched::free_frames_and_heap();
    let endpoints = crate::ipc::in_use();

    let status = crate::ipc::create().and_then(|ep| {
        let caps = table(&[
            Cap::Console,
            Cap::Factory { rights: FACTORY_ALL },
            Cap::Endpoint { ep, badge: 0, rights: SEND | RECV },
            Cap::Budget { pages: CAPTEST_BUDGET },
        ]);
        let tester = process::spawn(&CAPTEST, CAPTEST_HEAP, caps);
        crate::ipc::release_created(ep);
        tester.ok().and_then(|id| sched::join_timeout(id, WAIT_TICKS))
    });
    let mut ok = check(
        b"budget",
        b"a heap grows from a Budget at its fixed place, shrinks, and grows back zeroed; over budget refused, and a minted Budget counted in its original's; revoked or dropped, its pages freed; Box, Vec, String and BTreeMap on it, 2 MiB more refused, all freed back to one block; hog's heap revoked: its next read a page fault",
        status == Some(ExitStatus::Code(CAPTEST_HEAP_ALL)),
    );

    // Nothing left: every heap page went back, by revoke, drop, shrinking
    // or the end of its process.
    let deadline = crate::apic::ticks() + WAIT_TICKS;
    while sched::live_tasks() > 0 && crate::apic::ticks() < deadline {
        sched::sleep_ticks(1);
        sched::reap();
    }
    sched::reap();
    let leaked = baseline as i64 - sched::free_frames_and_heap() as i64;
    ok &= check(
        b"budget",
        b"all reaped: no tasks, no endpoints, and every heap page back with the frame allocator",
        sched::live_tasks() == 0 && crate::ipc::in_use() == endpoints && leaked == 0,
    );
    ok
}
