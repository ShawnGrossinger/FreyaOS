//! process.rs — user processes: a task with its own address space whose
//! code runs in ring 3.
//!
//! A process is a sched task (so it is scheduled, preempted, joined and
//! reaped like any other) plus:
//!   - its own address space (vmm::AddressSpace::new_user), holding its
//!     code, data and stack, and sharing the kernel half (kernel-only);
//!   - a capability table (cap.rs): what it may do. It starts with exactly
//!     what its creator hands it and nothing else (Zero Ambient Authority),
//!     and gains more only through IPC (ipc.rs).
//!
//! Its task starts in the kernel like every task (task_entry -> start)
//! and drops to ring 3 with enter_user (entry.asm). From then on it comes
//! back to the kernel only through an interrupt, an exception, or a system
//! call (syscall.rs). A CPU exception in ring 3 ends the process, not the
//! kernel: idt.rs reports it and calls sched::exit_with(Fault(vector)).
//!
//! Programs are flat binaries built into the kernel image: there is no file
//! system to load them from yet. Most are assembled from user/*.asm by
//! build.rs; since Entry 20 some are written in Rust (user/rust, built by
//! build/Makefile). Layout of every process (user/freya.inc and
//! user/rust/user.ld say the same):
//!
//!   0x0000_0000_4000_0000  code, read + execute (at most 1 MiB)
//!   0x0000_0000_4010_0000  data, read + write, zeroed: one page for a NASM
//!                          program; for a Rust one as many as its header
//!                          asks (Program::data_pages)
//!   0x0000_0001_0000_0000  where memory capabilities may be mapped
//!   to 0x0000_7F00_0000_0000  (system call map, mem.rs)
//!   0x0000_7FFF_0000_0000  top of the stack, read + write (16 KiB below)

use crate::cap::{Caps, CONSOLE_ONLY, NO_CAPS};
use crate::pmm::{self, PAGE_SIZE};
use crate::sched::{self, ExitStatus, SpawnError, TaskId};
use crate::vmm::{AddressSpace, PageFlags};

pub const USER_CODE_BASE: u64 = crate::vmm::USER_BASE;
const MAX_CODE_BYTES: u64 = 0x10_0000;
pub const USER_DATA_BASE: u64 = USER_CODE_BASE + MAX_CODE_BYTES;
pub const USER_DATA_PAGES: u64 = 1;
/// Most data pages a program may ask for (1 MiB).
const MAX_DATA_PAGES: u64 = 256;
pub const USER_STACK_TOP: u64 = 0x0000_7FFF_0000_0000;
pub const USER_STACK_PAGES: u64 = 4;

// ===========================================================================
// Programs
// ===========================================================================

pub struct Program {
    pub name: &'static str,
    pub image: &'static [u8],
}

impl Program {
    /// Data pages to map from USER_DATA_BASE. A Rust program says how many
    /// in its header ("FREYAPRG" at byte 8, then the count: user/rust/src/
    /// lib.rs); a NASM program has none and gets USER_DATA_PAGES.
    pub fn data_pages(&self) -> u64 {
        match self.image.get(16..24) {
            Some(n) if self.has_header() => u64::from_le_bytes([n[0], n[1], n[2], n[3], n[4], n[5], n[6], n[7]]),
            _ => USER_DATA_PAGES,
        }
    }

    /// A Rust program's header.
    fn has_header(&self) -> bool {
        matches!(self.image.get(8..16), Some(magic) if magic == b"FREYAPRG")
    }
}

/// Prints one line with the console capability in slot 0, then exits with
/// what write returned.
pub static HELLO: Program = Program {
    name: "hello",
    image: include_bytes!(concat!(env!("OUT_DIR"), "/hello.bin")),
};

/// IPC test program: its argument picks a role (user/ipc.asm).
pub static IPC: Program = Program {
    name: "ipc",
    image: include_bytes!(concat!(env!("OUT_DIR"), "/ipc.bin")),
};

/// A user-space driver in miniature (user/driver.asm).
pub static DRIVER: Program = Program {
    name: "driver",
    image: include_bytes!(concat!(env!("OUT_DIR"), "/driver.bin")),
};

/// The PS/2 controller driver: the keyboard, and the mouse's bytes
/// (user/rust/src/bin/ps2.rs, Entry 22; user/keyboard.asm before).
pub static PS2: Program = Program {
    name: "ps2",
    image: include_bytes!(concat!(env!("OUT_DIR"), "/ps2.bin")),
};

