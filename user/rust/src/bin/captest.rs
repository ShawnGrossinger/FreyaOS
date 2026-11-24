//! captest — the kernel's self-tests of spawn, endpoint and mint (Entry 21).
//!
//! The argument picks a test; the exit code has one bit per check that
//! passed, and kernel/src/factory.rs checks they all did.
//!
//! SPAWN (1), with slot 0 Console, 1 Factory, 2 an endpoint (SEND + RECV):
//!   bit 0  hello started with the console in its slot 0 and this
//!          endpoint for its exit signal: the signal comes, bit (1 << id)
//!   bit 1  the console capability where the Factory should be: refused
//!   bit 2  a program that doesn't exist: refused
//!   bit 3  a slot list naming an empty slot, or one past the last: refused
//!   bit 4  a slot list at a kernel address: refused (bad address)
//!   bit 5  an exit signal to something that isn't an endpoint: refused
//!
//! MINT (2), with slot 0 Console, 1 Factory, 2 an endpoint with every
//! right, on which the kernel has a collector (user/ipc.asm) waiting:
//!   bit 0  endpoint with the console where the Factory should be: refused
//!   bit 1  endpoint: a new one in the first free slot (3)
//!   bit 2  mint SEND with badge 7, and send through it (the collector
//!          reports the badge it saw: the kernel checks it is 7)
//!   bit 3  from that copy: more rights, or another badge: refused; the
//!          same badge again: allowed
//!   bit 4  the signal badge, no rights, an unknown right, or a capability
//!          that isn't an endpoint: refused
//!   bit 5  an exit signal to an endpoint capability without SEND: refused
//!   bit 6  slots fill up: mint and endpoint then say no slot is free
//!   bit 7  drop frees a slot, and mint lands in it again; dropping an
//!          empty slot, or one past the last, is refused
//!
//! BIND (3, Entry 22), with slot 0 Console, 1 the test interrupt line,
//! 2 an endpoint (SEND + RECV); exits with the refusals in bits 0 to 2 and
//! the signals received in bits 8 up:
//!   bit 0  binding the console capability as if it were a line: refused
//!   bit 1  binding to an endpoint capability without RECV: refused
//!   bit 2  irq_wait on the line once it is bound: refused
//!   then 5 signals, each bit (1 << 16) alone (the kernel raises the line
//!   once before this starts, then once each time it waits)
//!
//! SIGNAL (4, Entry 23), with slot 0 Console, 1 an endpoint (SEND + RECV,
//! no badge), 2 the same endpoint with SEND and badge 0x5, 3 the same with
//! RECV only and badge 0x2, 4 an endpoint nobody can receive on (SEND,
//! badge 1) (kernel/src/ipc.rs checks it exits with 0xF):
//!   bit 0  signal through slot 2, twice, then recv: one signal, 0x5 (the
//!          two merged), and signal never waited for a receiver
//!   bit 1  through slot 1, which has no badge: refused (bad argument)
//!   bit 2  through slot 3 (no SEND), or the console: refused
//!   bit 3  through slot 4: peer-gone
//!
//! INFO (5, Entry 23), with slot 0 Console, 1 Info, 2 an endpoint (SEND +
//! RECV, badge 0x2A) (kernel/src/info.rs checks it exits with 0x1F):
//!   bit 0  info: the header (100 ticks a second, some ticks so far, free
//!          memory no more than the usable) and a record for captest itself:
//!          running, a process, 3 capabilities
//!   bit 1  a buffer with room for the header only: the header, no records
//!   bit 2  refused: the console where Info should be; a buffer too small
//!          for the header
//!   bit 3  refused (bad address): a buffer at a kernel address; one in its
//!          own code, which it may read but not write
//!   bit 4  cap_info: slot 0 the console, 1 Info, 2 the endpoint with SEND
//!          + RECV and badge 0x2A, 3 empty; slot 32 refused
//!
//! PROCESS (6, Entry 24), with slot 0 Console, 1 Factory (every right), 2
//! an endpoint (SEND + RECV), 3 a stale Process capability: one for the
//! task before the spinning process the kernel has in that slot now
//! (kernel/src/factory.rs checks it exits with 0xFF, and that the spinning
//! process is still running):
//!   bit 0  hello with a Process capability (and no console, so it ends at
//!          once): its exit signal comes, and status says it exited with
//!          its code (write's refusal)
//!   bit 1  spin with a Process capability: running; kill stops it; its
//!          exit signal comes; status says killed; kill again: peer-gone
//!   bit 2  a Factory minted to start only hello: spin refused, endpoint
//!          refused, hello started
//!   bit 3  minting from that one: another program, a badge, no rights:
//!          refused
//!   bit 4  the stale Process capability: kill and status say peer-gone
//!   bit 5  kill and status on what isn't a Process capability, and an
//!          unknown spawn flag: refused
//!   bit 6  cap_info: the narrowed Factory's rights; spin's capability:
//!          its id and a generation
//!   bit 7  every slot taken, then spin asked for with a Process
//!          capability: refused (no slot), and not started (the kernel's
//!          check that nothing is left over would see it running)
//!
//! REVOKE (7, Entry 25), with slot 0 Console, 1 Factory (every right), 2
//! an endpoint (SEND + RECV), 3 Memory: one page, writable
//! (kernel/src/cap.rs checks it exits with 0x3F):
//!   bit 0  a copy of the console, and a copy of that: two derived; revoke
//!          removes both and leaves the console, with none derived
//!   bit 1  a copy and a copy of it; the first dropped: the second now
//!          counts as the console's, and revoke removes it
//!   bit 2  revoke and copy of an empty slot, and revoke past the last:
//!          refused
//!   bit 3  across processes: relay, lent a copy of the console and a
//!          Factory for tickers, starts a ticker with its console (both
//!          quiet). The copy lent has two derived from it, relay's and the
//!          ticker's; revoking it removes both, and the Factory's, one.
//!          relay notices and ends with no-capability; its ticker does
//!          too (the kernel checks nothing is left running)
//!   bit 4  memory: a copy mapped and written; mapped again: refused (one
//!          mapping per capability); the original revoked: the copy and
//!          its mapping gone (mapping the original at the same address now
//!          works, and shows what was written); another copy, then the
//!          original dropped: its mapping gone too
//!   bit 5  mint makes copies too: a SEND-only endpoint and a Factory for
//!          hello alone are derived from the endpoint and the Factory they
//!          were minted from; revoking those removes them, and only them
//!
//! RECEIVE (8), GIVE (9), GIVE_LATE (10), RECEIVE_ONE (11) (Entry 25):
//! a capability in a message, taken back (kernel/src/cap.rs pairs them):
//!   GIVE         calls through slot 1 (SEND + GRANT) with a copy of its
//!                console; once answered, revokes it and sends again with
//!                nothing; exits with how many the revoke removed (1: the
//!                receiver's)
//!   RECEIVE      receives on slot 1, looks at what came and answers, then
//!                receives again; exits with the kind of capability the
//!                first brought (1, the console) and, << 8, what that slot
//!                holds after the second (0: nothing)
//!   GIVE_LATE    sends a copy of its console, with nobody receiving yet:
//!                the kernel revokes it while it waits; exits with send's
//!                result
//!   RECEIVE_ONE  receives once; exits with 0x10 | the kind it brought (0:
//!                none: the capability revoked on the way wasn't delivered)
//!
//! HEAP (12, Entry 26), with slot 0 Console, 1 Factory, 2 an endpoint
//! (SEND + RECV), 3 a Budget of 256 pages (kernel/src/mem.rs checks it
//! exits with 0x1F):
//!   bit 0  heap grows to 4 pages at its fixed place, which keep what is
//!          written; shrunk to 2, those two still hold it; grown to 4 again,
//!          the new two are zeros, every byte (pages come to a heap fresh:
//!          nothing of another process's is left in them); cap_info counts
//!          them; 0 gives it up
//!   bit 1  limits: 257 pages refused, the heap as it was; all 256 allowed;
//!          a Budget of 64 minted from it refuses 65 and allows 64, and then
//!          the original allows only 192 more; dropping the copy frees its
//!          heap; with the original using 200, another copy of 64 may have
//!          only 56 (the limit above it counts); refused: the console, an
//!          empty slot, minting more or none, a heap bigger than its place
//!   bit 2  a copy's heap goes when the copy is revoked, or dropped
//!   bit 3  the allocator (freya::heap): Box, a Vec of 10,000, a String of
//!          100 pieces, a BTreeMap of 1,000, then 2 MiB more refused (the
//!          budget is 1 MiB) without changing the heap; all freed, the heap
//!          is one free block; then one allocation of all 1 MiB but 16
//!          bytes, which fits only if the free block at the heap's end is
//!          counted; and the heap given back (release), which it isn't
//!          while anything on it is in use
//!   bit 4  across processes: hog lent a Budget of 32 pages minted from
//!          this one fills it; revoking it frees hog's 32 pages, and hog's
//!          next read of them is a page fault that ends it
//! It ends with 8 pages on its heap, for the kernel to see freed when a
//! process ends.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use freya::*;

