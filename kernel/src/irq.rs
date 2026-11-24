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
//! vector, VECTOR_BASE + line. The I/O APIC (found through the ACPI MADT,
//! acpi.rs) carries the real devices there: every input starts masked, and
//! an ISA line is unmasked when a driver first acks or waits on it. A
//! level-triggered line is masked again from each interrupt until the
//! driver acks it (otherwise it would fire again at once, forever); an
//! edge-triggered one stays open, and the per-line count keeps any
//! interrupt that arrives while the driver is busy.
//!
//! An edge that arrives while its line is masked is lost (the I/O APIC
//! ignores it), and a device that holds its line up until it is serviced
//! then never makes another. So a driver arms its line with ack before it
//! empties its device, then waits (Entry 19: the keyboard driver emptied
//! the controller first, and a key held through boot killed the keyboard).
//!
//! Line 16 is a test line with no device behind it: the kernel raises it
//! by sending itself an interrupt through the local APIC, so the whole
//! path from interrupt to user-space driver can be tested without one.
//!
//! Interrupts as signals (Entry 22): irq_bind(irq, endpoint) makes a line's
//! interrupts arrive on an endpoint the driver receives on, as signal bit
//! (1 << line), instead of waking irq_wait. A driver then waits in one
//! place for its interrupts (several lines, if it has them) and for its
//! clients' requests: the PS/2 driver waits for the keyboard's line, the
//! mouse's line and the mouse driver's calls. Signals merge, so a bound
//! line says "it fired since you last looked", not how often: the driver
//! asks its device how much there is. Arming still works the same way:
//! ack before emptying the device. The binding holds a reference to the
//! endpoint until the line is bound again.

use crate::cap::Cap;
use crate::cpu::without_interrupts;
use crate::sched::{self, State};
use crate::syscall::{ERR_BAD_ARGUMENT, ERR_WRONG_CAPABILITY};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

pub const LINES: usize = 17;
pub const TEST_LINE: u8 = 16;
/// Line n arrives on vector VECTOR_BASE + n (0x40-0x50); entry.asm has a
/// stub for each.
pub const VECTOR_BASE: u8 = 0x40;

/// Interrupts that arrived and haven't been waited for yet, per line.
static PENDING: [AtomicU32; LINES] = [const { AtomicU32::new(0) }; LINES];
/// The endpoint each line's interrupts are signalled to, or UNBOUND.
const UNBOUND: u32 = u32::MAX;
static BOUND: [AtomicU32; LINES] = [const { AtomicU32::new(UNBOUND) }; LINES];
// Signal bit (1 << line) must fit one word.
const _: () = assert!(LINES <= 64);
static TOTAL: AtomicU32 = AtomicU32::new(0);

// ===========================================================================
// The I/O APIC
// ===========================================================================

const ISA_LINES: usize = 16;
const NOT_ROUTED: u32 = u32::MAX;
const IOAPIC_MASKED: u32 = 1 << 16;
const IOAPIC_LEVEL: u32 = 1 << 15;
const IOAPIC_ACTIVE_LOW: u32 = 1 << 13;

/// Virtual address of the I/O APIC's two registers (0 = none found).
static IOAPIC: AtomicU64 = AtomicU64::new(0);
/// Which I/O APIC input each ISA line is wired to.
static INPUT: [AtomicU32; ISA_LINES] = [const { AtomicU32::new(NOT_ROUTED) }; ISA_LINES];
static LEVEL: [AtomicBool; ISA_LINES] = [const { AtomicBool::new(false) }; ISA_LINES];

/// The registers' address, which init_ioapic maps; halts if it hasn't
/// (Entry 30: with IOAPIC still 0, the writes would land in low memory).
fn ioapic_base() -> u64 {
    let base = IOAPIC.load(Ordering::Relaxed);
    if base == 0 {
        crate::serial_write_str(b"[irq] FATAL: the I/O APIC used before it was mapped\n");
        crate::halt();
    }
    base
}

