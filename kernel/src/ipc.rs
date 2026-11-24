//! ipc.rs — synchronous message passing between processes.
//!
//! FreyaOS is a microkernel: drivers and services are processes, and this
//! is how they talk. The design follows L4/seL4:
//!
//!   - An endpoint is a kernel object processes meet at. It holds nothing:
//!     no buffer, no queue of messages, so it can't fill up or be used to
//!     exhaust kernel memory. Processes reach it only through Endpoint
//!     capabilities (cap.rs), which say what they may do with it.
//!   - send blocks until a receiver takes the message; recv blocks until a
//!     sender offers one. When both are there the kernel copies the
//!     message straight from one to the other (a rendezvous).
//!   - call = send + wait for the answer. The receiver can answer exactly
//!     once, with reply, and only the caller it received from.
//!   - A message is four 64-bit words, the badge of the capability it was
//!     sent through (who it came from, unforgeably), and optionally one
//!     capability. Passing a capability needs GRANT on the endpoint
//!     capability used to send. Replies may carry one too: the caller asked.
//!
//! Waiting is first come, first served (sched's wait sequence). If a server
//! ends before answering, everyone waiting for its reply wakes with
//! ERR_PEER_GONE. A process waiting here can still be killed.
//!
//! One outstanding call per server: a server that receives a second call
//! before replying to the first can no longer reply to the first (that
//! caller waits until the server ends). seL4 has the same rule.
//!
//! Entry 20 adds two things the console driver needs:
//!
//!   - Signals: the kernel can tell an endpoint's receiver that something
//!     happened without blocking and without a sender process. signal()
//!     sets bits; a receiver gets them, ahead of any message, as a message
//!     with SIGNAL_BADGE and the bits in its first word. Bits wait on the
//!     endpoint until someone receives, and repeats merge (seL4's
//!     notifications, bound to an endpoint). Nothing in a process can
//!     forge one: no capability carries SIGNAL_BADGE.
//!   - An endpoint nobody can receive from: while its creator still holds
//!     its reference, or a capability with RECV exists, a sender may wait
//!     for a receiver. Once neither does, nobody ever will: senders waiting
//!     wake with ERR_PEER_GONE, and later sends get it at once. (Before, a
//!     keyboard driver whose console had died would have waited forever.)
//!
//! Entry 23 lets a process signal too (system call signal): through an
//! endpoint capability with SEND, it sets the bits of that capability's
//! badge, so who may signal, and which bits, is decided by whoever mints
//! the capabilities. A server tells a client "something is waiting for
//! you" this way without ever waiting for that client: the PS/2 driver
//! tells the shell there are keys, and the mouse driver there are bytes,
//! and each then asks. That keeps the rule above (one held call per
//! server) from mattering: the PS/2 driver answers every call at once.

use crate::cap::{self, Cap, GRANT, RECV, SEND};
use crate::cpu::without_interrupts;
use crate::lock::{IrqGuard, IrqLock};
use crate::sched::{self, State, TaskId};
use crate::syscall::{ERR_BAD_ARGUMENT, ERR_INTERRUPTED, ERR_NO_GRANT, ERR_NO_REPLY, ERR_PEER_GONE, ERR_WRONG_CAPABILITY};

pub const MAX_ENDPOINTS: usize = 32;

/// No capability in a message / "don't transfer one" (-1 in a register).
pub const NO_SLOT: u64 = u64::MAX;

/// The badge of a signal (no capability is ever made with it).
pub const SIGNAL_BADGE: u64 = u64::MAX;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Message {
    pub badge: u64,
    pub words: [u64; 4],
    /// Capability travelling with the message (Cap::None for none).
    pub cap: Cap,
    /// On delivery: the slot it landed in, or NO_SLOT.
    pub cap_slot: u64,
    /// Where the capability was copied from (Entry 25): its position plus
    /// one (0: nowhere) and its serial number. If it has been removed by
    /// the time the message is delivered, the capability isn't delivered.
    pub cap_from: u16,
    pub cap_serial: u32,
}

impl Message {
    fn with(badge: u64, words: [u64; 4], (cap, origin): (Cap, cap::Origin)) -> Message {
        let (cap_from, cap_serial) = origin.map_or((0, 0), |(p, serial)| (p as u16 + 1, serial));
        Message { badge, words, cap, cap_slot: NO_SLOT, cap_from, cap_serial }
    }

