//! init — the first process (Entry 21): it starts the drivers and the
//! shell, and starts them again if they end.
//!
//! The kernel starts init and nothing else, with every capability it used
//! to hand out to drivers itself (kernel/src/main.rs: start_init):
//!
//!   slot 0  Console      1  Factory         2  its own endpoint (exit signals)
//!   slot 3  the console driver's endpoint, with every right
//!   slot 4  the screen   5  the system log  6  the status rows
//!   slot 7  the screen's description
//!   slot 8  Irq 1        9  IoPorts 0x60   10  IoPorts 0x64   11  Power
//!   slot 12 Irq 12 (Entry 22)            13 Info (Entry 23)
//!
//! It makes the other endpoints itself: the PS/2 driver's (Entry 22), the
//! mouse driver's and the shell's (Entry 23). It mints narrower copies of
//! each: RECV for the one that receives there, and SEND with a badge for
//! each caller, so each can tell who asks: on the console driver's, the
//! PS/2 driver 1, the mouse driver 3, the shell 4; on the PS/2 driver's,
//! the mouse driver 2, the shell 4. And copies that only signal (system
//! call signal: the badge is the bits set): the PS/2 driver's to say bytes
//! are waiting for the mouse driver (SIGNAL_BYTES) and keys for the shell
//! (SIGNAL_KEYS), and its own to tell the shell the console driver started
//! again (SIGNAL_REDRAW), so it sends the line being typed again.
//!
//! For the shell (Entry 24) it also mints a Factory that can start only
//! hello, spin, crash, ticker and relay (Entry 25), and makes no
//! endpoints, and a SEND copy of the
//! shell's own endpoint, where the programs the shell starts signal when
//! they end. Then it lets go of the full copies of the four endpoints it
//! has minted from: it won't mint again, and every RECV it still holds is
//! a minted one, so letting go of that is enough to stop anyone waiting
//! for a driver it has given up on. That leaves it 27 of its 32 slots in
//! use: the kernel's 14, three endpoints and fourteen copies made, less
//! the four let go of (29 at Entry 23).
//!
//! Then it starts each with exactly the slots it needs, asking the kernel
//! to signal its own
//! endpoint with bit (1 << id) when one ends, and waits there. A driver
//! that ends is started again, at most MAX_RESTARTS times; after that init
//! says so, leaves it stopped, and lets go of what only it would have
//! used. For the console driver that is the endpoint's RECV rights: while
//! init held them, the keyboard driver's next PageUp would have waited
//! for a console that never comes (typing and Ctrl+Alt+Del with it); once
//! nobody can receive there, it is told so at once (Entries 20 and 21).
//!
//! Which driver gets what is decided here, not in the kernel.

#![no_std]
#![no_main]

use freya::{Line, MAX_CAPS, NO_CAP, NO_SLOT, RECV, SEND, SIGNAL_BADGE, SIGNAL_BYTES, SIGNAL_KEYS, SIGNAL_REDRAW};

freya::entry!(main);

const CONSOLE: u8 = 0;
const FACTORY: u64 = 1;
const OWN: u64 = 2;
const CONSOLE_EP: u64 = 3;
const SCREEN: u8 = 4;
const LOG: u8 = 5;
const PANEL: u8 = 6;
const DESCRIPTION: u8 = 7;
const KEYBOARD_IRQ: u8 = 8;
const KEYBOARD_DATA: u8 = 9;
const KEYBOARD_STATUS: u8 = 10;
const POWER: u8 = 11;
const MOUSE_IRQ: u8 = 12;
const INFO: u8 = 13;

/// Badges: who is calling, on the endpoint called.
const PS2_TO_CONSOLE: u64 = 1;
const MOUSE_TO_PS2: u64 = 2;
const MOUSE_TO_CONSOLE: u64 = 3;
const SHELL_TO_PS2: u64 = 4;
const SHELL_TO_CONSOLE: u64 = 4;

/// Its place in the driver list, below.
const CONSOLE_DRIVER: usize = 0;

/// Times a driver is started again before init leaves it stopped.
const MAX_RESTARTS: u32 = 3;

struct Driver {
    name: &'static [u8],
    program: u64,
    /// Its slot i gets init's slot slots[i].
    slots: [u8; MAX_CAPS],
    /// Running as this process; None if it isn't running.
    id: Option<u64>,
    restarts: u32,
    /// Init's slots to let go of if it stops starting this driver again.
    drop_when_stopped: [u8; 2],
    /// What it means to be without it.
    without: &'static [u8],
}