/// A register is two accesses (select it, then read or write the window),
/// so each pair runs with interrupts off: a handler's own pair can't come
/// in between and leave the wrong register selected.
fn ioapic_read(reg: u32) -> u32 {
    let base = ioapic_base();
    without_interrupts(|| {
        // SAFETY: [mmio] IOREGSEL (base) and IOWIN (base + 0x10), both in
        // the 0x20 bytes init_ioapic mapped, uncached, at base.
        unsafe {
            (base as *mut u32).write_volatile(reg); // IOREGSEL
            ((base + 0x10) as *const u32).read_volatile() // IOWIN
        }
    })
}

fn ioapic_write(reg: u32, value: u32) {
    let base = ioapic_base();
    without_interrupts(|| {
        // SAFETY: [mmio] as in ioapic_read: the two registers init_ioapic
        // mapped.
        unsafe {
            (base as *mut u32).write_volatile(reg);
            ((base + 0x10) as *mut u32).write_volatile(value);
        }
    })
}

/// I/O APIC inputs the firmware had left unmasked, for the boot summary.
static FIRMWARE_LIVE: AtomicU32 = AtomicU32::new(0);

pub fn firmware_live_inputs() -> u32 {
    FIRMWARE_LIVE.load(Ordering::Relaxed)
}

pub struct IoApicSetup {
    pub version: u32,
    pub inputs: u32,
    pub routed: u32,
    /// Inputs the firmware had left unmasked (before they were masked).
    pub firmware_live: u32,
    /// Whether interrupts were still off when the inputs were masked.
    pub before_interrupts: bool,
}

/// Mask every input, then wire each ISA line to its vector (still masked),
/// with the polarity and trigger the MADT gives. Interrupts go to this CPU.
pub fn init_ioapic(info: &crate::acpi::Info) -> Result<IoApicSetup, &'static [u8]> {
    let io = info.ioapic.ok_or(&b"no I/O APIC in the ACPI MADT"[..])?;
    // SAFETY: [mmio] the I/O APIC's registers, at the address the MADT
    // gives, which no RAM shares; main calls init_ioapic once.
    let va = unsafe { crate::vmm::map_mmio(io.phys, 0x20) }.map_err(|_| &b"could not map the I/O APIC"[..])?;
    IOAPIC.store(va, Ordering::Relaxed);
    let version_reg = ioapic_read(1);
    let inputs = ((version_reg >> 16) & 0xFF) + 1;
    let before_interrupts = !crate::cpu::interrupts_enabled();
    let mut firmware_live = 0;
    for i in 0..inputs {
        let low = ioapic_read(0x10 + 2 * i);
        if low & IOAPIC_MASKED == 0 {
            firmware_live += 1;
            crate::serial_write_str(b"[ioapic] the firmware left input ");
            crate::serial_write_u64(i as u64);
            crate::serial_write_str(b" unmasked, vector ");
            crate::serial_write_u64((low & 0xFF) as u64);
            crate::serial_write_str(b" (now masked)\n");
        }
        ioapic_write(0x10 + 2 * i, IOAPIC_MASKED);
        ioapic_write(0x11 + 2 * i, 0);
    }
    // Overridden lines first: an override can move ISA line m onto input n
    // (e.g. the old timer, line 0, onto input 2), and then line n must not
    // take input n too.
    let destination = (crate::apic::id() as u32) << 24;
    let mut claimed = 0u64;
    let mut routed = 0;
    for overridden in [true, false] {
        for (line, route) in info.isa.iter().enumerate() {
            if (route.gsi != line as u32) != overridden || route.gsi < io.gsi_base {
                continue;
            }
            let input = route.gsi - io.gsi_base;
            if input >= inputs || input >= 64 || claimed & (1 << input) != 0 {
                continue;
            }
            claimed |= 1 << input;
            let low = (VECTOR_BASE as u32 + line as u32)
                | if route.active_low { IOAPIC_ACTIVE_LOW } else { 0 }
                | if route.level { IOAPIC_LEVEL } else { 0 }
                | IOAPIC_MASKED;
            ioapic_write(0x11 + 2 * input, destination);
            ioapic_write(0x10 + 2 * input, low);
            INPUT[line].store(input, Ordering::Relaxed);
            LEVEL[line].store(route.level, Ordering::Relaxed);
            routed += 1;
        }
    }
    FIRMWARE_LIVE.store(firmware_live, Ordering::Relaxed);
    Ok(IoApicSetup { version: version_reg & 0xFF, inputs, routed, firmware_live, before_interrupts })
}

