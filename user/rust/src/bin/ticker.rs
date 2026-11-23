//! ticker — a line a second, for as long as it holds the console (Entry 25).
//!
//! Slot 0: the console, lent to it (by the shell, or by a relay). Each
//! second it writes "ticker: tick N". When the console is taken back
//! (revoke), the write is refused, and ticker ends with that refusal as
//! its exit code: no-capability (-2). Nothing else stops it but kill.
//!
//! Its argument: bits 0 to 15 the ticks between lines (0: 100, a second);
//! TICKER_QUIET: write nothing, and instead look each time whether slot 0
//! still holds the console (the kernel's self-tests run it this way, so
//! they don't fill the log).

#![no_std]
#![no_main]

use freya::*;

freya::entry!(main);

const CONSOLE: u64 = 0;

fn main(arg: u64) -> u64 {
    let period = match arg & 0xFFFF {
        0 => 100,
        p => p,
    };
    let quiet = arg & TICKER_QUIET != 0;
    let mut n: u64 = 1;
    loop {
        if quiet {
            if !holds(CONSOLE, KIND_CONSOLE) {
                return ERR_NO_CAPABILITY as u64;
            }
        } else {
            let mut line = Line::<40>::new();
            line.str(b"ticker: tick ").dec(n).str(b"\n");
            let written = write(CONSOLE, line.as_bytes());
            if written < 0 {
                return written as u64; // the console was taken back
            }
        }
        n += 1;
        sleep(period);
    }
}
