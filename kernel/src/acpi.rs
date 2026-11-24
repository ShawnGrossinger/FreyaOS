//! acpi.rs — reading the firmware's ACPI tables.
//!
//! The firmware describes the machine in tables in memory. FreyaOS reads
//! three of them, and only reads them (it runs none of the firmware's AML
//! code):
//!
//!   - MADT ("APIC"): where the I/O APIC is, and how each ISA interrupt is
//!     wired to it (an "interrupt source override" may move a line to
//!     another input or change its polarity and trigger).
//!   - FADT ("FACP"): the power-management I/O ports (PM1 control, used to
//!     power off), how to switch the firmware into ACPI mode, and the reset
//!     register (used to restart).
//!   - DSDT: only for the \_S5 object, the value that means "soft off" for
//!     this machine. The DSDT is AML byte code; this finds \_S5 by a known
//!     pattern instead of running it, as most small kernels do.
//!
//! Finding them: the RSDP ("RSD PTR ") lives in the first KiB of the EBDA
//! or in 0xE0000-0xFFFFF; it points at the RSDT (32-bit entries) or, from
//! ACPI 2.0, the XSDT (64-bit entries), which list the other tables. Every
//! table carries a checksum; tables that fail it are ignored and counted.
//!
//! Tables usually sit in RAM or ACPI memory, which the physical map covers.
//! Some firmware puts them in "reserved" memory, which it doesn't; those
//! pages are mapped read-only through the device window instead.

use crate::paging::{self, PHYS_MAP_BASE};
use crate::vmm::{self, AddressSpace};

/// How one ISA interrupt (0-15) reaches the I/O APIC.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct IsaRoute {
    pub gsi: u32,
    pub active_low: bool,
    pub level: bool,
}

#[derive(Clone, Copy)]
pub struct IoApic {
    pub phys: u64,
    pub gsi_base: u32,
    pub id: u8,
}

pub struct Info {
    pub revision: u8,
    pub via_xsdt: bool,
    /// Entries in the RSDT/XSDT, and how many of them were readable tables.
    pub listed: u32,
    pub tables: u32,
    pub bad_checksums: u32,
    pub ioapic: Option<IoApic>,
    pub isa: [IsaRoute; 16],
    pub overrides: u32,
    pub pm1a_control: u16,
    pub pm1b_control: u16,
    pub smi_command: u16,
    pub acpi_enable: u8,
    pub sci: u16,
    /// \_S5: the SLP_TYP values for PM1a and PM1b that mean "soft off".
    pub s5: Option<(u8, u8)>,
    /// FADT reset register, when it is an I/O port: (port, value).
    pub reset: Option<(u16, u8)>,
}

static mut INFO: Option<Info> = None;

pub fn info() -> Option<&'static Info> {
    // SAFETY: [static] INFO is written once, by init(), whose only caller is
    // main at boot, before any task exists; after that it is only read, so
    // the 'static shared reference never sees it change.
    unsafe { (*(&raw const INFO)).as_ref() }
}

// ===========================================================================
// Reading physical memory
// ===========================================================================

/// A pointer to `len` bytes of physical memory at `phys`: through the
/// physical map if it covers them, otherwise freshly mapped (uncached,
/// which is fine for reading tables) through the device window.
fn view(phys: u64, len: u64) -> Option<*const u8> {
    if len == 0 || len > 4 << 20 || phys.checked_add(len).is_none() {
        return None;
    }
    let kernel = AddressSpace::kernel();
    let first = phys & !0xFFF;
    let last = (phys + len - 1) & !0xFFF;
    let covered = (first..=last).step_by(0x1000).all(|p| kernel.translate(PHYS_MAP_BASE + p).is_some());
    if covered {
        return Some(paging::phys_to_virt(phys) as *const u8);
    }
    // SAFETY: [mmio] firmware tables the physical map doesn't cover (they
    // sit in memory the E820 map calls reserved), mapped uncached for
    // reading; no RAM the frame allocator hands out is among them.
    unsafe { vmm::map_mmio(phys, len).ok().map(|va| va as *const u8) }
}

/// A table's bytes, read one at a time with bounds checks that can't
/// panic (a panic path would pull core::fmt into the kernel).
struct Bytes {
    ptr: *const u8,
    len: usize,
}