/// The mouse driver (user/rust/src/bin/mouse.rs, Entry 22).
pub static MOUSE: Program = Program {
    name: "mouse",
    image: include_bytes!(concat!(env!("OUT_DIR"), "/mouse.bin")),
};

/// Test program: its argument picks one thing to try (user/probe.asm).
pub static PROBE: Program = Program {
    name: "probe",
    image: include_bytes!(concat!(env!("OUT_DIR"), "/probe.bin")),
};

/// The console driver, written in Rust (user/rust/src/bin/console.rs).
pub static CONSOLE: Program = Program {
    name: "console",
    image: include_bytes!(concat!(env!("OUT_DIR"), "/console.bin")),
};

/// The first process, which starts the rest (user/rust/src/bin/init.rs).
pub static INIT: Program = Program {
    name: "init",
    image: include_bytes!(concat!(env!("OUT_DIR"), "/init.bin")),
};

/// Test program for spawn, endpoint and mint (user/rust/src/bin/captest.rs).
pub static CAPTEST: Program = Program {
    name: "captest",
    image: include_bytes!(concat!(env!("OUT_DIR"), "/captest.bin")),
};

// ===========================================================================
// Creating a process
// ===========================================================================

extern "C" {
    /// entry.asm: iretq to `rip` in ring 3 with stack `rsp`, `arg` in RDI.
    /// Reached by a jump from start(), with every other register all ones.
    fn enter_user(rip: u64, rsp: u64, arg: u64) -> !;
}

/// Start `program` as a new process with argument `arg` (in RDI) and
/// capability table `caps`. It runs when the scheduler picks it.
pub fn spawn(program: &Program, arg: u64, caps: Caps) -> Result<TaskId, SpawnError> {
    spawn_signalling(program, arg, caps, None)
}

/// spawn, and when the process ends signal endpoint `exit_signal` with bit
/// (1 << its id) (factory.rs).
pub fn spawn_signalling(program: &Program, arg: u64, caps: Caps, exit_signal: Option<u8>) -> Result<TaskId, SpawnError> {
    let data_pages = program.data_pages();
    // A Rust program with no zeroed statics asks for no data pages at all.
    if program.image.len() as u64 > MAX_CODE_BYTES || data_pages > MAX_DATA_PAGES {
        return Err(SpawnError::OutOfMemory);
    }
    let mut space = AddressSpace::new_user().map_err(|_| SpawnError::OutOfMemory)?;
    if unsafe { load(&mut space, program.image, data_pages) }.is_err() {
        unsafe { space.destroy() };
        return Err(SpawnError::OutOfMemory);
    }
    // The task owns the address space from here; reap() destroys it.
    sched::spawn_in_signalling(program.name, start, arg, space.root(), caps, exit_signal).inspect_err(|_| unsafe {
        AddressSpace::from_root(space.root()).destroy();
    })
}

/// Map code (copied in), data (zeroed) and a stack. Every frame mapped
/// belongs to the address space: if this fails part-way, destroy() frees
/// what was mapped.
unsafe fn load(space: &mut AddressSpace, image: &[u8], data_pages: u64) -> Result<(), ()> {
    let mut map_new = |va: u64, flags: PageFlags, src: &[u8]| -> Result<(), ()> {
        let frame = pmm::alloc_frame().ok_or(())? as u64; // arrives zeroed
        core::ptr::copy_nonoverlapping(src.as_ptr(), crate::paging::phys_to_virt(frame), src.len());
        space.map(va, frame, flags).map_err(|_| pmm::free_frame(frame as usize))
    };
    for (i, chunk) in image.chunks(PAGE_SIZE as usize).enumerate() {
        map_new(USER_CODE_BASE + i as u64 * PAGE_SIZE, PageFlags::USER_CODE, chunk)?;
    }
    for i in 0..data_pages {
        map_new(USER_DATA_BASE + i * PAGE_SIZE, PageFlags::USER_DATA, &[])?;
    }
    for i in 1..=USER_STACK_PAGES {
        map_new(USER_STACK_TOP - i * PAGE_SIZE, PageFlags::USER_DATA, &[])?;
    }
    Ok(())
}

