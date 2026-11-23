//! hog — takes memory until its budget says no, and keeps checking it
//! (Entry 26).
//!
//! Slots: 0 Console, 1 Budget. Every tenth of a second it takes another
//! 64 KiB block from the heap (heap.rs grows it through the Budget) and
//! fills it with a pattern of its own; every 256 KiB it says how much it
//! holds. When the budget won't give more, it says so, and from then on
//! reads all of it back once a second: it must still be what was written.
//!
//! It never looks at whether the Budget is still there. If the Budget is
//! revoked, its pages are freed with it, and hog's next read is a page
//! fault that ends it: memory taken back is gone, not just marked.
//!
//! Its argument: bits 0 to 15 the ticks between blocks (0: 10), bit 16
//! quiet (freya::TICKER_QUIET): no lines (the self-tests use it).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use freya::{heap, sleep, write, Line, TICKER_QUIET};

const CONSOLE: u64 = 0;
const BUDGET: u64 = 1;
const BLOCK: usize = 64 * 1024;
/// 4 MiB: more than any budget the shell lends could fill (the table is
/// on the stack, which has 16 KiB).
const MOST: usize = 64;
/// What exit code says the memory changed under it.
const CHANGED: u64 = 0xBAD;

freya::entry!(main);

fn main(arg: u64) -> u64 {
    let period = match arg & 0xFFFF {
        0 => 10,
        p => p,
    };
    let quiet = arg & TICKER_QUIET != 0;
    if let Err(e) = heap::use_budget(BUDGET) {
        if !quiet {
            write(CONSOLE, b"hog: I was lent no budget, so I have no heap\n");
        }
        return e as u64;
    }
    // A fixed table of blocks, so that the heap holds nothing but them.
    let mut blocks: [Option<Vec<u8>>; MOST] = [const { None }; MOST];
    let mut held = 0;
    while held < MOST {
        let mut block = Vec::new();
        if block.try_reserve_exact(BLOCK).is_err() {
            break; // the budget won't give more
        }
        block.resize(BLOCK, pattern(held));
        blocks[held] = Some(block);
        held += 1;
        if !quiet && held % 4 == 0 {
            say(b"hog: holding ", held, b"");
        }
        sleep(period);
    }
    if !quiet {
        say(b"hog: my budget won't give more; holding ", held, b", and reading it back every second");
    }
    loop {
        for (i, block) in blocks.iter().flatten().enumerate() {
            if block.iter().any(|&b| b != pattern(i)) {
                return CHANGED;
            }
        }
        sleep(100);
    }
}

/// Block i's byte: never zero, so a page that came back zeroed shows.
fn pattern(i: usize) -> u8 {
    (i as u8).wrapping_mul(37) | 1
}

fn say(before: &[u8], blocks: usize, after: &[u8]) {
    let mut l = Line::<120>::new();
    l.str(before).dec((blocks * BLOCK / 1024) as u64).str(b" KiB").str(after).str(b"\n");
    write(CONSOLE, l.as_bytes());
}