impl Bytes {
    fn u8(&self, at: usize) -> u8 {
        if at < self.len {
            // SAFETY: [mmio] ptr is view()'s, valid for len bytes, and at
            // is below len.
            unsafe { self.ptr.add(at).read_volatile() }
        } else {
            0
        }
    }
    fn u16(&self, at: usize) -> u16 {
        self.u8(at) as u16 | (self.u8(at + 1) as u16) << 8
    }
    fn u32(&self, at: usize) -> u32 {
        self.u16(at) as u32 | (self.u16(at + 2) as u32) << 16
    }
    fn u64(&self, at: usize) -> u64 {
        self.u32(at) as u64 | (self.u32(at + 4) as u64) << 32
    }
    fn is(&self, at: usize, text: &[u8]) -> bool {
        text.iter().enumerate().all(|(i, &b)| self.u8(at + i) == b)
    }
    fn sums_to_zero(&self) -> bool {
        (0..self.len).fold(0u8, |sum, i| sum.wrapping_add(self.u8(i))) == 0
    }
}

fn bytes(phys: u64, len: u64) -> Option<Bytes> {
    view(phys, len).map(|ptr| Bytes { ptr, len: len as usize })
}

/// A system description table at `phys`: header first (for its length),
/// then the whole table.
fn table(phys: u64) -> Option<Bytes> {
    let header = bytes(phys, 36)?;
    let len = header.u32(4) as u64;
    if len < 36 {
        return None;
    }
    bytes(phys, len)
}

// ===========================================================================
// Finding and parsing
// ===========================================================================

/// The RSDP, from the EBDA's first KiB or the BIOS area 0xE0000-0xFFFFF.
fn find_rsdp() -> Option<u64> {
    let ebda = bytes(0x40E, 2).map(|b| (b.u16(0) as u64) << 4).unwrap_or(0);
    let mut areas = [(0xE0000u64, 0x20000u64), (0, 0)];
    if ebda >= 0x80000 && ebda < 0xA0000 {
        areas[1] = (ebda, 1024);
    }
    for &(start, len) in areas.iter().rev() {
        if len == 0 {
            continue;
        }
        let area = bytes(start, len)?;
        for at in (0..len as usize).step_by(16) {
            if area.is(at, b"RSD PTR ") {
                let v1 = bytes(start + at as u64, 20)?;
                if v1.sums_to_zero() {
                    return Some(start + at as u64);
                }
            }
        }
    }
    None
}

/// Read everything FreyaOS needs. Call once, after the physical map and the
/// device window work.
pub fn init() -> Result<&'static Info, &'static [u8]> {
    let rsdp_phys = find_rsdp().ok_or(&b"no ACPI tables (RSDP not found)"[..])?;
    let info = parse(rsdp_phys)?;
    // SAFETY: [static] main calls init once, at boot, before anything calls
    // info(): no reference to INFO exists yet.
    unsafe { *(&raw mut INFO) = Some(info) };
    self::info().ok_or(&b"unreachable"[..])
}

/// Everything from the tables an RSDP at `rsdp_phys` leads to.
fn parse(rsdp_phys: u64) -> Result<Info, &'static [u8]> {
    let rsdp = bytes(rsdp_phys, 36).ok_or(&b"RSDP unreadable"[..])?;
    let revision = rsdp.u8(15);
    let xsdt = if revision >= 2 && bytes(rsdp_phys, rsdp.u32(20).max(36) as u64).is_some_and(|b| b.sums_to_zero()) {
        rsdp.u64(24)
    } else {
        0
    };
    let (root_phys, entry_size) = if xsdt != 0 { (xsdt, 8) } else { (rsdp.u32(16) as u64, 4) };
    let root = table(root_phys).ok_or(&b"RSDT/XSDT unreadable"[..])?;
    if !root.sums_to_zero() {
        return Err(b"RSDT/XSDT checksum is wrong");
    }

    let entries = (root.len - 36) / entry_size;
    let mut info = Info {
        revision,
        via_xsdt: xsdt != 0,
        listed: entries as u32,
        tables: 0,
        bad_checksums: 0,
        ioapic: None,
        isa: core::array::from_fn(|irq| IsaRoute { gsi: irq as u32, active_low: false, level: false }),
        overrides: 0,
        pm1a_control: 0,
        pm1b_control: 0,
        smi_command: 0,
        acpi_enable: 0,
        sci: 0,
        s5: None,
        reset: None,
    };

    for i in 0..entries {
        let at = 36 + i * entry_size;
        let phys = if entry_size == 8 { root.u64(at) } else { root.u32(at) as u64 };
        let Some(t) = table(phys) else { continue };
        if !t.sums_to_zero() {
            info.bad_checksums += 1;
            continue;
        }
        info.tables += 1;
        if t.is(0, b"APIC") {
            parse_madt(&t, &mut info);
        } else if t.is(0, b"FACP") {
            parse_fadt(&t, &mut info);
        }
    }

    Ok(info)
}

