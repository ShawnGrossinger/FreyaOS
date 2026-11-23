//! info.rs — what a process may learn about the system (Entry 23).
//!
//! Two system calls, for the shell's ps, uptime, mem and caps:
//!
//!   info(info, ptr, len): with an Info capability, a snapshot written to
//!   the caller's buffer: a 64-byte header, then a 48-byte record for each
//!   task, as many as fit (user/rust/src/lib.rs: SystemInfo, TaskInfo):
//!
//!     header   0  "FREYAINF"         record   0  id
//!              8  version (1)                 4  what it is doing (TASK_*)
//!             12  records that follow         8  the endpoint, task, line or
//!             16  timer ticks so far                tick it waits for
//!             24  ticks a second             16  name (16 bytes, 0-padded)
//!             32  free frames (4 KiB)        32  times switched in
//!             40  usable frames              40  capabilities held
//!             48  task switches              44  flags: 1 a process (its
//!             56  system calls                      own address space), 2 idle
//!
//!   Each record is taken with interrupts off, so it is consistent in
//!   itself; the whole is not one instant (tasks move on between records),
//!   which is all ps needs. Returns the bytes written.
//!
//!   cap_info(slot): what the caller's own slot holds: its kind, and up to
//!   three numbers (an endpoint's number, rights and badge; a port range's
//!   base and count; a line; a memory capability's pages and flags, never
//!   a physical address; a Factory's rights and a process's slot and
//!   generation, Entry 24). It needs no capability: it shows a process only
//!   what it already holds, and nothing outside it.
//!
//! An Info capability lets its holder see every task's name and state, so
//! the kernel hands it to init alone, and init passes it to the shell.

use crate::cap::Cap;
use crate::sched;
use crate::syscall::{copy_to_user, ERR_BAD_ARGUMENT, ERR_WRONG_CAPABILITY};

pub const HEADER_BYTES: usize = 64;
pub const RECORD_BYTES: usize = 48;
const VERSION: u32 = 1;

pub const FLAG_PROCESS: u32 = 1 << 0;
pub const FLAG_IDLE: u32 = 1 << 1;

// cap_info's kinds.
pub const KIND_NONE: i64 = 0;
pub const KIND_CONSOLE: i64 = 1;
pub const KIND_ENDPOINT: i64 = 2;
pub const KIND_IO_PORTS: i64 = 3;
pub const KIND_IRQ: i64 = 4;
pub const KIND_POWER: i64 = 5;
pub const KIND_MEMORY: i64 = 6;
pub const KIND_FACTORY: i64 = 7;
pub const KIND_INFO: i64 = 8;
pub const KIND_PROCESS: i64 = 9;

/// info(slot, ptr, len): the snapshot, if `slot` holds Info and `len` has
/// room for the header at least.
pub fn info(slot: u64, ptr: u64, len: u64) -> i64 {
    if sched::current_cap(slot) != Cap::Info {
        return ERR_WRONG_CAPABILITY;
    }
    if len < HEADER_BYTES as u64 {
        return ERR_BAD_ARGUMENT;
    }
    let room = ((len as usize - HEADER_BYTES) / RECORD_BYTES).min(sched::MAX_TASKS);
    let mut written = 0usize;
    for id in 0..sched::MAX_TASKS {
        if written == room {
            break;
        }
        let Some(t) = sched::summary(id) else { continue };
        let mut r = [0u8; RECORD_BYTES];
        r[0..4].copy_from_slice(&(id as u32).to_le_bytes());
        r[4..8].copy_from_slice(&t.what.to_le_bytes());
        r[8..16].copy_from_slice(&t.detail.to_le_bytes());
        let name = t.name.as_bytes();
        let n = name.len().min(16);
        r[16..16 + n].copy_from_slice(&name[..n]);
        r[32..40].copy_from_slice(&t.slices.to_le_bytes());
        r[40..44].copy_from_slice(&t.caps.to_le_bytes());
        let flags = if t.process { FLAG_PROCESS } else { 0 } | if t.idle { FLAG_IDLE } else { 0 };
        r[44..48].copy_from_slice(&flags.to_le_bytes());
        if let Err(e) = copy_to_user(ptr + (HEADER_BYTES + written * RECORD_BYTES) as u64, &r) {
            return e;
        }
        written += 1;
    }
    let (syscalls, _) = crate::syscall::counts();
    let mut h = [0u8; HEADER_BYTES];
    h[0..8].copy_from_slice(b"FREYAINF");
    h[8..12].copy_from_slice(&VERSION.to_le_bytes());
    h[12..16].copy_from_slice(&(written as u32).to_le_bytes());
    h[16..24].copy_from_slice(&crate::apic::ticks().to_le_bytes());
    h[24..32].copy_from_slice(&crate::apic::TIMER_HZ.to_le_bytes());
    h[32..40].copy_from_slice(&crate::pmm::free_frames().to_le_bytes());
    h[40..48].copy_from_slice(&crate::pmm::usable_frames().to_le_bytes());
    h[48..56].copy_from_slice(&sched::switches().to_le_bytes());
    h[56..64].copy_from_slice(&syscalls.to_le_bytes());
    if let Err(e) = copy_to_user(ptr, &h) {
        return e;
    }
    (HEADER_BYTES + written * RECORD_BYTES) as i64
}

