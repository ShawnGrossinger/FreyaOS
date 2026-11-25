//! tpm.rs — the TPM 1.2, through its TIS registers (Entry 33; manifesto
//! Phase B, its second half: the audit chain anchored in the TPM).
//!
//! What it does with a TPM, and all it does: reads PCR 15, and extends it
//! with each sealed segment's seal (audit.rs: anchor). Both are commands
//! whose effects don't outlast a reboot: a PCR goes back to zeros when the
//! machine resets. Nothing here takes ownership, creates keys or counters,
//! or writes the TPM's non-volatile memory, and nothing here can.
//!
//! Which TPM: in this entry, only QEMU's emulated one (vendor 0x1014, IBM,
//! which QEMU's TIS device reports; behind it swtpm, emulating a TPM 1.2 as
//! the T430 has). Any other TPM is found, named, and left alone: talking to
//! the T430's own chip is Entry 34's step, and gets its own entry and its
//! own care. So on the T430 this build reads the TIS identity registers
//! and nothing more.
//!
//! The interface: TCG PC Client TPM Interface Specification (TIS) 1.2,
//! memory-mapped at 0xFED40000, locality 0. A command is written byte by
//! byte into the data FIFO, started with tpmGo, and its response read back
//! the same way once dataAvail is set. Commands and responses are TPM 1.2
//! structures: big-endian, a 10-byte header (tag, size, ordinal or return
//! code) and the parameters.
//!
//! When: task context with interrupts on (the boot thread after the
//! scheduler starts, and the clock task), never from an interrupt handler
//! or with a lock held: a TPM 1.2 can take tens of milliseconds over a
//! command, and the waits are measured in timer ticks. One command at a
//! time, by a sleeping mutex.

use crate::lock::BootCell;
use crate::sync::Mutex;

/// Where TIS puts locality 0's registers.
pub const TIS_BASE: u64 = 0xFED4_0000;

// Registers, from the locality's base.
const ACCESS: u64 = 0x000;
const INTF_CAPABILITY: u64 = 0x014;
const STS: u64 = 0x018;
const DATA_FIFO: u64 = 0x024;
const DID_VID: u64 = 0xF00;
const RID: u64 = 0xF04;

// ACCESS bits.
const ACCESS_VALID: u8 = 0x80;
const ACCESS_ACTIVE: u8 = 0x20;
const ACCESS_REQUEST: u8 = 0x02;

// STS bits (its low byte; the next two are burstCount).
const STS_VALID: u8 = 0x80;
const STS_COMMAND_READY: u8 = 0x40;
const STS_GO: u8 = 0x20;
const STS_DATA_AVAIL: u8 = 0x10;
const STS_EXPECT: u8 = 0x08;

/// The PCR the audit chain extends: the last of PCRs 8 to 15, which the
/// TCG's PC Client specification leaves to the operating system. The
/// firmware measures into 0 to 7.
pub const PCR: u32 = 15;

/// The vendor id QEMU's TIS device reports (IBM's PCI vendor id).
pub const QEMU_VENDOR: u16 = 0x1014;

// TIS timeouts and the longest a short TPM 1.2 command may take, in timer
// ticks (TIMER_HZ, 100 a second): TIS's A, B and C are 750 ms, 2 s and
// 750 ms; an extend or a PCR read is a "short" command.
const TIMEOUT_A: u64 = 75;
const TIMEOUT_B: u64 = 200;
const TIMEOUT_C: u64 = 75;
const SHORT_DURATION: u64 = 300;

// TPM 1.2 return codes this file names.
pub const TPM_SUCCESS: u32 = 0;
pub const TPM_INVALID_POSTINIT: u32 = 0x26;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// No TPM in use (none found, or one this entry doesn't drive).
    NotInUse,
    /// Asked with interrupts off: the waits are counted in ticks.
    InterruptsOff,
    /// The TPM didn't do what TIS says within its time.
    Timeout(&'static str),
    /// It wanted more bytes than the command had, or fewer.
    Length,
    /// A response that isn't a TPM 1.2 response to it.
    Malformed,
    /// The TPM's own return code, not success.
    Tpm(u32),
}

impl Error {
    pub fn code(&self) -> u64 {
        match *self {
            Error::Tpm(rc) => rc as u64,
            Error::NotInUse => 0x1_0001,
            Error::InterruptsOff => 0x1_0002,
            Error::Timeout(_) => 0x1_0003,
            Error::Length => 0x1_0004,
            Error::Malformed => 0x1_0005,
        }
    }
}

