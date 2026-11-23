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
// .bss), computed by user.ld. NASM programs have no header and get one
// page.
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
pub const SYS_SPAWN: u64 = 15;
pub const SYS_ENDPOINT: u64 = 16;
pub const SYS_MINT: u64 = 17;
pub const SYS_DROP: u64 = 18;
pub const SYS_IRQ_BIND: u64 = 19;
pub const SYS_SIGNAL: u64 = 20;
pub const SYS_INFO: u64 = 21;
pub const SYS_CAP_INFO: u64 = 22;
pub const SYS_KILL: u64 = 23;
pub const SYS_STATUS: u64 = 24;

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
pub const ERR_NO_SLOT: i64 = -11;

/// Capability slots per process.
pub const MAX_CAPS: usize = 32;
/// Endpoint rights (mint).
pub const SEND: u64 = 1 << 0;
pub const RECV: u64 = 1 << 1;
pub const GRANT: u64 = 1 << 2;

/// The programs spawn can start (kernel/src/factory.rs: PROGRAMS).
pub const PROGRAM_HELLO: u64 = 0;
pub const PROGRAM_CONSOLE: u64 = 1;
pub const PROGRAM_PS2: u64 = 2;
pub const PROGRAM_MOUSE: u64 = 3;
pub const PROGRAM_SHELL: u64 = 4;
pub const PROGRAM_SPIN: u64 = 5;
pub const PROGRAM_CRASH: u64 = 6;
/// The programs' names, by number (kernel/src/process.rs).
pub const PROGRAM_NAMES: [&[u8]; 7] = [b"hello", b"console", b"ps2", b"mouse", b"shell", b"spin", b"crash"];

/// A Factory's rights (Entry 24): bit n lets it start program n; this one
/// lets it make endpoints. mint makes a copy with fewer.
pub const FACTORY_ENDPOINTS: u64 = 1 << 16;

// ===========================================================================
// Driver protocols: the first word of a call says what is asked
// ===========================================================================

/// To the console driver (src/bin/console.rs). SCROLL: word 1 is the
/// action; the answer is how many rows back the view is.
pub const CONSOLE_SCROLL: u64 = 1;
pub const SCROLL_BACK: u64 = 0;
pub const SCROLL_FORWARD: u64 = 1;
pub const SCROLL_TO_END: u64 = 2;
/// POINTER (Entry 22): words 1 and 2 are the mouse's movement (x right, y
/// up, as signed numbers), word 3 its buttons (bit 0 left, 1 right, 2
/// middle) and, in bits 8 to 15, the wheel (signed: negative is up).
pub const CONSOLE_POINTER: u64 = 2;
/// INPUT (Entry 23): the line being typed, shown on the screen's last row.
/// Word 1: an offset into the line (bits 0 to 15) and the line's whole
/// length (bits 16 to 31); words 2 and 3: the 16 bytes of the line from
/// that offset (as many as it has). The shell sends the whole line this
/// way, 16 bytes a call, each time it changes.
pub const CONSOLE_INPUT: u64 = 3;
pub const INPUT_CHUNK: usize = 16;

/// To the PS/2 driver (src/bin/ps2.rs), from the mouse driver. READ:
/// answered, at once or when some arrive, with the mouse's bytes: word 0
/// the count (up to 24), words 1 to 3 the bytes, eight to a word, first in
/// the lowest. SEND_TO_MOUSE: word 1 is a byte for the mouse; its answer
/// (0xFA, ...) arrives among the bytes READ returns.
pub const PS2_READ: u64 = 1;
pub const PS2_SEND_TO_MOUSE: u64 = 2;
/// READ_KEYS (Entry 23), from the shell: answered at once with the keys
/// typed since the last READ_KEYS, packed as READ packs bytes (up to 24).
/// Since Entry 23 the PS/2 driver answers every call at once, READ with
/// what it has, even nothing: it signals the mouse driver and the shell
/// when there is something to read (SIGNAL_BYTES, SIGNAL_KEYS).
pub const PS2_READ_KEYS: u64 = 3;
/// Most bytes or keys one READ or READ_KEYS answer holds.
pub const READ_MAX: usize = 24;

