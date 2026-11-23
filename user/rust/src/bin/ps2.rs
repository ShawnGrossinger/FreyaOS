//! ps2 — the PS/2 controller driver: the keyboard, and the mouse's bytes
//! (Entry 22; the keys for the shell since Entry 23).
//!
//! The controller (the 8042) serves two devices through the same two
//! ports: the keyboard and, on its second port, the mouse (on the T430 the
//! TrackPoint and the touchpad). It has one output buffer, holding one byte
//! from either, and the status register says whose it is. So one driver
//! owns the controller: it turns the keyboard's scancodes into keys for
//! the shell (src/bin/shell.rs), and keeps the mouse's bytes for the mouse
//! driver (src/bin/mouse.rs). A few keys it acts on itself, so they work
//! whatever becomes of the shell: Ctrl+Alt+Del, Ctrl+Alt+End, PageUp and
//! PageDown.
//!
//! Capabilities init gives it:
//!   slot 0  Console        to say it is ready
//!   slot 1  Irq line 1     the keyboard's interrupt
//!   slot 2  IoPorts 0x60   the controller's data port
//!   slot 3  IoPorts 0x64   its status (read) and command (write) port
//!   slot 4  Power          to restart or switch off
//!   slot 5  Endpoint       the console driver's (SEND): PageUp, PageDown
//!   slot 6  Endpoint       its own (RECV): both lines' interrupts, as
//!                          signals, and the calls of the mouse driver and
//!                          the shell
//!   slot 7  Irq line 12    the mouse's interrupt
//!   slot 8  Endpoint       the mouse driver's (SEND, a badge that is the
//!                          signal SIGNAL_BYTES): to say bytes are waiting
//!   slot 9  Endpoint       the shell's (SEND, badge SIGNAL_KEYS): to say
//!                          keys are waiting
//!
//! It waits in one place, its endpoint (kernel/src/irq.rs: irq_bind): a
//! signal means a line fired, so it empties the controller, then signals
//! whoever has something new to read (system call signal, Entry 23). A
//! call asks for what has arrived, the mouse's bytes (READ) or keys
//! (READ_KEYS), and is answered at once, with nothing if nothing has: the
//! caller waits for the next signal. Or it sends the mouse a byte.
//!
//! It never waits for either of them, so one that is slow, stuck or gone
//! can't stop the keyboard: what they haven't read waits in a ring, and
//! the oldest goes if it fills. And it never holds a call: the kernel lets
//! a server hold only one at a time (ipc.rs), and before Entry 23 it held
//! the mouse driver's READ until bytes came, which a second client's call
//! would have taken the place of. Without line 12 the mouse's port stays
//! switched off: an unread mouse byte would sit in the one output buffer
//! and hold back every keystroke behind it.
//!
//! Both lines are edge-triggered: the controller raises one when a byte
//! arrives and keeps it up until the byte is read, and an edge that comes
//! while a line is masked is lost. So the lines are armed (irq_ack) before
//! the controller is emptied, never after (Entry 19).
//!
//!   Ctrl+Alt+Del   restart
//!   Ctrl+Alt+End   switch off
//!   PageUp/Down    scroll the screen's log back / forward a page
//!   everything else that is a key the shell knows (lib.rs: KEY_*): to the
//!   shell; with Ctrl or Alt held, nothing else is typed

#![no_std]
#![no_main]

use freya::*;

freya::entry!(main);

const CONSOLE: u64 = 0;
const KEYBOARD_IRQ: u64 = 1;
const DATA_CAP: u64 = 2;
const STATUS_CAP: u64 = 3;
const POWER_CAP: u64 = 4;
const CONSOLE_EP: u64 = 5;
const OWN: u64 = 6;
const MOUSE_IRQ: u64 = 7;
const MOUSE_SIGNAL: u64 = 8;
const SHELL_SIGNAL: u64 = 9;

/// Who is calling: the badges init mints on this driver's endpoint.
const MOUSE_BADGE: u64 = 2;
const SHELL_BADGE: u64 = 4;

const DATA_PORT: u64 = 0x60;
const STATUS_PORT: u64 = 0x64;

// Status register.
const OUTPUT_FULL: u8 = 0x01; // a byte is waiting at the data port
const INPUT_FULL: u8 = 0x02; // the controller hasn't taken the last write yet
const FROM_MOUSE: u8 = 0x20; // the waiting byte came from the mouse

// Controller commands (written to port 0x64).
const READ_CONFIG: u8 = 0x20;
const WRITE_CONFIG: u8 = 0x60;
const DISABLE_MOUSE_PORT: u8 = 0xA7;
const ENABLE_MOUSE_PORT: u8 = 0xA8;
const DISABLE_KEYBOARD_PORT: u8 = 0xAD;
const ENABLE_KEYBOARD_PORT: u8 = 0xAE;
const TO_MOUSE: u8 = 0xD4; // the next data byte goes to the mouse