/// What was found at boot.
#[derive(Clone, Copy)]
pub struct Found {
    pub vendor: u16,
    pub device: u16,
    pub revision: u8,
    /// TIS's InterfaceVersion (INTF_CAPABILITY bits 28-30): 0 or 2 for a
    /// TPM 1.2, 3 for a TPM 2.0.
    pub interface: u8,
    /// Driven by this kernel (only QEMU's, in this entry).
    pub used: bool,
}

static FOUND: BootCell<Option<Found>> = BootCell::new();
/// The registers' virtual address, once mapped.
static REGS: BootCell<u64> = BootCell::new();
/// One command at a time.
static COMMANDS: Mutex<()> = Mutex::new(());

pub fn found() -> Option<Found> {
    FOUND.get().copied().flatten()
}

pub fn in_use() -> bool {
    found().is_some_and(|f| f.used)
}

/// Whether this build drives a TPM: only QEMU's, and only through a TPM 1.2
/// interface. Everything else is found, named and left alone (the T430's
/// own TPM is Entry 34's). A function of its own so that a self-test can
/// check it on every boot, TPM or not: the one rule here that only real
/// hardware could otherwise test.
pub fn drives(vendor: u16, interface: u8) -> bool {
    vendor == QEMU_VENDOR && interface != 3
}

pub fn vendor_name(vendor: u16) -> &'static str {
    match vendor {
        0x1014 => "IBM: QEMU's emulated TPM",
        0x15D1 => "Infineon",
        0x104A => "STMicroelectronics",
        0x1050 => "Winbond (Nuvoton)",
        0x1114 => "Atmel",
        0x14E4 => "Broadcom",
        0x8086 => "Intel",
        _ => "not one this kernel knows",
    }
}

fn r8(base: u64, reg: u64) -> u8 {
    // SAFETY: [mmio] base is the TIS window map_mmio gave init (4 KiB,
    // uncached), and reg is one of its byte registers.
    unsafe { core::ptr::read_volatile((base + reg) as *const u8) }
}

fn r32(base: u64, reg: u64) -> u32 {
    // SAFETY: [mmio] as r8; reg is a 4-byte-aligned 32-bit register.
    unsafe { core::ptr::read_volatile((base + reg) as *const u32) }
}

fn w8(base: u64, reg: u64, v: u8) {
    // SAFETY: [mmio] as r8; the writes made are TIS's (request and release
    // locality 0, commandReady, tpmGo, a command's bytes into the FIFO).
    unsafe { core::ptr::write_volatile((base + reg) as *mut u8, v) }
}

/// Look for a TPM and say what was found; start using it if it is QEMU's.
/// After the scheduler starts (the waits are in ticks). Returns what was
/// found and, if used, PCR 15 as it is now.
pub fn init() -> (Option<Found>, Option<[u8; 20]>) {
    // SAFETY: [mmio] 0xFED40000 is where TIS puts a TPM's registers on a
    // PC; if no TPM answers, reads there return all ones (or zeros) and
    // nothing is written below until a TPM has said who it is.
    let base = match unsafe { crate::vmm::map_mmio(TIS_BASE, 4096) } {
        Ok(va) => va,
        Err(_) => {
            FOUND.set(None);
            crate::serial_write_str(b"[tpm] the TIS window couldn't be mapped: no TPM used\n");
            return (None, None);
        }
    };
    let did_vid = r32(base, DID_VID);
    let vendor = did_vid as u16;
    if vendor == 0xFFFF || vendor == 0 {
        FOUND.set(None);
        crate::serial_write_str(b"[tpm] none found (nothing answers at 0xFED40000): the audit chain is not anchored\n");
        return (None, None);
    }
    let f = Found {
        vendor,
        device: (did_vid >> 16) as u16,
        revision: r8(base, RID),
        interface: ((r32(base, INTF_CAPABILITY) >> 28) & 7) as u8,
        used: false,
    };
    let f = Found { used: drives(f.vendor, f.interface), ..f };
    crate::serial_write_str(b"[tpm] found: vendor ");
    crate::serial_write_str(&hex16(f.vendor));
    crate::serial_write_str(b" (");
    crate::serial_write_str(vendor_name(f.vendor).as_bytes());
    crate::serial_write_str(b"), device ");
    crate::serial_write_str(&hex16(f.device));
    crate::serial_write_str(b", revision ");
    crate::serial_write_u64(f.revision as u64);
    crate::serial_write_str(if f.interface == 3 { b", a TPM 2.0 interface" } else { b", a TPM 1.2 interface" });
    if !f.used {
        crate::serial_write_str(if f.vendor == QEMU_VENDOR {
            b"; not used: this kernel speaks TPM 1.2\n" as &[u8]
        } else {
            b"; not used: this build drives only QEMU's emulated TPM (the T430's is Entry 34)\n"
        });
        FOUND.set(Some(f));
        return (Some(f), None);
    }
    crate::serial_write_str(b"\n");
    REGS.set(base);
    // The firmware starts a TPM (TPM_Startup) as the machine boots. If it
    // didn't (no TPM support in it), the TPM refuses everything until
    // something does: start it, clear, as the firmware would have.
    let mut pcr = pcr_read(PCR);
    if pcr == Err(Error::Tpm(TPM_INVALID_POSTINIT)) {
        crate::serial_write_str(b"[tpm] not started by the firmware: TPM_Startup(ST_CLEAR)\n");
        let _ = startup_clear();
        pcr = pcr_read(PCR);
    }
    match pcr {
        Ok(v) => {
            crate::serial_write_str(b"[tpm] PCR 15 at start: ");
            crate::serial_write_str(&hex20(&v));
            crate::serial_write_str(b"\n");
            FOUND.set(Some(f));
            (Some(f), Some(v))
        }
        Err(e) => {
            let f = Found { used: false, ..f };
            crate::serial_write_str(b"[tpm] PCR 15 can't be read (");
            say_error(e);
            crate::serial_write_str(b"): the audit chain is not anchored\n");
            FOUND.set(Some(f));
            (Some(f), None)
        }
    }
}