    fn origin(&self) -> cap::Origin {
        (self.cap_from != 0).then_some((self.cap_from as usize - 1, self.cap_serial))
    }
}

/// No message. cap_slot is 0 only so that an empty message (and so an
/// empty task) is all zero bytes; nothing reads it until deliver() sets it.
pub const EMPTY_MESSAGE: Message = Message { badge: 0, words: [0; 4], cap: Cap::None, cap_slot: 0, cap_from: 0, cap_serial: 0 };

struct Endpoints {
    /// References to each endpoint (capabilities naming it, plus its
    /// creator's until released). 0 = free.
    refs: [u32; MAX_ENDPOINTS],
    /// Who could still receive on each endpoint: its creator (until it lets
    /// go) plus every capability to it with RECV.
    receivers: [u32; MAX_ENDPOINTS],
    /// Signal bits waiting on each endpoint for its next receiver.
    signals: [u64; MAX_ENDPOINTS],
}

/// Entry 31: one lock over all three (lock.rs). Nothing here holds it
/// while calling out (deliver can come back through cap::put to retain),
/// or across sched::block. The operations below that take it more than
/// once, or take it and the task table, still run inside
/// without_interrupts, so an interrupt handler's signal() can't land
/// between the steps.
static ENDPOINTS: IrqLock<Endpoints> =
    IrqLock::new(Endpoints { refs: [0; MAX_ENDPOINTS], receivers: [0; MAX_ENDPOINTS], signals: [0; MAX_ENDPOINTS] });

#[track_caller]
fn endpoints() -> IrqGuard<'static, Endpoints> {
    ENDPOINTS.lock()
}

/// A new endpoint, with one reference held by the caller: let go of it
/// with release_created when done handing out capabilities to it.
pub fn create() -> Option<u8> {
    let mut e = endpoints();
    let ep = e.refs.iter().position(|&n| n == 0)?;
    e.refs[ep] = 1;
    e.receivers[ep] = 1;
    e.signals[ep] = 0;
    Some(ep as u8)
}

/// The creator lets go of its reference (and so of its chance to hand out
/// a RECV capability later).
pub fn release_created(ep: u8) {
    release_receiver(ep);
    release(ep);
}

/// A capability with RECV to `ep` was stored in a table.
pub fn retain_receiver(ep: u8) {
    endpoints().receivers[ep as usize % MAX_ENDPOINTS] += 1;
}

/// One fewer who could receive on `ep`. At none, nobody ever will again:
/// every sender waiting there wakes with ERR_PEER_GONE.
pub fn release_receiver(ep: u8) {
    without_interrupts(|| {
        let left = {
            let mut e = endpoints();
            let n = &mut e.receivers[ep as usize % MAX_ENDPOINTS];
            *n = n.saturating_sub(1);
            *n
        };
        if left > 0 {
            return;
        }
        for t in sched::tasks().iter_mut() {
            if t.state == State::Sending(ep) {
                t.result = ERR_PEER_GONE;
                t.state = State::Ready;
            }
        }
    });
}

/// Set signal `bits` on `ep` for its receiver: delivered at once to one
/// waiting in recv, else kept (merged with any already waiting) for the
/// next recv. Never blocks; any context with interrupts off or on.
pub fn signal(ep: u8, bits: u64) {
    without_interrupts(|| {
        let ep_index = ep as usize % MAX_ENDPOINTS;
        if endpoints().refs[ep_index] == 0 {
            return;
        }
        let pending = endpoints().signals[ep_index] | bits;
        match sched::oldest(|t| t.state == State::Receiving(ep)) {
            Some(receiver) => {
                endpoints().signals[ep_index] = 0;
                deliver(receiver, signal_message(pending));
                sched::task(receiver).state = State::Ready;
            }
            None => endpoints().signals[ep_index] = pending,
        }
    });
}

fn signal_message(bits: u64) -> Message {
    Message::with(SIGNAL_BADGE, [bits, 0, 0, 0], (Cap::None, None))
}

pub fn retain(ep: u8) {
    endpoints().refs[ep as usize % MAX_ENDPOINTS] += 1;
}

pub fn release(ep: u8) {
    let mut e = endpoints();
    let n = &mut e.refs[ep as usize % MAX_ENDPOINTS];
    *n = n.saturating_sub(1);
}

/// References to endpoint `ep` (for the self-tests).
pub fn refcount(ep: u8) -> u32 {
    endpoints().refs[ep as usize % MAX_ENDPOINTS]
}

