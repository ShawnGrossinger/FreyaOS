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

#![no_std]
#![no_main]

use freya::*;

freya::entry!(main);

const CONSOLE_SLOT: u64 = 0;
const FACTORY: u64 = 1;
const EP: u64 = 2;

const SPAWN: u64 = 1;
const MINT: u64 = 2;

fn main(arg: u64) -> u64 {
    match arg {
        SPAWN => spawn_tests(),
        MINT => mint_tests(),
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

    let kernel_list = spawn_raw(FACTORY, PROGRAM_HELLO, 0, 0xFFFF_8000_0000_0000, NO_CAP) == ERR_BAD_ADDRESS;
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