/// Mask or unmask ISA line `line` at the I/O APIC (no-op if not routed).
fn set_masked(line: u8, masked: bool) {
    let Some(input) = INPUT.get(line as usize).map(|i| i.load(Ordering::Relaxed)) else { return };
    if input == NOT_ROUTED || IOAPIC.load(Ordering::Relaxed) == 0 {
        return;
    }
    crate::cpu::without_interrupts(|| {
        let low = ioapic_read(0x10 + 2 * input);
        ioapic_write(0x10 + 2 * input, if masked { low | IOAPIC_MASKED } else { low & !IOAPIC_MASKED });
    });
}

/// The redirection entry (low half) for ISA line `line`, for the self-test.
pub fn entry_of(line: u8) -> Option<(u32, u32)> {
    let input = INPUT.get(line as usize)?.load(Ordering::Relaxed);
    (input != NOT_ROUTED && IOAPIC.load(Ordering::Relaxed) != 0).then(|| (input, ioapic_read(0x10 + 2 * input)))
}

fn is_level(line: u8) -> bool {
    LEVEL.get(line as usize).is_some_and(|l| l.load(Ordering::Relaxed))
}

fn is_routed_or_test(line: u8) -> bool {
    line == TEST_LINE || INPUT.get(line as usize).is_some_and(|i| i.load(Ordering::Relaxed) != NOT_ROUTED)
}

/// interrupt_dispatch, for vectors VECTOR_BASE..VECTOR_BASE + LINES.
/// Interrupt context: count it, acknowledge it, wake the driver.
pub fn on_interrupt(line: u8) {
    if is_level(line) {
        set_masked(line, true); // until the driver acks it
    }
    TOTAL.fetch_add(1, Ordering::Relaxed);
    crate::apic::end_of_interrupt();
    match BOUND[line as usize].load(Ordering::Relaxed) {
        UNBOUND => {
            PENDING[line as usize].fetch_add(1, Ordering::Relaxed);
            sched::wake_irq_waiter(line);
        }
        ep => crate::ipc::signal(ep as u8, 1 << line),
    }
}

/// Line count, so idt.rs can tell a line's vector from anything else.
pub fn line_of(vector: u64) -> Option<u8> {
    let line = vector.checked_sub(VECTOR_BASE as u64)?;
    (line < LINES as u64).then_some(line as u8)
}

