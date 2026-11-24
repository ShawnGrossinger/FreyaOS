//! apic.rs — the local APIC and its timer: the kernel's heartbeat.
//!
//! Each CPU core has a local APIC, an interrupt controller with a built-in
//! timer, reached through a 4 KiB page of memory-mapped registers (usually
//! at physical 0xFEE00000). This module:
//!
//!   1. enables the APIC and maps its registers (uncached, via vmm),
//!   2. measures how fast its timer counts, against the PIT, whose
//!      frequency is fixed at 1.193182 MHz on every PC,
//!   3. runs the timer periodically at TIMER_HZ on TIMER_VECTOR,
//!   4. counts ticks; every tick ends with an End-Of-Interrupt (EOI) write,
//!      without which the APIC delivers no further interrupts of that kind.
//!
//! Interrupt handlers must not allocate: the heap halts on re-entry, and a
//! tick can land in the middle of an allocation. on_timer() only touches
//! atomics and APIC registers.

use core::arch::asm;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::{inb, outb};

/// Vector the timer interrupt arrives on. Above the remapped PIC range
/// (0x20-0x2F), so the two can never be confused. The stub in entry.asm
/// (isr_stub_48) is hard-wired to this number.
pub const TIMER_VECTOR: u8 = 0x30;
/// Vector for spurious interrupts (an interrupt that vanished before the
/// CPU acknowledged it). Must end in 0xF on older APICs; 0xFF is
/// conventional. Spurious interrupts must NOT be acknowledged with EOI.
pub const SPURIOUS_VECTOR: u8 = 0xFF;
/// Ticks per second.
pub const TIMER_HZ: u64 = 100;

// --- Register offsets (Intel SDM Vol. 3, "Local APIC Register Address Map") ---
const REG_ID: usize = 0x020;
const REG_VERSION: usize = 0x030;
const REG_TPR: usize = 0x080; // task priority: 0 = accept everything
const REG_EOI: usize = 0x0B0;
const REG_SVR: usize = 0x0F0; // spurious vector + software enable (bit 8)
const REG_ISR: usize = 0x100; // in-service bits, 8 registers 0x10 apart
const REG_LVT_CMCI: usize = 0x2F0; // only if the version register's max LVT >= 6
const REG_LVT_TIMER: usize = 0x320;
const REG_LVT_THERMAL: usize = 0x330; // only if max LVT >= 5
const REG_LVT_PERF: usize = 0x340; // only if max LVT >= 4
const REG_LVT_LINT0: usize = 0x350;
const REG_LVT_LINT1: usize = 0x360;
const REG_LVT_ERROR: usize = 0x370;
const REG_TIMER_INITIAL: usize = 0x380;
const REG_TIMER_CURRENT: usize = 0x390;
const REG_TIMER_DIVIDE: usize = 0x3E0;

const LVT_MASKED: u32 = 1 << 16;
const LVT_TIMER_PERIODIC: u32 = 1 << 17;
const LVT_DELIVERY_NMI: u32 = 0b100 << 8;
const SVR_ENABLE: u32 = 1 << 8;
const DIVIDE_BY_16: u32 = 0b0011;

const IA32_APIC_BASE: u32 = 0x1B;
const APIC_BASE_ENABLE: u64 = 1 << 11;

/// PIT input clock, Hz.
const PIT_HZ: u64 = 1_193_182;

static BASE: AtomicU64 = AtomicU64::new(0); // virtual address of the register page
static TICKS: AtomicU64 = AtomicU64::new(0);
static SPURIOUS: AtomicU64 = AtomicU64::new(0);
static COUNTS_PER_10MS: AtomicU32 = AtomicU32::new(0);

/// The local APIC's interrupt sources other than the timer (its "local
/// vector table"), and what the firmware had left in each before init()
/// set them. Every one of them can send an interrupt on a vector of its
/// own choosing, so any left unmasked is a source the kernel didn't ask for.
pub const LVT_NAMES: [&[u8]; 5] = [b"LINT0", b"LINT1", b"error", b"perf", b"thermal"];
const LVT_REGS: [usize; 5] = [REG_LVT_LINT0, REG_LVT_LINT1, REG_LVT_ERROR, REG_LVT_PERF, REG_LVT_THERMAL];
static FIRMWARE_LVT: [AtomicU32; 6] = [const { AtomicU32::new(LVT_MASKED) }; 6]; // last: CMCI
static MAX_LVT: AtomicU32 = AtomicU32::new(0);
/// The register page, which init() maps; halts if it hasn't (Entry 30:
/// with BASE still 0, an access would land in low memory instead).
fn base() -> usize {
    let base = BASE.load(Ordering::Relaxed) as usize;
    if base == 0 {
        crate::serial_write_str(b"[apic] FATAL: a register used before init mapped them\n");
        crate::halt();
    }
    base
}

