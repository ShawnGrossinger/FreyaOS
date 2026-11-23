//! crash — tries to write into the kernel (Entry 24).
//!
//! The kernel's memory is mapped in every process's address space (the
//! upper half), but for the kernel only: a write from ring 3 is a page
//! fault (#PF, protection violation, user mode). The kernel ends this
//! process and nothing else, says so in the log, and whoever started it
//! learns it ended by vector 14 (status, through its Process capability).
//!
//! Slot 0: the console (if it was given one), to say what it is about to do.

#![no_std]
#![no_main]

freya::entry!(main);

/// The start of the kernel's image, through the kernel's map of all RAM.
const KERNEL: u64 = 0xFFFF_8000_0010_0000;

fn main(_arg: u64) -> u64 {
    freya::write(0, b"crash: writing to the kernel's memory\n");
    unsafe { core::ptr::write_volatile(KERNEL as *mut u64, 0) };
    // Never reached: the write faults.
    freya::write(0, b"crash: the write went through\n");
    1
}
