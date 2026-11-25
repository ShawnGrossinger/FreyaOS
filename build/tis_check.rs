//! tis_check.rs — the kernel's TIS protocol (kernel/src/tis.rs, this very
//! file compiled here) run against models of TPM 1.2 chips that are
//! stricter than QEMU's (Entry 34).
//!
//! QEMU's emulated TPM answers at once, takes 64 bytes a burst, and goes
//! straight back to ready. A real chip on the LPC bus, like the T430's,
//! may take a byte at a time, set stsValid late, stall its burst count,
//! need commandReady twice when the firmware left a response unread,
//! answer "busy, try again" just after boot, and take a while over a
//! command; all within what the TCG's TIS specification allows. Each
//! model here does some of that, and counts every step the kernel takes
//! that TIS doesn't allow (a register touched without the locality, more
//! bytes than the burst count said, tpmGo before the command is complete,
//! the FIFO read with nothing in it, ...) as a violation.
//!
//! Then the faults a broken or missing chip shows: a locality never
//! granted, a TPM never ready, a command that never finishes, one that
//! wants a byte too few, a response longer than its header says or with
//! an impossible size, a TPM that stays busy, one that needs a self-test,
//! and one gone from the bus. Each must end in the error that names it,
//! with the TPM left as TIS wants (locality 0 let go, a half-done command
//! abandoned), and nothing sent but PCRRead and Extend.
//!
//! Build: rustc --edition 2021 -O tis_check.rs   (make tis-check)
//! Run:   ./tis_check   (exit status 0 if everything held)

#![allow(dead_code)]

#[path = "../kernel/src/tis.rs"]
mod tis;
#[path = "../kernel/src/sha1.rs"]
mod sha1;

use tis::*;

const ACCESS_PENDING: u8 = 0x04;
/// Simulated time per register access.
const ACCESS_US: u64 = 10;

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Idle,
    Ready,
    Reception,
    Execution,
    Completion,
}

#[derive(Clone, Default)]
struct Quirks {
    name: &'static str,
    /// The most burstCount ever says.
    max_burst: usize,
    /// After this many bytes through the FIFO, burstCount reads 0 for
    /// `stall_reads` reads (0: never).
    stall_every: usize,
    stall_reads: usize,
    /// STS reads with stsValid clear after each byte written.
    valid_lag: usize,
    /// How long after commandReady is asked for it takes effect, in µs.
    ready_us: u64,
    /// ACCESS reads before locality 0 is granted.
    locality_lag: usize,
    /// commandReady in Completion goes to Idle; a second, to Ready.
    complete_to_idle: bool,
    /// How long a command runs, in ticks.
    exec_ticks: u64,
    /// The first this many commands are answered TPM_DOING_SELFTEST.
    busy_first: u32,
    /// The firmware left a response unread, and locality 0 active.
    stale_response: bool,
    // Faults.
    never_locality: bool,
    never_ready: bool,
    hang: bool,
    expect_early: bool,
    extra_byte: bool,
    size_huge: bool,
    busy_always: bool,
    needs_selftest: bool,
    gone: bool,
}

struct Chip {
    q: Quirks,
    now_us: u64,
    state: State,
    active: bool,
    granting: Option<usize>,
    readying: Option<u64>,
    valid_wait: usize,
    burst_left: usize,
    since_stall: usize,
    stall_left: usize,
    cmd: Vec<u8>,
    resp: Vec<u8>,
    at: usize,
    done_at: u64,
    busy_left: u32,
    pcrs: [[u8; 20]; 24],
    ordinals: Vec<u32>,
    violations: Vec<String>,
    commands: u32,
    ready_writes: u32,
    aborted: u32,
}

impl Chip {
    fn new(q: Quirks) -> Chip {
        let mut c = Chip {
            busy_left: q.busy_first,
            q,
            now_us: 1_000_000,
            state: State::Idle,
            active: false,
            granting: None,
            readying: None,
            valid_wait: 0,
            burst_left: 0,
            since_stall: 0,
            stall_left: 0,
            cmd: Vec::new(),
            resp: Vec::new(),
            at: 0,
            done_at: 0,
            pcrs: [[0; 20]; 24],
            ordinals: Vec::new(),
            violations: Vec::new(),
            commands: 0,
            ready_writes: 0,
            aborted: 0,
        };
        if c.q.stale_response {
            // What the firmware's last command left: a response nobody read.
            c.state = State::Completion;
            c.resp = vec![0x00, 0xC4, 0, 0, 0, 10, 0, 0, 0, 0];
            c.active = true;
        }
        c
    }

