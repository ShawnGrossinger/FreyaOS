//! relay — lends on what it was lent (Entry 25).
//!
//! Slot 0: the console, lent to it. Slot 1: a Factory that starts only
//! ticker, lent to it too. It starts a ticker of its own with its console
//! in the ticker's slot 0: that copy is derived from relay's, which is
//! derived from the one its lender kept. So when the lender revokes what it
//! lent, the console goes from relay and from the ticker, in one call, and
//! both notice and end: two processes, one revoke, and relay never asked.
//!
//! It then waits, looking each period whether it still holds the console,
//! and ends with no-capability (-2) when it doesn't. Its argument is
//! ticker's (period, TICKER_QUIET), passed on to its ticker.

#![no_std]
#![no_main]

use freya::*;

freya::entry!(main);

const CONSOLE: u64 = 0;
const FACTORY: u64 = 1;

fn main(arg: u64) -> u64 {
    let period = match arg & 0xFFFF {
        0 => 100,
        p => p,
    };
    let quiet = arg & TICKER_QUIET != 0;
    let mut slots = [NO_SLOT; MAX_CAPS];
    slots[0] = CONSOLE as u8;
    let id = spawn(FACTORY, PROGRAM_TICKER, arg, &slots, NO_CAP);
    if !quiet {
        let mut line = Line::<80>::new();
        if id >= 0 {
            line.str(b"relay: started ticker as process ").dec(id as u64).str(b", with my console\n");
        } else {
            line.str(b"relay: couldn't start a ticker (error -").dec(id.unsigned_abs()).str(b")\n");
        }
        write(CONSOLE, line.as_bytes());
    }
    loop {
        sleep(period);
        if !holds(CONSOLE, KIND_CONSOLE) {
            return ERR_NO_CAPABILITY as u64;
        }
    }
}
