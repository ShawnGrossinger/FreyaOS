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

#![no_std]
#![no_main]

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
const TEST_LINE: u64 = 16;

fn main(arg: u64) -> u64 {
    match arg {
        SPAWN => spawn_tests(),
        MINT => mint_tests(),
        BIND => bind_tests(),
        SIGNAL => signal_tests(),
        INFO => info_tests(),
        PROCESS => process_tests(),
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
                && me.is_some_and(|t| t.what == TASK_RUNNING && t.flags & TASK_FLAG_PROCESS != 0 && t.caps == 3)
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