/// MADT: entries from offset 44, each [type, length, ...].
fn parse_madt(t: &Bytes, info: &mut Info) {
    let mut at = 44;
    while at + 2 <= t.len {
        let (kind, len) = (t.u8(at), t.u8(at + 1) as usize);
        if len < 2 {
            break;
        }
        match kind {
            // I/O APIC: id, -, address, first GSI. Keep the one with the
            // ISA interrupts (GSI 0 up).
            1 if len >= 12 => {
                let ioapic = IoApic { id: t.u8(at + 2), phys: t.u32(at + 4) as u64, gsi_base: t.u32(at + 8) };
                if info.ioapic.is_none() || ioapic.gsi_base == 0 {
                    info.ioapic = Some(ioapic);
                }
            }
            // Interrupt source override: bus 0 (ISA), source IRQ, GSI, flags
            // (polarity bits 0-1: 1 high, 3 low; trigger bits 2-3: 1 edge,
            // 3 level; 0 = the ISA default, high and edge).
            2 if len >= 10 && t.u8(at + 2) == 0 => {
                let irq = t.u8(at + 3) as usize;
                let flags = t.u16(at + 8);
                if let Some(route) = info.isa.get_mut(irq) {
                    *route = IsaRoute { gsi: t.u32(at + 4), active_low: flags & 3 == 3, level: (flags >> 2) & 3 == 3 };
                    info.overrides += 1;
                }
            }
            _ => {}
        }
        at += len;
    }
}

/// FADT: the fields at their ACPI offsets (the table grew over versions,
/// so later fields are read only if it is long enough).
fn parse_fadt(t: &Bytes, info: &mut Info) {
    info.sci = t.u16(46);
    info.smi_command = t.u32(48) as u16;
    info.acpi_enable = t.u8(52);
    info.pm1a_control = t.u32(64) as u16;
    info.pm1b_control = t.u32(68) as u16;
    // ACPI 2.0+: the extended (GAS) forms, used if the old ones are 0 and
    // they are I/O ports (address space 1).
    if info.pm1a_control == 0 && t.len >= 184 && t.u8(172) == 1 {
        info.pm1a_control = t.u64(176) as u16;
    }
    if info.pm1b_control == 0 && t.len >= 196 && t.u8(184) == 1 {
        info.pm1b_control = t.u64(188) as u16;
    }
    // Reset register (flags bit 10), if it is an I/O port.
    const RESET_REG_SUP: u32 = 1 << 10;
    if t.len >= 129 && t.u32(112) & RESET_REG_SUP != 0 && t.u8(116) == 1 && t.u64(120) <= 0xFFFF {
        info.reset = Some((t.u64(120) as u16, t.u8(128)));
    }
    let dsdt = if t.len >= 148 && t.u64(140) != 0 { t.u64(140) } else { t.u32(40) as u64 };
    if let Some(d) = table(dsdt) {
        if d.is(0, b"DSDT") {
            info.s5 = find_s5(&d);
        }
    }
}

/// \_S5: `Name (_S5, Package (n) { SLP_TYPa, SLP_TYPb, ... })` in AML is
/// NameOp (0x08), optionally '\', "_S5_", PackageOp (0x12), a package
/// length (1-4 bytes: the top two bits of the first say how many follow),
/// the element count, then the two integers.
fn find_s5(d: &Bytes) -> Option<(u8, u8)> {
    for at in 36..d.len.saturating_sub(4) {
        if !d.is(at, b"_S5_") {
            continue;
        }
        let named = d.u8(at - 1) == 0x08 || (d.u8(at - 1) == b'\\' && d.u8(at - 2) == 0x08);
        if !named || d.u8(at + 4) != 0x12 {
            continue;
        }
        let mut p = at + 5;
        p += 1 + (d.u8(p) >> 6) as usize; // package length
        p += 1; // element count
        let (a, next) = aml_integer(d, p)?;
        let (b, _) = aml_integer(d, next)?;
        return Some((a as u8 & 7, b as u8 & 7));
    }
    None
}