/// Endpoints that exist.
pub fn in_use() -> usize {
    endpoints().refs.iter().filter(|&&n| n > 0).count()
}

// ===========================================================================
// Operations (task context; each runs with interrupts off while it touches
// task state, and blocks through the scheduler)
// ===========================================================================

/// The endpoint behind the caller's slot `slot`, if it has `need` rights.
fn endpoint(slot: u64, need: u8) -> Result<(u8, u64, u8), i64> {
    match sched::current_cap(slot) {
        Cap::Endpoint { ep, badge, rights } if rights & need == need => Ok((ep, badge, rights)),
        _ => Err(ERR_WRONG_CAPABILITY),
    }
}

/// The capability the caller wants to pass along (slot `xfer`), if any,
/// and where the copy comes from (Entry 25).
fn outgoing(xfer: u64, may_grant: bool) -> Result<(Cap, cap::Origin), i64> {
    if xfer == NO_SLOT {
        return Ok((Cap::None, None));
    }
    if !may_grant {
        return Err(ERR_NO_GRANT);
    }
    match cap::current_with_origin(xfer) {
        (Cap::None, _) => Err(ERR_WRONG_CAPABILITY),
        found => Ok(found),
    }
}

/// Hand `msg` to task `to`: its capability (if any) goes into the first
/// free slot of `to`'s table, as a copy of the sender's (Entry 25). No free
/// slot, or the sender's has been removed since it was sent (revoked while
/// the message waited): the capability isn't delivered, and cap_slot says
/// NO_SLOT. Interrupts must be off.
fn deliver(to: TaskId, mut msg: Message) {
    msg.cap_slot = NO_SLOT;
    if msg.cap != Cap::None {
        // Bound first: in the `if let` below, the guard would live through
        // its body, and cap::put takes the table again.
        let free = cap::free_slot(&sched::task(to).caps);
        if let Some(slot) = free {
            if cap::put(to, slot, msg.cap, msg.origin()) {
                msg.cap_slot = slot as u64;
            }
        }
    }
    let mut t = sched::task(to);
    t.msg = msg;
    t.result = 0;
}

/// send (call = false) or call (call = true) through endpoint capability
/// `slot`. For a call, the answer comes back in the returned message.
pub fn send(slot: u64, words: [u64; 4], xfer: u64, call: bool) -> (i64, Message) {
    let (ep, badge, rights) = match endpoint(slot, SEND) {
        Ok(e) => e,
        Err(e) => return (e, EMPTY_MESSAGE),
    };
    let cap = match outgoing(xfer, rights & GRANT != 0) {
        Ok(c) => c,
        Err(e) => return (e, EMPTY_MESSAGE),
    };
    let msg = Message::with(badge, words, cap);

    without_interrupts(|| {
        if endpoints().receivers[ep as usize % MAX_ENDPOINTS] == 0 {
            return (ERR_PEER_GONE, EMPTY_MESSAGE); // nobody can ever receive it
        }
        let me = sched::current();
        match sched::oldest(|t| t.state == State::Receiving(ep)) {
            Some(receiver) => {
                deliver(receiver, msg);
                sched::task(receiver).state = State::Ready;
                if !call {
                    return (0, EMPTY_MESSAGE);
                }
                sched::task(receiver).reply_to = Some(me);
                // SAFETY: [switch] interrupts off (inside
                // without_interrupts) and no guard held, in a state the
                // receiver's reply ends, or kill.
                unsafe { sched::block(State::AwaitingReply(receiver)) };
            }
            None => {
                // Nobody waiting: offer it and wait. A receiver takes it
                // from here (and, for a call, moves us to AwaitingReply).
                {
                    let mut t = sched::task(me);
                    t.msg = msg;
                    t.calling = call;
                }
                // SAFETY: [switch] interrupts off and the guard above gone,
                // in a state a receiver taking the message ends, or kill,
                // or the endpoint's last receiver going (ERR_PEER_GONE).
                unsafe { sched::block(State::Sending(ep)) };
            }
        }
        // Woken: delivered (send), answered (call), or interrupted.
        let t = sched::task(me);
        (t.result, if call { t.msg } else { EMPTY_MESSAGE })
    })
}