freya::entry!(main);

const CONSOLE_SLOT: u64 = 0;
const FACTORY: u64 = 1;
const EP: u64 = 2;

const SPAWN: u64 = 1;
const MINT: u64 = 2;
const BIND: u64 = 3;
const SIGNAL: u64 = 4;
const INFO: u64 = 5;
const PROCESS: u64 = 6;
const REVOKE: u64 = 7;
const RECEIVE: u64 = 8;
const GIVE: u64 = 9;
const GIVE_LATE: u64 = 10;
const RECEIVE_ONE: u64 = 11;
const HEAP: u64 = 12;
const TEST_LINE: u64 = 16;

fn main(arg: u64) -> u64 {
    match arg {
        SPAWN => spawn_tests(),
        MINT => mint_tests(),
        BIND => bind_tests(),
        SIGNAL => signal_tests(),
        INFO => info_tests(),
        PROCESS => process_tests(),
        REVOKE => revoke_tests(),
        RECEIVE => receive(true),
        RECEIVE_ONE => receive(false),
        GIVE => give(true),
        GIVE_LATE => give(false),
        HEAP => heap_tests(),
        _ => 0,
    }
}

fn bit(n: u32, ok: bool) -> u64 {
    (ok as u64) << n
}

fn spawn_tests() -> u64 {
    let mut list = [NO_SLOT; MAX_CAPS];
    list[0] = CONSOLE_SLOT as u8;
    let id = spawn(FACTORY, PROGRAM_HELLO, 0, &list, EP);
    let signalled = id >= 0
        && matches!(recv(EP), Ok(m) if m.badge == SIGNAL_BADGE && m.words[0] == 1 << id);

    let no_factory = spawn(CONSOLE_SLOT, PROGRAM_HELLO, 0, &list, NO_CAP) == ERR_NO_CAPABILITY;
    let no_program = spawn(FACTORY, 99, 0, &list, NO_CAP) == ERR_BAD_ARGUMENT;

    let mut empty = list;
    empty[1] = 9; // nothing there
    let mut past = list;
    past[1] = 40; // past the last slot
    let bad_slots = spawn(FACTORY, PROGRAM_HELLO, 0, &empty, NO_CAP) == ERR_BAD_ARGUMENT
        && spawn(FACTORY, PROGRAM_HELLO, 0, &past, NO_CAP) == ERR_BAD_ARGUMENT;

    let kernel_list = spawn_raw(FACTORY, PROGRAM_HELLO, 0, 0xFFFF_8000_0000_0000, NO_CAP, 0).0 == ERR_BAD_ADDRESS;
    let exit_not_endpoint = spawn(FACTORY, PROGRAM_HELLO, 0, &list, CONSOLE_SLOT) == ERR_WRONG_CAPABILITY;

    bit(0, signalled) | bit(1, no_factory) | bit(2, no_program) | bit(3, bad_slots) | bit(4, kernel_list) | bit(5, exit_not_endpoint)
}

