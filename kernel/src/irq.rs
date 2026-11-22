//! irq.rs — the driver model's kernel half: interrupt lines and I/O ports
//! handed to user-space drivers through capabilities.
//!
//! In FreyaOS a driver is an ordinary process. The kernel does exactly two
//! things for it, each only with a capability (cap.rs):
//!
//!   - I/O ports (Cap::IoPorts { base, count }): port_in / port_out on
//!     ports inside that range, done by the kernel on the driver's behalf.
//!     (Ring 3 can't touch ports itself: IOPL is 0 and the TSS has no I/O
//!     permission bitmap.) Memory-mapped devices will come the same way, as
//!     a capability to map a device's frames.
//!   - Interrupt lines (Cap::Irq { line }): wait blocks until the line has
//!     fired; ack says the driver has dealt with it. Each line counts its
//!     interrupts, so one that fires while the driver is busy isn't lost.
//!
//! Lines 0-15 are the ISA interrupts (keyboard = 1, ...). Each has its own
//! vector, VECTOR_BASE + line; Phase 5 routes the real devices onto them
//! through the I/O APIC, which then also masks a line until it is acked.
//! Line 16 is a test line with no device behind it: the kernel raises it
//! by sending itself an interrupt through the local APIC, so the whole
//! path from interrupt to user-space driver can be tested today.

use crate::cap::Cap;
use crate::cpu::without_interrupts;
use crate::sched::{self, State};
use crate::syscall::{ERR_BAD_ARGUMENT, ERR_WRONG_CAPABILITY};
use core::sync::atomic::{AtomicU32, Ordering};

pub const LINES: usize = 17;
pub const TEST_LINE: u8 = 16;
/// Line n arrives on vector VECTOR_BASE + n (0x40-0x50); entry.asm has a
/// stub for each.
pub const VECTOR_BASE: u8 = 0x40;

/// Interrupts that arrived and haven't been waited for yet, per line.
static PENDING: [AtomicU32; LINES] = [const { AtomicU32::new(0) }; LINES];
static TOTAL: AtomicU32 = AtomicU32::new(0);

/// interrupt_dispatch, for vectors VECTOR_BASE..VECTOR_BASE + LINES.
/// Interrupt context: count it, acknowledge it, wake the driver.
pub fn on_interrupt(line: u8) {
    PENDING[line as usize].fetch_add(1, Ordering::Relaxed);
    TOTAL.fetch_add(1, Ordering::Relaxed);
    crate::apic::end_of_interrupt();
    sched::wake_irq_waiter(line);
}

/// Line count, so idt.rs can tell a line's vector from anything else.
pub fn line_of(vector: u64) -> Option<u8> {
    let line = vector.checked_sub(VECTOR_BASE as u64)?;
    (line < LINES as u64).then_some(line as u8)
}

/// Raise the test line (sends this CPU an interrupt on its vector).
pub fn raise_test_line() {
    crate::apic::send_self_ipi(VECTOR_BASE + TEST_LINE);
}

/// Interrupts delivered on all lines so far.
pub fn total() -> u32 {
    TOTAL.load(Ordering::Relaxed)
}

fn line_cap(slot: u64) -> Result<u8, i64> {
    match sched::current_cap(slot) {
        Cap::Irq { line } if (line as usize) < LINES => Ok(line),
        _ => Err(ERR_WRONG_CAPABILITY),
    }
}

/// irq_wait(slot): block until the line in Cap::Irq slot `slot` has fired;
/// consume one interrupt. Returns 0 (or ERR_INTERRUPTED if killed).
pub fn wait(slot: u64) -> i64 {
    let line = match line_cap(slot) {
        Ok(l) => l,
        Err(e) => return e,
    };
    without_interrupts(|| unsafe {
        loop {
            let pending = &PENDING[line as usize];
            if pending.load(Ordering::Relaxed) > 0 {
                pending.fetch_sub(1, Ordering::Relaxed);
                return 0;
            }
            sched::task(sched::current()).result = 0;
            sched::block(State::IrqWait(line));
            let result = sched::task(sched::current()).result;
            if result != 0 {
                return result;
            }
        }
    })
}

/// irq_ack(slot): the driver has handled the line. Nothing to unmask yet
/// (the test line has no device); Phase 5's I/O APIC unmasks it here.
pub fn ack(slot: u64) -> i64 {
    match line_cap(slot) {
        Ok(_) => 0,
        Err(e) => e,
    }
}