    fn violation(&mut self, what: &str) {
        self.violations.push(format!("{} (state {:?})", what, self.state));
    }

    fn tick(&mut self) {
        self.now_us += ACCESS_US;
        if self.readying.is_some_and(|t| self.now_us >= t) {
            self.readying = None;
            if self.state == State::Idle {
                self.state = State::Ready;
            }
        }
        if self.state == State::Execution && !self.q.hang && self.now_us >= self.done_at {
            self.state = State::Completion;
            self.at = 0;
        }
    }

    fn size(&self) -> Option<usize> {
        (self.cmd.len() >= 6).then(|| u32::from_be_bytes([self.cmd[2], self.cmd[3], self.cmd[4], self.cmd[5]]) as usize)
    }

    fn complete(&self) -> bool {
        self.size().is_some_and(|n| self.cmd.len() >= n)
    }

    fn expect(&self) -> bool {
        match self.size() {
            None => true,
            Some(n) if self.q.expect_early => self.cmd.len() + 1 < n,
            Some(n) => self.cmd.len() < n,
        }
    }

    fn sts(&mut self) -> u8 {
        if !self.active {
            self.violation("STS read without locality 0");
            return 0xFF;
        }
        let valid = if self.valid_wait > 0 {
            self.valid_wait -= 1;
            false
        } else {
            true
        };
        let ready = if self.state == State::Ready { STS_COMMAND_READY } else { 0 };
        let expect = self.state == State::Reception && self.expect();
        let avail = self.state == State::Completion && self.at < self.resp.len();
        if !valid {
            // Expect and dataAvail mean nothing yet: say the opposite.
            return ready | if expect { 0 } else { STS_EXPECT } | if avail { 0 } else { STS_DATA_AVAIL };
        }
        STS_VALID | ready | if expect { STS_EXPECT } else { 0 } | if avail { STS_DATA_AVAIL } else { 0 }
    }

    fn burst_count(&mut self) -> usize {
        let n = if self.stall_left > 0 {
            self.stall_left -= 1;
            0
        } else {
            match self.state {
                State::Ready | State::Reception => self.q.max_burst,
                State::Completion => self.q.max_burst.min(self.resp.len() - self.at),
                _ => 0,
            }
        };
        self.burst_left = n;
        n
    }

    fn moved_a_byte(&mut self) {
        self.since_stall += 1;
        if self.q.stall_every > 0 && self.since_stall >= self.q.stall_every {
            self.since_stall = 0;
            self.stall_left = self.q.stall_reads;
        }
    }

    fn fifo_write(&mut self, v: u8) {
        if !self.active {
            return self.violation("FIFO written without locality 0");
        }
        match self.state {
            State::Ready => {
                self.state = State::Reception;
                self.cmd.clear();
            }
            State::Reception => {}
            _ => return self.violation("FIFO written when not taking a command"),
        }
        if self.burst_left == 0 {
            self.violation("FIFO written past what burstCount said");
        } else {
            self.burst_left -= 1;
        }
        if self.complete() {
            return self.violation("FIFO written past the command's end");
        }
        self.cmd.push(v);
        self.valid_wait = self.q.valid_lag;
        self.moved_a_byte();
    }

    fn fifo_read(&mut self) -> u8 {
        if !self.active {
            self.violation("FIFO read without locality 0");
            return 0xFF;
        }
        if self.state != State::Completion || self.at >= self.resp.len() {
            self.violation("FIFO read with nothing in it");
            return 0xFF;
        }
        if self.burst_left == 0 {
            self.violation("FIFO read past what burstCount said");
        } else {
            self.burst_left -= 1;
        }
        let b = self.resp[self.at];
        self.at += 1;
        self.moved_a_byte();
        b
    }