/// The keys READ_KEYS hands over: printable ASCII as typed, and these.
pub const KEY_ENTER: u8 = b'\n';
pub const KEY_BACKSPACE: u8 = 0x08;
pub const KEY_ESCAPE: u8 = 0x1B;
pub const KEY_UP: u8 = 0x80;
pub const KEY_DOWN: u8 = 0x81;

/// Signal bits init's capabilities set (system call signal, Entry 23). On
/// the shell's endpoint: keys are waiting (from the PS/2 driver); the
/// console driver started again, so the line being typed must be sent
/// again (from init). On the mouse driver's: its bytes are waiting. No
/// process id is 0 or 1, so the kernel's exit signals (bit 1 << id) never
/// use these bits.
pub const SIGNAL_KEYS: u64 = 1 << 0;
pub const SIGNAL_REDRAW: u64 = 1 << 1;
pub const SIGNAL_BYTES: u64 = 1 << 0;
/// "No capability here" in spawn's slot list.
pub const NO_SLOT: u8 = 0xFF;

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

/// Start built-in program `program` with `arg`, using the Factory
/// capability in `factory` (which must cover it). `slots[i]` is the
/// caller's slot to copy into the new process's slot i (NO_SLOT for none).
/// If `exit_ep` is an endpoint capability (not NO_CAP), it is signalled
/// with bit (1 << id) when the new process ends. Returns its id.
pub fn spawn(factory: u64, program: u64, arg: u64, slots: &[u8; MAX_CAPS], exit_ep: u64) -> i64 {
    spawn_raw(factory, program, arg, slots.as_ptr() as u64, exit_ep, 0).0
}

/// spawn, and a Process capability for the new process (Entry 24), to ask
/// how it is doing (status) and stop it (kill): Ok((its id, the slot the
/// capability landed in)).
pub fn spawn_process(factory: u64, program: u64, arg: u64, slots: &[u8; MAX_CAPS], exit_ep: u64) -> Result<(u64, u64), i64> {
    match spawn_raw(factory, program, arg, slots.as_ptr() as u64, exit_ep, SPAWN_PROCESS_CAP) {
        (id, slot) if id >= 0 => Ok((id as u64, slot)),
        (e, _) => Err(e),
    }
}

/// spawn's flags: hand back a Process capability (in R9).
pub const SPAWN_PROCESS_CAP: u64 = 1 << 0;

/// spawn, with the slot list at any address and any flags (the self-tests
/// pass bad ones): (the id or an error, the Process capability's slot).
pub fn spawn_raw(factory: u64, program: u64, arg: u64, slots: u64, exit_ep: u64, flags: u64) -> (i64, u64) {
    let (result, cap_slot): (i64, u64);
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_SPAWN => result,
            in("rdi") factory,
            in("rsi") program,
            in("rdx") arg,
            in("r10") slots,
            in("r8") exit_ep,
            inlateout("r9") flags => cap_slot,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    (result, cap_slot)
}

/// Stop the process the Process capability in `slot` names (Entry 24).
/// Peer-gone if it has already ended.
pub fn kill(slot: u64) -> i64 {
    syscall3(SYS_KILL, slot, 0, 0)
}

/// How the process the Process capability in `slot` names is doing:
/// Ok((STATUS_RUNNING, 0)), or how it ended, with its exit code or the
/// CPU exception's vector. Peer-gone once its task slot holds another
/// process.
pub fn status(slot: u64) -> Result<(i64, u64), i64> {
    let (how, value): (i64, u64);
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_STATUS => how,
            inlateout("rdi") slot => value,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if how < 0 {
        return Err(how);
    }
    Ok((how, value))
}
pub const STATUS_RUNNING: i64 = 0;
pub const STATUS_EXITED: i64 = 1;
pub const STATUS_FAULT: i64 = 2;
pub const STATUS_KILLED: i64 = 3;