fn read(reg: usize) -> u32 {
    let base = base();
    // SAFETY: [mmio] init mapped the APIC's 4 KiB register page at base
    // (uncached), and every reg is one of the REG_ offsets inside it.
    unsafe { core::ptr::read_volatile((base + reg) as *const u32) }
}

fn write(reg: usize, value: u32) {
    let base = base();
    // SAFETY: [mmio] as in read: a register in the page init mapped.
    unsafe { core::ptr::write_volatile((base + reg) as *mut u32, value) }
}

/// Busy-wait `ms` milliseconds (at most 54) on PIT channel 2, by polling.
/// Channel 2 is the one whose gate and output are visible in port 0x61, so
/// it can be timed without taking a single interrupt.
pub(crate) fn pit_wait_ms(ms: u64) {
    let count = (PIT_HZ * ms / 1000) as u16;
    // SAFETY: [port] the PIT's channel 2 (0x42, programmed through 0x43)
    // and its gate in port 0x61 are used by nothing else in the kernel;
    // only 0x61's two low bits change (the speaker stays off). The callers
    // are boot (before the scheduler starts) and power's last steps; two
    // waits interleaving could only spoil the timing.
    unsafe {
        // Gate on (bit 0), speaker data off (bit 1).
        outb(0x61, (inb(0x61) & !0x02) | 0x01);
        // Channel 2, low byte then high byte, mode 0 (OUT goes high at
        // terminal count), binary.
        outb(0x43, 0b1011_0000);
        outb(0x42, count as u8);
        outb(0x42, (count >> 8) as u8); // counting starts here
        while inb(0x61) & 0x20 == 0 {} // bit 5 = channel 2 output
    }
}

/// What init() found, for the boot log.
pub struct ApicInfo {
    pub phys_base: u64,
    pub id: u32,
    pub version: u32,
    pub counts_per_10ms: u32,
    /// The largest of the calibration's measurements (Entry 27).
    pub most_per_10ms: u32,
}

/// How many times the timer is measured against the PIT (Entry 27).
const CALIBRATIONS: usize = 5;

/// Enable the local APIC, calibrate its timer and start it at TIMER_HZ.
/// Interrupts stay disabled; the caller decides when to `sti`.
pub fn init() -> Result<ApicInfo, &'static [u8]> {
    if crate::cpu::cpuid(1).3 & (1 << 9) == 0 {
        return Err(b"CPU reports no local APIC (CPUID.1:EDX bit 9)");
    }

    // --- Find and enable it ---
    // SAFETY: [cpu] IA32_APIC_BASE exists on every CPU with a local APIC,
    // which CPUID said this one has.
    let msr = unsafe { crate::cpu::rdmsr(IA32_APIC_BASE) };
    let phys_base = msr & 0x000F_FFFF_FFFF_F000;
    // SAFETY: [cpu] sets only the enable bit, keeping the base address the
    // firmware chose; interrupts are off, so nothing arrives before init
    // has masked every source but the timer.
    unsafe { crate::cpu::wrmsr(IA32_APIC_BASE, msr | APIC_BASE_ENABLE) };

    // --- Map its registers: uncached, no-execute ---
    // SAFETY: [mmio] the APIC's register page, at the address its MSR
    // gives, which no RAM shares; mapped once, here.
    let va = match unsafe { crate::vmm::map_mmio(phys_base, 4096) } {
        Ok(va) => va,
        Err(_) => return Err(b"could not map the APIC register page"),
    };
    BASE.store(va, Ordering::Relaxed);

    // --- Basic configuration ---
    // Every source of interrupts except the timer is masked, whatever the
    // firmware left in it (Entry 15: the T430 took an interrupt nobody
    // asked for). LINT1 stays the NMI input it is wired as on every PC.
    let max_lvt = (read(REG_VERSION) >> 16) & 0xFF;
    MAX_LVT.store(max_lvt, Ordering::Relaxed);
    for (i, &reg) in LVT_REGS.iter().enumerate() {
        if present(reg, max_lvt) {
            FIRMWARE_LVT[i].store(read(reg), Ordering::Relaxed);
        }
    }
    if max_lvt >= 6 {
        FIRMWARE_LVT[5].store(read(REG_LVT_CMCI), Ordering::Relaxed);
    }
    write(REG_TPR, 0); // accept every priority
    mask_local_sources(max_lvt);
    write(REG_SVR, SVR_ENABLE | SPURIOUS_VECTOR as u32);

    // --- Calibrate: how far does the timer count in 10 ms? ---
    // Measured CALIBRATIONS times, and the least kept (Entry 27). Anything
    // that holds the CPU up while the timer counts (the host, under an
    // emulator; SMM, on hardware) adds counts and never takes any away, so
    // the least is the truest. Measured once, it was 41% high in one of
    // Shawn's QEMU boots, and every tick after it 1.4 times too long.
    write(REG_TIMER_DIVIDE, DIVIDE_BY_16);
    write(REG_LVT_TIMER, LVT_MASKED); // one-shot, no interrupt while measuring
    let mut measured = [0u32; CALIBRATIONS];
    for i in 0..CALIBRATIONS {
        write(REG_TIMER_INITIAL, u32::MAX);
        pit_wait_ms(10);
        measured[i] = u32::MAX - read(REG_TIMER_CURRENT);
        write(REG_TIMER_INITIAL, 0); // stop
    }
    let counts = measured.iter().copied().min().unwrap_or(0);
    let most = measured.iter().copied().max().unwrap_or(0);
    if counts == 0 {
        return Err(b"APIC timer did not count during calibration");
    }
    COUNTS_PER_10MS.store(counts, Ordering::Relaxed);

    // --- Run: periodic, one interrupt every 1/TIMER_HZ seconds ---
    let per_tick = (counts as u64 * 100 / TIMER_HZ) as u32;
    write(REG_LVT_TIMER, LVT_TIMER_PERIODIC | TIMER_VECTOR as u32);
    write(REG_TIMER_INITIAL, per_tick);

    Ok(ApicInfo {
        phys_base,
        id: read(REG_ID) >> 24,
        version: read(REG_VERSION) & 0xFF,
        counts_per_10ms: counts,
        most_per_10ms: most,
    })
}