/// cap_info(slot): (kind, a, b, c) for the caller's slot, or an error for
/// a slot past the last.
pub fn cap_info(slot: u64) -> (i64, u64, u64, u64) {
    if slot >= crate::cap::MAX_CAPS as u64 {
        return (ERR_BAD_ARGUMENT, 0, 0, 0);
    }
    match sched::current_cap(slot) {
        Cap::None => (KIND_NONE, 0, 0, 0),
        Cap::Console => (KIND_CONSOLE, 0, 0, 0),
        Cap::Endpoint { ep, badge, rights } => (KIND_ENDPOINT, ep as u64, rights as u64, badge),
        Cap::IoPorts { base, count } => (KIND_IO_PORTS, base as u64, count as u64, 0),
        Cap::Irq { line } => (KIND_IRQ, line as u64, 0, 0),
        Cap::Power => (KIND_POWER, 0, 0, 0),
        Cap::Memory { pages, flags, .. } => (KIND_MEMORY, pages as u64, flags as u64, 0),
        Cap::Factory { rights } => (KIND_FACTORY, rights as u64, 0, 0),
        Cap::Info => (KIND_INFO, 0, 0, 0),
        Cap::Process { id, generation } => (KIND_PROCESS, id as u64, generation as u64, 0),
    }
}

// ===========================================================================
// Self-test
// ===========================================================================

/// captest's info test (user/rust/src/bin/captest.rs, INFO) and its exit
/// code when every check passes.
const CAPTEST_INFO: u64 = 5;
const CAPTEST_INFO_ALL: u64 = 0x1F;
/// The endpoint's badge in captest's slot 2 (cap_info must report it).
pub const INFO_TEST_BADGE: u64 = 0x2A;

pub fn selftest() -> bool {
    use crate::cap::{table, RECV, SEND};
    let Some(ep) = crate::ipc::create() else {
        return crate::selftest::check(b"info", b"an endpoint for the info test", false);
    };
    let started = crate::process::spawn(
        &crate::process::CAPTEST,
        CAPTEST_INFO,
        table(&[Cap::Console, Cap::Info, Cap::Endpoint { ep, badge: INFO_TEST_BADGE, rights: SEND | RECV }]),
    );
    crate::ipc::release_created(ep);
    let status = started.ok().and_then(|id| sched::join_timeout(id, 500));
    crate::selftest::check(
        b"info",
        b"info: the header, and captest's own record (running, a process, 3 capabilities); header only when that is all the room; refused: no Info, too small, a kernel or read-only buffer; cap_info: console, Info, the endpoint's number, rights and badge, empty, past the last",
        status == Some(sched::ExitStatus::Code(CAPTEST_INFO_ALL)),
    )
}