pub fn hex16(v: u16) -> [u8; 6] {
    let d = b"0123456789abcdef";
    [b'0', b'x', d[(v >> 12) as usize & 15], d[(v >> 8) as usize & 15], d[(v >> 4) as usize & 15], d[v as usize & 15]]
}

pub fn say_error(e: Error) {
    match e {
        Error::Tpm(rc) => {
            crate::serial_write_str(b"TPM return code ");
            crate::serial_write_hex(rc as u64);
        }
        Error::Timeout(what) => {
            crate::serial_write_str(b"timed out: ");
            crate::serial_write_str(what.as_bytes());
        }
        Error::NotInUse => crate::serial_write_str(b"no TPM in use"),
        Error::InterruptsOff => crate::serial_write_str(b"asked with interrupts off"),
        Error::Length => crate::serial_write_str(b"the command's length and the TPM's didn't agree"),
        Error::Malformed => crate::serial_write_str(b"not a TPM 1.2 response"),
    }
}

pub fn hex20(v: &[u8; 20]) -> [u8; 40] {
    let mut s = [0u8; 40];
    for (i, b) in v.iter().enumerate() {
        s[2 * i] = b"0123456789abcdef"[(b >> 4) as usize];
        s[2 * i + 1] = b"0123456789abcdef"[(b & 15) as usize];
    }
    s
}

/// Wait until `done`, for at most `limit` timer ticks.
fn wait(limit: u64, what: &'static str, mut done: impl FnMut() -> bool) -> Result<(), Error> {
    let start = crate::apic::ticks();
    loop {
        if done() {
            return Ok(());
        }
        if crate::apic::ticks() - start > limit {
            return Err(Error::Timeout(what));
        }
        core::hint::spin_loop();
    }
}

fn burst(base: u64) -> Result<usize, Error> {
    let mut n = 0;
    wait(TIMEOUT_A, "burstCount", || {
        n = ((r32(base, STS) >> 8) & 0xFFFF) as usize;
        n > 0
    })?;
    Ok(n)
}

