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
//! and below its stack's region, so a mapping can't land on either.

use crate::cap::{Cap, MEM_UNCACHED, MEM_WRITE, MEM_WRITE_COMBINING};
use crate::pmm::PAGE_SIZE;
use crate::sched;
use crate::syscall::{ERR_BAD_ADDRESS, ERR_NO_CAPABILITY, ERR_NO_MEMORY};
use crate::vmm::{AddressSpace, MapError, PageFlags};

pub const MAP_BASE: u64 = 0x0000_0001_0000_0000;
pub const MAP_END: u64 = 0x0000_7F00_0000_0000;

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
    let page_flags = PageFlags::user_borrowed(
        flags & MEM_WRITE != 0,
        flags & MEM_WRITE_COMBINING != 0,
        flags & MEM_UNCACHED != 0,
    );
    let mut space = unsafe { AddressSpace::from_root(sched::current_space()) };
    for i in 0..pages as u64 {
        let result = unsafe { space.map(address + i * PAGE_SIZE, base + i * PAGE_SIZE, page_flags) };
        if let Err(e) = result {
            // Undo what was mapped; the frames were only borrowed, so
            // nothing is freed.
            for j in 0..i {
                let _ = unsafe { space.unmap(address + j * PAGE_SIZE) };
            }
            return if e == MapError::OutOfMemory { ERR_NO_MEMORY } else { ERR_BAD_ADDRESS };
        }
    }
    crate::console::on_map(base); // the screen: this process draws now (Entry 21)
    0
}
