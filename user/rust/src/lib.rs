//! freya — the runtime of a FreyaOS user program written in Rust (Entry 20).
//!
//! Everything a program needs and nothing more: the entry point, the system
//! calls (the same numbers and registers as user/freya.inc and
//! kernel/src/syscall.rs), and a panic handler. No allocator, no files, no
//! threads: a program gets its memory from user.ld's fixed layout and its
//! authority from the capability slots its creator filled.
//!
//! A program is a binary in src/bin/ that names its main function with
//! `freya::entry!`. It starts with the argument its creator chose and every
//! other register clear; whatever main returns is its exit code.

#![no_std]

use core::arch::asm;

// ===========================================================================
// Entry point and program header
// ===========================================================================
//
// _start is the image's first byte (user.ld keeps .text.start first). A
// short jump skips the header, which the kernel reads before it maps the
// program (kernel/src/process.rs, Program::data_pages): "FREYAPRG" at byte
// 8, then how many data pages from 0x4010_0000 the program needs (its
// start block and .bss), computed by user.ld. NASM programs have no header
// and get one page.
core::arch::global_asm!(
    ".section .text.start, \"ax\"",
    ".global _start",
    "_start:",
    "    jmp 2f",
    "    .balign 8",
    "    .ascii \"FREYAPRG\"",
    "    .quad __data_pages",
    "2:",
    "    and rsp, -16",
    "    call freya_main",
    "    mov rdi, rax",
    "    xor eax, eax", // SYS_EXIT
    "    syscall",
    "    ud2",
);

/// `freya::entry!(main)`: `main(arg: u64) -> u64` is the program; its
/// result is the exit code.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[no_mangle]
        extern "C" fn freya_main(arg: u64) -> u64 {
            let main: fn(u64) -> u64 = $main;
            main(arg)
        }
    };
}

// ===========================================================================
// Layout (kernel/src/process.rs)
// ===========================================================================

/// The start block: two pages the kernel may fill before the program runs.
pub const START_BLOCK: usize = 0x4010_0000;
pub const START_BLOCK_BYTES: usize = 0x2000;
/// Where memory capabilities may be mapped (system call map): from 4 GiB up
/// to the bottom of the stack's region.
pub const MAP_BASE: u64 = 0x0000_0001_0000_0000;
pub const MAP_END: u64 = 0x0000_7F00_0000_0000;
pub const PAGE: u64 = 4096;

// ===========================================================================
// System calls
// ===========================================================================

pub const SYS_EXIT: u64 = 0;
pub const SYS_YIELD: u64 = 1;
pub const SYS_WRITE: u64 = 2;
pub const SYS_SEND: u64 = 3;
pub const SYS_RECV: u64 = 4;
pub const SYS_CALL: u64 = 5;
pub const SYS_REPLY: u64 = 6;
pub const SYS_PORT_IN: u64 = 7;
pub const SYS_PORT_OUT: u64 = 8;
pub const SYS_IRQ_WAIT: u64 = 9;
pub const SYS_IRQ_ACK: u64 = 10;
pub const SYS_POWER: u64 = 11;
// 12 was console_scroll (Entry 18); the console process scrolls now.
pub const SYS_MAP: u64 = 13;
pub const SYS_NOTE: u64 = 14;

pub const ERR_BAD_SYSCALL: i64 = -1;
pub const ERR_NO_CAPABILITY: i64 = -2;
pub const ERR_BAD_ADDRESS: i64 = -3;
pub const ERR_WRONG_CAPABILITY: i64 = -4;
pub const ERR_NO_GRANT: i64 = -5;
pub const ERR_PEER_GONE: i64 = -6;
pub const ERR_INTERRUPTED: i64 = -7;
pub const ERR_NO_REPLY: i64 = -8;
pub const ERR_BAD_ARGUMENT: i64 = -9;
pub const ERR_NO_MEMORY: i64 = -10;

/// No capability to pass along with a message.
pub const NO_CAP: u64 = u64::MAX;
/// The badge of a signal from the kernel (ipc.rs): words[0] holds the bits.
pub const SIGNAL_BADGE: u64 = u64::MAX;