/// Timer interrupt (called from interrupt_dispatch). EOI goes first: the
/// scheduler may switch to another task here, and the APIC delivers no
/// more timer interrupts until this one is acknowledged.
pub fn on_timer() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    write(REG_EOI, 0);
    crate::console::kick(); // what the console driver should look at (Entry 20)
    crate::sched::on_tick();
}

/// This CPU's local APIC id (where the I/O APIC should send interrupts).
pub fn id() -> u8 {
    (read(REG_ID) >> 24) as u8
}

/// The registers exist once init() has mapped them.
pub fn ready() -> bool {
    BASE.load(Ordering::Relaxed) != 0
}

/// The local APIC is handling an interrupt on `vector` (its in-service bit
/// is set). A CPU exception never sets it, so on an exception vector this
/// tells an interrupt from the exception. False before init().
pub fn in_service(vector: u8) -> bool {
    ready() && read(REG_ISR + 0x10 * (vector as usize / 32)) & (1 << (vector % 32)) != 0
}

/// Mask every local interrupt source except the timer; LINT1 is the NMI
/// input on every PC, so it is set to deliver exactly that.
fn mask_local_sources(max_lvt: u32) {
    write(REG_LVT_LINT0, LVT_MASKED); // the PIC's old route in; PICs are masked anyway
    write(REG_LVT_LINT1, LVT_DELIVERY_NMI);
    write(REG_LVT_ERROR, LVT_MASKED);
    write(REG_LVT_PERF, LVT_MASKED);
    if max_lvt >= 5 {
        write(REG_LVT_THERMAL, LVT_MASKED);
    }
    if max_lvt >= 6 {
        write(REG_LVT_CMCI, LVT_MASKED);
    }
}

/// Every local source but the timer is masked and LINT1 is NMI.
fn local_sources_masked(max_lvt: u32) -> bool {
    let mut quiet = read(REG_LVT_LINT0) & LVT_MASKED != 0
        && read(REG_LVT_LINT1) & (LVT_MASKED | 0b111 << 8) == LVT_DELIVERY_NMI
        && read(REG_LVT_ERROR) & LVT_MASKED != 0
        && read(REG_LVT_PERF) & LVT_MASKED != 0;
    if max_lvt >= 5 {
        quiet &= read(REG_LVT_THERMAL) & LVT_MASKED != 0;
    }
    if max_lvt >= 6 {
        quiet &= read(REG_LVT_CMCI) & LVT_MASKED != 0;
    }
    quiet
}

fn present(reg: usize, max_lvt: u32) -> bool {
    match reg {
        REG_LVT_THERMAL => max_lvt >= 5,
        REG_LVT_CMCI => max_lvt >= 6,
        _ => true,
    }
}

