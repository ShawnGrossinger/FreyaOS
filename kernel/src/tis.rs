//! tis.rs — talking to a TPM 1.2 through its TIS registers (Entry 34):
//! the protocol, apart from the registers it runs over.
//!
//! The kernel runs it over the memory-mapped registers at 0xFED40000
//! (tpm.rs). build/tis_check.rs runs this same file over a model of a TPM
//! that is stricter than QEMU's: one that takes a byte at a time, sets its
//! status bits late, stalls its burst count, needs commandReady twice when
//! the firmware left a response unread, and answers "busy, try again" at
//! first. QEMU's emulated TPM does none of that; a real chip may do any of
//! it, and the T430's is real.
//!
//! The interface is the TCG PC Client TPM Interface Specification (TIS)
//! 1.2, locality 0. A command is written into the data FIFO as the TPM's
//! burst count allows, the TPM saying after each burst whether it expects
//! more; started with tpmGo; and its response read back once dataAvail is
//! set: a 10-byte header that gives the whole length, then the rest.
//! Commands and responses are TPM 1.2 structures: big-endian, the header
//! (tag, size, then the ordinal or the return code) and the parameters.
//!
//! What may be sent, and to which TPM, is tpm.rs's to decide: this file
//! carries out a command it is given.

/// The registers a transaction is run over, and the time.
pub trait Bus {
    fn r8(&mut self, reg: u64) -> u8;
    fn r32(&mut self, reg: u64) -> u32;
    fn w8(&mut self, reg: u64, v: u8);
    /// Timer ticks (100 a second) since some fixed time.
    fn ticks(&mut self) -> u64;
    /// Let about `ticks` go by without holding up anything else.
    fn pause(&mut self, ticks: u64);
}

// Registers, from locality 0's base.
pub const ACCESS: u64 = 0x000;
pub const INT_ENABLE: u64 = 0x008;
pub const INT_VECTOR: u64 = 0x00C;
pub const INTF_CAPABILITY: u64 = 0x014;
pub const STS: u64 = 0x018;
pub const DATA_FIFO: u64 = 0x024;
pub const DID_VID: u64 = 0xF00;
pub const RID: u64 = 0xF04;

// ACCESS bits.
pub const ACCESS_VALID: u8 = 0x80;
pub const ACCESS_ACTIVE: u8 = 0x20;
pub const ACCESS_REQUEST: u8 = 0x02;

// STS bits (its low byte; the next two are burstCount). stsValid says
// whether Expect and dataAvail mean anything yet.
pub const STS_VALID: u8 = 0x80;
pub const STS_COMMAND_READY: u8 = 0x40;
pub const STS_GO: u8 = 0x20;
pub const STS_DATA_AVAIL: u8 = 0x10;
pub const STS_EXPECT: u8 = 0x08;

/// INT_ENABLE's globalIntEnable: whether the TPM raises interrupts at all.
pub const INT_GLOBAL: u32 = 1 << 31;

// TIS's timeouts, and the longest a short TPM 1.2 command (an extend, a
// PCR read) may take, in timer ticks: A, B and C are 750 ms, 2 s and
// 750 ms.
pub const TIMEOUT_A: u64 = 75;
pub const TIMEOUT_B: u64 = 200;
pub const TIMEOUT_C: u64 = 75;
pub const SHORT_DURATION: u64 = 300;
/// How often commandReady is asked for again while the TPM isn't ready.
pub const READY_AGAIN: u64 = 10;
/// "Busy, try again" (TPM_RETRY, TPM_DOING_SELFTEST): tried again this
/// many times, this many ticks apart (3 s in all), before giving up.
pub const RETRIES: u32 = 30;
pub const RETRY_PAUSE: u64 = 10;

// TPM 1.2 ordinals: the only three this kernel has a use for.
pub const ORD_EXTEND: u32 = 0x14;
pub const ORD_PCR_READ: u32 = 0x15;
pub const ORD_STARTUP: u32 = 0x99;