// Configuration byte.
const CONFIG_KEYBOARD_IRQ: u8 = 0x01;
const CONFIG_MOUSE_IRQ: u8 = 0x02;
const CONFIG_KEYBOARD_CLOCK_OFF: u8 = 0x10;
const CONFIG_MOUSE_CLOCK_OFF: u8 = 0x20;
/// What to assume if the controller doesn't say: translation on (scancode
/// set 1, as the firmware sets it), the POST-passed flag, the keyboard's
/// interrupt.
const CONFIG_FALLBACK: u8 = 0x45;

/// Polls of the status register before giving up on the controller (each
/// a system call: a few tenths of a second in all).
const PATIENCE: u32 = 100_000;

fn fail(code: i64) -> ! {
    exit(code as u64)
}

fn status() -> u8 {
    let v = inb(STATUS_CAP, STATUS_PORT);
    if v < 0 {
        fail(v);
    }
    v as u8
}

fn data() -> u8 {
    let v = inb(DATA_CAP, DATA_PORT);
    if v < 0 {
        fail(v);
    }
    v as u8
}

/// Wait (bounded) until the controller takes a write.
fn ready_for_input() {
    for _ in 0..PATIENCE {
        if status() & INPUT_FULL == 0 {
            return;
        }
    }
}

fn command(c: u8) {
    ready_for_input();
    outb(STATUS_CAP, STATUS_PORT, c);
}

fn write_data(b: u8) {
    ready_for_input();
    outb(DATA_CAP, DATA_PORT, b);
}

/// Wait (bounded) for a byte, and read it; None if none came.
fn read_answer() -> Option<u8> {
    for _ in 0..PATIENCE {
        if status() & OUTPUT_FULL != 0 {
            return Some(data());
        }
    }
    None
}

/// Throw away whatever is waiting (at most 64 bytes).
fn flush() {
    for _ in 0..64 {
        if status() & OUTPUT_FULL == 0 {
            return;
        }
        data();
    }
}

// ---------------------------------------------------------------------------
// The mouse's bytes and the keys, kept until they are asked for
// ---------------------------------------------------------------------------

const RING: usize = 256;

struct Ring {
    bytes: [u8; RING],
    head: usize,
    len: usize,
}

impl Ring {
    fn push(&mut self, b: u8) {
        if self.len == RING {
            // Full: the oldest goes. The mouse driver finds the start of
            // the next packet again (bit 3 of a packet's first byte); a
            // shell this far behind has lost its keys either way.
            self.head = (self.head + 1) % RING;
            self.len -= 1;
        }
        self.bytes[(self.head + self.len) % RING] = b;
        self.len += 1;
    }

    /// Up to READ_MAX bytes, packed as READ and READ_KEYS answer them.
    fn take(&mut self) -> [u64; 4] {
        let n = self.len.min(READ_MAX);
        let mut words = [n as u64, 0, 0, 0];
        for i in 0..n {
            words[1 + i / 8] |= (self.bytes[self.head] as u64) << (8 * (i % 8));
            self.head = (self.head + 1) % RING;
        }
        self.len -= n;
        words
    }
}

// ---------------------------------------------------------------------------
// The keyboard (scancode set 1, US layout)
// ---------------------------------------------------------------------------

/// Scancode set 1 to ASCII for keys 0x00 to 0x39 (0: nothing to type).
const KEYS: &[u8; 0x3A] = b"\0\x001234567890-=\0\0qwertyuiop[]\0\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const SHIFTED_KEYS: &[u8; 0x3A] = b"\0\x00!@#$%^&*()_+\0\0QWERTYUIOP{}\0\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

struct Keyboard {
    ctrl: bool,
    alt: bool,
    shift: bool,
    /// The last byte was the 0xE0 prefix.
    extended: bool,
    /// Keys for the shell, not yet read.
    keys: Ring,
    /// Keys arrived since the shell was last told.
    new_keys: bool,
}

impl Keyboard {
    fn byte(&mut self, b: u8) {
        if b == 0xE0 {
            self.extended = true;
            return;
        }
        let key = b & 0x7F;
        let down = b & 0x80 == 0;
        match key {
            0x1D => self.ctrl = down,  // Ctrl (left, or right with E0)
            0x38 => self.alt = down,   // Alt (left, or right with E0)
            // E0 2A and E0 36 are fake shifts (Print Screen and friends).
            0x2A | 0x36 if !self.extended => self.shift = down,
            _ if down => self.press(key),
            _ => {} // other releases don't matter
        }
        self.extended = false;
    }