/// An LVT entry is a live source of interrupts on a vector of its own:
/// unmasked with fixed delivery (mode 0).
fn live_fixed(value: u32) -> bool {
    value & LVT_MASKED == 0 && (value >> 8) & 0b111 == 0
}

/// How many LVT entries the firmware had left as live sources.
pub fn firmware_live_count() -> u32 {
    (0..6).filter(|&i| live_fixed(FIRMWARE_LVT[i].load(Ordering::Relaxed))).count() as u32
}

/// For the boot log: every LVT entry the firmware had left as a live source
/// of fixed interrupts, with its vector. Returns how many.
pub fn report_firmware_lvts() -> u32 {
    let mut live = 0;
    for i in 0..6 {
        let value = FIRMWARE_LVT[i].load(Ordering::Relaxed);
        if live_fixed(value) {
            live += 1;
            crate::serial_write_str(b"[apic] the firmware left the ");
            crate::serial_write_str(if i < 5 { LVT_NAMES[i] } else { b"CMCI" });
            crate::serial_write_str(b" entry unmasked, vector ");
            crate::serial_write_u64((value & 0xFF) as u64);
            crate::serial_write_str(b" (now masked)\n");
        }
    }
    live
}

/// End-Of-Interrupt for any other interrupt handled through the local APIC.
pub fn end_of_interrupt() {
    write(REG_EOI, 0);
}