/// The process's task starts here, in the kernel, already in its own
/// address space (the scheduler loaded it), and leaves for ring 3.
///
/// Every register is filled with ones first, so that if enter_user ever
/// forgets to clear one, the process sees all ones (and probe's ring test
/// fails) rather than a value that happened to be zero anyway. Clobbering
/// rbx and rbp is fine: this never returns.
fn start(arg: u64) {
    unsafe {
        core::arch::asm!(
            "pcmpeqd xmm0, xmm0",
            "pcmpeqd xmm1, xmm1",
            "pcmpeqd xmm2, xmm2",
            "pcmpeqd xmm3, xmm3",
            "pcmpeqd xmm4, xmm4",
            "pcmpeqd xmm5, xmm5",
            "pcmpeqd xmm6, xmm6",
            "pcmpeqd xmm7, xmm7",
            "pcmpeqd xmm8, xmm8",
            "pcmpeqd xmm9, xmm9",
            "pcmpeqd xmm10, xmm10",
            "pcmpeqd xmm11, xmm11",
            "pcmpeqd xmm12, xmm12",
            "pcmpeqd xmm13, xmm13",
            "pcmpeqd xmm14, xmm14",
            "pcmpeqd xmm15, xmm15",
            "mov rax, -1",
            "mov rbx, -1",
            "mov rcx, -1",
            "mov rbp, -1",
            "mov r8, -1",
            "mov r9, -1",
            "mov r10, -1",
            "mov r11, -1",
            "mov r12, -1",
            "mov r13, -1",
            "mov r14, -1",
            "mov r15, -1",
            "jmp {enter}",
            enter = sym enter_user,
            in("rdi") USER_CODE_BASE,
            in("rsi") USER_STACK_TOP,
            in("rdx") arg,
            options(noreturn),
        )
    }
}

/// Demo: kernel code reading a process's page (blocked by SMAP) or calling
/// into it (blocked by SMEP). Both halt with a kernel #PF report. Skipped,
/// with a message, on a CPU without the feature: there the access would
/// simply succeed, and running a process's code in ring 0 is exactly what
/// SMEP exists to prevent.
pub fn demo_kernel_touches_user(execute: bool) {
    let (smep, smap) = crate::cpu::smep_smap_enabled();
    if (execute && !smep) || (!execute && !smap) {
        s(if execute { b"[demo] this CPU has no SMEP; nothing to show\n" } else { b"[demo] this CPU has no SMAP; nothing to show\n" });
        return;
    }
    let Ok(mut space) = AddressSpace::new_user() else { return };
    if unsafe { load(&mut space, HELLO.image, USER_DATA_PAGES) }.is_err() {
        unsafe { space.destroy() };
        return;
    }
    let entry: fn(u64) = if execute { touch_by_calling } else { touch_by_reading };
    if let Ok(id) = sched::spawn_in("touch-demo", entry, 0, space.root(), NO_CAPS) {
        sched::join(id);
    }
}

/// Kernel code, running in a process's address space, reads its code page.
fn touch_by_reading(_: u64) {
    unsafe { core::ptr::read_volatile(USER_CODE_BASE as *const u64) };
}

/// Kernel code, running in a process's address space, calls its code.
fn touch_by_calling(_: u64) {
    let f: extern "C" fn() = unsafe { core::mem::transmute(USER_CODE_BASE as *const ()) };
    f();
}

/// Timer interrupt about to return to ring 3: if the process was killed,
/// this is where it stops.
pub fn on_return_to_user() {
    if sched::kill_requested() {
        sched::exit_with(ExitStatus::Killed);
    }
}

// ===========================================================================
// Self-test
// ===========================================================================

use crate::selftest::verdict;
use crate::syscall::{ERR_BAD_ADDRESS, ERR_BAD_SYSCALL, ERR_NO_CAPABILITY};
use crate::{serial_write_str as s, serial_write_u64 as dec};
use core::sync::atomic::{AtomicU64, Ordering};