/// An AML integer constant at `p`: (value, position after it).
fn aml_integer(d: &Bytes, p: usize) -> Option<(u64, usize)> {
    match d.u8(p) {
        0x00 => Some((0, p + 1)),                         // ZeroOp
        0x01 => Some((1, p + 1)),                         // OneOp
        0x0A => Some((d.u8(p + 1) as u64, p + 2)),        // BytePrefix
        0x0B => Some((d.u16(p + 1) as u64, p + 3)),       // WordPrefix
        0x0C => Some((d.u32(p + 1) as u64, p + 5)),       // DWordPrefix
        _ => None,
    }
}

// ===========================================================================
// Self-test
// ===========================================================================

use crate::selftest::verdict;
use crate::{serial_write_hex as hex, serial_write_str as s, serial_write_u64 as dec};

/// Tables shaped like an ACPI 2.0+ laptop's (the T430's kind), which QEMU's
/// firmware never produces: an RSDP pointing at an XSDT, a FADT whose
/// PM1a control and DSDT are only in the 64-bit fields, a reset register,
/// level-triggered and active-low overrides, and \_S5 written with byte
/// prefixes. The kernel image is identity-mapped RAM, so this buffer's
/// address is also its physical address.
#[repr(C, align(4096))]
struct Synthetic([u8; 1024]);
static mut SYNTHETIC: Synthetic = Synthetic([0; 1024]);

fn parse_synthetic() -> Option<Info> {
    const RSDP: usize = 0;
    const XSDT: usize = 64;
    const FADT: usize = 128;
    const MADT: usize = 400;
    const DSDT: usize = 500;
    // SAFETY: [static] only this function touches SYNTHETIC, and only
    // selftest calls it, once, at boot.
    let t = unsafe { &mut (*(&raw mut SYNTHETIC)).0 };
    let base = t.as_ptr() as u64;
    let put = |t: &mut [u8; 1024], at: usize, bytes: &[u8]| {
        for (i, &b) in bytes.iter().enumerate() {
            if let Some(slot) = t.get_mut(at + i) {
                *slot = b;
            }
        }
    };
    let checksum = |t: &mut [u8; 1024], at: usize, len: usize, field: usize| {
        let sum = (at..at + len).fold(0u8, |s, i| s.wrapping_add(*t.get(i).unwrap_or(&0)));
        if let Some(slot) = t.get_mut(field) {
            *slot = slot.wrapping_sub(sum);
        }
    };
    let header = |t: &mut [u8; 1024], at: usize, sig: &[u8], len: u32| {
        put(t, at, sig);
        put(t, at + 4, &len.to_le_bytes());
        put(t, at + 8, &[2]);
    };

    // RSDP, revision 2: XSDT only (the 32-bit RSDT address is 0).
    put(t, RSDP, b"RSD PTR ");
    put(t, RSDP + 15, &[2]);
    put(t, RSDP + 20, &36u32.to_le_bytes());
    put(t, RSDP + 24, &(base + XSDT as u64).to_le_bytes());
    checksum(t, RSDP, 20, RSDP + 8);
    checksum(t, RSDP, 36, RSDP + 32);

    header(t, XSDT, b"XSDT", 52);
    put(t, XSDT + 36, &(base + FADT as u64).to_le_bytes());
    put(t, XSDT + 44, &(base + MADT as u64).to_le_bytes());
    checksum(t, XSDT, 52, XSDT + 9);

    header(t, FADT, b"FACP", 244);
    put(t, FADT + 46, &9u16.to_le_bytes()); // SCI
    put(t, FADT + 48, &0xB2u32.to_le_bytes()); // SMI command
    put(t, FADT + 52, &[0xF0]); // ACPI_ENABLE
    put(t, FADT + 112, &(1u32 << 10).to_le_bytes()); // RESET_REG_SUP
    put(t, FADT + 116, &[1, 8, 0, 1]); // reset register: I/O space
    put(t, FADT + 120, &0xCF9u64.to_le_bytes());
    put(t, FADT + 128, &[0x06]);
    put(t, FADT + 140, &(base + DSDT as u64).to_le_bytes()); // X_DSDT only
    put(t, FADT + 172, &[1, 16, 0, 2]); // X_PM1a_CNT_BLK: I/O space
    put(t, FADT + 176, &0x1004u64.to_le_bytes());
    checksum(t, FADT, 244, FADT + 9);

    header(t, MADT, b"APIC", 44 + 12 + 3 * 10);
    put(t, MADT + 36, &0xFEE0_0000u32.to_le_bytes());
    put(t, MADT + 44, &[1, 12, 2, 0]); // I/O APIC id 2
    put(t, MADT + 48, &0xFEC0_0000u32.to_le_bytes());
    put(t, MADT + 56, &[2, 10, 0, 0, 2, 0, 0, 0, 0, 0]); // IRQ 0 -> GSI 2
    put(t, MADT + 66, &[2, 10, 0, 9, 9, 0, 0, 0, 0x0D, 0]); // IRQ 9: level, high
    put(t, MADT + 76, &[2, 10, 0, 10, 10, 0, 0, 0, 0x0F, 0]); // IRQ 10: level, low
    checksum(t, MADT, 86, MADT + 9);

    // DSDT: some unrelated AML, then Name (\_S5, Package (4) { 7, 7, 0, 0 }).
    let aml: [u8; 21] = [0x10, 0x05, b'_', b'S', b'B', b'_', 0x08, b'\\', b'_', b'S', b'5', b'_', 0x12, 0x0A, 0x04, 0x0A, 0x07, 0x0A, 0x07, 0x00, 0x00];
    header(t, DSDT, b"DSDT", 36 + aml.len() as u32);
    put(t, DSDT + 36, &aml);
    checksum(t, DSDT, 36 + aml.len(), DSDT + 9);

    parse(base + RSDP as u64).ok()
}