    fn sts_write(&mut self, v: u8) {
        if !self.active {
            return self.violation("STS written without locality 0");
        }
        match v {
            STS_COMMAND_READY => {
                self.ready_writes += 1;
                match self.state {
                    State::Idle => {
                        if !self.q.never_ready && self.readying.is_none() {
                            self.readying = Some(self.now_us + self.q.ready_us);
                        }
                    }
                    State::Ready => {}
                    State::Reception | State::Execution => {
                        self.aborted += 1;
                        self.state = State::Idle;
                        self.cmd.clear();
                    }
                    State::Completion => {
                        self.resp.clear();
                        self.state = if self.q.complete_to_idle || self.q.never_ready { State::Idle } else { State::Ready };
                    }
                }
            }
            STS_GO => {
                if self.state != State::Reception || !self.complete() {
                    return self.violation("tpmGo before the command was complete");
                }
                self.resp = self.execute();
                self.at = 0;
                self.state = State::Execution;
                self.done_at = self.now_us + self.q.exec_ticks * 10_000;
            }
            _ => self.violation("STS written with bits TIS doesn't have software write together"),
        }
    }

    fn execute(&mut self) -> Vec<u8> {
        self.commands += 1;
        let c = self.cmd.clone();
        let ordinal = u32::from_be_bytes([c[6], c[7], c[8], c[9]]);
        self.ordinals.push(ordinal);
        let u32_at = |i: usize| u32::from_be_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]);
        let (rc, params): (u32, Vec<u8>) = if c[0..2] != [0x00, 0xC1] {
            (0x1E, vec![])
        } else if self.q.busy_always || self.busy_left > 0 {
            self.busy_left = self.busy_left.saturating_sub(1);
            (TPM_DOING_SELFTEST, vec![])
        } else if self.q.needs_selftest {
            (TPM_NEEDS_SELFTEST, vec![])
        } else {
            match ordinal {
                ORD_PCR_READ if c.len() == 14 => match u32_at(10) as usize {
                    i if i < 24 => (TPM_SUCCESS, self.pcrs[i].to_vec()),
                    _ => (TPM_BADINDEX, vec![]),
                },
                ORD_EXTEND if c.len() == 34 => match u32_at(10) as usize {
                    i if i < 24 => {
                        let mut d = [0u8; 20];
                        d.copy_from_slice(&c[14..34]);
                        self.pcrs[i] = sha1::extend(&self.pcrs[i], &d);
                        (TPM_SUCCESS, self.pcrs[i].to_vec())
                    }
                    _ => (TPM_BADINDEX, vec![]),
                },
                ORD_STARTUP => (TPM_SUCCESS, vec![]),
                _ => (TPM_BAD_ORDINAL, vec![]),
            }
        };
        let size = if self.q.size_huge { 4096 } else { 10 + params.len() as u32 };
        let mut r = vec![0x00, 0xC4];
        r.extend_from_slice(&size.to_be_bytes());
        r.extend_from_slice(&rc.to_be_bytes());
        r.extend_from_slice(&params);
        if self.q.extra_byte {
            r.push(0);
        }
        r
    }
}

impl Bus for Chip {
    fn r8(&mut self, reg: u64) -> u8 {
        self.tick();
        if self.q.gone {
            return 0xFF;
        }
        match reg {
            ACCESS => {
                if let Some(n) = self.granting {
                    if n == 0 {
                        self.granting = None;
                        self.active = true;
                    } else {
                        self.granting = Some(n - 1);
                    }
                }
                ACCESS_VALID | if self.active { ACCESS_ACTIVE } else { 0 } | if self.granting.is_some() { ACCESS_PENDING } else { 0 }
            }
            STS => self.sts(),
            DATA_FIFO => self.fifo_read(),
            RID => 78,
            INT_VECTOR => 0,
            _ => {
                self.violation("a byte read from a register TIS doesn't have there");
                0xFF
            }
        }
    }

    fn r32(&mut self, reg: u64) -> u32 {
        self.tick();
        if self.q.gone {
            return 0xFFFF_FFFF;
        }
        match reg {
            STS => {
                let low = self.sts() as u32;
                if !self.active {
                    return 0xFFFF_FFFF;
                }
                low | (self.burst_count() as u32) << 8
            }
            DID_VID => 0x0000_104A,
            INTF_CAPABILITY | INT_ENABLE => 0,
            _ => {
                self.violation("a word read from a register TIS doesn't have there");
                0xFFFF_FFFF
            }
        }
    }

