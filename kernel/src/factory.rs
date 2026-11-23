//! factory.rs — making processes and endpoints from user space (Entry 21).
//!
//! Until Entry 21 the kernel started every process itself and decided what
//! each one held. Now it starts one, init, and hands it the capabilities it
//! used to hand out, plus the Factory capability, which lets init make the
//! rest of the system:
//!
//!   spawn(factory, program, arg, slots, exit_ep)
//!       Start one of the built-in programs (PROGRAMS: by number) with
//!       argument `arg`. `slots` points at MAX_CAPS bytes in the caller's
//!       memory: byte i names the caller's slot to copy into the new
//!       process's slot i, or 0xFF for none. Every slot named must hold a
//!       capability (a mistake is refused, not passed on as an empty slot).
//!       `exit_ep` is an endpoint capability with SEND, or -1: when the new
//!       process ends, that endpoint is signalled with bit (1 << its id).
//!       Returns the new process's id. With `flags` bit 0 (Entry 24), a
//!       Process capability for it lands in the caller's first free slot
//!       (its slot comes back in R9): the caller can then ask how it is
//!       doing (status) and stop it (kill).
//!
//!   endpoint(factory)
//!       A new endpoint, as a capability with SEND, RECV and GRANT and no
//!       badge, in the caller's first free slot. Returns the slot. mint
//!       (cap.rs) makes narrower copies to hand out.
//!
//!   kill(process), status(process)   (Entry 24)
//!       Stop the process a Process capability names, or ask how it is
//!       doing: running, or how it ended (an exit code, a CPU exception, or
//!       killed). The capability names a task slot and which of its tasks,
//!       so once the slot holds another process it is stale: both answer
//!       peer-gone, and nothing is stopped.
//!
//! A Factory's rights (Entry 24) say which programs it may start (bit n:
//! program n) and whether it may make endpoints; mint makes copies with
//! fewer. Init holds one with every right and gives the shell one that
//! starts only hello, spin and crash.
//!
//! Copying a capability into a process the caller creates needs nothing
//! more than holding it: the caller could use it itself. Giving one to a
//! process that already runs still needs GRANT on an endpoint (ipc.rs).

use crate::cap::{self, Cap, Caps, GRANT, MAX_CAPS, RECV, SEND};
use crate::process::{self, Program};
use crate::sched;
use crate::syscall::{copy_from_user, ERR_BAD_ARGUMENT, ERR_NO_CAPABILITY, ERR_NO_MEMORY, ERR_NO_SLOT, ERR_WRONG_CAPABILITY};

/// The programs spawn can start, by number (user/freya.inc and
/// user/rust/src/lib.rs: PROGRAM_*). The kernel's test programs aren't
/// among them: only its own self-tests start those.
pub static PROGRAMS: [&Program; 10] = [
    &process::HELLO,
    &process::CONSOLE,
    &process::PS2,
    &process::MOUSE,
    &process::SHELL,
    &process::SPIN,
    &process::CRASH,
    &process::TICKER,
    &process::RELAY,
    &process::HOG,
];
const _: () = assert!(PROGRAMS.len() <= 16, "a Factory's rights have 16 program bits");

/// spawn's flags: a Process capability for the new process, please.
pub const SPAWN_PROCESS_CAP: u64 = 1 << 0;

/// "No capability here" in spawn's slot list.
pub const NO_SLOT_BYTE: u8 = 0xFF;

/// The rights of the Factory in `slot`, if it holds one.
fn factory_rights(slot: u64) -> Option<u32> {
    match sched::current_cap(slot) {
        Cap::Factory { rights } => Some(rights),
        _ => None,
    }
}

/// spawn; returns the new process's id, and the slot its Process
/// capability landed in (NO_SLOT if none was asked for).
pub fn spawn(factory: u64, program: u64, arg: u64, slots: u64, exit_ep: u64, flags: u64) -> (i64, u64) {
    let no_cap = crate::ipc::NO_SLOT;
    let Some(rights) = factory_rights(factory) else {
        return (ERR_NO_CAPABILITY, no_cap);
    };
    let Some(&image) = PROGRAMS.get(program as usize) else {
        return (ERR_BAD_ARGUMENT, no_cap);
    };
    if rights & (1 << program) == 0 {
        return (ERR_WRONG_CAPABILITY, no_cap); // a Factory that doesn't cover it
    }
    if flags & !SPAWN_PROCESS_CAP != 0 {
        return (ERR_BAD_ARGUMENT, no_cap);
    }
    let want_cap = flags & SPAWN_PROCESS_CAP != 0;
    // Room for the Process capability first: once the process runs, there
    // is no taking it back.
    if want_cap && sched::free_cap_slots() == 0 {
        return (ERR_NO_SLOT, no_cap);
    }
    match start(image, arg, slots, exit_ep) {
        Ok((id, generation)) if want_cap => match sched::store_cap(Cap::Process { id: id as u8, generation }) {
            Some(slot) => (id as i64, slot as u64),
            None => (id as i64, no_cap), // can't happen: the caller's table only changes in its own calls
        },
        Ok((id, _)) => (id as i64, no_cap),
        Err(e) => (e, no_cap),
    }
}