fn mint_tests() -> u64 {
    let no_factory = endpoint(CONSOLE_SLOT) == ERR_NO_CAPABILITY;
    let new = endpoint(FACTORY);
    let made = new == 3;

    let badged = mint(EP, SEND, 7);
    let sent = badged >= 0 && send(badged as u64, [1, 2, 3, 4], NO_CAP) == 0;

    let from_badged = badged >= 0
        && mint(badged as u64, SEND | RECV, 7) == ERR_BAD_ARGUMENT
        && mint(badged as u64, SEND, 8) == ERR_BAD_ARGUMENT
        && mint(badged as u64, SEND, 7) >= 0;

    let refused = mint(EP, SEND, SIGNAL_BADGE) == ERR_BAD_ARGUMENT
        && mint(EP, 0, 0) == ERR_BAD_ARGUMENT
        && mint(EP, 1 << 3, 0) == ERR_BAD_ARGUMENT
        && mint(CONSOLE_SLOT, SEND, 0) == ERR_WRONG_CAPABILITY;

    let receive_only = mint(EP, RECV, 0);
    let mut list = [NO_SLOT; MAX_CAPS];
    list[0] = CONSOLE_SLOT as u8;
    let exit_without_send =
        receive_only >= 0 && spawn(FACTORY, PROGRAM_HELLO, 0, &list, receive_only as u64) == ERR_WRONG_CAPABILITY;

    // Fill every slot that is left, then ask for one more of each.
    let mut filled = 0;
    while mint(EP, SEND, 0) >= 0 {
        filled += 1;
        if filled > MAX_CAPS {
            break;
        }
    }
    let used = 3 + 1 + 2 + 1; // given, new, badged and its copy, receive_only
    let full = filled == MAX_CAPS - used && mint(EP, SEND, 0) == ERR_NO_SLOT && endpoint(FACTORY) == ERR_NO_SLOT;

    let freed = drop_cap(9) == 0 && mint(EP, SEND, 0) == 9;
    let dropped_twice = drop_cap(9) == 0 && drop_cap(9) == ERR_NO_CAPABILITY && drop_cap(40) == ERR_NO_CAPABILITY;

    bit(0, no_factory) | bit(1, made) | bit(2, sent) | bit(3, from_badged) | bit(4, refused) | bit(5, exit_without_send) | bit(6, full)
        | bit(7, freed && dropped_twice)
}

