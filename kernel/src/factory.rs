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
//!       Returns the new process's id.
//!
//!   endpoint(factory)
//!       A new endpoint, as a capability with SEND, RECV and GRANT and no
//!       badge, in the caller's first free slot. Returns the slot. mint
//!       (cap.rs) makes narrower copies to hand out.
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
pub static PROGRAMS: [&Program; 3] = [&process::HELLO, &process::CONSOLE, &process::KEYBOARD];

/// "No capability here" in spawn's slot list.
pub const NO_SLOT_BYTE: u8 = 0xFF;

fn has_factory(slot: u64) -> bool {
    sched::current_cap(slot) == Cap::Factory
}

pub fn spawn(factory: u64, program: u64, arg: u64, slots: u64, exit_ep: u64) -> i64 {
    if !has_factory(factory) {
        return ERR_NO_CAPABILITY;
    }
    let Some(&program) = PROGRAMS.get(program as usize) else {
        return ERR_BAD_ARGUMENT;
    };
    let mut list = [0u8; MAX_CAPS];
    if let Err(e) = copy_from_user(slots, &mut list) {
        return e;
    }
    let mut caps: Caps = cap::NO_CAPS;
    for (i, &from) in list.iter().enumerate() {
        if from == NO_SLOT_BYTE {
            continue;
        }
        match sched::current_cap(from as u64) {
            Cap::None => return ERR_BAD_ARGUMENT, // an empty slot, or past the last
            c => caps[i] = c,
        }
    }
    let exit_signal = match exit_ep {
        crate::ipc::NO_SLOT => None,
        slot => match sched::current_cap(slot) {
            Cap::Endpoint { ep, rights, .. } if rights & SEND != 0 => Some(ep),
            _ => return ERR_WRONG_CAPABILITY,
        },
    };
    match process::spawn_signalling(program, arg, caps, exit_signal) {
        Ok(id) => id as i64,
        Err(_) => ERR_NO_MEMORY,
    }
}

pub fn endpoint(factory: u64) -> i64 {
    if !has_factory(factory) {
        return ERR_NO_CAPABILITY;
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
            let caps = cap::table(&[Cap::Console, Cap::Factory, Cap::Endpoint { ep: own, badge: 0, rights: SEND | RECV }]);
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
                cap::table(&[Cap::Console, Cap::Factory, Cap::Endpoint { ep, badge: 0, rights: SEND | RECV | GRANT }]),
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

    // 3. Nothing left behind: the processes, their endpoints, their frames.
    sched::reap();
    let leaked = baseline as i64 - sched::free_frames_and_heap() as i64;
    ok &= check(
        b"factory",
        b"all reaped: no tasks, no endpoints and no frames left over",
        sched::live_tasks() == 0 && ipc::in_use() == endpoints && leaked == 0,
    );
    ok
}