/// Wait for a message on endpoint capability `slot`.
pub fn recv(slot: u64) -> (i64, Message) {
    let (ep, _, _) = match endpoint(slot, RECV) {
        Ok(e) => e,
        Err(e) => return (e, EMPTY_MESSAGE),
    };
    without_interrupts(|| {
        let me = sched::current();
        let pending = core::mem::take(&mut endpoints().signals[ep as usize % MAX_ENDPOINTS]);
        if pending != 0 {
            deliver(me, signal_message(pending));
        } else if let Some(sender) = sched::oldest(|t| t.state == State::Sending(ep)) {
            let (msg, calling) = {
                let mut s = sched::task(sender);
                if s.calling {
                    s.state = State::AwaitingReply(me);
                } else {
                    s.result = 0;
                    s.state = State::Ready;
                }
                (s.msg, s.calling)
            };
            if calling {
                sched::task(me).reply_to = Some(sender);
            }
            deliver(me, msg);
        } else {
            // SAFETY: [switch] interrupts off (inside without_interrupts)
            // and no guard held, in Receiving(ep): a sender, a signal, or
            // kill ends it.
            unsafe { sched::block(State::Receiving(ep)) };
        }
        let t = sched::task(me);
        (t.result, t.msg)
    })
}

/// signal(slot) (Entry 23): set the bits of the badge of the caller's
/// endpoint capability in `slot`, which needs SEND, on that endpoint, for
/// its receiver: as signal() above, never blocking. The receiver can't
/// tell them from the kernel's, so the endpoint's owner picks badges that
/// don't share bits with the kernel's signals there (init's choices:
/// user/rust/src/bin/init.rs). A capability without a badge has no bits
/// to set; an endpoint nobody can receive on any more gets peer-gone, as a
/// send would.
pub fn signal_through(slot: u64) -> i64 {
    let (ep, badge, _) = match endpoint(slot, SEND) {
        Ok(e) => e,
        Err(e) => return e,
    };
    if badge == 0 {
        return ERR_BAD_ARGUMENT;
    }
    without_interrupts(|| {
        if endpoints().receivers[ep as usize % MAX_ENDPOINTS] == 0 {
            return ERR_PEER_GONE;
        }
        signal(ep, badge);
        0
    })
}

/// Answer the last call this task received.
pub fn reply(words: [u64; 4], xfer: u64) -> i64 {
    let cap = match outgoing(xfer, true) {
        Ok(c) => c,
        Err(e) => return e,
    };
    without_interrupts(|| {
        let me = sched::current();
        let Some(caller) = sched::task(me).reply_to.take() else {
            return ERR_NO_REPLY;
        };
        if sched::task(caller).state != State::AwaitingReply(me) {
            return ERR_PEER_GONE; // it was killed while waiting
        }
        deliver(caller, Message::with(0, words, cap));
        sched::task(caller).state = State::Ready;
        0
    })
}

/// sched::exit_with: task `dead` is gone; nobody will answer its callers.
pub fn on_exit(dead: TaskId) {
    for t in sched::tasks().iter_mut() {
        if t.state == State::AwaitingReply(dead) {
            t.result = ERR_PEER_GONE;
            t.state = State::Ready;
        }
    }
}

/// sched::kill: a process blocked in IPC or waiting for an interrupt wakes
/// with ERR_INTERRUPTED, and its system call's return path ends it.
pub fn interrupt_wait(id: TaskId) {
    let mut t = sched::task(id);
    // Sleeping too (Entry 25: system call sleep), so a killed process
    // doesn't sleep out its time first.
    if matches!(
        t.state,
        State::Sending(_) | State::Receiving(_) | State::AwaitingReply(_) | State::IrqWait(_) | State::Sleeping(_)
    ) {
        t.result = ERR_INTERRUPTED;
        t.state = State::Ready;
    }
}

// ===========================================================================
// Self-test (user/ipc.asm plays every role)
// ===========================================================================

use crate::cap::table;
use crate::process::{self, IPC};
use crate::sched::ExitStatus;
use crate::selftest::{check, verdict};
use crate::{serial_write_str as s, serial_write_u64 as dec};

const ECHO_SERVER: u64 = 0;
const CLIENT: u64 = 1;
const PINGER: u64 = 2;
const GIVER: u64 = 3;
const TAKER: u64 = 4;
const WRONG_CAPS: u64 = 5;
const DYING_SERVER: u64 = 6;
const LOG_SERVER: u64 = 7;
const LOG_CLIENT: u64 = 8;
const RECEIVER: u64 = 9;
const SENDER: u64 = 10;
const COLLECTOR: u64 = 11;
const PATIENT_SERVER: u64 = 12;
const SIGNALLED: u64 = 13;