// user/probe.asm modes (argument = mode | value << 8).
const RING: u64 = 0;
const READ_KERNEL: u64 = 1;
const READ_PHYSMAP: u64 = 2;
const WRITE_CODE: u64 = 3;
const EXEC_DATA: u64 = 4;
const CLI: u64 = 5;
const INT_TIMER_VECTOR: u64 = 6;
const PORT_IO: u64 = 7;
const WRITE_KERNEL_POINTER: u64 = 8;
const WRITE_PAST_END: u64 = 9;
const BAD_SYSCALL: u64 = 10;
const SPIN: u64 = 11;
const ISOLATE: u64 = 12;
const MXCSR: u64 = 13;
const MAP_READ: u64 = 14;
const MAP_WRITE: u64 = 15;
const MAP_TWICE: u64 = 16;
const MAP_THEN_PRINT: u64 = 17;

/// Every wait is bounded: a broken kernel fails the test instead of
/// hanging the boot.
const WAIT_TICKS: u64 = 500;

/// The id of a test process that started; a failed check if it didn't.
fn started(result: Result<TaskId, SpawnError>) -> Option<TaskId> {
    result.map_err(|_| crate::selftest::check(b"user", b"start a test process", false)).ok()
}

/// Run a process to completion; None if it couldn't start or didn't end.
/// Memory capabilities: a frame of the kernel's, with a known value in it,
/// mapped by probe processes in every way that should work and every way
/// that shouldn't. The frame must still be the kernel's at the end: a
/// process's address space must never free what it only borrowed.
fn selftest_memory() -> bool {
    use crate::cap::{table, Cap, MEM_WRITE};
    use crate::mem::{MAP_BASE, MAP_END};
    const PATTERN: u64 = 0x0123_4567_89AB_CDEF;
    const WRITTEN: u64 = 0x5EED_5EED_5EED_5EED; // what probe's map_write stores
    let Some(page) = (unsafe { pmm::alloc_frame() }) else {
        return crate::selftest::check(b"user", b"a frame for the memory capability tests", false);
    };
    let page = page as u64;
    let word = crate::paging::phys_to_virt(page) as *mut u64;
    unsafe { word.write_volatile(PATTERN) };
    let ro = Cap::Memory { base: page, pages: 1, flags: 0 };
    let rw = Cap::Memory { base: page, pages: 1, flags: MEM_WRITE };
    let at = MAP_BASE;

    let read = run(&PROBE, MAP_READ | at << 8, table(&[ro]));
    let write_ro = run(&PROBE, MAP_WRITE | at << 8, table(&[ro]));
    let write_rw = run(&PROBE, MAP_WRITE | at << 8, table(&[rw]));
    let written = unsafe { word.read_volatile() } == WRITTEN;
    let print = run(&PROBE, MAP_THEN_PRINT | at << 8, table(&[ro, Cap::Console]));
    let mut ok = crate::selftest::check(
        b"user",
        b"a Memory capability maps a kernel page where asked: read-only reads it and can't write it (#PF), writable writes it; write() won't print from it",
        read == code(PATTERN as i64) && write_ro == Some(ExitStatus::Fault(14)) && write_rw == code(0) && written && print == code(ERR_BAD_ADDRESS),
    );

    let refused = |arg: u64, caps: Caps| run(&PROBE, arg, caps) == code(ERR_BAD_ADDRESS);
    let no_cap = run(&PROBE, MAP_READ | at << 8, NO_CAPS) == code(ERR_NO_CAPABILITY);
    let console = run(&PROBE, MAP_READ | at << 8, CONSOLE_ONLY) == code(ERR_NO_CAPABILITY);
    let misaligned = refused(MAP_READ | (at + 8) << 8, table(&[ro]));
    let below = refused(MAP_READ | (USER_DATA_BASE + 0x10_0000) << 8, table(&[ro]));
    let past = refused(MAP_READ | MAP_END << 8, table(&[ro]));
    let twice = run(&PROBE, MAP_TWICE | at << 8, table(&[ro])) == code(ERR_BAD_ADDRESS);
    sched::reap();
    let kept = !pmm::is_frame_free(page) && unsafe { word.read_volatile() } == WRITTEN;
    ok &= crate::selftest::check(
        b"user",
        b"map refused with no capability or the console's, misaligned, below or past the window, or over a mapping; the page is still the kernel's after them all",
        no_cap && console && misaligned && below && past && twice && kept,
    );
    unsafe { pmm::free_frame(page as usize) };
    ok
}

fn run(program: &Program, arg: u64, caps: Caps) -> Option<ExitStatus> {
    let id = started(spawn(program, arg, caps))?;
    sched::join_timeout(id, WAIT_TICKS)
}

