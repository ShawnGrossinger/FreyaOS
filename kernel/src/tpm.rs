//! tpm.rs — the TPM 1.2s this kernel drives, and what it may send them
//! (Entry 33: QEMU's emulated one; Entry 34: the T430's own).
//!
//! What it does with a TPM, and all it does: reads PCR 15, and extends it
//! with each sealed segment's seal (audit.rs: anchor). Both are commands
//! whose effects don't outlast a reboot: a PCR goes back to zeros when the
//! machine resets. Nothing here takes ownership, creates keys or counters,
//! or writes the TPM's non-volatile memory, and nothing here can: every
//! command goes through `run`, which sends only what `may_send` allows,
//! and a self-test checks that list on every boot (audit.rs).
//!
//! Which TPMs: only the two this build has been tried on, each named by
//! its vendor and device id (`CHIPS`): QEMU's emulated TPM 1.2 (swtpm
//! behind QEMU's TIS device), and the T430's, an STMicroelectronics TPM
//! 1.2 (vendor 0x104A, device 0x0000; Kali reports it as manufacturer
//! "STM ", TCG 1.2, firmware 13.12, enabled, active and owned). Any other
//! TPM is found, named and left alone. Only QEMU's may be started
//! (TPM_Startup) if the firmware didn't: the T430's firmware starts its
//! own, and this kernel doesn't do the firmware's part on real hardware.
//!
//! When anything goes wrong (the TPM times out, answers out of turn,
//! returns an error, or gives a PCR value that doesn't check out),
//! nothing more is sent to it until the next boot. A TPM that has
//! misbehaved once isn't asked again on the strength of a guess.
//!
//! The protocol itself, over the registers, is tis.rs's. Here: the
//! registers (memory-mapped at 0xFED40000, uncached), task context with
//! interrupts on (the waits are in timer ticks), one command at a time.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::lock::{BootCell, IrqLock};
use crate::sync::Mutex;
use crate::tis::{self, Bus};

pub use crate::tis::Error;

/// Where TIS puts locality 0's registers.
pub const TIS_BASE: u64 = 0xFED4_0000;

/// The PCR the audit chain extends: the last of PCRs 8 to 15, which the
/// TCG's PC Client specification leaves to the operating system. The
/// firmware measures into 0 to 7. The only PCR this kernel extends.
pub const PCR: u32 = 15;

/// A TPM this build has been tried on, and so drives.
pub struct Chip {
    pub vendor: u16,
    pub device: u16,
    pub name: &'static str,
    /// Whether this kernel may start it (TPM_Startup) if the firmware
    /// didn't.
    pub may_start: bool,
}

pub static CHIPS: [Chip; 2] = [
    Chip { vendor: 0x1014, device: 0x0001, name: "QEMU's emulated TPM 1.2", may_start: true },
    Chip { vendor: 0x104A, device: 0x0000, name: "the T430's STMicroelectronics TPM 1.2", may_start: false },
];

/// The TPM this build drives, if it is one: a chip in `CHIPS`, through a
/// TPM 1.2 interface (TIS's InterfaceVersion 0 or 2; 3 is a TPM 2.0).
/// Everything else is left alone. A function of its own so that a
/// self-test can check it on every boot, TPM or not.
pub fn drives(vendor: u16, device: u16, interface: u8) -> Option<&'static Chip> {
    if !matches!(interface, 0 | 2) {
        return None;
    }
    CHIPS.iter().find(|c| c.vendor == vendor && c.device == device)
}