fn bind_tests() -> u64 {
    const IRQ: u64 = 1;
    let not_a_line = irq_bind(CONSOLE_SLOT, EP) == ERR_WRONG_CAPABILITY;
    let send_only = mint(EP, SEND, 0);
    let no_recv = send_only >= 0 && irq_bind(IRQ, send_only as u64) == ERR_WRONG_CAPABILITY;
    if irq_bind(IRQ, EP) != 0 {
        return 0;
    }
    let no_wait = irq_wait(IRQ) == ERR_BAD_ARGUMENT;
    irq_ack(IRQ);
    let mut signals = 0;
    for _ in 0..5 {
        match recv(EP) {
            Ok(m) if m.badge == SIGNAL_BADGE && m.words[0] == 1 << TEST_LINE => signals += 1,
            _ => break,
        }
    }
    bit(0, not_a_line) | bit(1, no_recv) | bit(2, no_wait) | signals << 8
}

fn signal_tests() -> u64 {
    const OWN: u64 = 1;
    const BADGED: u64 = 2;
    const RECV_ONLY: u64 = 3;
    const NOBODY: u64 = 4;
    let sent = signal(BADGED) == 0 && signal(BADGED) == 0;
    let merged = sent && matches!(recv(OWN), Ok(m) if m.badge == SIGNAL_BADGE && m.words[0] == 0x5);
    let no_badge = signal(OWN) == ERR_BAD_ARGUMENT;
    let no_send = signal(RECV_ONLY) == ERR_WRONG_CAPABILITY && signal(CONSOLE_SLOT) == ERR_WRONG_CAPABILITY;
    let gone = signal(NOBODY) == ERR_PEER_GONE;
    bit(0, merged) | bit(1, no_badge) | bit(2, no_send) | bit(3, gone)
}

/// Room for the header and every task there can be.
static mut BUF: [u8; INFO_HEADER + 32 * INFO_RECORD] = [0; INFO_HEADER + 32 * INFO_RECORD];