pub fn selftest(result: &Result<&'static Info, &'static [u8]>) -> bool {
    let synthetic_ok = parse_synthetic().is_some_and(|i| {
        let io = i.ioapic.unwrap_or(IoApic { phys: 0, gsi_base: 99, id: 0 });
        i.via_xsdt
            && i.revision == 2
            && i.listed == 2
            && i.tables == 2
            && i.pm1a_control == 0x1004
            && i.smi_command == 0xB2
            && i.acpi_enable == 0xF0
            && i.sci == 9
            && i.reset == Some((0xCF9, 6))
            && i.s5 == Some((7, 7))
            && io.phys == 0xFEC0_0000
            && io.id == 2
            && io.gsi_base == 0
            && i.overrides == 3
            && i.isa[0] == IsaRoute { gsi: 2, active_low: false, level: false }
            && i.isa[9] == IsaRoute { gsi: 9, active_low: false, level: true }
            && i.isa[10] == IsaRoute { gsi: 10, active_low: true, level: true }
            && i.isa[1] == IsaRoute { gsi: 1, active_low: false, level: false }
    });
    let synthetic_ok = crate::selftest::check(
        b"acpi",
        b"ACPI 2.0 tables like a laptop's (XSDT, 64-bit FADT fields, reset register, level/low overrides, byte-prefixed \\_S5) read correctly",
        synthetic_ok,
    );

    let info = match result {
        Ok(i) => i,
        Err(why) => {
            s(b"[acpi] self-test ACPI tables: ");
            s(why);
            return verdict(false) && synthetic_ok;
        }
    };
    let mut ok = synthetic_ok;
    s(b"[acpi] self-test ACPI ");
    s(if info.revision >= 2 { b"2.0+" } else { b"1.0" });
    s(if info.via_xsdt { b" via XSDT: " } else { b" via RSDT: " });
    dec(info.listed as u64);
    s(b" listed, ");
    dec(info.tables as u64);
    s(b" with good checksums, ");
    dec(info.bad_checksums as u64);
    s(b" bad; I/O APIC id ");
    let ioapic = info.ioapic.unwrap_or(IoApic { phys: 0, gsi_base: 0, id: 0 });
    dec(ioapic.id as u64);
    s(b" at ");
    hex(ioapic.phys);
    s(b", ");
    dec(info.overrides as u64);
    s(b" interrupt overrides; FADT found");
    ok &= verdict(info.ioapic.is_some() && info.pm1a_control != 0);

    s(b"[acpi] self-test power off possible: \\_S5 SLP_TYPa=");
    let (a, _) = info.s5.unwrap_or((0xFF, 0xFF));
    dec(a as u64);
    s(b", PM1a control port ");
    hex(info.pm1a_control as u64);
    s(b"; restart via ");
    match info.reset {
        Some((port, value)) => {
            s(b"reset register port ");
            hex(port as u64);
            s(b" value ");
            hex(value as u64);
        }
        None => s(b"the keyboard controller"),
    }
    ok &= verdict(info.s5.is_some() && info.pm1a_control != 0);
    ok
}
