//! spin — runs until it is stopped (Entry 24).
//!
//! It never makes a system call after its first line, so it never gives the
//! CPU up of its own accord: the timer takes it away every 10 ms, and the
//! shell, the drivers and everything else go on as before. The shell's kill
//! stops it, through the Process capability spawn handed the shell; the
//! kernel ends it the next time it would go back to ring 3.
//!
//! Slot 0: the console (if it was given one), for its one line.

#![no_std]
#![no_main]

freya::entry!(main);

fn main(_arg: u64) -> u64 {
    freya::write(0, b"spin: running until stopped\n");
    let mut n: u64 = 0;
    loop {
        // Work the compiler can't throw away, and no system call.
        n = core::hint::black_box(n.wrapping_add(1));
    }
}