fn start(program: &Program, arg: u64, slots: u64, exit_ep: u64) -> Result<(sched::TaskId, u32), i64> {
    let mut list = [0u8; MAX_CAPS];
    copy_from_user(slots, &mut list)?;
    // Each copy remembers its original (Entry 25), so the caller can take
    // it back later with revoke; one revoked before the process is made
    // isn't given to it.
    let mut caps: Caps = cap::NO_CAPS;
    let mut origins: [cap::Origin; MAX_CAPS] = [None; MAX_CAPS];
    for (i, &from) in list.iter().enumerate() {
        if from == NO_SLOT_BYTE {
            continue;
        }
        match cap::current_with_origin(from as u64) {
            (Cap::None, _) => return Err(ERR_BAD_ARGUMENT), // an empty slot, or past the last
            (c, origin) => (caps[i], origins[i]) = (c, origin),
        }
    }
    let exit_signal = match exit_ep {
        crate::ipc::NO_SLOT => None,
        slot => match sched::current_cap(slot) {
            Cap::Endpoint { ep, rights, .. } if rights & SEND != 0 => Some(ep),
            _ => return Err(ERR_WRONG_CAPABILITY),
        },
    };
    process::spawn_signalling(program, arg, caps, &origins, exit_signal).map_err(|_| ERR_NO_MEMORY)
}

pub fn endpoint(factory: u64) -> i64 {
    let Some(rights) = factory_rights(factory) else {
        return ERR_NO_CAPABILITY;
    };
    if rights & cap::FACTORY_ENDPOINTS == 0 {
        return ERR_WRONG_CAPABILITY;
    }
    let Some(ep) = crate::ipc::create() else {
        return ERR_NO_MEMORY;
    };
    let stored = sched::store_cap(Cap::Endpoint { ep, badge: 0, rights: SEND | RECV | GRANT });
    // The capability (if it found a slot) now holds the endpoint; with no
    // slot, letting go frees it again.
    crate::ipc::release_created(ep);
    match stored {
        Some(slot) => slot as i64,
        None => ERR_NO_SLOT,
    }
}

/// What status returns (Entry 24): the process runs, or how it ended (its
/// exit code, or the CPU exception's vector, goes in the second value).
pub const STATUS_RUNNING: i64 = 0;
pub const STATUS_EXITED: i64 = 1;
pub const STATUS_FAULT: i64 = 2;
pub const STATUS_KILLED: i64 = 3;

/// The Process capability in `slot`, if that is what it holds.
fn process_cap(slot: u64) -> Result<(usize, u32), i64> {
    match sched::current_cap(slot) {
        Cap::Process { id, generation } => Ok((id as usize, generation)),
        _ => Err(ERR_WRONG_CAPABILITY),
    }
}

/// kill(process): stop it. Peer-gone if it has ended (or its slot holds
/// another process since).
pub fn kill(slot: u64) -> i64 {
    match process_cap(slot) {
        Ok((id, generation)) if sched::kill_numbered(id, generation) => 0,
        Ok(_) => crate::syscall::ERR_PEER_GONE,
        Err(e) => e,
    }
}

/// status(process): (STATUS_*, the exit code or vector). Peer-gone once
/// its slot holds another process: how it ended isn't kept any longer.
pub fn status(slot: u64) -> (i64, u64) {
    let (id, generation) = match process_cap(slot) {
        Ok(p) => p,
        Err(e) => return (e, 0),
    };
    match sched::process_state(id, generation) {
        None => (crate::syscall::ERR_PEER_GONE, 0),
        Some(None) => (STATUS_RUNNING, 0),
        Some(Some(sched::ExitStatus::Code(code))) => (STATUS_EXITED, code),
        Some(Some(sched::ExitStatus::Fault(vector))) => (STATUS_FAULT, vector as u64),
        Some(Some(sched::ExitStatus::Killed)) => (STATUS_KILLED, 0),
    }
}

// ===========================================================================
// Self-test (Entry 21): user/rust/src/bin/captest.rs does the asking
// ===========================================================================

use crate::selftest::check;

/// captest's tests (its argument) and the exit code when every check
/// passes: one bit per check.
const CAPTEST_SPAWN: u64 = 1;
const CAPTEST_SPAWN_ALL: u64 = 0x3F;
const CAPTEST_MINT: u64 = 2;
const CAPTEST_MINT_ALL: u64 = 0xFF;
const CAPTEST_PROCESS: u64 = 6;
const CAPTEST_PROCESS_ALL: u64 = 0xFF;
/// user/ipc.asm's collector: receive `value` messages, exit with their
/// badges.
const IPC_COLLECTOR: u64 = 11;
const WAIT_TICKS: u64 = 500;