/// A slot list: `from[i]` into slot i, nothing in the rest.
fn slots(from: &[u8]) -> [u8; MAX_CAPS] {
    let mut list = [NO_SLOT; MAX_CAPS];
    list[..from.len()].copy_from_slice(from);
    list
}

fn say(text: &[u8]) {
    freya::write(CONSOLE as u64, text);
}

/// Start `d`; Ok(id), or the error.
fn start(d: &mut Driver) -> Result<u64, i64> {
    let result = freya::spawn(FACTORY, d.program, 0, &d.slots, OWN);
    if result < 0 {
        d.id = None;
        return Err(result);
    }
    d.id = Some(result as u64);
    Ok(result as u64)
}

fn couldnt_start(d: &Driver, error: i64, again: bool) {
    let mut line = Line::<200>::new();
    line.str(b"couldn't start the ").str(d.name).str(if again { b" again" } else { b"" });
    line.str(b" (error -").dec(error.unsigned_abs()).str(b"): ").str(d.without).str(b"\n");
    say(line.as_bytes());
}

/// A copy of the capability in `slot` (an endpoint's or, since Entry 24,
/// a Factory) with `rights` and `badge`, in a slot spawn's list can name;
/// NO_SLOT if it couldn't be made (and then spawning a driver that needs
/// it fails, and says so).
fn minted(slot: u8, rights: u64, badge: u64) -> u8 {
    if slot == NO_SLOT {
        return NO_SLOT;
    }
    match freya::mint(slot as u64, rights, badge) {
        copy if copy >= 0 => copy as u8,
        error => {
            let mut line = Line::<96>::new();
            line.str(b"couldn't copy a capability (error -").dec(error.unsigned_abs()).str(b")\n");
            say(line.as_bytes());
            NO_SLOT
        }
    }
}

/// A new endpoint, in a slot spawn's list can name; NO_SLOT if it
/// couldn't be made (and then what needs it can't start, and says so).
fn new_endpoint(whose: &[u8]) -> u8 {
    match freya::endpoint(FACTORY) {
        slot if slot >= 0 => slot as u8,
        _ => {
            let mut line = Line::<96>::new();
            line.str(b"couldn't make the ").str(whose).str(b" endpoint\n");
            say(line.as_bytes());
            NO_SLOT
        }
    }
}