    fn w8(&mut self, reg: u64, v: u8) {
        self.tick();
        if self.q.gone {
            return;
        }
        match (reg, v) {
            (ACCESS, ACCESS_REQUEST) => {
                if !self.active && self.granting.is_none() && !self.q.never_locality {
                    self.granting = Some(self.q.locality_lag);
                }
            }
            (ACCESS, ACCESS_ACTIVE) => {
                self.active = false;
                self.granting = None;
            }
            (STS, v) => self.sts_write(v),
            (DATA_FIFO, v) => self.fifo_write(v),
            _ => self.violation("a write TIS doesn't have software make"),
        }
    }

    fn ticks(&mut self) -> u64 {
        self.tick();
        self.now_us / 10_000
    }

    fn pause(&mut self, ticks: u64) {
        self.now_us += ticks * 10_000;
        self.tick();
    }
}

/// Left as TIS wants after a command, or after a failed one: locality 0
/// let go, no command half taken or still running, nothing out of order,
/// and nothing sent but PCRRead and Extend.
fn left_right(c: &Chip) -> Result<(), String> {
    if let Some(v) = c.violations.first() {
        return Err(format!("{} protocol violations, the first: {}", c.violations.len(), v));
    }
    if c.active || c.granting.is_some() {
        return Err("locality 0 not let go".into());
    }
    if matches!(c.state, State::Reception | State::Execution) {
        return Err(format!("left in {:?}", c.state));
    }
    if let Some(o) = c.ordinals.iter().find(|&&o| o != ORD_PCR_READ && o != ORD_EXTEND) {
        return Err(format!("sent ordinal {:#x}", o));
    }
    Ok(())
}

/// PCR 15 read, extended five times, read back; PCR 0 read; each value
/// checked against SHA-1 here.
fn normal(q: Quirks) -> Result<String, String> {
    let name = q.name;
    let mut c = Chip::new(q);
    let start = c.now_us;
    let fail = |what: &str, e: Error| format!("{}: {}: {:?}", name, what, e);
    let mut want = pcr_read(&mut c, 15).map_err(|e| fail("PCR 15 read", e))?;
    if want != [0; 20] {
        return Err(format!("{}: PCR 15 didn't start at zeros", name));
    }
    for i in 0..5u8 {
        let digest = [i.wrapping_mul(37).wrapping_add(1); 20];
        let got = extend(&mut c, 15, &digest).map_err(|e| fail("extend", e))?;
        want = sha1::extend(&want, &digest);
        if got != want {
            return Err(format!("{}: extend {} gave a value that isn't SHA-1 of the one before and the digest", name, i));
        }
    }
    if pcr_read(&mut c, 15).map_err(|e| fail("PCR 15 read back", e))? != want {
        return Err(format!("{}: PCR 15 read back isn't the last extend's value", name));
    }
    if pcr_read(&mut c, 0).map_err(|e| fail("PCR 0 read", e))? != [0; 20] {
        return Err(format!("{}: PCR 0 changed", name));
    }
    left_right(&c).map_err(|e| format!("{}: {}", name, e))?;
    Ok(format!("{}: right; {} commands, commandReady written {} times, {} ms", name, c.commands, c.ready_writes, (c.now_us - start) / 1000))
}

/// A fault: the first command must fail as `expected` says, and leave the
/// TPM as TIS wants. Then `after` checks anything particular to it.
fn fault(q: Quirks, expected: fn(&Error) -> bool, after: fn(&Chip) -> Result<(), String>) -> Result<String, String> {
    let name = q.name;
    let mut c = Chip::new(q);
    let r = extend(&mut c, 15, &[0xA5; 20]);
    match r {
        Ok(_) => return Err(format!("{}: NOT caught: the extend succeeded", name)),
        Err(e) if !expected(&e) => return Err(format!("{}: caught, but as {:?}", name, e)),
        Err(_) => {}
    }
    if c.pcrs[15] != [0; 20] && !c.q.hang && !c.q.extra_byte && !c.q.size_huge {
        return Err(format!("{}: PCR 15 changed though the extend failed", name));
    }
    left_right(&c).map_err(|e| format!("{}: {}", name, e))?;
    after(&c).map_err(|e| format!("{}: {}", name, e))?;
    Ok(format!("{}: {:?}", name, r.unwrap_err()))
}