fn code(value: i64) -> Option<ExitStatus> {
    Some(ExitStatus::Code(value as u64))
}

fn status_name(status: Option<ExitStatus>) -> &'static [u8] {
    match status {
        Some(ExitStatus::Code(_)) => b"exited",
        Some(ExitStatus::Fault(14)) => b"#PF",
        Some(ExitStatus::Fault(13)) => b"#GP",
        Some(ExitStatus::Fault(_)) => b"other fault",
        Some(ExitStatus::Killed) => b"killed",
        None => b"did not finish",
    }
}

static FP_STEPS: AtomicU64 = AtomicU64::new(0);
static FP_SAME: AtomicU64 = AtomicU64::new(0);

/// Kernel task for the MXCSR test: floating-point work that yields between
/// chunks, so it runs while the process sits in its system calls with its
/// own MXCSR saved. Inheriting the process's MXCSR (every SSE exception
/// unmasked) would make the first inexact result fault (#XM) in the kernel.
fn fp_task(_: u64) {
    let mut state = (5u64, 0.0f64);
    let mut steps = 0;
    for _ in 0..50 {
        crate::apic::work(&mut state, 1_000);
        steps += 1_000;
        sched::yield_now();
    }
    // The reference: the same chunks (work() restarts its step counter on
    // every call), uninterrupted.
    let mut quiet = (5u64, 0.0f64);
    for _ in 0..50 {
        crate::cpu::without_interrupts(|| crate::apic::work(&mut quiet, 1_000));
    }
    FP_STEPS.store(steps, Ordering::Relaxed);
    FP_SAME.store((quiet.0 == state.0 && quiet.1.to_bits() == state.1.to_bits()) as u64, Ordering::Relaxed);
}