const WAIT_TICKS: u64 = 500;

/// captest's signal test (user/rust/src/bin/captest.rs, SIGNAL), the
/// badge it signals with, and its exit code when every check passes.
const CAPTEST_SIGNAL: u64 = 4;
const SIGNAL_BITS: u64 = 0x5;
const CAPTEST_SIGNAL_ALL: u64 = 0xF;

fn endpoint_cap(ep: u8, badge: u64, rights: u8) -> Cap {
    Cap::Endpoint { ep, badge, rights }
}

/// Start ipc.asm in role `mode` with `value`; slot 0 gets `slot0` and slot
/// 1 the endpoint capability.
fn start(mode: u64, value: u64, slot0: Cap, endpoint: Cap) -> Option<TaskId> {
    process::spawn(&IPC, mode | value << 8, table(&[slot0, endpoint]))
        .map_err(|_| check(b"ipc", b"start a test process", false))
        .ok()
}

fn finish(id: Option<TaskId>) -> Option<ExitStatus> {
    sched::join_timeout(id?, WAIT_TICKS)
}

fn code(value: i64) -> Option<ExitStatus> {
    Some(ExitStatus::Code(value as u64))
}

/// A fresh endpoint for one test; its creator's reference is dropped by
/// `done` once the processes hold their own.
fn fresh() -> u8 {
    create().unwrap_or(0)
}

fn done(ep: u8) {
    release_created(ep);
}