/// Send one command and read its response into `out`. Returns the
/// response's length; its return code is the caller's to look at.
fn transact(cmd: &[u8], out: &mut [u8]) -> Result<usize, Error> {
    if !crate::cpu::interrupts_enabled() {
        return Err(Error::InterruptsOff);
    }
    let Some(&base) = REGS.get() else { return Err(Error::NotInUse) };
    let _one_at_a_time = COMMANDS.lock();

    // Locality 0.
    if r8(base, ACCESS) & ACCESS_ACTIVE == 0 {
        w8(base, ACCESS, ACCESS_REQUEST);
        wait(TIMEOUT_A, "locality 0", || r8(base, ACCESS) & (ACCESS_VALID | ACCESS_ACTIVE) == ACCESS_VALID | ACCESS_ACTIVE)?;
    }
    let result = (|| {
        // Ready for a command.
        w8(base, STS, STS_COMMAND_READY);
        wait(TIMEOUT_B, "commandReady", || r8(base, STS) & STS_COMMAND_READY != 0)?;

        // All but the last byte, as the TPM takes them; it must still
        // expect more. Then the last, after which it must expect none.
        let (body, last) = cmd.split_at(cmd.len() - 1);
        let mut sent = 0;
        while sent < body.len() {
            let n = burst(base)?.min(body.len() - sent);
            for &b in &body[sent..sent + n] {
                w8(base, DATA_FIFO, b);
            }
            sent += n;
            wait(TIMEOUT_C, "stsValid", || r8(base, STS) & STS_VALID != 0)?;
            if r8(base, STS) & STS_EXPECT == 0 {
                return Err(Error::Length);
            }
        }
        burst(base)?;
        w8(base, DATA_FIFO, last[0]);
        wait(TIMEOUT_C, "stsValid", || r8(base, STS) & STS_VALID != 0)?;
        if r8(base, STS) & STS_EXPECT != 0 {
            return Err(Error::Length);
        }

        // Run it, and wait for the response.
        w8(base, STS, STS_GO);
        wait(SHORT_DURATION, "the response", || r8(base, STS) & (STS_VALID | STS_DATA_AVAIL) == STS_VALID | STS_DATA_AVAIL)?;

        // The header, which says how long the rest is, then the rest.
        let mut got = 0;
        let mut want = 10;
        while got < want {
            let n = burst(base)?.min(want - got);
            for _ in 0..n {
                out[got] = r8(base, DATA_FIFO);
                got += 1;
            }
            if got == 10 && want == 10 {
                want = u32::from_be_bytes([out[2], out[3], out[4], out[5]]) as usize;
                if !(10..=out.len()).contains(&want) {
                    return Err(Error::Malformed);
                }
            }
        }
        Ok(got)
    })();
    // Whatever happened: back to ready (which also abandons a command left
    // half-done), and locality 0 let go.
    w8(base, STS, STS_COMMAND_READY);
    w8(base, ACCESS, ACCESS_ACTIVE);
    result
}

/// A TPM 1.2 command: tag TPM_TAG_RQU_COMMAND (0x00C1), its size, the
/// ordinal, then `params`. Ok: the response's parameters (after its
/// 10-byte header), when its return code is TPM_SUCCESS.
fn command(ordinal: u32, params: &[u8], out: &mut [u8; 64]) -> Result<usize, Error> {
    let mut cmd = [0u8; 64];
    let size = 10 + params.len();
    cmd[0..2].copy_from_slice(&0x00C1u16.to_be_bytes());
    cmd[2..6].copy_from_slice(&(size as u32).to_be_bytes());
    cmd[6..10].copy_from_slice(&ordinal.to_be_bytes());
    cmd[10..size].copy_from_slice(params);
    let n = transact(&cmd[..size], out)?;
    // TPM_TAG_RSP_COMMAND (0x00C4).
    if n < 10 || out[0..2] != [0x00, 0xC4] {
        return Err(Error::Malformed);
    }
    let rc = u32::from_be_bytes([out[6], out[7], out[8], out[9]]);
    if rc != TPM_SUCCESS {
        return Err(Error::Tpm(rc));
    }
    Ok(n - 10)
}

const TPM_ORD_EXTEND: u32 = 0x14;
const TPM_ORD_PCR_READ: u32 = 0x15;
const TPM_ORD_STARTUP: u32 = 0x99;
const TPM_ST_CLEAR: u16 = 0x0001;

/// TPM_PCRRead: a PCR's value.
pub fn pcr_read(index: u32) -> Result<[u8; 20], Error> {
    let mut out = [0u8; 64];
    let n = command(TPM_ORD_PCR_READ, &index.to_be_bytes(), &mut out)?;
    if n != 20 {
        return Err(Error::Malformed);
    }
    let mut v = [0u8; 20];
    v.copy_from_slice(&out[10..30]);
    Ok(v)
}

/// TPM_Extend: PCR `index` becomes SHA-1(itself || digest). Returns its
/// new value, as the TPM computed it.
pub fn extend(index: u32, digest: &[u8; 20]) -> Result<[u8; 20], Error> {
    let mut params = [0u8; 24];
    params[0..4].copy_from_slice(&index.to_be_bytes());
    params[4..24].copy_from_slice(digest);
    let mut out = [0u8; 64];
    let n = command(TPM_ORD_EXTEND, &params, &mut out)?;
    if n != 20 {
        return Err(Error::Malformed);
    }
    let mut v = [0u8; 20];
    v.copy_from_slice(&out[10..30]);
    Ok(v)
}

/// TPM_Startup(ST_CLEAR): what the firmware does as the machine boots. Only
/// for QEMU's TPM, and only when it says it hasn't been started.
fn startup_clear() -> Result<(), Error> {
    let mut out = [0u8; 64];
    command(TPM_ORD_STARTUP, &TPM_ST_CLEAR.to_be_bytes(), &mut out).map(|_| ())
}