/// Send this CPU an interrupt on `vector` (ICR, "self" shorthand, fixed
/// delivery, edge). Used to raise irq.rs's test line.
///
/// On real hardware the send is asynchronous: the ICR's delivery-status bit
/// stays set until the APIC has sent it, and Intel's manual says not to
/// write the ICR again until it clears. (QEMU sends at once, so the T430 was
/// the first machine to show this; see Entry 14.) Returns false if a send
/// never completed.
pub fn send_self_ipi(vector: u8) -> bool {
    const REG_ICR_LOW: usize = 0x300;
    const ICR_SEND_PENDING: u32 = 1 << 12;
    const ICR_ASSERT: u32 = 1 << 14;
    const ICR_TO_SELF: u32 = 0b01 << 18;
    let idle = || {
        for _ in 0..1_000_000 {
            if read(REG_ICR_LOW) & ICR_SEND_PENDING == 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    };
    if !idle() {
        return false;
    }
    write(REG_ICR_LOW, vector as u32 | ICR_ASSERT | ICR_TO_SELF);
    idle()
}

/// Spurious interrupt: count it, and do NOT send EOI.
pub fn on_spurious() {
    SPURIOUS.fetch_add(1, Ordering::Relaxed);
}

/// Timer ticks since the timer started.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Timer interrupts have arrived since interrupts went on: what the
/// scheduler needs, whatever the self-test made of their rate (Entry 22).
pub fn ticking() -> bool {
    ticks() > 0
}

/// For selftest below: the boot thread turns interrupts on for good there.
fn enable_interrupts() {
    // SAFETY: [cpu] only selftest calls this, from the boot thread, holding
    // no lock (it isn't inside without_interrupts): from here boot runs with
    // interrupts on.
    unsafe { asm!("sti", options(nomem, nostack)) };
}

fn disable_interrupts() {
    // SAFETY: [cpu] only selftest calls this, around work it wants
    // uninterrupted, and turns them on again right after.
    unsafe { asm!("cli", options(nomem, nostack)) };
}

// ===========================================================================
// Self-test
// ===========================================================================

/// One unit of deterministic work that keeps integer and SSE registers
/// busy. Run with interrupts on and off, it must give identical results.
#[inline(never)]
pub(crate) fn work(state: &mut (u64, f64), steps: u64) {
    let (mut a, mut x) = *state;
    for i in 0..core::hint::black_box(steps) {
        a = a.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(i | 1);
        x = x * 0.999_9 + ((a >> 40) & 0x3FF) as f64 * 0.5;
    }
    *state = (a, x);
}

/// Checks, in order. Leaves interrupts ENABLED.
pub fn selftest(info: &ApicInfo) -> bool {
    use crate::selftest::check;
    let mut ok = true;

    ok &= check(b"pic", b"legacy PICs remapped to 0x20-0x2F and every line masked",
                crate::pic::masks() == (0xFF, 0xFF));

    // SAFETY: [cpu] reading IA32_APIC_BASE, which init already read.
    let enabled = unsafe { crate::cpu::rdmsr(IA32_APIC_BASE) } & APIC_BASE_ENABLE != 0
        && read(REG_SVR) & SVR_ENABLE != 0;
    ok &= check(b"apic", b"local APIC enabled (IA32_APIC_BASE and SVR)", enabled);

    // Every interrupt source but the timer masked; LINT1 is NMI. Then play
    // the firmware: leave two sources live (the performance counter and
    // APIC error entries, which can't fire unless set up further), and
    // check the masking puts them back. Interrupts are off meanwhile.
    let max_lvt = MAX_LVT.load(Ordering::Relaxed);
    let mut quiet = local_sources_masked(max_lvt);
    quiet &= crate::cpu::without_interrupts(|| {
        write(REG_LVT_PERF, 0x90);
        write(REG_LVT_ERROR, 0x90);
        let live = !local_sources_masked(max_lvt);
        mask_local_sources(max_lvt);
        live && local_sources_masked(max_lvt)
    });
    let firmware_live = report_firmware_lvts();
    crate::serial_write_str(b"[apic] self-test every local interrupt source but the timer is masked (LINT1: NMI), and masking undoes live ones; ");
    crate::serial_write_u64(max_lvt as u64 + 1);
    crate::serial_write_str(b" LVT entries, ");
    crate::serial_write_u64(firmware_live as u64);
    crate::serial_write_str(b" left live by the firmware");
    ok &= crate::selftest::verdict(quiet);

    crate::serial_write_str(b"[apic] self-test timer calibrated against the PIT: ");
    crate::serial_write_u64(info.counts_per_10ms as u64);
    crate::serial_write_str(b" counts per 10 ms, the least of ");
    crate::serial_write_u64(CALIBRATIONS as u64);
    crate::serial_write_str(b" measurements (the most ");
    crate::serial_write_u64(info.most_per_10ms as u64);
    crate::serial_write_str(b")");
    ok &= crate::selftest::verdict(info.counts_per_10ms > 100);

    // --- Interrupts on from here ---
    enable_interrupts();

    // Ticks over 100 ms timed independently by the PIT: expect about 10.
    // Under an emulator on a busy host, the virtual timer can be held up
    // and then catch up in a burst while the polled PIT keeps real time
    // (Entry 22: 1 tick in one boot of Shawn's test run, then the next 5
    // came twice as fast). So up to three measurements are made, each one
    // reported; a timer that is missing or at the wrong rate fails all
    // three. Real hardware passes the first.
    let mut counts = [0u64; 3];
    let mut attempts = 0;
    for count in counts.iter_mut() {
        let start = ticks();
        for _ in 0..10 {
            pit_wait_ms(10);
        }
        *count = ticks() - start;
        attempts += 1;
        if (8..=12).contains(count) {
            break;
        }
    }
    let counted = counts[attempts - 1];
    crate::serial_write_str(b"[apic] self-test ");
    crate::serial_write_u64(counted);
    crate::serial_write_str(b" ticks in 100 ms measured by the PIT (expect about 10)");
    if attempts > 1 {
        crate::serial_write_str(b"; attempt ");
        crate::serial_write_u64(attempts as u64);
        crate::serial_write_str(b", after");
        for &c in &counts[..attempts - 1] {
            crate::serial_write_str(b" ");
            crate::serial_write_u64(c);
        }
    }
    ok &= crate::selftest::verdict((8..=12).contains(&counted));

    // Work that timer interrupts land in the middle of must come out exactly
    // as if it had never been interrupted: every general-purpose and SSE
    // register is saved and restored around each tick.
    // If the timer isn't ticking, the loop below could wait forever: fail
    // instead. The chunk cap bounds it even if ticks stop part-way.
    const MAX_CHUNKS: u64 = 10_000;
    let mut interrupted = (1u64, 0.0f64);
    let mut chunks = 0u64;
    let begin = ticks();
    if counts.iter().any(|&c| c > 0) {
        while ticks() < begin + 5 && chunks < MAX_CHUNKS {
            work(&mut interrupted, 10_000);
            chunks += 1;
        }
    }
    let ticks_seen = ticks() - begin;
    disable_interrupts();
    let mut quiet = (1u64, 0.0f64);
    for _ in 0..chunks {
        work(&mut quiet, 10_000);
    }
    enable_interrupts();
    let same = interrupted.0 == quiet.0 && interrupted.1.to_bits() == quiet.1.to_bits();
    crate::serial_write_str(b"[apic] self-test work interrupted by ");
    crate::serial_write_u64(ticks_seen);
    crate::serial_write_str(b" ticks (");
    crate::serial_write_u64(chunks * 10_000);
    crate::serial_write_str(b" steps) matches the same work with interrupts off");
    ok &= crate::selftest::verdict(ticks_seen >= 5 && same);

    ok
}
