//! mouse — the PS/2 mouse driver (Entry 22). On the T430 the mouse is the
//! TrackPoint and the touchpad: until an operating system asks for more,
//! both talk to the PS/2 controller's second port as one ordinary mouse.
//!
//! Capabilities init gives it:
//!   slot 0  Console     to say what it found
//!   slot 1  Endpoint    the PS/2 driver's (SEND, badge 2): the mouse's
//!                       bytes, and a way to send it bytes
//!   slot 2  Endpoint    the console driver's (SEND, badge 3): where the
//!                       pointer is drawn
//!
//! It owns no port and no interrupt: the PS/2 driver (src/bin/ps2.rs)
//! owns the controller and keeps the mouse's bytes until they are asked
//! for. This driver speaks the mouse's own protocol: it sets the defaults,
//! looks for a wheel (the IntelliMouse sequence: sample rates 200, 100,
//! 80, then the id: 3 or 4 means a wheel and 4-byte packets), switches
//! reporting on, then turns each packet into movement, buttons and wheel
//! for the console driver, which draws the pointer and scrolls (Entry 22).
//!
//! A packet: byte 0 holds the buttons (bit 0 left, 1 right, 2 middle), the
//! signs of x and y (bits 4, 5), overflow (bits 6, 7) and a bit that is
//! always set (bit 3), by which a lost byte is noticed and the next packet
//! found; bytes 1 and 2 are x and y (y up); a 4th, the wheel, in its low
//! four bits.
//!
//! If the PS/2 driver goes, this ends, and init starts it again.

#![no_std]
#![no_main]

use freya::*;

freya::entry!(main);

const CONSOLE: u64 = 0;
const PS2: u64 = 1;
const CONSOLE_EP: u64 = 2;

// The mouse's answers.
const ACK: u8 = 0xFA;
const RESEND: u8 = 0xFE;
const ERROR: u8 = 0xFC;

// Its commands.
const SET_SAMPLE_RATE: u8 = 0xF3;
const GET_ID: u8 = 0xF2;
const ENABLE_REPORTING: u8 = 0xF4;
const DISABLE_REPORTING: u8 = 0xF5;
const SET_DEFAULTS: u8 = 0xF6;

/// No ACK after resending (the mouse kept asking again, or said error).
const NO_ACK: i64 = -100;

/// The mouse's bytes, as the PS/2 driver hands them over.
struct Input {
    bytes: [u8; 24],
    next: usize,
    len: usize,
}

impl Input {
    /// The next byte from the mouse, waiting for one if need be.
    fn byte(&mut self) -> Result<u8, i64> {
        while self.next == self.len {
            let m = call(PS2, [PS2_READ, 0, 0, 0], NO_CAP)?;
            let n = m.words[0];
            if n > 24 {
                return Err(n as i64); // an error, not a count
            }
            for i in 0..n as usize {
                self.bytes[i] = (m.words[1 + i / 8] >> (8 * (i % 8))) as u8;
            }
            self.next = 0;
            self.len = n as usize;
        }
        let b = self.bytes[self.next];
        self.next += 1;
        Ok(b)
    }

    /// Send the mouse a byte and wait for its ACK. Bytes of a packet that
    /// was already on its way may come first; a RESEND means once more.
    fn send(&mut self, byte: u8) -> Result<(), i64> {
        for _ in 0..3 {
            let m = call(PS2, [PS2_SEND_TO_MOUSE, byte as u64, 0, 0], NO_CAP)?;
            if m.words[0] != 0 {
                return Err(m.words[0] as i64);
            }
            for _ in 0..8 {
                match self.byte()? {
                    ACK => return Ok(()),
                    RESEND => break,
                    ERROR => return Err(NO_ACK),
                    _ => {}
                }
            }
        }
        Err(NO_ACK)
    }

    fn sample_rate(&mut self, rate: u8) -> Result<(), i64> {
        self.send(SET_SAMPLE_RATE)?;
        self.send(rate)
    }
}

fn main(_arg: u64) -> u64 {
    let mut input = Input { bytes: [0; 24], next: 0, len: 0 };
    let wheel = match setup(&mut input) {
        Ok(w) => w,
        Err(e) => {
            let mut line = Line::<80>::new();
            line.str(b"mouse: no answer from the mouse (error -").dec(e.unsigned_abs()).str(b")\n");
            write(CONSOLE, line.as_bytes());
            return e as u64;
        }
    };
    write(
        CONSOLE,
        if wheel { b"mouse ready: 4-byte packets, with a wheel\n" } else { b"mouse ready: 3-byte packets, no wheel\n" },
    );
    match packets(&mut input, wheel) {
        Ok(()) => 0,
        Err(e) => e as u64,
    }
}

/// Defaults, the wheel if there is one, reporting on. Returns whether it
/// has a wheel.
fn setup(input: &mut Input) -> Result<bool, i64> {
    input.send(DISABLE_REPORTING)?; // whatever the firmware left running
    input.send(SET_DEFAULTS)?;
    input.sample_rate(200)?;
    input.sample_rate(100)?;
    input.sample_rate(80)?;
    input.send(GET_ID)?;
    let id = input.byte()?;
    input.sample_rate(100)?;
    input.send(ENABLE_REPORTING)?;
    Ok(id == 3 || id == 4)
}

/// Packets for ever: each one to the console driver.
fn packets(input: &mut Input, wheel: bool) -> Result<(), i64> {
    loop {
        let b0 = input.byte()?;
        if b0 & 0x08 == 0 {
            continue; // not the first byte of a packet: look for the next
        }
        let b1 = input.byte()?;
        let b2 = input.byte()?;
        let b3 = if wheel { input.byte()? } else { 0 };
        if b0 & 0xC0 != 0 {
            continue; // overflow: this movement can't be trusted
        }
        let dx = b1 as i64 - if b0 & 0x10 != 0 { 256 } else { 0 };
        let dy = b2 as i64 - if b0 & 0x20 != 0 { 256 } else { 0 };
        let z = (b3 & 0x0F) as i64;
        let z = if z >= 8 { z - 16 } else { z };
        let buttons = (b0 & 0x07) as u64 | ((z as u8) as u64) << 8;
        // If the console driver is gone for good the call fails at once,
        // and the mouse simply has nowhere to point.
        let _ = call(CONSOLE_EP, [CONSOLE_POINTER, dx as u64, dy as u64, buttons], NO_CAP);
    }
}