fn main(_arg: u64) -> u64 {
    let ps2_ep = new_endpoint(b"PS/2 driver's");
    let mouse_ep = new_endpoint(b"mouse driver's");
    let shell_ep = new_endpoint(b"shell's");
    let console_recv = minted(CONSOLE_EP as u8, RECV, 0);
    let ps2_to_console = minted(CONSOLE_EP as u8, SEND, PS2_TO_CONSOLE);
    let mouse_to_console = minted(CONSOLE_EP as u8, SEND, MOUSE_TO_CONSOLE);
    let shell_to_console = minted(CONSOLE_EP as u8, SEND, SHELL_TO_CONSOLE);
    let ps2_recv = minted(ps2_ep, RECV, 0);
    let mouse_to_ps2 = minted(ps2_ep, SEND, MOUSE_TO_PS2);
    let shell_to_ps2 = minted(ps2_ep, SEND, SHELL_TO_PS2);
    let mouse_recv = minted(mouse_ep, RECV, 0);
    let bytes_for_mouse = minted(mouse_ep, SEND, SIGNAL_BYTES);
    let shell_recv = minted(shell_ep, RECV, 0);
    let keys_for_shell = minted(shell_ep, SEND, SIGNAL_KEYS);
    let redraw_for_shell = minted(shell_ep, SEND, SIGNAL_REDRAW);
    let exits_for_shell = minted(shell_ep, SEND, 0);
    let programs_for_shell = (1 << freya::PROGRAM_HELLO)
        | (1 << freya::PROGRAM_SPIN)
        | (1 << freya::PROGRAM_CRASH)
        | (1 << freya::PROGRAM_TICKER)
        | (1 << freya::PROGRAM_RELAY);
    let factory_for_shell = minted(FACTORY as u8, programs_for_shell, 0);
    // Everything is minted: the full copies go (see the top of this file).
    for full in [CONSOLE_EP as u8, ps2_ep, mouse_ep, shell_ep] {
        if full != NO_SLOT {
            freya::drop_cap(full as u64);
        }
    }

    let mut drivers = [
        Driver {
            name: b"console driver",
            program: freya::PROGRAM_CONSOLE,
            slots: slots(&[CONSOLE, console_recv, SCREEN, LOG, PANEL, DESCRIPTION]),
            id: None,
            restarts: 0,
            drop_when_stopped: [console_recv, NO_SLOT],
            without: b"the kernel keeps drawing the screen",
        },
        Driver {
            name: b"PS/2 driver",
            program: freya::PROGRAM_PS2,
            slots: slots(&[
                CONSOLE,
                KEYBOARD_IRQ,
                KEYBOARD_DATA,
                KEYBOARD_STATUS,
                POWER,
                ps2_to_console,
                ps2_recv,
                MOUSE_IRQ,
                bytes_for_mouse,
                keys_for_shell,
            ]),
            id: None,
            restarts: 0,
            drop_when_stopped: [ps2_recv, NO_SLOT],
            without: b"no keyboard: to switch off, hold the power button (safe: nothing is written to disk)",
        },
        Driver {
            name: b"mouse driver",
            program: freya::PROGRAM_MOUSE,
            slots: slots(&[CONSOLE, mouse_to_ps2, mouse_to_console, mouse_recv]),
            id: None,
            restarts: 0,
            drop_when_stopped: [mouse_recv, NO_SLOT],
            without: b"no pointer",
        },
        Driver {
            name: b"shell",
            program: freya::PROGRAM_SHELL,
            slots: slots(&[CONSOLE, shell_recv, shell_to_ps2, shell_to_console, INFO, POWER, factory_for_shell, exits_for_shell]),
            id: None,
            restarts: 0,
            drop_when_stopped: [shell_recv, NO_SLOT],
            without: b"no shell: Ctrl+Alt+Del still restarts and Ctrl+Alt+End switches off",
        },
    ];

    // One line for everything started, so it reads as one step.
    let mut line = Line::<200>::new();
    line.str(b"started");
    let mut started = 0;
    let last = drivers.len() - 1;
    for (i, d) in drivers.iter_mut().enumerate() {
        match start(d) {
            Ok(id) => {
                if started > 0 {
                    line.str(if i == last { b" and" } else { b"," });
                }
                line.str(b" the ").str(d.name).str(b" (process ").dec(id).str(b")");
                started += 1;
            }
            Err(e) => couldnt_start(d, e, false),
        }
    }
    if started > 0 {
        line.str(b"\n");
        say(line.as_bytes());
    }

    // Wait for drivers to end, and start them again.
    loop {
        let msg = match freya::recv(OWN) {
            Ok(m) => m,
            Err(e) => return e as u64,
        };
        if msg.badge != SIGNAL_BADGE || msg.cap_slot != NO_CAP {
            continue; // nobody else has this endpoint; ignore anything odd
        }
        let ended = msg.words[0];
        for (i, d) in drivers.iter_mut().enumerate() {
            let Some(id) = d.id else { continue };
            if ended & (1 << id) == 0 {
                continue;
            }
            let mut line = Line::<200>::new();
            line.str(b"the ").str(d.name).str(b" (process ").dec(id).str(b") ended");
            if d.restarts >= MAX_RESTARTS {
                d.id = None;
                line.str(b"; started again ").dec(MAX_RESTARTS as u64).str(b" times already, so it stays stopped: ");
                line.str(d.without).str(b"\n");
                say(line.as_bytes());
                for &slot in d.drop_when_stopped.iter().filter(|&&s| s != NO_SLOT) {
                    freya::drop_cap(slot as u64);
                }
                continue;
            }
            d.restarts += 1;
            match start(d) {
                Ok(new) => {
                    line.str(b": started it again as process ").dec(new);
                    line.str(b" (").dec(d.restarts as u64).str(b" of ").dec(MAX_RESTARTS as u64).str(b")\n");
                    say(line.as_bytes());
                    if i == CONSOLE_DRIVER && redraw_for_shell != NO_SLOT {
                        // A new console driver has a blank input row: the
                        // shell sends the line being typed again.
                        freya::signal(redraw_for_shell as u64);
                    }
                }
                Err(e) => {
                    line.str(b"\n");
                    say(line.as_bytes());
                    couldnt_start(d, e, true);
                }
            }
        }
    }
}