/// The commands this kernel may send to `chip`: TPM_PCRRead and
/// TPM_Extend, and TPM_Startup to a chip it may start. Nothing else, ever.
pub fn may_send(chip: &Chip, ordinal: u32) -> bool {
    match ordinal {
        tis::ORD_PCR_READ | tis::ORD_EXTEND => true,
        tis::ORD_STARTUP => chip.may_start,
        _ => false,
    }
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

/// What was found at boot.
#[derive(Clone, Copy)]
pub struct Found {
    pub vendor: u16,
    pub device: u16,
    pub revision: u8,
    /// TIS's InterfaceVersion (INTF_CAPABILITY bits 28-30): 0 or 2 for a
    /// TPM 1.2, 3 for a TPM 2.0.
    pub interface: u8,
    /// The interrupt line the firmware left the TPM raising, if it left
    /// its interrupts on. Left as it is: this kernel polls.
    pub interrupts: Option<u8>,
    /// The chip this build drives it as; None: left alone.
    pub chip: Option<&'static Chip>,
}

static FOUND: BootCell<Option<Found>> = BootCell::new();
/// The registers' virtual address, once mapped, for a TPM this build drives.
static REGS: BootCell<u64> = BootCell::new();
/// One command at a time.
static COMMANDS: Mutex<()> = Mutex::new(());
/// What stopped it: after this, nothing more is sent until the next boot.
static STOPPED: IrqLock<Option<Error>> = IrqLock::new(None);
/// Commands sent (each try of one), and register reads and writes made.
static SENT: AtomicU64 = AtomicU64::new(0);
static ACCESSES: AtomicU64 = AtomicU64::new(0);
/// FREYA_DEMO=tpm_lost: from now on every register reads 0xFF and writes
/// go nowhere, as with a chip gone from the bus.
static LOST: AtomicBool = AtomicBool::new(false);

pub fn found() -> Option<Found> {
    FOUND.get().copied().flatten()
}

/// Whether this build drives the TPM found (stopped or not).
pub fn driven() -> bool {
    found().is_some_and(|f| f.chip.is_some())
}

/// What stopped it, if anything did.
pub fn stopped() -> Option<Error> {
    *STOPPED.lock()
}

/// Driven, and not stopped: commands may still be sent.
pub fn in_use() -> bool {
    driven() && stopped().is_none()
}

/// Commands sent so far (each try of one), for the self-test and demo.
pub fn sent() -> u64 {
    SENT.load(Ordering::Relaxed)
}

/// Stop: nothing more goes to the TPM this boot. The first reason stays.
/// `during`: what was being done, for the line saying so.
pub fn stop(e: Error, during: &[u8]) {
    {
        let mut s = STOPPED.lock();
        if s.is_some() {
            return;
        }
        *s = Some(e);
    }
    crate::serial_write_str(b"[tpm] ");
    crate::serial_write_str(during);
    crate::serial_write_str(b": ");
    say_error(e);
    crate::serial_write_str(b"; nothing more is sent to the TPM this boot\n");
}

fn lost() -> bool {
    option_env!("FREYA_DEMO") == Some("tpm_lost") && LOST.load(Ordering::Relaxed)
}

struct Mmio {
    base: u64,
}

impl Bus for Mmio {
    fn r8(&mut self, reg: u64) -> u8 {
        ACCESSES.fetch_add(1, Ordering::Relaxed);
        if lost() {
            return 0xFF;
        }
        // SAFETY: [mmio] base is the TIS window map_mmio gave init (4 KiB,
        // uncached), and reg one of its byte registers (tis.rs).
        unsafe { core::ptr::read_volatile((self.base + reg) as *const u8) }
    }

    fn r32(&mut self, reg: u64) -> u32 {
        ACCESSES.fetch_add(1, Ordering::Relaxed);
        if lost() {
            return 0xFFFF_FFFF;
        }
        // SAFETY: [mmio] as r8; reg is a 4-byte-aligned 32-bit register.
        unsafe { core::ptr::read_volatile((self.base + reg) as *const u32) }
    }

    fn w8(&mut self, reg: u64, v: u8) {
        ACCESSES.fetch_add(1, Ordering::Relaxed);
        if lost() {
            return;
        }
        // SAFETY: [mmio] as r8; the writes tis.rs makes are TIS's (request
        // and release locality 0, commandReady, tpmGo, a command's bytes
        // into the FIFO), and only after `run` let the command through.
        unsafe { core::ptr::write_volatile((self.base + reg) as *mut u8, v) }
    }

    fn ticks(&mut self) -> u64 {
        crate::apic::ticks()
    }

    fn pause(&mut self, ticks: u64) {
        crate::sched::sleep_ticks(ticks);
    }
}

/// Every command goes through here: to a TPM this build drives, one it may
/// send to that TPM, not after a stop, with interrupts on, one at a time.
/// Anything that goes wrong at the TPM stops it.
fn run<T>(ordinal: u32, during: &[u8], f: impl FnOnce(&mut Mmio) -> Result<T, Error>) -> Result<T, Error> {
    let Some(chip) = found().and_then(|f| f.chip) else { return Err(Error::NotInUse) };
    let Some(&base) = REGS.get() else { return Err(Error::NotInUse) };
    if !may_send(chip, ordinal) {
        return Err(Error::Refused);
    }
    if !crate::cpu::interrupts_enabled() {
        return Err(Error::InterruptsOff);
    }
    let _one_at_a_time = COMMANDS.lock();
    if stopped().is_some() {
        return Err(Error::Stopped);
    }
    SENT.fetch_add(1, Ordering::Relaxed);
    let result = f(&mut Mmio { base });
    if let Err(e) = result {
        // A TPM the firmware didn't start, that this kernel may start:
        // init starts it rather than stopping.
        let not_started = e == Error::Tpm(tis::TPM_INVALID_POSTINIT) && chip.may_start;
        if e.from_the_tpm() && !not_started {
            stop(e, during);
        }
    }
    result
}

/// TPM_PCRRead.
pub fn pcr_read(index: u32) -> Result<[u8; 20], Error> {
    run(tis::ORD_PCR_READ, b"reading a PCR", |b| tis::pcr_read(b, index))
}

/// TPM_Extend of PCR 15, the only PCR this kernel extends. Returns its new
/// value, as the TPM computed it.
pub fn extend(digest: &[u8; 20]) -> Result<[u8; 20], Error> {
    run(tis::ORD_EXTEND, b"extending PCR 15", |b| tis::extend(b, PCR, digest))
}

fn startup_clear() -> Result<(), Error> {
    run(tis::ORD_STARTUP, b"starting it (TPM_Startup)", tis::startup_clear)
}

/// Look for a TPM and say what was found; start using it if this build
/// drives it. After the scheduler starts (the waits are in ticks).
/// Returns what was found and, if driven, PCR 15 as it is now.
pub fn init() -> (Option<Found>, Option<[u8; 20]>) {
    // SAFETY: [mmio] 0xFED40000 is where TIS puts a TPM's registers on a
    // PC; if no TPM answers, reads there return all ones (or zeros), and
    // until a TPM has said who it is, and is one this build drives,
    // nothing is written there.
    let base = match unsafe { crate::vmm::map_mmio(TIS_BASE, 4096) } {
        Ok(va) => va,
        Err(_) => {
            FOUND.set(None);
            crate::serial_write_str(b"[tpm] the TIS window couldn't be mapped: no TPM used\n");
            return (None, None);
        }
    };
    let mut bus = Mmio { base };
    let did_vid = bus.r32(tis::DID_VID);
    let vendor = did_vid as u16;
    if vendor == 0xFFFF || vendor == 0 {
        FOUND.set(None);
        crate::serial_write_str(b"[tpm] none found (nothing answers at 0xFED40000): the audit chain is not anchored\n");
        return (None, None);
    }
    // Reads only, all of them: who it is, which interface, and whether the
    // firmware left its interrupts on.
    let device = (did_vid >> 16) as u16;
    let revision = bus.r8(tis::RID);
    let interface = ((bus.r32(tis::INTF_CAPABILITY) >> 28) & 7) as u8;
    let interrupts = if bus.r32(tis::INT_ENABLE) & tis::INT_GLOBAL != 0 { Some(bus.r8(tis::INT_VECTOR) & 0x0F) } else { None };
    let f = Found { vendor, device, revision, interface, interrupts, chip: drives(vendor, device, interface) };
    FOUND.set(Some(f));

    crate::serial_write_str(b"[tpm] found: vendor ");
    crate::serial_write_str(&hex16(f.vendor));
    crate::serial_write_str(b" (");
    crate::serial_write_str(vendor_name(f.vendor).as_bytes());
    crate::serial_write_str(b"), device ");
    crate::serial_write_str(&hex16(f.device));
    crate::serial_write_str(b", revision ");
    crate::serial_write_u64(f.revision as u64);
    crate::serial_write_str(if matches!(f.interface, 0 | 2) { b", a TPM 1.2 interface" } else { b", a TPM 2.0 interface" });
    match f.interrupts {
        None => crate::serial_write_str(b", its interrupts off\n"),
        Some(line) => {
            crate::serial_write_str(b", its interrupts on (line ");
            crate::serial_write_u64(line as u64);
            crate::serial_write_str(b"; left so: this kernel polls)\n");
        }
    }
    let Some(chip) = f.chip else {
        crate::serial_write_str(if matches!(f.interface, 0 | 2) {
            b"[tpm] left alone: not a TPM this build has been tried on (it drives QEMU's emulated TPM 1.2 and the T430's STMicroelectronics TPM 1.2, no other)\n" as &[u8]
        } else {
            b"[tpm] left alone: this kernel speaks TPM 1.2\n"
        });
        return (Some(f), None);
    };
    crate::serial_write_str(b"[tpm] driven as ");
    crate::serial_write_str(chip.name.as_bytes());
    crate::serial_write_str(if chip.may_start {
        b": PCR 15 read and extended, and nothing else sent but TPM_Startup if the firmware didn't\n" as &[u8]
    } else {
        b": PCR 15 read and extended, and nothing else sent\n"
    });
    REGS.set(base);

    let mut pcr = pcr_read(PCR);
    if pcr == Err(Error::Tpm(tis::TPM_INVALID_POSTINIT)) && chip.may_start {
        // (Any other chip run has stopped, and left alone.)
        crate::serial_write_str(b"[tpm] not started by the firmware: TPM_Startup(ST_CLEAR)\n");
        pcr = startup_clear().and_then(|()| pcr_read(PCR));
    }
    match pcr {
        Ok(v) => {
            crate::serial_write_str(b"[tpm] PCR 15 at start: ");
            crate::serial_write_str(&hex20(&v));
            crate::serial_write_str(b"\n");
            (Some(f), Some(v))
        }
        // run has stopped it and said why.
        Err(_) => (Some(f), None),
    }
}

/// FREYA_DEMO=tpm_lost: the TPM stops answering, as if gone from the bus.
/// The next command fails and stops it; after that, commands asked for are
/// refused without a register touched.
pub fn demo_lost() {
    if !in_use() {
        crate::serial_write_str(b"[tpm] demo: no TPM in use, nothing to show (make run-tpm)\n");
        return;
    }
    crate::serial_write_str(b"[tpm] demo: from here every TPM register reads 0xff and writes go nowhere, as with a chip gone from the bus\n");
    LOST.store(true, Ordering::Relaxed);
    let first = pcr_read(PCR);
    crate::serial_write_str(b"[tpm] demo: PCR 15 read: ");
    match first {
        Ok(_) => crate::serial_write_str(b"an answer (NOT expected)"),
        Err(e) => say_error(e),
    }
    crate::serial_write_str(if stopped().is_some() { b"; stopped" } else { b"; NOT stopped" });
    crate::serial_write_str(b"\n");
    let (sent_before, touched) = (sent(), ACCESSES.load(Ordering::Relaxed));
    let again = (pcr_read(PCR), extend(&[0x5A; 20]));
    let refused = again == (Err(Error::Stopped), Err(Error::Stopped));
    crate::serial_write_str(b"[tpm] demo: asked twice more (a read, an extend): ");
    crate::serial_write_str(if refused { b"both refused" } else { b"NOT both refused" });
    crate::serial_write_str(b", with ");
    crate::serial_write_u64(sent() - sent_before);
    crate::serial_write_str(b" commands sent and ");
    crate::serial_write_u64(ACCESSES.load(Ordering::Relaxed) - touched);
    crate::serial_write_str(b" registers touched\n");
}

pub fn hex16(v: u16) -> [u8; 6] {
    let d = b"0123456789abcdef";
    [b'0', b'x', d[(v >> 12) as usize & 15], d[(v >> 8) as usize & 15], d[(v >> 4) as usize & 15], d[v as usize & 15]]
}

pub fn hex20(v: &[u8; 20]) -> [u8; 40] {
    let mut s = [0u8; 40];
    for (i, b) in v.iter().enumerate() {
        s[2 * i] = b"0123456789abcdef"[(b >> 4) as usize];
        s[2 * i + 1] = b"0123456789abcdef"[(b & 15) as usize];
    }
    s
}

pub fn say_error(e: Error) {
    match e {
        Error::Tpm(rc) => {
            crate::serial_write_str(b"TPM return code ");
            crate::serial_write_hex(rc as u64);
            let name = tis::rc_name(rc);
            if !name.is_empty() {
                crate::serial_write_str(b" (");
                crate::serial_write_str(name.as_bytes());
                crate::serial_write_str(b")");
            }
        }
        Error::Timeout(what) => {
            crate::serial_write_str(b"timed out waiting for ");
            crate::serial_write_str(what.as_bytes());
        }
        Error::Length(what) | Error::Wrong(what) => crate::serial_write_str(what.as_bytes()),
        Error::Malformed => crate::serial_write_str(b"not a TPM 1.2 response"),
        Error::NotInUse => crate::serial_write_str(b"no TPM in use"),
        Error::InterruptsOff => crate::serial_write_str(b"asked with interrupts off"),
        Error::Refused => crate::serial_write_str(b"a command this build never sends to this TPM"),
        Error::Stopped => crate::serial_write_str(b"stopped after an earlier error"),
    }
}
