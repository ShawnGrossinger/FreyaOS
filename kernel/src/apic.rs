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
const REG_LVT_TIMER: usize = 0x320;
const REG_LVT_LINT0: usize = 0x350;
const REG_LVT_ERROR: usize = 0x370;
const REG_TIMER_INITIAL: usize = 0x380;
const REG_TIMER_CURRENT: usize = 0x390;
const REG_TIMER_DIVIDE: usize = 0x3E0;

const LVT_MASKED: u32 = 1 << 16;
const LVT_TIMER_PERIODIC: u32 = 1 << 17;
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

fn read(reg: usize) -> u32 {
    let base = BASE.load(Ordering::Relaxed) as usize;
    unsafe { core::ptr::read_volatile((base + reg) as *const u32) }
}

fn write(reg: usize, value: u32) {
    let base = BASE.load(Ordering::Relaxed) as usize;
    unsafe { core::ptr::write_volatile((base + reg) as *mut u32, value) }
}

fn cpuid_edx(leaf: u32) -> u32 {
    let edx: u32;
    unsafe {
        asm!("mov {tmp}, rbx", "cpuid", "mov rbx, {tmp}",
             tmp = out(reg) _, inout("eax") leaf => _, inout("ecx") 0u32 => _, out("edx") edx,
             options(nostack, preserves_flags));
    }
    edx
}

unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags));
    (hi as u64) << 32 | lo as u64
}

unsafe fn wrmsr(msr: u32, value: u64) {
    asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32,
         options(nostack, preserves_flags));
}

/// Busy-wait `ms` milliseconds (at most 54) on PIT channel 2, by polling.
/// Channel 2 is the one whose gate and output are visible in port 0x61, so
/// it can be timed without taking a single interrupt.
fn pit_wait_ms(ms: u64) {
    let count = (PIT_HZ * ms / 1000) as u16;
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
}

/// Enable the local APIC, calibrate its timer and start it at TIMER_HZ.
/// Interrupts stay disabled; the caller decides when to `sti`.
pub fn init() -> Result<ApicInfo, &'static [u8]> {
    if cpuid_edx(1) & (1 << 9) == 0 {
        return Err(b"CPU reports no local APIC (CPUID.1:EDX bit 9)");
    }

    // --- Find and enable it ---
    let msr = unsafe { rdmsr(IA32_APIC_BASE) };
    let phys_base = msr & 0x000F_FFFF_FFFF_F000;
    unsafe { wrmsr(IA32_APIC_BASE, msr | APIC_BASE_ENABLE) };

    // --- Map its registers: uncached, no-execute ---
    let va = match unsafe { crate::vmm::map_mmio(phys_base, 4096) } {
        Ok(va) => va,
        Err(_) => return Err(b"could not map the APIC register page"),
    };
    BASE.store(va, Ordering::Relaxed);

    // --- Basic configuration ---
    write(REG_TPR, 0); // accept every priority
    write(REG_LVT_LINT0, LVT_MASKED); // the PIC's old route in; PICs are masked anyway
    write(REG_LVT_ERROR, LVT_MASKED);
    write(REG_SVR, SVR_ENABLE | SPURIOUS_VECTOR as u32);

    // --- Calibrate: how far does the timer count in 10 ms? ---
    write(REG_TIMER_DIVIDE, DIVIDE_BY_16);
    write(REG_LVT_TIMER, LVT_MASKED); // one-shot, no interrupt while measuring
    write(REG_TIMER_INITIAL, u32::MAX);
    pit_wait_ms(10);
    let counts = u32::MAX - read(REG_TIMER_CURRENT);
    write(REG_TIMER_INITIAL, 0); // stop
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
    })
}

/// Timer interrupt (called from interrupt_dispatch). EOI goes first: the
/// scheduler may switch to another task here, and the APIC delivers no
/// more timer interrupts until this one is acknowledged.
pub fn on_timer() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    write(REG_EOI, 0);
    crate::sched::on_tick();
}

/// End-Of-Interrupt for any other interrupt handled through the local APIC.
pub fn end_of_interrupt() {
    write(REG_EOI, 0);
}

/// Send this CPU an interrupt on `vector` (ICR, "self" shorthand, fixed
/// delivery, edge). Used to raise irq.rs's test line.
pub fn send_self_ipi(vector: u8) {
    const REG_ICR_LOW: usize = 0x300;
    const ICR_ASSERT: u32 = 1 << 14;
    const ICR_TO_SELF: u32 = 0b01 << 18;
    write(REG_ICR_LOW, vector as u32 | ICR_ASSERT | ICR_TO_SELF);
}

/// Spurious interrupt: count it, and do NOT send EOI.
pub fn on_spurious() {
    SPURIOUS.fetch_add(1, Ordering::Relaxed);
}

/// Timer ticks since the timer started.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

pub fn enable_interrupts() {
    unsafe { asm!("sti", options(nomem, nostack)) };
}

fn disable_interrupts() {
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

    let enabled = unsafe { rdmsr(IA32_APIC_BASE) } & APIC_BASE_ENABLE != 0
        && read(REG_SVR) & SVR_ENABLE != 0;
    ok &= check(b"apic", b"local APIC enabled (IA32_APIC_BASE and SVR)", enabled);

    crate::serial_write_str(b"[apic] self-test timer calibrated against the PIT: ");
    crate::serial_write_u64(info.counts_per_10ms as u64);
    crate::serial_write_str(b" counts per 10 ms");
    ok &= crate::selftest::verdict(info.counts_per_10ms > 100);

    // --- Interrupts on from here ---
    enable_interrupts();

    // Ticks over 100 ms timed independently by the PIT: expect about 10.
    let start = ticks();
    for _ in 0..10 {
        pit_wait_ms(10);
    }
    let counted = ticks() - start;
    crate::serial_write_str(b"[apic] self-test ");
    crate::serial_write_u64(counted);
    crate::serial_write_str(b" ticks in 100 ms measured by the PIT (expect about 10)");
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
    if counted > 0 {
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
