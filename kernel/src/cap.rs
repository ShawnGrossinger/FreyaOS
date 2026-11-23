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
//!     system with it.
//!
//! Capabilities are values; copying one into another table (spawn, or a
//! transfer in a message) copies the authority. Endpoints are reference
//! counted by the capabilities that name them (ipc::retain / release).
//!
//! An endpoint capability can be copied with less authority (system call
//! mint, Entry 21): fewer rights, and a badge if it had none yet. That is
//! how init gives the console driver only RECV on its endpoint and the
//! keyboard driver only SEND, with a badge that names it. And a process
//! can let go of a capability (system call drop).

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
    /// May create processes and endpoints (factory.rs).
    Factory,
}

/// Memory capability flags.
pub const MEM_WRITE: u8 = 1 << 0;
/// Write-combining: for a framebuffer.
pub const MEM_WRITE_COMBINING: u8 = 1 << 1;
/// Uncached: for device memory that must see every access (VGA text).
pub const MEM_UNCACHED: u8 = 1 << 2;

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
pub fn mint(slot: u64, rights: u64, badge: u64) -> i64 {
    use crate::syscall::{ERR_BAD_ARGUMENT, ERR_NO_SLOT, ERR_WRONG_CAPABILITY};
    let Cap::Endpoint { ep, badge: old, rights: have } = crate::sched::current_cap(slot) else {
        return ERR_WRONG_CAPABILITY;
    };
    let all = (SEND | RECV | GRANT) as u64;
    if rights == 0 || rights & !all != 0 || rights & !(have as u64) != 0 {
        return ERR_BAD_ARGUMENT; // none, unknown, or more than it has
    }
    if badge == crate::ipc::SIGNAL_BADGE || (old != 0 && badge != old) {
        return ERR_BAD_ARGUMENT;
    }
    let copy = Cap::Endpoint { ep, badge, rights: rights as u8 };
    match crate::sched::store_cap(copy) {
        Some(slot) => slot as i64,
        None => ERR_NO_SLOT,
    }
}

/// drop(slot): let go of the capability in the caller's `slot` (Entry 21).
/// What it held is released as if the process had ended: an endpoint loses
/// a reference, and with it maybe its last possible receiver, which wakes
/// whoever waits to send there with ERR_PEER_GONE. Init uses it when it
/// stops starting a driver again, so that nobody waits for that driver.
pub fn drop_cap(slot: u64) -> i64 {
    match crate::sched::take_cap(slot) {
        Cap::None => crate::syscall::ERR_NO_CAPABILITY,
        cap => {
            release(cap);
            0
        }
    }
}

pub fn retain_all(caps: &Caps) {
    caps.iter().for_each(|&c| retain(c));
}

pub fn release_all(caps: &Caps) {
    caps.iter().for_each(|&c| release(c));
}