fn info_tests() -> u64 {
    const INFO_SLOT: u64 = 1;
    let buf = unsafe { &mut *(&raw mut BUF) };
    let mine = match info(INFO_SLOT, buf) {
        Ok(sys) => {
            let me = (0..sys.tasks).map(|i| task_info(buf, i)).find(|t| t.name() == b"captest");
            sys.ticks_per_second == 100
                && sys.ticks > 0
                && sys.free_frames > 0
                && sys.free_frames <= sys.usable_frames
                && me.is_some_and(|t| t.what == TASK_RUNNING && t.flags & TASK_FLAG_PROCESS != 0 && t.caps == 3 && t.frames > 0)
                && sys.frames[FRAMES_PROCESS_CODE] > 0
                && sys.frames[FRAMES_KERNEL_IMAGE] > 0
                && sys.frames[FRAMES_FREE] > 0
        }
        Err(_) => false,
    };
    let header_only = matches!(info(INFO_SLOT, &mut buf[..INFO_HEADER + INFO_RECORD - 1]), Ok(sys) if sys.tasks == 0);
    let refused = info(CONSOLE_SLOT, buf).err() == Some(ERR_WRONG_CAPABILITY)
        && info(INFO_SLOT, &mut buf[..INFO_HEADER - 1]).err() == Some(ERR_BAD_ARGUMENT);
    let code_start = main as *const () as u64 & !0xFFF;
    let bad_address = info_raw(INFO_SLOT, 0xFFFF_8000_0000_0000, 4096) == ERR_BAD_ADDRESS
        && info_raw(INFO_SLOT, code_start, 4096) == ERR_BAD_ADDRESS;
    let caps = cap_info(0) == Ok((KIND_CONSOLE, 0, 0, 0))
        && cap_info(1) == Ok((KIND_INFO, 0, 0, 0))
        && matches!(cap_info(2), Ok((KIND_ENDPOINT, _, rights, 0x2A)) if rights == SEND | RECV)
        && cap_info(3) == Ok((KIND_NONE, 0, 0, 0))
        && cap_info(32) == Err(ERR_BAD_ARGUMENT);
    bit(0, mine) | bit(1, header_only) | bit(2, refused) | bit(3, bad_address) | bit(4, caps)
}