/// Raise the test line (sends this CPU an interrupt on its vector).
/// Raise the test line and wait until the CPU has taken the interrupt.
///
/// An interrupt that is sent but not yet taken sits as one bit in the local
/// APIC's request register, so a second send of the same vector before the
/// CPU takes the first merges with it: two sends, one interrupt. Real
/// hardware delivers a little after the send; QEMU delivers at once, which
/// is why only the T430 showed it (Entry 14). Waiting here makes "raised"
/// mean "delivered". Returns false if it wasn't delivered within the bound
/// (or interrupts are off, when it can't be).
pub fn raise_test_line() -> bool {
    let before = total();
    if !crate::apic::send_self_ipi(VECTOR_BASE + TEST_LINE) || !crate::cpu::interrupts_enabled() {
        return false;
    }
    for _ in 0..10_000_000 {
        if total() != before {
            return true;
        }
        core::hint::spin_loop();
    }
    false
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
    if !is_routed_or_test(line) || BOUND[line as usize].load(Ordering::Relaxed) != UNBOUND {
        return ERR_BAD_ARGUMENT; // no such line here, or it signals instead
    }
    set_masked(line, false); // a driver is waiting: let it through
    // SAFETY: [switch] sched::block with interrupts off (inside
    // without_interrupts), in IrqWait(line): on_interrupt for that line, or
    // kill, makes the task Ready again.
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

/// irq_bind(irq, endpoint): the line in Cap::Irq slot `irq` signals the
/// endpoint in slot `endpoint` (which the caller must be able to receive
/// on) with bit (1 << line) from now on. Interrupts counted while it was
/// unbound arrive as one signal at once. Returns 0.
pub fn bind(irq: u64, endpoint: u64) -> i64 {
    let line = match line_cap(irq) {
        Ok(l) => l,
        Err(e) => return e,
    };
    let Cap::Endpoint { ep, rights, .. } = sched::current_cap(endpoint) else {
        return ERR_WRONG_CAPABILITY;
    };
    if rights & crate::cap::RECV == 0 {
        return ERR_WRONG_CAPABILITY;
    }
    if !is_routed_or_test(line) {
        return ERR_BAD_ARGUMENT;
    }
    crate::ipc::retain(ep);
    let (old, waiting) = without_interrupts(|| {
        let old = BOUND[line as usize].swap(ep as u32, Ordering::Relaxed);
        (old, PENDING[line as usize].swap(0, Ordering::Relaxed))
    });
    if old != UNBOUND {
        crate::ipc::release(old as u8);
    }
    if waiting > 0 {
        crate::ipc::signal(ep, 1 << line);
    }
    0
}

/// irq_ack(slot): the driver is ready for the line's next interrupt, so it
/// is unmasked: again, for a level-triggered line (masked by its last
/// interrupt); for the first time, when a driver arms its line before it
/// empties its device (see the top of this file).
pub fn ack(slot: u64) -> i64 {
    match line_cap(slot) {
        Ok(line) => {
            set_masked(line, false);
            0
        }
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
    // SAFETY: [port] every byte of [port, port + width) is inside the range
    // of the IoPorts capability the caller holds (checked above): whoever
    // granted it decided this process may drive those ports. The only
    // ranges the kernel makes are the PS/2 controller's (0x60 and 0x64, for
    // the PS/2 driver) and, for a self-test, the CMOS clock's (0x70-0x71);
    // capabilities copied from them can only be narrower. Neither device
    // can reach memory, and the kernel keeps no state of its own in either.
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
const POWER_UNKNOWN: u64 = 4;

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
        b"the keyboard port with a CMOS capability, no capability, the wrong kind, a bad width, the power switch without a Power capability: all refused",
        outside == Some(ExitStatus::Code(WRONG as u64)) && none == Some(ExitStatus::Code(WRONG as u64))
            && misuse == Some(ExitStatus::Code(0)),
    );
    // The Power capability itself, asked for an action that doesn't exist
    // (so this test can never restart or switch off the machine it runs on).
    let unknown = run(POWER_UNKNOWN, Cap::Power, Cap::None);
    ok &= check(
        b"driver",
        b"a Power capability asked for an action that doesn't exist: refused as a bad argument",
        unknown == Some(ExitStatus::Code(ERR_BAD_ARGUMENT as u64)),
    );

    // 3. An interrupt line delivered to a user-space driver, 10 times. The
    //    driver blocks in irq_wait; each interrupt wakes it. They come in
    //    back-to-back pairs, so the second of each pair arrives while the
    //    driver is still busy with the first: counted, not lost.
    //    Each raise waits until the CPU has taken the interrupt (see
    //    raise_test_line), so the pair is two interrupts, not one.
    let before = total();
    let driver = process::spawn(&DRIVER, IRQ_LOOP, table(&[Cap::None, TEST_IRQ])).ok();
    let mut handled = None;
    let mut raised = 0u64;
    if let Some(id) = driver {
        for _ in 0..5 {
            sched::sleep_ticks(1);
            raised += raise_test_line() as u64;
            raised += raise_test_line() as u64;
        }
        handled = sched::join_timeout(id, 500);
    }
    crate::serial_write_str(b"[driver] self-test an interrupt line handed to a user-space driver, in back-to-back pairs: ");
    crate::serial_write_u64(raised);
    crate::serial_write_str(b" of 10 delivered, ");
    crate::serial_write_u64(total() as u64 - before as u64);
    crate::serial_write_str(b" counted, ");
    match handled {
        Some(ExitStatus::Code(n)) => {
            crate::serial_write_str(b"driver handled ");
            crate::serial_write_u64(n);
        }
        // The driver exits only after 10; anything else means it was still
        // waiting when join gave up, and the count it had is unknown.
        _ => crate::serial_write_str(b"driver still waiting after 5 s"),
    }
    ok &= verdict(
        raised == 10
            && total() - before == 10
            && handled == Some(ExitStatus::Code(10))
            && PENDING[TEST_LINE as usize].load(Ordering::Relaxed) == 0,
    );

    ok &= selftest_bind();
    ok
}

/// captest's bind test (user/rust/src/bin/captest.rs, BIND): three
/// refusals, then a bound test line signalled 5 times.
const CAPTEST_BIND: u64 = 3;
const BIND_SIGNALS: u64 = 5;
const BIND_REFUSALS_ALL: u64 = 0x7;

/// 4. The test line bound to an endpoint: an interrupt raised before the
///    bind arrives as a signal at once, then each one raised while the
///    process waits in recv wakes it with bit (1 << 16).
fn selftest_bind() -> bool {
    use crate::cap::{RECV, SEND};
    let Some(ep) = crate::ipc::create() else {
        return check(b"driver", b"an endpoint for the bind test", false);
    };
    let delivered_early = raise_test_line();
    let caps = table(&[Cap::Console, TEST_IRQ, Cap::Endpoint { ep, badge: 0, rights: SEND | RECV }]);
    let started = process::spawn(&process::CAPTEST, CAPTEST_BIND, caps);
    crate::ipc::release_created(ep);
    let mut raised = delivered_early as u64;
    let mut status = None;
    if let Ok(id) = started {
        for _ in 1..BIND_SIGNALS {
            // Raise only once it waits again, so no two signals merge.
            for _ in 0..200 {
                if sched::blocked_in_ipc(id) {
                    break;
                }
                sched::sleep_ticks(1);
            }
            raised += raise_test_line() as u64;
        }
        status = sched::join_timeout(id, 500);
    }
    unbind(TEST_LINE);
    check(
        b"driver",
        b"an interrupt line bound to an endpoint arrives as signals (one waiting from before the bind, then 4); binding something not an Irq, or to an endpoint without RECV, and irq_wait on a bound line: refused",
        raised == BIND_SIGNALS && status == Some(ExitStatus::Code(BIND_REFUSALS_ALL | BIND_SIGNALS << 8)),
    )
}

/// Take a line's binding away (self-tests: the test line goes back to
/// irq_wait), letting go of the endpoint it held.
fn unbind(line: u8) {
    let old = BOUND[line as usize].swap(UNBOUND, Ordering::Relaxed);
    if old != UNBOUND {
        crate::ipc::release(old as u8);
    }
}

/// After init_ioapic: the hardware says what we told it. The facts a
/// failure turns on come first: on a machine without a serial port only
/// the first 160 characters of a failed line stay on screen (Entry 16).
pub fn selftest_ioapic(setup: &IoApicSetup) -> bool {
    use crate::{serial_write_str as s, serial_write_u64 as dec};
    s(b"[ioapic] self-test set up with interrupts ");
    s(if setup.before_interrupts { b"off" } else { b"ON" });
    s(b"; ");
    dec(setup.inputs as u64);
    s(b" inputs masked (");
    dec(setup.firmware_live as u64);
    s(b" left live by the firmware), ");
    dec(setup.routed as u64);
    s(b" ISA lines wired; keyboard ");
    let expected = VECTOR_BASE as u32 + 1;
    let keyboard_ok = match entry_of(1) {
        Some((input, low)) => {
            s(b"on input ");
            dec(input as u64);
            s(b", vector ");
            dec((low & 0xFF) as u64);
            s(if low & IOAPIC_MASKED != 0 { b" masked" } else { b" UNMASKED" });
            s(if low & IOAPIC_LEVEL != 0 { b", level" } else { b", edge" });
            s(if low & IOAPIC_ACTIVE_LOW != 0 { b", active low" } else { b", active high" });
            low & 0xFF == expected && low & IOAPIC_MASKED != 0
        }
        None => {
            s(b"not wired");
            false
        }
    };
    s(b"; version ");
    dec(setup.version as u64);
    verdict(setup.before_interrupts && setup.version >= 0x11 && setup.inputs >= 16 && setup.routed >= 15 && keyboard_ok)
}