    fn press(&mut self, key: u8) {
        if self.ctrl && self.alt {
            match key {
                0x53 => return power_request(POWER_RESTART), // Del (or the keypad's)
                0x4F => return power_request(POWER_OFF),     // End
                _ => {}
            }
        }
        let typed = match key {
            0x49 => return scroll(SCROLL_BACK),    // PageUp (E0 49; 49 alone: the keypad's 9)
            0x51 => return scroll(SCROLL_FORWARD), // PageDown (E0 51; the keypad's 3)
            0x48 if self.extended => KEY_UP,
            0x50 if self.extended => KEY_DOWN,
            0x1C => KEY_ENTER,                     // Enter, or the keypad's (E0 1C)
            _ if self.extended => return,          // other arrows and the like
            0x01 => KEY_ESCAPE,
            0x0E => KEY_BACKSPACE,
            _ if self.ctrl || self.alt => return,  // kept for shortcuts
            _ => {
                let table = if self.shift { SHIFTED_KEYS } else { KEYS };
                match table.get(key as usize).copied().unwrap_or(0) {
                    0 => return,
                    c => c,
                }
            }
        };
        self.keys.push(typed);
        self.new_keys = true;
    }
}

/// A call to the console driver. If there is none (it ended for good, or
/// never started) the call fails at once with peer-gone and nothing
/// happens; while init starts it again, the call waits for it.
fn scroll(action: u64) {
    let _ = call(CONSOLE_EP, [CONSOLE_SCROLL, action, 0, 0], NO_CAP);
}

fn power_request(action: u64) {
    power(POWER_CAP, action); // returns only if refused
    write(CONSOLE, b"power request refused\n");
}

// ---------------------------------------------------------------------------
// Start, and the loop
// ---------------------------------------------------------------------------

fn main(_arg: u64) -> u64 {
    if irq_bind(KEYBOARD_IRQ, OWN) != 0 {
        write(CONSOLE, b"ps2: no keyboard interrupt to wait for\n");
        return 1;
    }
    let mouse = irq_bind(MOUSE_IRQ, OWN) == 0;

    // Quiet the controller while it is set up: both ports off, its output
    // emptied, then its configuration read and written back with the
    // interrupts and clocks wanted.
    command(DISABLE_KEYBOARD_PORT);
    command(DISABLE_MOUSE_PORT);
    flush();
    command(READ_CONFIG);
    let config = read_answer().unwrap_or(CONFIG_FALLBACK);
    let mut new = (config | CONFIG_KEYBOARD_IRQ) & !CONFIG_KEYBOARD_CLOCK_OFF;
    new = if mouse {
        (new | CONFIG_MOUSE_IRQ) & !CONFIG_MOUSE_CLOCK_OFF
    } else {
        (new & !CONFIG_MOUSE_IRQ) | CONFIG_MOUSE_CLOCK_OFF
    };
    command(WRITE_CONFIG);
    write_data(new);

    // Arm, then empty, then wait (Entry 19).
    irq_ack(KEYBOARD_IRQ);
    if mouse {
        irq_ack(MOUSE_IRQ);
    }
    command(ENABLE_KEYBOARD_PORT);
    if mouse {
        command(ENABLE_MOUSE_PORT);
    }
    flush();
    write(CONSOLE, b"keyboard ready: Ctrl+Alt+Del restart, Ctrl+Alt+End switch off, PgUp/PgDn scroll\n");
    if !mouse {
        write(CONSOLE, b"ps2: no interrupt line for the mouse: its port stays off\n");
    }

    let empty = Ring { bytes: [0; RING], head: 0, len: 0 };
    let mut keyboard = Keyboard { ctrl: false, alt: false, shift: false, extended: false, keys: empty, new_keys: false };
    let mut bytes = Ring { bytes: [0; RING], head: 0, len: 0 };

    loop {
        let msg = match recv(OWN) {
            Ok(m) => m,
            Err(e) => return e as u64,
        };
        if msg.badge == SIGNAL_BADGE {
            // A line fired: read everything waiting, whoever it is from.
            let mut new_bytes = false;
            loop {
                let s = status();
                if s & OUTPUT_FULL == 0 {
                    break;
                }
                let b = data();
                if s & FROM_MOUSE != 0 {
                    if mouse {
                        bytes.push(b);
                        new_bytes = true;
                    }
                } else {
                    keyboard.byte(b);
                }
            }
            irq_ack(KEYBOARD_IRQ); // again, for a level-triggered line
            if mouse {
                irq_ack(MOUSE_IRQ);
            }
            // Tell them; neither signal waits. If one of them is gone for
            // good the signal is refused (peer-gone), and that is all.
            if new_bytes {
                signal(MOUSE_SIGNAL);
            }
            if keyboard.new_keys {
                keyboard.new_keys = false;
                signal(SHELL_SIGNAL);
            }
            continue;
        }
        let answer = match (msg.badge, msg.words[0]) {
            (MOUSE_BADGE, PS2_READ) => bytes.take(),
            (MOUSE_BADGE, PS2_SEND_TO_MOUSE) if mouse => {
                command(TO_MOUSE);
                write_data(msg.words[1] as u8);
                [0; 4]
            }
            (SHELL_BADGE, PS2_READ_KEYS) => keyboard.keys.take(),
            _ => [ERR_BAD_ARGUMENT as u64, 0, 0, 0],
        };
        reply(answer, NO_CAP);
    }
}