/// port_in(slot, port, width) / port_out(slot, port, width, value), with
/// width 1, 2 or 4 bytes, all of [port, port + width) inside the range of
/// Cap::IoPorts slot `slot`. port_in returns the value read.
pub fn port_io(slot: u64, port: u64, width: u64, value: u64, write: bool) -> i64 {
    let Cap::IoPorts { base, count } = sched::current_cap(slot) else {
        return ERR_WRONG_CAPABILITY;
    };
    if !matches!(width, 1 | 2 | 4) || port > 0xFFFF {
        return ERR_BAD_ARGUMENT;
    }
    if port < base as u64 || port + width > base as u64 + count as u64 {
        return ERR_WRONG_CAPABILITY;
    }
    let port = port as u16;
    unsafe {
        use core::arch::asm;
        match (width, write) {
            (1, false) => {
                let v: u8;
                asm!("in al, dx", in("dx") port, out("al") v, options(nomem, nostack, preserves_flags));
                v as i64
            }
            (2, false) => {
                let v: u16;
                asm!("in ax, dx", in("dx") port, out("ax") v, options(nomem, nostack, preserves_flags));
                v as i64
            }
            (4, false) => {
                let v: u32;
                asm!("in eax, dx", in("dx") port, out("eax") v, options(nomem, nostack, preserves_flags));
                v as i64
            }
            (1, true) => {
                asm!("out dx, al", in("dx") port, in("al") value as u8, options(nomem, nostack, preserves_flags));
                0
            }
            (2, true) => {
                asm!("out dx, ax", in("dx") port, in("ax") value as u16, options(nomem, nostack, preserves_flags));
                0
            }
            _ => {
                asm!("out dx, eax", in("dx") port, in("eax") value as u32, options(nomem, nostack, preserves_flags));
                0
            }
        }
    }
}

// ===========================================================================
// Self-test (user/driver.asm)
// ===========================================================================

use crate::cap::table;
use crate::process::{self, DRIVER};
use crate::sched::ExitStatus;
use crate::selftest::{check, verdict};
use crate::syscall::ERR_WRONG_CAPABILITY as WRONG;

const READ_CLOCK: u64 = 0;
const OUTSIDE_RANGE: u64 = 1;
const IRQ_LOOP: u64 = 2;
const MISUSE: u64 = 3;

const CMOS: Cap = Cap::IoPorts { base: 0x70, count: 2 };
const TEST_IRQ: Cap = Cap::Irq { line: TEST_LINE };

fn run(mode: u64, slot0: Cap, slot1: Cap) -> Option<ExitStatus> {
    let id = process::spawn(&DRIVER, mode, table(&[slot0, slot1]))
        .map_err(|_| check(b"driver", b"start a test driver", false))
        .ok()?;
    sched::join_timeout(id, 500)
}

pub fn selftest() -> bool {
    let mut ok = true;

    // 1. A real device from ring 3: the CMOS real-time clock, ports 0x70-0x71.
    let clock = run(READ_CLOCK, CMOS, Cap::None);
    crate::serial_write_str(b"[driver] self-test a user-space driver reads the CMOS clock through its port capability: seconds = ");
    let seconds = match clock {
        Some(ExitStatus::Code(v)) => v,
        _ => u64::MAX,
    };
    crate::serial_write_hex(seconds);
    ok &= verdict(seconds < 0x60); // binary (< 60) or BCD (< 0x60)

    // 2. Only what the capability covers, only the right kind.
    let outside = run(OUTSIDE_RANGE, CMOS, Cap::None);
    let none = run(READ_CLOCK, Cap::None, Cap::None);
    let misuse = run(MISUSE, CMOS, TEST_IRQ);
    ok &= check(
        b"driver",
        b"the keyboard port with a CMOS capability, no capability, the wrong kind, a bad width: all refused",
        outside == Some(ExitStatus::Code(WRONG as u64)) && none == Some(ExitStatus::Code(WRONG as u64))
            && misuse == Some(ExitStatus::Code(0)),
    );

    // 3. An interrupt line delivered to a user-space driver, 10 times. The
    //    driver blocks in irq_wait; each interrupt wakes it. They come in
    //    back-to-back pairs, so the second of each pair arrives while the
    //    driver is still busy with the first: counted, not lost.
    let before = total();
    let driver = process::spawn(&DRIVER, IRQ_LOOP, table(&[Cap::None, TEST_IRQ])).ok();
    let mut handled = None;
    if let Some(id) = driver {
        for _ in 0..5 {
            sched::sleep_ticks(1);
            raise_test_line();
            raise_test_line();
        }
        handled = sched::join_timeout(id, 500);
    }
    crate::serial_write_str(b"[driver] self-test an interrupt line handed to a user-space driver, in back-to-back pairs: ");
    crate::serial_write_u64(total() as u64 - before as u64);
    crate::serial_write_str(b" raised, driver handled ");
    crate::serial_write_u64(match handled {
        Some(ExitStatus::Code(n)) => n,
        _ => 0,
    });
    ok &= verdict(handled == Some(ExitStatus::Code(10)) && PENDING[TEST_LINE as usize].load(Ordering::Relaxed) == 0);

    ok
}