pub fn selftest() -> bool {
    let mut ok = true;
    // Earlier tests' finished tasks may not have been reaped yet: do it
    // now, or their stacks would show up as frames "gained" at the end.
    sched::reap();
    let baseline = sched::free_frames_and_heap();

    // 1. Call and reply; the server tells its two clients apart by badge.
    let ep = fresh();
    let server = start(ECHO_SERVER, 2, Cap::None, endpoint_cap(ep, 0, RECV));
    let a = start(CLIENT, 0, Cap::None, endpoint_cap(ep, 0x77, SEND));
    let b = start(CLIENT, 0, Cap::None, endpoint_cap(ep, 0x88, SEND));
    // Creator + three capabilities. (Nobody has been reaped yet: this
    // task stays ready, so the idle task, which reaps, can't run.)
    let refs = refcount(ep);
    done(ep);
    let (sa, sb, ss) = (finish(a), finish(b), finish(server));
    ok &= check(
        b"ipc",
        b"call/reply: two clients get their answers, the server sees their badges, the endpoint counts 4 references",
        sa == code(0x77) && sb == code(0x88) && ss == code(2) && refs == 4,
    );

    // 2. Many round trips, answered in order.
    let ep = fresh();
    let server = start(ECHO_SERVER, 100, Cap::None, endpoint_cap(ep, 0, RECV));
    let pinger = start(PINGER, 100, Cap::None, endpoint_cap(ep, 1, SEND));
    done(ep);
    let (sp, ss) = (finish(pinger), finish(server));
    ok &= check(b"ipc", b"100 call/reply round trips between two processes, each answered in order", sp == code(100) && ss == code(100));

    // 2b. First come, first served: three senders queue up one at a time
    //     (each is blocked before the next starts); the collector must get
    //     them in that order.
    let ep = fresh();
    let mut senders = [None; 3];
    for (i, slot) in senders.iter_mut().enumerate() {
        *slot = start(SENDER, 0, Cap::None, endpoint_cap(ep, i as u64 + 1, SEND));
        let deadline = crate::apic::ticks() + 100;
        while slot.is_some_and(|id| !sched::blocked_in_ipc(id)) && crate::apic::ticks() < deadline {
            sched::sleep_ticks(1);
        }
    }
    let collector = start(COLLECTOR, 3, Cap::None, endpoint_cap(ep, 0, RECV));
    done(ep);
    let order = finish(collector);
    let sent = senders.iter().all(|&id| finish(id) == code(0));
    ok &= check(b"ipc", b"three senders waiting on one endpoint are received first come, first served (1, 2, 3)", sent && order == code(0x010203));

    // 3. Authority that wasn't there at birth: the taker can't print, is
    //    sent the console capability, and then can.
    let (_, before) = crate::syscall::counts();
    let ep = fresh();
    let taker = start(TAKER, 0, Cap::None, endpoint_cap(ep, 0, RECV));
    let giver = start(GIVER, 0, Cap::Console, endpoint_cap(ep, 0, SEND | GRANT));
    done(ep);
    let (st, sg) = (finish(taker), finish(giver));
    let (_, after) = crate::syscall::counts();
    let printed = after - before;
    s(b"[ipc] self-test a process born without the console is sent it in a message, then prints (");
    dec(printed);
    s(b" bytes)");
    ok &= verdict(printed > 0 && st == Some(ExitStatus::Code(printed)) && sg == code(0));

    // 4. Passing a capability needs GRANT; without it nothing is sent.
    let ep = fresh();
    let giver = start(GIVER, 0, Cap::Console, endpoint_cap(ep, 0, SEND));
    done(ep);
    ok &= check(b"ipc", b"passing a capability through an endpoint capability without GRANT: refused", finish(giver) == code(ERR_NO_GRANT));

    // 5. The wrong capability for the job, or none.
    let ep = fresh();
    let misuser = start(WRONG_CAPS, 0, Cap::Console, endpoint_cap(ep, 0, SEND));
    done(ep);
    ok &= check(
        b"ipc",
        b"recv on a send-only capability, send via the console or an empty slot, reply with no call: all refused",
        finish(misuser) == code(0),
    );

    // 6. A server that ends mid-call doesn't strand its caller.
    let ep = fresh();
    let server = start(DYING_SERVER, 0, Cap::None, endpoint_cap(ep, 0, RECV));
    let client = start(CLIENT, 0, Cap::None, endpoint_cap(ep, 1, SEND));
    done(ep);
    let (sc, ss) = (finish(client), finish(server));
    ok &= check(b"ipc", b"a server that ends without answering: its caller gets peer-gone, not a hang", sc == code(ERR_PEER_GONE) && ss == code(0));

    // 6b. The caller is killed while the server holds its call: the late
    //     reply must find nobody (peer-gone) and wake nothing.
    let (calls, go) = (fresh(), fresh());
    let server = process::spawn(
        &IPC,
        PATIENT_SERVER,
        table(&[Cap::None, endpoint_cap(calls, 0, RECV), endpoint_cap(go, 0, RECV)]),
    )
    .ok();
    let client = start(CLIENT, 0, Cap::None, endpoint_cap(calls, 1, SEND));
    let deadline = crate::apic::ticks() + 100;
    let both_waiting = || [server, client].iter().all(|id| id.is_some_and(sched::blocked_in_ipc));
    while !both_waiting() && crate::apic::ticks() < deadline {
        sched::sleep_ticks(1);
    }
    let killed = client.is_some_and(sched::kill) && finish(client) == Some(ExitStatus::Killed);
    let goer = start(SENDER, 0, Cap::None, endpoint_cap(go, 0, SEND));
    done(calls);
    done(go);
    let late = finish(server);
    let _ = finish(goer);
    ok &= check(
        b"ipc",
        b"a caller killed while its server holds the call: the late reply gets peer-gone and wakes nobody",
        killed && late == code(ERR_PEER_GONE),
    );

    // 7. A service: the log server holds the console; its client holds only
    //    an endpoint capability, and prints through the server.
    let ep = fresh();
    let server = start(LOG_SERVER, 1, Cap::Console, endpoint_cap(ep, 0, RECV));
    let client = start(LOG_CLIENT, 0, Cap::None, endpoint_cap(ep, 0x10, SEND));
    done(ep);
    let (sc, ss) = (finish(client), finish(server));
    ok &= check(
        b"ipc",
        b"a process with no console (its own write refused) prints through a log server that has one",
        sc == code(32) && ss == code(1),
    );

    // 8. Blocked in IPC with nobody coming: can still be stopped.
    let ep = fresh();
    let stuck = start(RECEIVER, 0, Cap::None, endpoint_cap(ep, 0, RECV));
    done(ep);
    sched::sleep_ticks(3);
    let killed = stuck.is_some_and(sched::kill);
    ok &= check(b"ipc", b"a process blocked in recv with no sender can be killed", killed && finish(stuck) == Some(ExitStatus::Killed));

    // 8b. Signals (Entry 20): one waiting receiver gets the first at once;
    //     the next two, sent before it receives again, wait and merge.
    let ep = fresh();
    let waiter = start(SIGNALLED, 0, Cap::None, endpoint_cap(ep, 0, RECV));
    let deadline = crate::apic::ticks() + 100;
    while waiter.is_some_and(|id| !sched::blocked_in_ipc(id)) && crate::apic::ticks() < deadline {
        sched::sleep_ticks(1);
    }
    // As one step, with interrupts off (Entry 24): a timer tick between the
    // first and the second let the receiver take the first and wait again,
    // so the second reached it directly (0x0201); seen once in boot-4G.
    without_interrupts(|| {
        signal(ep, 0x01);
        signal(ep, 0x02);
        signal(ep, 0x04);
    });
    done(ep);
    ok &= check(
        b"ipc",
        b"signals: the first reaches a receiver waiting in recv; two more sent before it receives again wait and merge (0x01, then 0x06)",
        finish(waiter) == code(0x0601),
    );

    // 8c. An endpoint nobody can receive from (Entry 20): the sender waiting
    //     there when its creator lets go wakes with peer-gone, and a later
    //     one gets it at once.
    let ep = fresh();
    let waiting = start(SENDER, 0, Cap::None, endpoint_cap(ep, 0, SEND));
    let deadline = crate::apic::ticks() + 100;
    while waiting.is_some_and(|id| !sched::blocked_in_ipc(id)) && crate::apic::ticks() < deadline {
        sched::sleep_ticks(1);
    }
    retain(ep); // keep the endpoint itself while `later` is started
    done(ep);
    let later = start(SENDER, 0, Cap::None, endpoint_cap(ep, 0, SEND));
    let (sw, sl) = (finish(waiting), finish(later));
    release(ep);
    ok &= check(
        b"ipc",
        b"an endpoint no one can receive from any more: the sender waiting there gets peer-gone, and so does the next at once",
        sw == code(ERR_PEER_GONE) && sl == code(ERR_PEER_GONE),
    );

    // 8d. Signals from a process (Entry 23): captest's SIGNAL test sets
    //     bits through a badged capability and receives them, and is
    //     refused without a badge, without SEND, or where nobody can
    //     receive any more (captest.rs says which bit is which).
    //     `nobody` stops counting its creator as a receiver before captest
    //     starts, not after (Entry 28): captest signals it at once, so a
    //     tick or a host stall between spawn and done let it get 0 where
    //     peer-gone was right (2 boots of 39 on Shawn's machine). The
    //     reference stays until captest holds its own.
    let ep = fresh();
    let nobody = fresh();
    release_receiver(nobody);
    let tester = process::spawn(
        &process::CAPTEST,
        CAPTEST_SIGNAL,
        table(&[
            Cap::Console,
            endpoint_cap(ep, 0, SEND | RECV),
            endpoint_cap(ep, SIGNAL_BITS, SEND),
            endpoint_cap(ep, 0x2, RECV),
            endpoint_cap(nobody, 0x1, SEND),
        ]),
    )
    .ok();
    done(ep);
    release(nobody);
    let got = finish(tester);
    ok &= check(
        b"ipc",
        b"a process signals through a badged capability: the badge's bits arrive once, merged (0x5), without blocking; refused: no badge, no SEND, the console; peer-gone where nobody can receive",
        got == code(CAPTEST_SIGNAL_ALL as i64),
    );
    if got != code(CAPTEST_SIGNAL_ALL as i64) {
        // Which checks failed (Entry 28): a bit missing from 0xF names one.
        s(b"[ipc]   captest's signal checks: ");
        match got {
            Some(ExitStatus::Code(c)) => {
                s(b"exit code ");
                crate::serial_write_hex(c);
                s(b" of 0xF (bit 0 merged, 1 no badge, 2 no SEND, 3 peer-gone)\n");
            }
            _ => s(b"it did not exit by itself\n"),
        }
    }

    // 9. Everything given back: endpoints, tasks, frames.
    sched::reap();
    let leaked = baseline as i64 - sched::free_frames_and_heap() as i64;
    s(b"[ipc] self-test all reaped: ");
    dec(in_use() as u64);
    s(b" endpoints, ");
    dec(sched::live_tasks() as u64);
    s(b" tasks left, ");
    dec(leaked.unsigned_abs());
    s(if leaked < 0 { b" frames gained" } else { b" frames leaked" });
    ok &= verdict(in_use() == 0 && sched::live_tasks() == 0 && leaked == 0);
    ok
}