// TPM 1.2 return codes this kernel names.
pub const TPM_SUCCESS: u32 = 0;
pub const TPM_BADINDEX: u32 = 0x02;
pub const TPM_DEACTIVATED: u32 = 0x06;
pub const TPM_DISABLED: u32 = 0x07;
pub const TPM_FAIL: u32 = 0x09;
pub const TPM_BAD_ORDINAL: u32 = 0x0A;
pub const TPM_INVALID_POSTINIT: u32 = 0x26;
pub const TPM_RETRY: u32 = 0x800;
pub const TPM_NEEDS_SELFTEST: u32 = 0x801;
pub const TPM_DOING_SELFTEST: u32 = 0x802;

pub fn rc_name(rc: u32) -> &'static str {
    match rc {
        TPM_BADINDEX => "TPM_BADINDEX: no such PCR",
        TPM_DEACTIVATED => "TPM_DEACTIVATED: switched to inactive in the BIOS",
        TPM_DISABLED => "TPM_DISABLED: switched off in the BIOS",
        TPM_FAIL => "TPM_FAIL",
        TPM_BAD_ORDINAL => "TPM_BAD_ORDINAL",
        TPM_INVALID_POSTINIT => "TPM_INVALID_POSTINIT: not started by the firmware",
        TPM_RETRY => "TPM_RETRY: busy",
        TPM_NEEDS_SELFTEST => "TPM_NEEDS_SELFTEST: its self-test not run",
        TPM_DOING_SELFTEST => "TPM_DOING_SELFTEST: still testing itself",
        _ => "",
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The TPM didn't do what TIS says within its time.
    Timeout(&'static str),
    /// It wanted more of the command than there was, or less; or it had
    /// more response than its header said.
    Length(&'static str),
    /// A response that isn't a TPM 1.2 response.
    Malformed,
    /// The TPM's own return code, not success.
    Tpm(u32),
    /// An answer that doesn't check out (a PCR's new value that isn't
    /// SHA-1 of the old and the digest: audit.rs).
    Wrong(&'static str),
    // Refusals of this kernel's own (tpm.rs), where nothing was sent:
    /// No TPM in use (none found, or one this build doesn't drive).
    NotInUse,
    /// Asked with interrupts off: the waits are counted in ticks.
    InterruptsOff,
    /// A command this build never sends to this TPM.
    Refused,
    /// Stopped after an earlier error: nothing more is sent this boot.
    Stopped,
}

impl Error {
    /// A number for the audit chain's records.
    pub fn code(&self) -> u64 {
        match *self {
            Error::Tpm(rc) => rc as u64,
            Error::NotInUse => 0x1_0001,
            Error::InterruptsOff => 0x1_0002,
            Error::Timeout(_) => 0x1_0003,
            Error::Length(_) => 0x1_0004,
            Error::Malformed => 0x1_0005,
            Error::Refused => 0x1_0006,
            Error::Stopped => 0x1_0007,
            Error::Wrong(_) => 0x1_0008,
        }
    }

    /// Whether anything reached the TPM: an error from it, or from the
    /// exchange with it, as opposed to this kernel's own refusals.
    pub fn from_the_tpm(&self) -> bool {
        !matches!(*self, Error::NotInUse | Error::InterruptsOff | Error::Refused | Error::Stopped)
    }
}

/// Wait until `done`, for at most `limit` ticks.
fn wait<B: Bus>(bus: &mut B, limit: u64, what: &'static str, mut done: impl FnMut(&mut B) -> bool) -> Result<(), Error> {
    let start = bus.ticks();
    loop {
        if done(bus) {
            return Ok(());
        }
        if bus.ticks().wrapping_sub(start) > limit {
            return Err(Error::Timeout(what));
        }
        core::hint::spin_loop();
    }
}

/// The status, once stsValid says its Expect and dataAvail bits mean
/// something.
fn valid<B: Bus>(bus: &mut B) -> Result<u8, Error> {
    let mut s = 0;
    wait(bus, TIMEOUT_C, "stsValid", |b| {
        s = b.r8(STS);
        s & STS_VALID != 0
    })?;
    Ok(s)
}

fn data_avail<B: Bus>(bus: &mut B) -> bool {
    bus.r8(STS) & (STS_VALID | STS_DATA_AVAIL) == STS_VALID | STS_DATA_AVAIL
}

/// How many bytes the TPM takes, or gives, without waiting. It may say 0
/// for a while (its FIFO full, or not yet refilled).
fn burst<B: Bus>(bus: &mut B) -> Result<usize, Error> {
    let mut n = 0;
    wait(bus, TIMEOUT_A, "burstCount", |b| {
        n = ((b.r32(STS) >> 8) & 0xFFFF) as usize;
        n > 0
    })?;
    Ok(n)
}

fn have_locality<B: Bus>(bus: &mut B) -> bool {
    bus.r8(ACCESS) & (ACCESS_VALID | ACCESS_ACTIVE) == ACCESS_VALID | ACCESS_ACTIVE
}

/// Send one command and read its response into `out`. Returns the
/// response's length; its return code is the caller's to look at.
/// Whatever happens, the TPM is left ready (a command half done is
/// abandoned) and locality 0 let go.
pub fn transact<B: Bus>(bus: &mut B, cmd: &[u8], out: &mut [u8]) -> Result<usize, Error> {
    if cmd.len() < 10 || out.len() < 10 {
        return Err(Error::Malformed);
    }
    if !have_locality(bus) {
        bus.w8(ACCESS, ACCESS_REQUEST);
        if let Err(e) = wait(bus, TIMEOUT_A, "locality 0", |b| have_locality(b)) {
            // Withdraw the request; without the locality, no other
            // register is touched.
            bus.w8(ACCESS, ACCESS_ACTIVE);
            return Err(e);
        }
    }
    let result = exchange(bus, cmd, out);
    bus.w8(STS, STS_COMMAND_READY);
    bus.w8(ACCESS, ACCESS_ACTIVE);
    result
}

fn exchange<B: Bus>(bus: &mut B, cmd: &[u8], out: &mut [u8]) -> Result<usize, Error> {
    // Ready for a command. A TPM that is idle becomes ready when asked; one
    // still holding a response (the firmware's, unread) may only go idle
    // on the first asking and ready on the second (TIS leaves that to the
    // chip). So it is asked again every 100 ms until it is ready.
    let mut asked = None;
    wait(bus, TIMEOUT_B, "commandReady", |b| {
        if b.r8(STS) & STS_COMMAND_READY != 0 {
            return true;
        }
        let now = b.ticks();
        if asked.map_or(true, |t| now.wrapping_sub(t) >= READY_AGAIN) {
            b.w8(STS, STS_COMMAND_READY);
            asked = Some(now);
        }
        false
    })?;

    // All but the last byte, as the TPM takes them; it must still expect
    // more after each burst. Then the last, after which it must expect
    // none.
    let (body, last) = cmd.split_at(cmd.len() - 1);
    let mut sent = 0;
    while sent < body.len() {
        let n = burst(bus)?.min(body.len() - sent);
        for &byte in &body[sent..sent + n] {
            bus.w8(DATA_FIFO, byte);
        }
        sent += n;
        if valid(bus)? & STS_EXPECT == 0 {
            return Err(Error::Length("the TPM expected no more of the command before its end"));
        }
    }
    burst(bus)?;
    bus.w8(DATA_FIFO, last[0]);
    if valid(bus)? & STS_EXPECT != 0 {
        return Err(Error::Length("the TPM still expected more after the command's last byte"));
    }

    // Run it, and wait for the response.
    bus.w8(STS, STS_GO);
    wait(bus, SHORT_DURATION, "the response", |b| data_avail(b))?;

    // The header, which gives the whole length, then the rest; each burst
    // only once dataAvail says there is more.
    let mut got = 0;
    let mut want = 10;
    while got < want {
        wait(bus, TIMEOUT_C, "dataAvail", |b| data_avail(b))?;
        let n = burst(bus)?.min(want - got);
        for _ in 0..n {
            out[got] = bus.r8(DATA_FIFO);
            got += 1;
        }
        if got == 10 && want == 10 {
            want = u32::from_be_bytes([out[2], out[3], out[4], out[5]]) as usize;
            if !(10..=out.len()).contains(&want) {
                return Err(Error::Malformed);
            }
        }
    }
    // And nothing after it.
    if valid(bus)? & STS_DATA_AVAIL != 0 {
        return Err(Error::Length("the TPM had more response than its header said"));
    }
    Ok(got)
}

/// A TPM 1.2 command: tag TPM_TAG_RQU_COMMAND (0x00C1), its size, the
/// ordinal, then `params`. Ok: the length of the response's parameters,
/// which follow its 10-byte header in `out`, when its return code is
/// TPM_SUCCESS. A TPM that answers busy (TPM_RETRY, TPM_DOING_SELFTEST)
/// didn't run the command, so it is sent again, a little later.
pub fn command<B: Bus>(bus: &mut B, ordinal: u32, params: &[u8], out: &mut [u8; 64]) -> Result<usize, Error> {
    let mut cmd = [0u8; 64];
    let size = 10 + params.len();
    if size > cmd.len() {
        return Err(Error::Malformed);
    }
    cmd[0..2].copy_from_slice(&0x00C1u16.to_be_bytes());
    cmd[2..6].copy_from_slice(&(size as u32).to_be_bytes());
    cmd[6..10].copy_from_slice(&ordinal.to_be_bytes());
    cmd[10..size].copy_from_slice(params);
    let mut tries = 0;
    loop {
        let n = transact(bus, &cmd[..size], out)?;
        // TPM_TAG_RSP_COMMAND (0x00C4).
        if out[0..2] != [0x00, 0xC4] {
            return Err(Error::Malformed);
        }
        match u32::from_be_bytes([out[6], out[7], out[8], out[9]]) {
            TPM_SUCCESS => return Ok(n - 10),
            TPM_RETRY | TPM_DOING_SELFTEST if tries < RETRIES => {
                tries += 1;
                bus.pause(RETRY_PAUSE);
            }
            rc => return Err(Error::Tpm(rc)),
        }
    }
}

/// TPM_PCRRead: a PCR's value.
pub fn pcr_read<B: Bus>(bus: &mut B, index: u32) -> Result<[u8; 20], Error> {
    let mut out = [0u8; 64];
    if command(bus, ORD_PCR_READ, &index.to_be_bytes(), &mut out)? != 20 {
        return Err(Error::Malformed);
    }
    let mut v = [0u8; 20];
    v.copy_from_slice(&out[10..30]);
    Ok(v)
}

/// TPM_Extend: PCR `index` becomes SHA-1(itself || digest). Returns its
/// new value, as the TPM computed it.
pub fn extend<B: Bus>(bus: &mut B, index: u32, digest: &[u8; 20]) -> Result<[u8; 20], Error> {
    let mut params = [0u8; 24];
    params[0..4].copy_from_slice(&index.to_be_bytes());
    params[4..24].copy_from_slice(digest);
    let mut out = [0u8; 64];
    if command(bus, ORD_EXTEND, &params, &mut out)? != 20 {
        return Err(Error::Malformed);
    }
    let mut v = [0u8; 20];
    v.copy_from_slice(&out[10..30]);
    Ok(v)
}

/// TPM_Startup(ST_CLEAR): what the firmware does as the machine boots.
pub fn startup_clear<B: Bus>(bus: &mut B) -> Result<(), Error> {
    let mut out = [0u8; 64];
    command(bus, ORD_STARTUP, &0x0001u16.to_be_bytes(), &mut out).map(|_| ())
}