fn main() {
    let qemu_like = Quirks { name: "like QEMU's", max_burst: 64, exec_ticks: 0, ..Default::default() };
    let strict = Quirks {
        name: "strict (8-byte bursts stalling every 5 bytes, stsValid 4 reads late, ready 2 ms after asked, locality 5 reads after, \
               commandReady twice from a response the firmware left unread, 30 ms a command, busy for the first 3)",
        max_burst: 8,
        stall_every: 5,
        stall_reads: 3,
        valid_lag: 4,
        ready_us: 2_000,
        locality_lag: 5,
        complete_to_idle: true,
        exec_ticks: 3,
        busy_first: 3,
        stale_response: true,
        ..Default::default()
    };
    let bytewise = Quirks { name: "a byte at a time", max_burst: 1, valid_lag: 1, ready_us: 100, locality_lag: 1, complete_to_idle: true, exec_ticks: 1, ..Default::default() };
    let slow = Quirks { name: "slow (2.5 s a command, ready 1.5 s after asked)", max_burst: 4, ready_us: 1_500_000, exec_ticks: 250, ..Default::default() };

    let mut lines = Vec::new();
    let mut failed = Vec::new();
    for q in [qemu_like, strict.clone(), bytewise, slow] {
        match normal(q) {
            Ok(s) => lines.push(s),
            Err(s) => failed.push(s),
        }
    }
    let chips = lines.len();

    let base = || Quirks { max_burst: 8, valid_lag: 2, exec_ticks: 1, ..Default::default() };
    let none = |_: &Chip| Ok(());
    type Check = fn(&Chip) -> Result<(), String>;
    let faults: Vec<(Quirks, fn(&Error) -> bool, Check)> = vec![
        (Quirks { name: "locality never granted", never_locality: true, ..base() }, |e| *e == Error::Timeout("locality 0"), |c| {
            if c.ready_writes > 0 { Err("STS touched without the locality".into()) } else { Ok(()) }
        }),
        (Quirks { name: "never ready", never_ready: true, ..base() }, |e| *e == Error::Timeout("commandReady"), |c| {
            if c.ready_writes < 2 { Err("commandReady asked only once".into()) } else { Ok(()) }
        }),
        (Quirks { name: "a command that never finishes", hang: true, ..base() }, |e| *e == Error::Timeout("the response"), |c| {
            if c.aborted != 1 { Err("the command left running, not abandoned".into()) } else { Ok(()) }
        }),
        (Quirks { name: "wants a byte too few", expect_early: true, ..base() }, |e| matches!(e, Error::Length(_)), |c| {
            if c.aborted != 1 || c.commands != 0 { Err("the half-sent command not abandoned".into()) } else { Ok(()) }
        }),
        (Quirks { name: "a byte more than its header says", extra_byte: true, ..base() }, |e| matches!(e, Error::Length(_)), none),
        (Quirks { name: "an impossible size", size_huge: true, ..base() }, |e| *e == Error::Malformed, none),
        (Quirks { name: "busy for good", busy_always: true, ..base() }, |e| *e == Error::Tpm(TPM_DOING_SELFTEST), |c| {
            if c.commands != RETRIES + 1 { Err(format!("sent {} times, not {}", c.commands, RETRIES + 1)) } else { Ok(()) }
        }),
        (Quirks { name: "needs a self-test", needs_selftest: true, ..base() }, |e| *e == Error::Tpm(TPM_NEEDS_SELFTEST), |c| {
            if c.commands != 1 { Err("sent again: only busy is tried again".into()) } else { Ok(()) }
        }),
        (Quirks { name: "gone from the bus", gone: true, ..base() }, |e| matches!(e, Error::Length(_)), none),
    ];
    let mut caught = Vec::new();
    for (q, expected, after) in faults {
        match fault(q, expected, after) {
            Ok(s) => caught.push(s),
            Err(s) => failed.push(s),
        }
    }

    if failed.is_empty() {
        println!(
            "tis_check: OK: {} chips each read and extended PCR 15 right, 0 protocol violations, nothing sent but PCRRead and Extend; {} faults each caught, the TPM let go",
            chips,
            caught.len()
        );
    } else {
        println!("tis_check: FAILED: {}", failed[0]);
    }
    for l in lines.iter().chain(caught.iter()) {
        println!("  {}", l);
    }
    for f in failed.iter().skip(1) {
        println!("  FAILED: {}", f);
    }
    std::process::exit(if failed.is_empty() { 0 } else { 1 });
}