#[inline(always)]
fn syscall3(number: u64, a: u64, b: u64, c: u64) -> i64 {
    let result: i64;
    // SAFETY: the kernel's system call ABI: RCX and R11 are destroyed,
    // every other register and the SSE state are kept.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number => result,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

pub fn exit(code: u64) -> ! {
    syscall3(SYS_EXIT, code, 0, 0);
    unreachable!()
}

pub fn yield_now() {
    syscall3(SYS_YIELD, 0, 0, 0);
}

/// Print `bytes` to the system log through the Console capability in
/// `slot` (at most 256 bytes a call; returns how many were written).
pub fn write(slot: u64, bytes: &[u8]) -> i64 {
    syscall3(SYS_WRITE, slot, bytes.as_ptr() as u64, bytes.len() as u64)
}

/// Like write, but to the serial port only, not the system log: notes for
/// test.sh that shouldn't appear on screen.
pub fn note(slot: u64, bytes: &[u8]) -> i64 {
    syscall3(SYS_NOTE, slot, bytes.as_ptr() as u64, bytes.len() as u64)
}

/// Map the Memory capability in `slot` at `addr` (page-aligned, inside
/// [MAP_BASE, MAP_END)).
pub fn map(slot: u64, addr: u64) -> i64 {
    syscall3(SYS_MAP, slot, addr, 0)
}

#[derive(Clone, Copy, Default)]
pub struct Message {
    /// Who it came from: the badge of the capability it was sent through,
    /// or SIGNAL_BADGE for a signal from the kernel.
    pub badge: u64,
    pub words: [u64; 4],
    /// Where a capability that came with it landed, or NO_CAP.
    pub cap_slot: u64,
}

/// Wait for a message (or a signal) on the endpoint capability in `slot`.
pub fn recv(slot: u64) -> Result<Message, i64> {
    let (result, badge, w0, w1, w2, w3, cap_slot): (i64, u64, u64, u64, u64, u64, u64);
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_RECV => result,
            inlateout("rdi") slot => badge,
            lateout("rsi") w0,
            lateout("rdx") w1,
            lateout("r10") w2,
            lateout("r8") w3,
            lateout("r9") cap_slot,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if result < 0 {
        return Err(result);
    }
    Ok(Message { badge, words: [w0, w1, w2, w3], cap_slot })
}

/// Send through `slot` and wait for the answer.
pub fn call(slot: u64, words: [u64; 4], xfer: u64) -> Result<Message, i64> {
    let (result, badge, w0, w1, w2, w3, cap_slot): (i64, u64, u64, u64, u64, u64, u64);
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_CALL => result,
            inlateout("rdi") slot => badge,
            inlateout("rsi") words[0] => w0,
            inlateout("rdx") words[1] => w1,
            inlateout("r10") words[2] => w2,
            inlateout("r8") words[3] => w3,
            inlateout("r9") xfer => cap_slot,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if result < 0 {
        return Err(result);
    }
    Ok(Message { badge, words: [w0, w1, w2, w3], cap_slot })
}

/// Answer the last call received.
pub fn reply(words: [u64; 4], xfer: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_REPLY => result,
            in("rsi") words[0],
            in("rdx") words[1],
            in("r10") words[2],
            in("r8") words[3],
            in("r9") xfer,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

// ===========================================================================
// Text without core::fmt (it would triple a small program's size)
// ===========================================================================

/// A line of text built in place: strings and numbers, cut at N bytes.
pub struct Line<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> Line<N> {
    pub const fn new() -> Self {
        Line { buf: [0; N], len: 0 }
    }
    pub fn str(&mut self, s: &[u8]) -> &mut Self {
        for &b in s {
            if self.len < N {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
        self
    }
    pub fn dec(&mut self, mut v: u64) -> &mut Self {
        let mut digits = [0u8; 20];
        let mut n = 0;
        loop {
            digits[n] = b'0' + (v % 10) as u8;
            n += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        while n > 0 {
            n -= 1;
            self.str(&[digits[n]]);
        }
        self
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

// ===========================================================================
// Panic
// ===========================================================================

/// A Rust panic in a user program: say so through slot 0 (if it holds the
/// console; the kernel refuses otherwise) and exit with 0xDEAD.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let mut line = Line::<96>::new();
    line.str(b"panic");
    if let Some(at) = info.location() {
        line.str(b" at ").str(at.file().as_bytes()).str(b":").dec(at.line() as u64);
    }
    line.str(b"\n");
    write(0, line.as_bytes());
    exit(0xDEAD)
}