fn code(value: u64) -> Option<sched::ExitStatus> {
    Some(sched::ExitStatus::Code(value))
}

/// Runs on the boot thread with the scheduler going.
pub fn selftest() -> bool {
    use crate::ipc;
    sched::reap();
    let baseline = sched::free_frames_and_heap();
    let endpoints = ipc::in_use();

    // 1. spawn, and the signal when what it started ends.
    let spawn_ok = match ipc::create() {
        Some(own) => {
            let caps = cap::table(&[Cap::Console, Cap::Factory { rights: cap::FACTORY_ALL }, Cap::Endpoint { ep: own, badge: 0, rights: SEND | RECV }]);
            let started = process::spawn(&process::CAPTEST, CAPTEST_SPAWN, caps);
            ipc::release_created(own);
            started.ok().and_then(|id| sched::join_timeout(id, WAIT_TICKS)) == code(CAPTEST_SPAWN_ALL)
        }
        None => false,
    };
    let mut ok = check(
        b"factory",
        b"spawn starts a program with the slots asked for and signals when it ends; refused: no Factory, unknown program, empty or missing slot, kernel address, exit signal to a non-endpoint",
        spawn_ok,
    );

    // 2. endpoint and mint; the collector reports the badge it was sent.
    let (minted, badge) = match ipc::create() {
        Some(ep) => {
            let collector = process::spawn(&process::IPC, IPC_COLLECTOR | 1 << 8, cap::table(&[Cap::None, Cap::Endpoint { ep, badge: 0, rights: RECV }]));
            let tester = process::spawn(
                &process::CAPTEST,
                CAPTEST_MINT,
                cap::table(&[Cap::Console, Cap::Factory { rights: cap::FACTORY_ALL }, Cap::Endpoint { ep, badge: 0, rights: SEND | RECV | GRANT }]),
            );
            ipc::release_created(ep);
            let t = tester.ok().and_then(|id| sched::join_timeout(id, WAIT_TICKS));
            let c = collector.ok().and_then(|id| sched::join_timeout(id, WAIT_TICKS));
            (t == code(CAPTEST_MINT_ALL), c == code(7))
        }
        None => (false, false),
    };
    ok &= check(
        b"factory",
        b"endpoint makes one; mint narrows rights and sets a badge once (7 arrived); refused: more rights, a new badge, the signal badge, no rights; both stop when slots run out; drop frees one",
        minted && badge,
    );

    // 3. Process capabilities and narrowed Factories (Entry 24). Beside
    //    captest the kernel runs a spinning process, and hands captest a
    //    Process capability for the task its slot held before: a stale one,
    //    which must stop nothing.
    let spin = process::spawn_signalling(&process::SPIN, 0, cap::NO_CAPS, &[None; MAX_CAPS], None).ok();
    let (tested, spared) = match (ipc::create(), spin) {
        (Some(ep), Some((spin_id, generation))) => {
            let stale = Cap::Process { id: spin_id as u8, generation: generation.wrapping_sub(1) };
            let tester = process::spawn(
                &process::CAPTEST,
                CAPTEST_PROCESS,
                cap::table(&[
                    Cap::Console,
                    Cap::Factory { rights: cap::FACTORY_ALL },
                    Cap::Endpoint { ep, badge: 0, rights: SEND | RECV },
                    stale,
                ]),
            );
            ipc::release_created(ep);
            let t = tester.ok().and_then(|id| sched::join_timeout(id, WAIT_TICKS));
            (t == code(CAPTEST_PROCESS_ALL), sched::process_state(spin_id, generation) == Some(None))
        }
        (ep, _) => {
            if let Some(ep) = ep {
                ipc::release_created(ep);
            }
            (false, false)
        }
    };
    let spin_stopped = spin.is_some_and(|(id, _)| sched::kill(id) && sched::join_timeout(id, WAIT_TICKS) == Some(sched::ExitStatus::Killed));
    ok &= check(
        b"factory",
        b"Process capabilities: hello's exit code and spin's kill reported by status, kill again peer-gone; a Factory minted to start only hello refuses spin and endpoints; a stale Process capability stops nothing (the spinning process in its slot still runs); no slot for the capability: refused, nothing started",
        tested && spared && spin_stopped,
    );

    // 4. Nothing left behind: the processes, their endpoints, their frames.
    sched::reap();
    let leaked = baseline as i64 - sched::free_frames_and_heap() as i64;
    ok &= check(
        b"factory",
        b"all reaped: no tasks, no endpoints and no frames left over",
        sched::live_tasks() == 0 && ipc::in_use() == endpoints && leaked == 0,
    );
    ok
}