/// A new endpoint (SEND, RECV and GRANT, no badge), made with the Factory
/// capability in `factory`. Returns the slot it landed in.
pub fn endpoint(factory: u64) -> i64 {
    syscall3(SYS_ENDPOINT, factory, 0, 0)
}

/// A copy of the endpoint capability in `slot` with `rights` (a subset of
/// its own) and `badge` (settable only on one without a badge). Returns
/// the slot it landed in.
pub fn mint(slot: u64, rights: u64, badge: u64) -> i64 {
    syscall3(SYS_MINT, slot, rights, badge)
}

/// The interrupt line in Irq slot `irq` signals the endpoint in slot
/// `endpoint` (one the caller receives on) with bit (1 << line).
pub fn irq_bind(irq: u64, endpoint: u64) -> i64 {
    syscall3(SYS_IRQ_BIND, irq, endpoint, 0)
}

/// Wait for the line in Irq slot `irq` to fire (a line not bound to an
/// endpoint).
pub fn irq_wait(irq: u64) -> i64 {
    syscall3(SYS_IRQ_WAIT, irq, 0, 0)
}

/// The line in Irq slot `irq` is ready for its next interrupt: unmask it
/// (arm it, before emptying the device; again after a level-triggered
/// interrupt).
pub fn irq_ack(irq: u64) -> i64 {
    syscall3(SYS_IRQ_ACK, irq, 0, 0)
}

/// Read one byte from I/O port `port` through the IoPorts capability in
/// `slot`; negative on error.
pub fn inb(slot: u64, port: u64) -> i64 {
    syscall3(SYS_PORT_IN, slot, port, 1)
}

/// Write one byte to I/O port `port` through the IoPorts capability in
/// `slot`.
pub fn outb(slot: u64, port: u64, value: u8) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_PORT_OUT => result,
            in("rdi") slot,
            in("rsi") port,
            in("rdx") 1u64,
            in("r10") value as u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Restart (POWER_RESTART) or switch off (POWER_OFF), with the Power
/// capability in `slot`. Returns only if refused.
pub fn power(slot: u64, action: u64) -> i64 {
    syscall3(SYS_POWER, slot, action, 0)
}
pub const POWER_RESTART: u64 = 0;
pub const POWER_OFF: u64 = 1;

/// Set the bits of the badge of the endpoint capability in `slot` (which
/// needs SEND) as signals for whoever receives there; never waits.
pub fn signal(slot: u64) -> i64 {
    syscall3(SYS_SIGNAL, slot, 0, 0)
}

/// Let go of the capability in `slot`.
pub fn drop_cap(slot: u64) -> i64 {
    syscall3(SYS_DROP, slot, 0, 0)
}