/// Wait (a few signals at most) for process `id`'s exit signal on EP.
fn wait_exit(id: u64) -> bool {
    for _ in 0..8 {
        match recv(EP) {
            Ok(m) if m.badge == SIGNAL_BADGE && m.words[0] & (1 << id) != 0 => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

fn process_tests() -> u64 {
    const STALE: u64 = 3;
    let none = [NO_SLOT; MAX_CAPS];

    let ended = match spawn_process(FACTORY, PROGRAM_HELLO, 0, &none, EP) {
        Ok((id, cap)) => wait_exit(id) && status(cap) == Ok((STATUS_EXITED, ERR_NO_CAPABILITY as u64)),
        Err(_) => false,
    };

    let mut spin = None;
    let stopped = match spawn_process(FACTORY, PROGRAM_SPIN, 0, &none, EP) {
        Ok((id, cap)) => {
            spin = Some((id, cap));
            status(cap) == Ok((STATUS_RUNNING, 0))
                && kill(cap) == 0
                && wait_exit(id)
                && status(cap) == Ok((STATUS_KILLED, 0))
                && kill(cap) == ERR_PEER_GONE
        }
        Err(_) => false,
    };

    let narrow = mint(FACTORY, 1 << PROGRAM_HELLO, 0);
    let narrowed = narrow >= 0
        && spawn(narrow as u64, PROGRAM_SPIN, 0, &none, NO_CAP) == ERR_WRONG_CAPABILITY
        && endpoint(narrow as u64) == ERR_WRONG_CAPABILITY
        && spawn(narrow as u64, PROGRAM_HELLO, 0, &none, NO_CAP) >= 0;
    let refused = narrow >= 0
        && mint(narrow as u64, 1 << PROGRAM_SPIN, 0) == ERR_BAD_ARGUMENT
        && mint(narrow as u64, 1 << PROGRAM_HELLO, 5) == ERR_BAD_ARGUMENT
        && mint(narrow as u64, 0, 0) == ERR_BAD_ARGUMENT;

    let stale = kill(STALE) == ERR_PEER_GONE && status(STALE) == Err(ERR_PEER_GONE);

    let wrong = kill(CONSOLE_SLOT) == ERR_WRONG_CAPABILITY
        && status(EP) == Err(ERR_WRONG_CAPABILITY)
        && spawn_raw(FACTORY, PROGRAM_HELLO, 0, none.as_ptr() as u64, NO_CAP, 1 << 1).0 == ERR_BAD_ARGUMENT;

    let described = narrow >= 0
        && cap_info(narrow as u64) == Ok((KIND_FACTORY, 1 << PROGRAM_HELLO, 0, 0))
        && matches!(spin, Some((id, cap)) if matches!(cap_info(cap), Ok((KIND_PROCESS, i, g, 0)) if i == id && g > 0));

    // Last, as it fills the table.
    let mut filled = 0;
    while filled < MAX_CAPS && mint(EP, SEND, 0) >= 0 {
        filled += 1;
    }
    let no_room = spawn_process(FACTORY, PROGRAM_SPIN, 0, &none, NO_CAP) == Err(ERR_NO_SLOT);

    bit(0, ended) | bit(1, stopped) | bit(2, narrowed) | bit(3, refused) | bit(4, stale) | bit(5, wrong) | bit(6, described)
        | bit(7, no_room)
}

fn revoke_tests() -> u64 {
    const MEMORY: u64 = 3;
    const A: u64 = MAP_BASE;
    const B: u64 = MAP_BASE + 0x10_0000;
    const WRITTEN: u64 = 0x1234_5678;
    let gone = |slot: i64| slot < 0 || !holds(slot as u64, KIND_CONSOLE);

    let a = copy(CONSOLE_SLOT);
    let b = if a >= 0 { copy(a as u64) } else { a };
    let chain = b >= 0
        && derived(CONSOLE_SLOT) == 2
        && revoke(CONSOLE_SLOT) == 2
        && gone(a)
        && gone(b)
        && holds(CONSOLE_SLOT, KIND_CONSOLE)
        && derived(CONSOLE_SLOT) == 0;

    let a = copy(CONSOLE_SLOT);
    let b = if a >= 0 { copy(a as u64) } else { a };
    let handed_up = b >= 0
        && drop_cap(a as u64) == 0
        && derived(CONSOLE_SLOT) == 1
        && revoke(CONSOLE_SLOT) == 1
        && gone(b);

    let refused = revoke(20) == ERR_NO_CAPABILITY && copy(20) == ERR_NO_CAPABILITY && revoke(40) == ERR_NO_CAPABILITY;

    // mint makes copies too: a narrower endpoint and a narrower Factory are
    // derived from the ones they were minted from, and revoked with them.
    let send_only = mint(EP, SEND, 5);
    let hello_only = mint(FACTORY, 1 << PROGRAM_HELLO, 0);
    let minted = send_only >= 0
        && hello_only >= 0
        && derived(EP) == 1
        && derived(FACTORY) == 1
        && revoke(EP) == 1
        && revoke(FACTORY) == 1
        && !holds(send_only as u64, KIND_ENDPOINT)
        && !holds(hello_only as u64, KIND_FACTORY)
        && holds(EP, KIND_ENDPOINT)
        && holds(FACTORY, KIND_FACTORY);

    // Across processes: relay, and the ticker relay starts.
    let loan = copy(CONSOLE_SLOT);
    let tickers = mint(FACTORY, 1 << PROGRAM_TICKER, 0);
    let mut slots = [NO_SLOT; MAX_CAPS];
    slots[0] = loan as u8;
    slots[1] = tickers as u8;
    let across = loan >= 0
        && tickers >= 0
        && match spawn_process(FACTORY, PROGRAM_RELAY, TICKER_QUIET | 1, &slots, EP) {
            Ok((id, process)) => {
                // Wait (bounded) until relay has started its ticker.
                for _ in 0..300 {
                    if derived(loan as u64) == 2 {
                        break;
                    }
                    sleep(1);
                }
                derived(loan as u64) == 2
                    && revoke(loan as u64) == 2
                    && revoke(tickers as u64) == 1
                    && wait_exit(id)
                    && status(process) == Ok((STATUS_EXITED, ERR_NO_CAPABILITY as u64))
            }
            Err(_) => false,
        };

    // Memory.
    let read = || unsafe { core::ptr::read_volatile(A as *const u64) };
    let second = copy(MEMORY);
    let memory = second >= 0
        && map(second as u64, A) == 0
        && {
            unsafe { core::ptr::write_volatile(A as *mut u64, WRITTEN) };
            read() == WRITTEN
        }
        && map(second as u64, B) == ERR_BAD_ARGUMENT
        && revoke(MEMORY) == 1
        && !holds(second as u64, KIND_MEMORY)
        && map(MEMORY, A) == 0 // A was unmapped, or this would be refused
        && read() == WRITTEN
        && {
            let third = copy(MEMORY);
            third >= 0 && drop_cap(MEMORY) == 0 && map(third as u64, A) == 0 && read() == WRITTEN
        };

    bit(0, chain) | bit(1, handed_up) | bit(2, refused) | bit(3, across) | bit(4, memory) | bit(5, minted)
}

/// GIVE and GIVE_LATE: a copy of the console sent through slot 1.
fn give(twice: bool) -> u64 {
    const OUT: u64 = 1;
    let lent = copy(CONSOLE_SLOT);
    if lent < 0 {
        return lent as u64;
    }
    if !twice {
        return send(OUT, [0; 4], lent as u64) as u64;
    }
    // A call, answered once the receiver has looked at what came: a send
    // returns as soon as the message is delivered, and revoking then could
    // beat the receiver to its look.
    if call(OUT, [0; 4], lent as u64).is_err() {
        return 0xBAD;
    }
    let removed = revoke(lent as u64);
    send(OUT, [0; 4], NO_CAP);
    removed as u64
}

/// RECEIVE and RECEIVE_ONE: what the messages on slot 1 brought.
fn receive(twice: bool) -> u64 {
    const IN: u64 = 1;
    let Ok(first) = recv(IN) else { return 0xBAD };
    let kind = |slot: u64| match cap_info(slot) {
        Ok((k, ..)) => k as u64,
        Err(_) => 0,
    };
    if !twice {
        return 0x10 | if first.cap_slot == NO_CAP { 0 } else { kind(first.cap_slot) };
    }
    let brought = kind(first.cap_slot);
    reply([0; 4], NO_CAP); // looked: GIVE may revoke now
    if recv(IN).is_err() {
        return 0xBAD;
    }
    brought | kind(first.cap_slot) << 8
}

/// HEAP (12, Entry 26): see the top of this file.
fn heap_tests() -> u64 {
    const BUDGET: u64 = 3;
    const PAGES: u64 = 256;
    let info = |slot: u64| cap_info(slot).unwrap_or((0, 0, 0, 0));
    let word = |at: i64, page: u64| (at as u64 + page * PAGE) as *mut u64;
    let read = |at: i64, page: u64| unsafe { word(at, page).read_volatile() };
    let fill = |at: i64, page: u64, v: u64| unsafe { word(at, page).write_volatile(v) };

    // bit 0
    let at = heap(BUDGET, 4);
    let placed = at == (HEAP_BASE + BUDGET * HEAP_SPAN) as i64;
    let grow_shrink = placed && {
        for i in 0..4 {
            fill(at, i, 0x1111 * (i + 1));
        }
        heap(BUDGET, 2) == at
            && (0..2).all(|i| read(at, i) == 0x1111 * (i + 1))
            && heap(BUDGET, 4) == at
            && (2..4).all(|i| (0..PAGE / 8).all(|w| unsafe { word(at, i).add(w as usize).read_volatile() } == 0))
            && info(BUDGET) == (KIND_BUDGET, PAGES, 4, 4)
            && heap(BUDGET, 0) == at
            && info(BUDGET) == (KIND_BUDGET, PAGES, 0, 0)
    };

    // bit 1
    let over = heap(BUDGET, PAGES + 1) == ERR_OVER_BUDGET && info(BUDGET).3 == 0;
    let all = heap(BUDGET, PAGES) == at && heap(BUDGET, 0) == at;
    let m = mint(BUDGET, 64, 0);
    let narrower = m >= 0 && {
        let m = m as u64;
        heap(m, 65) == ERR_OVER_BUDGET
            && heap(m, 64) == (HEAP_BASE + m * HEAP_SPAN) as i64
            && heap(BUDGET, PAGES - 64 + 1) == ERR_OVER_BUDGET
            && heap(BUDGET, PAGES - 64) == at
            && info(BUDGET) == (KIND_BUDGET, PAGES, PAGES, PAGES - 64)
            && info(m) == (KIND_BUDGET, 64, 64, 64)
            && heap(BUDGET, 0) == at
            && drop_cap(m) == 0
            && info(BUDGET).2 == 0
    };
    let m2 = mint(BUDGET, 64, 0);
    let above = m2 >= 0 && {
        let m2 = m2 as u64;
        heap(BUDGET, 200) == at
            && heap(m2, 64) == ERR_OVER_BUDGET
            && heap(m2, 56) > 0
            && info(BUDGET).2 == PAGES
            && heap(m2, 0) > 0
            && heap(BUDGET, 0) == at
            && drop_cap(m2) == 0
    };
    let refused = heap(CONSOLE_SLOT, 1) == ERR_WRONG_CAPABILITY
        && heap(20, 1) == ERR_NO_CAPABILITY
        && heap(40, 1) == ERR_NO_CAPABILITY
        && mint(BUDGET, PAGES + 1, 0) == ERR_BAD_ARGUMENT
        && mint(BUDGET, 0, 0) == ERR_BAD_ARGUMENT
        && heap(BUDGET, 1 << 40) == ERR_BAD_ARGUMENT;
    let limits = over && all && narrower && above && refused;

    // bit 2
    let c = copy(BUDGET);
    let revoked = c >= 0
        && heap(c as u64, 8) > 0
        && info(BUDGET).2 == 8
        && revoke(BUDGET) == 1
        && info(BUDGET).2 == 0
        && !holds(c as u64, KIND_BUDGET);
    let d = copy(BUDGET);
    let dropped = d >= 0 && heap(d as u64, 8) > 0 && info(BUDGET).2 == 8 && drop_cap(d as u64) == 0 && info(BUDGET).2 == 0;

    // bit 3
    let ready = heap::use_budget(BUDGET).is_ok();
    let used = ready && {
        let boxed = Box::new(0x5A5A_u64);
        let numbers: Vec<u64> = (0..10_000).collect();
        let mut text = String::new();
        for _ in 0..100 {
            text.push_str("piece ");
        }
        let mut squares = BTreeMap::new();
        for i in 0..1000u64 {
            squares.insert(i, i * i);
        }
        for i in (0..1000u64).step_by(2) {
            squares.remove(&i);
        }
        let before = heap::stats().bytes;
        let too_much = Vec::<u8>::new().try_reserve_exact(2 << 20).is_err();
        let kept = !heap::release() && heap::stats().bytes == before;
        *boxed == 0x5A5A
            && numbers.iter().sum::<u64>() == 49_995_000
            && text.len() == 600
            && squares.len() == 500
            && squares.get(&999) == Some(&998_001)
            && too_much
            && kept
            && heap::stats().bytes == before
    };
    let st = heap::stats();
    let freed = used && st.in_use == 0 && st.free_blocks == 1 && st.free == st.bytes && st.bytes > 0 && info(BUDGET).3 == st.bytes / PAGE;
    let whole = {
        let mut all = Vec::<u8>::new();
        all.try_reserve_exact((PAGES * PAGE - 16) as usize).is_ok() && heap::stats().bytes == PAGES * PAGE
    };
    let released = heap::release() && heap::stats().bytes == 0 && info(BUDGET).3 == 0;
    let allocator = freed && whole && released;

    // bit 4
    let lent = mint(BUDGET, 32, 0);
    let across = lent >= 0 && {
        let lent = lent as u64;
        let mut slots = [NO_SLOT; MAX_CAPS];
        slots[0] = CONSOLE_SLOT as u8;
        slots[1] = lent as u8;
        match spawn_process(FACTORY, PROGRAM_HOG, TICKER_QUIET | 1, &slots, EP) {
            Ok((id, process)) => {
                let mut full = false;
                for _ in 0..200 {
                    if info(lent).2 == 32 {
                        full = true;
                        break;
                    }
                    sleep(1);
                }
                let before = info(BUDGET).2;
                let ok = full
                    && revoke(lent) == 1
                    && info(BUDGET).2 == before - 32
                    && wait_exit(id)
                    && status(process) == Ok((STATUS_FAULT, 14));
                drop_cap(process);
                ok
            }
            Err(_) => false,
        }
    };
    if lent >= 0 {
        drop_cap(lent as u64);
    }

    // Left for the end of this process to free.
    heap(BUDGET, 8);
    bit(0, grow_shrink) | bit(1, limits) | bit(2, revoked && dropped) | bit(3, allocator) | bit(4, across)
}