/// Runs on the boot thread with the scheduler going.
pub fn selftest() -> bool {
    let mut ok = true;
    sched::reap(); // so earlier tests' finished tasks don't skew the baseline
    let baseline = sched::free_frames_and_heap();
    let (_, bytes_before) = crate::syscall::counts();

    // 1. Ring 3, for real: the process reads its own CS and exits with its
    //    privilege level, having checked it started with every register
    //    clear (nothing of the kernel's leaks into it).
    let ring = run(&PROBE, RING, NO_CAPS);
    ok &= crate::selftest::check(
        b"user",
        b"a process starts in ring 3 (CS RPL 3) with every register clear but its argument",
        ring == code(3),
    );

    // 2. With the console capability, write works.
    let hello = run(&HELLO, 0, CONSOLE_ONLY);
    let (_, bytes_after) = crate::syscall::counts();
    let written = bytes_after - bytes_before;
    s(b"[user] self-test hello with the console capability wrote ");
    dec(written);
    s(b" bytes and exited with that count");
    ok &= verdict(written > 0 && hello == Some(ExitStatus::Code(written)));

    // 3. Without it, the same program is refused: no ambient authority.
    let refused = run(&HELLO, 0, NO_CAPS);
    let (_, bytes_now) = crate::syscall::counts();
    ok &= crate::selftest::check(
        b"user",
        b"the same program without the capability: write refused (no capability), nothing printed",
        refused == code(ERR_NO_CAPABILITY) && bytes_now == bytes_after,
    );

    // 3b. Memory capabilities (Entry 20): a page of the kernel's, lent.
    ok &= selftest_memory();

    // 4. Pointers are checked.
    let kernel_ptr = run(&PROBE, WRITE_KERNEL_POINTER, CONSOLE_ONLY);
    let past_end = run(&PROBE, WRITE_PAST_END, CONSOLE_ONLY);
    ok &= crate::selftest::check(
        b"user",
        b"write from a kernel address, or running past the process's memory: refused (bad address)",
        kernel_ptr == code(ERR_BAD_ADDRESS) && past_end == code(ERR_BAD_ADDRESS),
    );

    // 5. Unknown system calls are refused.
    let bad = run(&PROBE, BAD_SYSCALL, NO_CAPS);
    ok &= crate::selftest::check(b"user", b"an unknown system call returns bad-syscall", bad == code(ERR_BAD_SYSCALL));

    // 6. Memory protection: each of these kills only the process.
    let faults = [
        run(&PROBE, READ_KERNEL, NO_CAPS),
        run(&PROBE, READ_PHYSMAP, NO_CAPS),
        run(&PROBE, WRITE_CODE, NO_CAPS),
        run(&PROBE, EXEC_DATA, NO_CAPS),
    ];
    s(b"[user] self-test reading the kernel (identity map, physical map), writing its own code, running its data:");
    for status in faults {
        s(b" ");
        s(status_name(status));
    }
    ok &= verdict(faults.iter().all(|&f| f == Some(ExitStatus::Fault(14))));

    // 7. Privilege: cli, a software interrupt to a kernel vector, port I/O.
    let privileged = [
        run(&PROBE, CLI, NO_CAPS),
        run(&PROBE, INT_TIMER_VECTOR, NO_CAPS),
        run(&PROBE, PORT_IO, NO_CAPS),
    ];
    s(b"[user] self-test cli, int 0x30 and in from a port in ring 3:");
    for status in privileged {
        s(b" ");
        s(status_name(status));
    }
    ok &= verdict(privileged.iter().all(|&f| f == Some(ExitStatus::Fault(13))));

    // 8. A process that never gives up the CPU is preempted anyway, and can
    //    be stopped.
    let spin = started(spawn(&PROBE, SPIN, NO_CAPS));
    let (slices, killed, status) = match spin {
        Some(id) => {
            let deadline = crate::apic::ticks() + 200;
            while sched::slices(id) < 4 && crate::apic::ticks() < deadline {
                sched::sleep_ticks(1);
            }
            let slices = sched::slices(id);
            (slices, sched::kill(id), sched::join_timeout(id, WAIT_TICKS))
        }
        None => (0, false, None),
    };
    s(b"[user] self-test a process spinning in ring 3 was preempted ");
    dec(slices.saturating_sub(1));
    s(b" times, then ");
    s(status_name(status));
    ok &= verdict(slices >= 4 && killed && status == Some(ExitStatus::Killed));

    // 9. Two processes at the same addresses don't see each other: each
    //    stores its own value in its data page, registers and xmm0, yields
    //    50 times (interleaving with the other), and checks them all.
    let a = started(spawn(&PROBE, ISOLATE | 0x1111 << 8, NO_CAPS));
    let b = started(spawn(&PROBE, ISOLATE | 0x2222 << 8, NO_CAPS));
    let (sa, sb) = match (a, b) {
        (Some(a), Some(b)) => (sched::join_timeout(a, WAIT_TICKS), sched::join_timeout(b, WAIT_TICKS)),
        _ => (None, None),
    };
    ok &= crate::selftest::check(
        b"user",
        b"two processes at the same addresses keep their own memory, registers and SSE state across 50 system calls",
        sa == code(0x1111) && sb == code(0x2222),
    );

    // 10. A process's MXCSR is its own: it unmasks every SSE exception,
    //     while a kernel task does floating-point work in between its
    //     system calls.
    FP_SAME.store(0, Ordering::Relaxed);
    let p = started(spawn(&PROBE, MXCSR, NO_CAPS));
    let k = sched::spawn("fp-work", fp_task, 0).ok();
    let (sp, sk) = match (p, k) {
        (Some(p), Some(k)) => (sched::join_timeout(p, WAIT_TICKS), sched::join_timeout(k, WAIT_TICKS)),
        _ => (None, None),
    };
    ok &= crate::selftest::check(
        b"user",
        b"a process unmasking every SSE exception keeps that MXCSR to itself; kernel float work beside it is exact",
        sp == code(0) && sk == Some(ExitStatus::Code(0)) && FP_SAME.load(Ordering::Relaxed) == 1
            && FP_STEPS.load(Ordering::Relaxed) == 50_000,
    );

    // 11. Every process is gone and has given back everything.
    sched::reap();
    let leaked = baseline as i64 - sched::free_frames_and_heap() as i64;
    let (calls, _) = crate::syscall::counts();
    s(b"[user] self-test every process reaped: ");
    dec(sched::live_tasks() as u64);
    s(b" tasks and ");
    dec(crate::vmm::user_spaces());
    s(b" address spaces left, ");
    dec(leaked.unsigned_abs());
    s(if leaked < 0 { b" frames gained" } else { b" frames leaked" });
    s(b" (");
    dec(calls);
    s(b" system calls)");
    ok &= verdict(sched::live_tasks() == 0 && crate::vmm::user_spaces() == 0 && leaked == 0);

    ok
}