/// Send `words` through `slot` (and the capability in `xfer`, or NO_CAP);
/// returns once someone has received it.
pub fn send(slot: u64, words: [u64; 4], xfer: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_SEND => result,
            in("rdi") slot,
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
// What the system says about itself (Entry 23: kernel/src/info.rs)
// ===========================================================================

/// What cap_info says a slot holds.
pub const KIND_NONE: i64 = 0;
pub const KIND_CONSOLE: i64 = 1;
pub const KIND_ENDPOINT: i64 = 2;
pub const KIND_IO_PORTS: i64 = 3;
pub const KIND_IRQ: i64 = 4;
pub const KIND_POWER: i64 = 5;
pub const KIND_MEMORY: i64 = 6;
pub const KIND_FACTORY: i64 = 7;
pub const KIND_INFO: i64 = 8;
pub const KIND_PROCESS: i64 = 9;

/// The capability in the caller's own `slot`: Ok((kind, a, b, c)), where
/// an endpoint's a, b, c are its number, rights and badge; a port range's
/// its base and count; an interrupt's its line; memory's its pages and
/// flags; a Factory's its rights; a process's its task slot (its id) and
/// generation (Entry 24).
pub fn cap_info(slot: u64) -> Result<(i64, u64, u64, u64), i64> {
    let (kind, a, b, c): (i64, u64, u64, u64);
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_CAP_INFO => kind,
            inlateout("rdi") slot => a,
            lateout("rsi") b,
            lateout("rdx") c,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if kind < 0 {
        return Err(kind);
    }
    Ok((kind, a, b, c))
}

/// The snapshot's header, and one record per task after it.
pub const INFO_HEADER: usize = 64;
pub const INFO_RECORD: usize = 48;

/// What a task is doing (TaskInfo::what).
pub const TASK_RUNNING: u32 = 1;
pub const TASK_READY: u32 = 2;
pub const TASK_SLEEPING: u32 = 3;
pub const TASK_JOINING: u32 = 4;
pub const TASK_SENDING: u32 = 5;
pub const TASK_RECEIVING: u32 = 6;
pub const TASK_AWAITING_REPLY: u32 = 7;
pub const TASK_LOCK: u32 = 8;
pub const TASK_IRQ: u32 = 9;
pub const TASK_FINISHED: u32 = 10;
pub const TASK_STARTING: u32 = 11;

pub const TASK_FLAG_PROCESS: u32 = 1 << 0;
pub const TASK_FLAG_IDLE: u32 = 1 << 1;

pub struct SystemInfo {
    pub tasks: usize,
    pub ticks: u64,
    pub ticks_per_second: u64,
    pub free_frames: u64,
    pub usable_frames: u64,
    pub switches: u64,
    pub syscalls: u64,
}

pub struct TaskInfo {
    pub id: u32,
    pub what: u32,
    /// The endpoint, task, interrupt line or tick it waits for.
    pub detail: u64,
    pub name: [u8; 16],
    pub slices: u64,
    pub caps: u32,
    pub flags: u32,
}

impl TaskInfo {
    /// The name, without the zeros after it.
    pub fn name(&self) -> &[u8] {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(16);
        &self.name[..end]
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u32_at(b, at) as u64 | (u32_at(b, at + 4) as u64) << 32
}

/// A snapshot of the system into `buf`, with the Info capability in
/// `slot`: the header, then as many tasks as fit. Returns the header, or
/// the error.
pub fn info(slot: u64, buf: &mut [u8]) -> Result<SystemInfo, i64> {
    let n = syscall3(SYS_INFO, slot, buf.as_mut_ptr() as u64, buf.len() as u64);
    if n < 0 {
        return Err(n);
    }
    if n < INFO_HEADER as i64 || &buf[..8] != b"FREYAINF" || u32_at(buf, 8) != 1 {
        return Err(ERR_BAD_ARGUMENT);
    }
    Ok(SystemInfo {
        tasks: u32_at(buf, 12) as usize,
        ticks: u64_at(buf, 16),
        ticks_per_second: u64_at(buf, 24),
        free_frames: u64_at(buf, 32),
        usable_frames: u64_at(buf, 40),
        switches: u64_at(buf, 48),
        syscalls: u64_at(buf, 56),
    })
}

/// Raw info, for the self-tests that pass bad buffers.
pub fn info_raw(slot: u64, ptr: u64, len: u64) -> i64 {
    syscall3(SYS_INFO, slot, ptr, len)
}

/// Task record `i` of a snapshot info() wrote into `buf`.
pub fn task_info(buf: &[u8], i: usize) -> TaskInfo {
    let r = &buf[INFO_HEADER + i * INFO_RECORD..INFO_HEADER + (i + 1) * INFO_RECORD];
    let mut name = [0u8; 16];
    name.copy_from_slice(&r[16..32]);
    TaskInfo {
        id: u32_at(r, 0),
        what: u32_at(r, 4),
        detail: u64_at(r, 8),
        name,
        slices: u64_at(r, 32),
        caps: u32_at(r, 40),
        flags: u32_at(r, 44),
    }
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
