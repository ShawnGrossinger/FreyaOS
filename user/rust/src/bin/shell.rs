//! shell — the first shell (Entry 23): a line typed, a command run.
//!
//! Capabilities init gives it, and nothing else (caps lists them):
//!   slot 0  Console    to write what it shows: the system log, where the
//!                      console driver shows the shell's lines in white
//!                      and without the kernel's "[user N 'shell']"
//!   slot 1  Endpoint   its own (RECV): signals that keys are waiting
//!                      (from the PS/2 driver), or that the console driver
//!                      started again (from init)
//!   slot 2  Endpoint   the PS/2 driver's (SEND, badge 4): READ_KEYS
//!   slot 3  Endpoint   the console driver's (SEND, badge 4): the line
//!                      being typed (INPUT), and back to the newest line
//!                      on Enter (SCROLL)
//!   slot 4  Info       uptime, memory and the tasks (ps, uptime, mem)
//!   slot 5  Power      restart and off
//!   slot 6  Factory    (Entry 24) starts hello, spin, crash, ticker and
//!                      relay (Entry 25), nothing else
//!   slot 7  Endpoint   its own again (SEND): where the programs it starts
//!                      signal when they end (bit 1 << id)
//!
//! No port, no interrupt, no screen: it can't reach a device, and a fault
//! in it ends only it (init starts it again). Its Factory starts only the
//! test programs; run anything else and the kernel refuses.
//!
//! Loans (Entry 25): a program isn't given the shell's own console, but a
//! copy of it (copy), which the shell keeps in a slot of its own; relay is
//! also lent a Factory that starts only ticker (mint). Whatever the program
//! makes of them, passes on or hands to a program it starts is derived from
//! those loans, so revoke ID takes all of it back in one call, from every
//! process that has it, and tells how many capabilities went. The program
//! goes on running: what it does without them is up to it (ticker and
//! relay end; spin doesn't notice). When a program ends, the shell takes
//! back anything it had passed on that is still out, and lets the loans go.
//!
//! Programs (Entry 24): run starts one in the background, with the console
//! in its slot 0 and nothing else, and keeps the Process capability spawn
//! hands back (up to MAX_JOBS at once). jobs lists them; kill stops one,
//! through that capability, and nothing else: a process this shell didn't
//! start, it holds no capability for. When one ends, the kernel signals
//! the shell's endpoint, and the shell asks how (status), says so, and
//! lets the capability go. The keyboard's own keys stay with the PS/2 driver
//! (Ctrl+Alt+Del, Ctrl+Alt+End, PageUp, PageDown), so they work with no
//! shell at all.
//!
//! The PS/2 driver signals when keys are waiting; the shell then asks for
//! them (READ_KEYS), which it answers at once. The shell edits the line
//! itself: characters, Backspace, Esc (clears it), Up and Down (the last
//! 8 lines entered); after each change it sends the console driver the
//! whole line, prompt first, to show on the screen's last row. Enter puts
//! the line in the log after the prompt, and runs it.

#![no_std]
#![no_main]

use freya::*;

freya::entry!(main);

const CONSOLE: u64 = 0;
const OWN: u64 = 1;
const PS2: u64 = 2;
const CONSOLE_EP: u64 = 3;
const INFO: u64 = 4;
const POWER_SLOT: u64 = 5;
const FACTORY: u64 = 6;
const EXITS: u64 = 7;

/// Programs running from this shell at once, at most.
const MAX_JOBS: usize = 8;

#[derive(Clone, Copy)]
struct Job {
    id: u64,
    /// The slot of its Process capability.
    cap: u64,
    program: u64,
    /// The slots of what it was lent (Entry 25), NOT_LENT where nothing.
    loans: [u64; LOANS],
}

/// A program is lent the console; relay also a Factory for tickers.
const LOANS: usize = 2;
const NOT_LENT: u64 = u64::MAX;

const PROMPT: &[u8] = b"freya> ";
const MAX_LINE: usize = 64;
/// Lines entered that Up and Down go back through.
const KEPT: usize = 8;

struct Shell {
    line: [u8; MAX_LINE],
    len: usize,
    /// Lines entered, the newest at (entered - 1) % KEPT.
    history: [[u8; MAX_LINE]; KEPT],
    history_len: [usize; KEPT],
    entered: usize,
    /// How far back Up has gone (0: the line being typed), and that line,
    /// kept while browsing.
    back: usize,
    draft: [u8; MAX_LINE],
    draft_len: usize,
    jobs: [Option<Job>; MAX_JOBS],
}

static mut STATE: Shell = unsafe { core::mem::zeroed() };
/// ps's snapshot: the header and every task there can be.
static mut INFO_BUF: [u8; INFO_HEADER + 32 * INFO_RECORD] = [0; INFO_HEADER + 32 * INFO_RECORD];

fn main(_arg: u64) -> u64 {
    let sh = unsafe { &mut *(&raw mut STATE) };
    say(b"shell ready: type help and press Enter\n");
    sh.show();
    loop {
        let msg = match recv(OWN) {
            Ok(m) => m,
            Err(e) => return e as u64,
        };
        if msg.badge != SIGNAL_BADGE {
            // Nobody is meant to call the shell; don't leave a caller
            // waiting if one does.
            reply([ERR_BAD_ARGUMENT as u64, 0, 0, 0], NO_CAP);
            continue;
        }
        if msg.words[0] & SIGNAL_REDRAW != 0 {
            sh.show();
        }
        if msg.words[0] & SIGNAL_KEYS != 0 {
            sh.read_keys();
        }
        // Every other bit is a process that ended: bit (1 << its id).
        let ended = msg.words[0] & !(SIGNAL_KEYS | SIGNAL_REDRAW);
        if ended != 0 {
            sh.ended(ended);
        }
    }
}

fn say(text: &[u8]) {
    write(CONSOLE, text);
}

impl Shell {
    /// Every key waiting at the PS/2 driver.
    fn read_keys(&mut self) {
        loop {
            let Ok(m) = call(PS2, [PS2_READ_KEYS, 0, 0, 0], NO_CAP) else { return };
            let n = m.words[0] as usize;
            if n > READ_MAX {
                return; // an error, not a count
            }
            let mut changed = false;
            for i in 0..n {
                changed |= self.key((m.words[1 + i / 8] >> (8 * (i % 8))) as u8);
            }
            if changed {
                self.show();
            }
            if n < READ_MAX {
                return;
            }
        }
    }

    /// One key. Returns whether the line changed since it was last shown.
    fn key(&mut self, k: u8) -> bool {
        match k {
            KEY_ENTER => {
                self.enter();
                return false;
            }
            KEY_BACKSPACE => self.len = self.len.saturating_sub(1),
            KEY_ESCAPE => self.len = 0,
            KEY_UP => self.browse(1),
            KEY_DOWN => self.browse(-1),
            0x20..=0x7E if self.len < MAX_LINE => {
                self.line[self.len] = k;
                self.len += 1;
            }
            _ => return false,
        }
        true
    }

    /// Up (1) or Down (-1) through the lines entered; Down past the newest
    /// gives back the line that was being typed.
    fn browse(&mut self, step: isize) {
        let kept = self.entered.min(KEPT);
        let to = (self.back as isize + step).clamp(0, kept as isize) as usize;
        if to == self.back {
            return;
        }
        if self.back == 0 {
            self.draft = self.line;
            self.draft_len = self.len;
        }
        self.back = to;
        if to == 0 {
            self.line = self.draft;
            self.len = self.draft_len;
        } else {
            let i = (self.entered - to) % KEPT;
            self.line = self.history[i];
            self.len = self.history_len[i];
        }
    }

    /// The prompt and the line, to the console driver's input row: the
    /// whole of it each time, INPUT_CHUNK bytes a call.
    fn show(&self) {
        let mut text = [0u8; PROMPT.len() + MAX_LINE];
        text[..PROMPT.len()].copy_from_slice(PROMPT);
        text[PROMPT.len()..PROMPT.len() + self.len].copy_from_slice(&self.line[..self.len]);
        let total = PROMPT.len() + self.len;
        let mut offset = 0;
        while offset < total {
            let mut words = [0u64; 2];
            for i in 0..INPUT_CHUNK.min(total - offset) {
                words[i / 8] |= (text[offset + i] as u64) << (8 * (i % 8));
            }
            // Peer-gone if there is no console driver any more: then the
            // kernel draws the screen, and has no input row.
            if call(CONSOLE_EP, [CONSOLE_INPUT, (offset | total << 16) as u64, words[0], words[1]], NO_CAP).is_err() {
                return;
            }
            offset += INPUT_CHUNK;
        }
    }

    fn enter(&mut self) {
        let _ = call(CONSOLE_EP, [CONSOLE_SCROLL, SCROLL_TO_END, 0, 0], NO_CAP);
        let mut line = [0u8; MAX_LINE];
        let len = self.len;
        line[..len].copy_from_slice(&self.line[..len]);
        self.len = 0;
        self.back = 0;
        if len > 0 {
            let mut echo = Line::<80>::new();
            echo.str(PROMPT).str(&line[..len]).str(b"\n");
            say(echo.as_bytes());
            let newest = (self.entered + KEPT - 1) % KEPT;
            if self.entered == 0 || self.history[newest][..self.history_len[newest]] != line[..len] {
                self.history[self.entered % KEPT] = line;
                self.history_len[self.entered % KEPT] = len;
                self.entered += 1;
            }
        }
        self.show();
        if len > 0 {
            self.run(&line[..len]);
        }
    }
}

// ---------------------------------------------------------------------------
// The commands
// ---------------------------------------------------------------------------

const HELP: &[&[u8]] = &[
    b"commands:\n",
    b"  help       this list\n",
    b"  echo TEXT  print TEXT\n",
    b"  ps         the tasks: what each is doing, and the capabilities it holds\n",
    b"  caps       the capabilities this shell holds: all it may do\n",
    b"  uptime     time since the timer started\n",
    b"  mem        memory free\n",
    b"  run NAME   start a program: hello, spin (runs until stopped), crash (writes to the kernel),\n",
    b"             ticker (a line a second while it holds the console), relay (lends it to a ticker)\n",
    b"  jobs       the programs started here that are still running\n",
    b"  kill ID    stop one of them (or kill NAME: every one of that name)\n",
    b"  revoke ID  take back what this shell lent it, from it and from whoever it passed it to\n",
    b"  restart    restart the machine\n",
    b"  off        switch the machine off\n",
    b"keys: Up/Down earlier lines, Esc clears the line, PgUp/PgDn scroll the log\n",
];

impl Shell {
    fn run(&mut self, line: &[u8]) {
        let line = trim(line);
        let (name, rest) = match line.iter().position(|&b| b == b' ') {
            Some(i) => (&line[..i], trim(&line[i + 1..])),
            None => (line, &line[line.len()..]),
        };
        match name {
            b"help" => HELP.iter().for_each(|l| say(l)),
            b"echo" => {
                let mut l = Line::<80>::new();
                l.str(rest).str(b"\n");
                say(l.as_bytes());
            }
            b"ps" => ps(),
            b"caps" => self.caps(),
            b"uptime" => uptime(),
            b"mem" => mem(),
            b"run" => self.start(rest),
            b"jobs" => self.list_jobs(),
            b"kill" => self.stop(rest),
            b"revoke" => self.take_back(rest),
            b"restart" => power_off_or_restart(POWER_RESTART, b"restarting\n"),
            b"off" => power_off_or_restart(POWER_OFF, b"switching off\n"),
            b"" => {}
            _ => {
                let mut l = Line::<120>::new();
                l.str(name).str(b": no such command (help lists them)\n");
                say(l.as_bytes());
            }
        }
    }

    /// run NAME: start it, with a copy of the console lent to it (and, for
    /// relay, a Factory for tickers) and nothing else, and keep its Process
    /// capability.
    fn start(&mut self, name: &[u8]) {
        let Some(program) = PROGRAM_NAMES.iter().position(|&n| n == name) else {
            let mut l = Line::<120>::new();
            if name.is_empty() {
                l.str(b"run what?");
            } else {
                l.str(name).str(b": no such program");
            }
            l.str(b" (programs: ");
            for (i, n) in PROGRAM_NAMES.iter().enumerate() {
                l.str(if i > 0 { b", " } else { b"" }).str(n);
            }
            l.str(b")\n");
            return say(l.as_bytes());
        };
        let Some(free) = self.jobs.iter().position(|j| j.is_none()) else {
            let mut l = Line::<120>::new();
            l.dec(MAX_JOBS as u64).str(b" programs are running from this shell already: stop one first (kill)\n");
            return say(l.as_bytes());
        };
        let loans = match lend(program as u64) {
            Ok(loans) => loans,
            Err(e) => return error(b"couldn't make what it is lent", e),
        };
        let mut slots = [NO_SLOT; MAX_CAPS];
        for (i, &loan) in loans.iter().enumerate() {
            if loan != NOT_LENT {
                slots[i] = loan as u8;
            }
        }
        let mut l = Line::<120>::new();
        let started = spawn_process(FACTORY, program as u64, 0, &slots, EXITS);
        if started.is_err() {
            let_go(&loans);
        }
        match started {
            Ok((id, cap)) => {
                self.jobs[free] = Some(Job { id, cap, program: program as u64, loans });
                l.str(b"started ").str(name).str(b" as process ").dec(id);
            }
            Err(ERR_WRONG_CAPABILITY) => {
                l.str(name).str(b": this shell's Factory capability doesn't cover it (caps shows what it does)");
            }
            Err(e) => {
                l.str(b"couldn't start ").str(name).str(b" (error -").dec(e.unsigned_abs()).str(b")");
            }
        }
        l.str(b"\n");
        say(l.as_bytes());
    }

    fn list_jobs(&self) {
        let mut any = false;
        for j in self.jobs.iter().flatten() {
            if !any {
                say(b" id  program\n");
                any = true;
            }
            let mut l = Line::<120>::new();
            right(&mut l, j.id, 3);
            l.str(b"  ").str(PROGRAM_NAMES[j.program as usize]).str(b"\n");
            say(l.as_bytes());
        }
        if !any {
            say(b"no programs started from this shell are running\n");
        }
    }

    /// kill ID, or kill NAME: through the Process capabilities it holds,
    /// and only those.
    fn stop(&mut self, what: &[u8]) {
        if what.is_empty() {
            return say(b"kill what? a process number or a program's name (jobs lists them)\n");
        }
        let number = parse(what);
        let mut found = false;
        for j in self.jobs.iter().flatten() {
            let name = PROGRAM_NAMES[j.program as usize];
            if number != Some(j.id) && name != what {
                continue;
            }
            found = true;
            let mut l = Line::<120>::new();
            match kill(j.cap) {
                0 => l.str(b"stopping "),
                _ => l.str(b"already ended: "), // its exit signal is on its way
            };
            l.str(name).str(b" (process ").dec(j.id).str(b")\n");
            say(l.as_bytes());
        }
        if !found {
            let mut l = Line::<160>::new();
            match number {
                Some(id) => l
                    .str(b"process ")
                    .dec(id)
                    .str(b": this shell holds no capability for it, so it can't stop it (only what it started)\n"),
                None => l.str(b"no ").str(what).str(b" started from this shell is running\n"),
            };
            say(l.as_bytes());
        }
    }

    /// revoke ID, or revoke NAME: take back everything derived from what
    /// this shell lent it, wherever it went.
    fn take_back(&mut self, what: &[u8]) {
        if what.is_empty() {
            return say(b"revoke what? a process number or a program's name (jobs lists them)\n");
        }
        let number = parse(what);
        let mut found = false;
        for j in self.jobs.iter().flatten() {
            let name = PROGRAM_NAMES[j.program as usize];
            if number != Some(j.id) && name != what {
                continue;
            }
            found = true;
            let mut l = Line::<160>::new();
            l.str(b"took back what this shell lent ").str(name).str(b" (process ").dec(j.id).str(b"): ");
            match take_back(&j.loans) {
                Ok(0) => l.str(b"nothing was still out"),
                Ok(1) => l.str(b"1 capability"),
                Ok(n) => l.dec(n).str(b" capabilities"),
                Err(e) => l.str(b"refused (error -").dec(e.unsigned_abs()).str(b")"),
            };
            l.str(b"\n");
            say(l.as_bytes());
        }
        if !found {
            let mut l = Line::<160>::new();
            match number {
                Some(id) => l.str(b"process ").dec(id).str(b": this shell lent it nothing (only what it started)\n"),
                None => l.str(b"no ").str(what).str(b" started from this shell is running\n"),
            };
            say(l.as_bytes());
        }
    }

    /// The processes with these bits (1 << id) ended: say how, take back
    /// what they passed on, and let their capabilities go.
    fn ended(&mut self, bits: u64) {
        for slot in self.jobs.iter_mut() {
            let Some(j) = *slot else { continue };
            if j.id >= 64 || bits & (1 << j.id) == 0 {
                continue;
            }
            let mut l = Line::<120>::new();
            l.str(PROGRAM_NAMES[j.program as usize]).str(b" (process ").dec(j.id).str(b") ended: ");
            match status(j.cap) {
                Ok((STATUS_EXITED, code)) => {
                    l.str(b"exit code ");
                    if (code as i64) < 0 {
                        l.str(b"-").dec((code as i64).unsigned_abs()).str(error_name(code as i64));
                    } else {
                        l.dec(code);
                    }
                }
                Ok((STATUS_FAULT, vector)) => {
                    l.str(b"stopped by the CPU: exception ").dec(vector).str(exception_name(vector));
                }
                Ok((STATUS_KILLED, _)) => {
                    l.str(b"stopped (kill)");
                }
                Ok((STATUS_RUNNING, _)) => continue, // not this one after all
                _ => {
                    l.str(b"how is no longer known");
                }
            }
            // What it lent on to a program of its own that still runs: the
            // loan ends with it.
            match take_back(&j.loans) {
                Ok(0) | Err(_) => {}
                Ok(1) => {
                    l.str(b"; took back 1 capability it had passed on");
                }
                Ok(n) => {
                    l.str(b"; took back ").dec(n).str(b" capabilities it had passed on");
                }
            }
            l.str(b"\n");
            say(l.as_bytes());
            let_go(&j.loans);
            drop_cap(j.cap);
            *slot = None;
        }
    }
}

/// What the shell lends `program` (Entry 25): a copy of its console, and
/// for relay a Factory that starts tickers and nothing else. Each is derived
/// from the shell's own, and stays in a slot of the shell's.
fn lend(program: u64) -> Result<[u64; LOANS], i64> {
    let mut loans = [NOT_LENT; LOANS];
    let console = copy(CONSOLE);
    if console < 0 {
        return Err(console);
    }
    loans[0] = console as u64;
    if program == PROGRAM_RELAY {
        let tickers = mint(FACTORY, 1 << PROGRAM_TICKER, 0);
        if tickers < 0 {
            let_go(&loans);
            return Err(tickers);
        }
        loans[1] = tickers as u64;
    }
    Ok(loans)
}

/// Revoke each loan: everything derived from them, in every process.
/// Returns how many capabilities went.
fn take_back(loans: &[u64; LOANS]) -> Result<u64, i64> {
    let mut gone = 0;
    for &loan in loans.iter().filter(|&&l| l != NOT_LENT) {
        let n = revoke(loan);
        if n < 0 {
            return Err(n);
        }
        gone += n as u64;
    }
    Ok(gone)
}

fn let_go(loans: &[u64; LOANS]) {
    for &loan in loans.iter().filter(|&&l| l != NOT_LENT) {
        drop_cap(loan);
    }
}

/// The kernel's error codes, by name, for exit codes.
fn error_name(code: i64) -> &'static [u8] {
    match code {
        ERR_NO_CAPABILITY => b" (no capability)",
        ERR_WRONG_CAPABILITY => b" (wrong capability)",
        ERR_PEER_GONE => b" (peer gone)",
        ERR_INTERRUPTED => b" (interrupted)",
        ERR_NO_SLOT => b" (no free slot)",
        _ => b"",
    }
}

fn exception_name(vector: u64) -> &'static [u8] {
    match vector {
        0 => b" (divide error)",
        6 => b" (invalid opcode)",
        13 => b" (general protection)",
        14 => b" (page fault)",
        _ => b"",
    }
}

/// A decimal number, if `s` is one.
fn parse(s: &[u8]) -> Option<u64> {
    if s.is_empty() || s.len() > 9 || !s.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(s.iter().fold(0, |n, &b| n * 10 + (b - b'0') as u64))
}

fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&b| b != b' ').unwrap_or(s.len());
    let end = s.iter().rposition(|&b| b != b' ').map_or(start, |i| i + 1);
    &s[start..end]
}

fn error(what: &[u8], e: i64) {
    let mut l = Line::<96>::new();
    l.str(what).str(b" (error -").dec(e.unsigned_abs()).str(b")\n");
    say(l.as_bytes());
}

fn snapshot() -> Option<(SystemInfo, &'static [u8])> {
    let buf = unsafe { &mut *(&raw mut INFO_BUF) };
    match info(INFO, buf) {
        Ok(sys) => Some((sys, buf)),
        Err(e) => {
            error(b"the system's state can't be read", e);
            None
        }
    }
}

/// A number right-aligned in `width` columns.
fn right(l: &mut Line<120>, v: u64, width: usize) {
    let mut digits = 1;
    let mut x = v;
    while x >= 10 {
        x /= 10;
        digits += 1;
    }
    for _ in digits..width {
        l.str(b" ");
    }
    l.dec(v);
}

fn ps() {
    let Some((sys, buf)) = snapshot() else { return };
    say(b" id  name        caps  doing\n");
    let mut processes = 0;
    for i in 0..sys.tasks {
        let t = task_info(buf, i);
        let mut l = Line::<120>::new();
        right(&mut l, t.id as u64, 3);
        l.str(b"  ").str(t.name());
        for _ in t.name().len()..10 {
            l.str(b" ");
        }
        if t.flags & TASK_FLAG_PROCESS != 0 {
            processes += 1;
            right(&mut l, t.caps as u64, 6);
        } else {
            l.str(b"kernel");
        }
        l.str(b"  ");
        let d = t.detail;
        match t.what {
            TASK_RUNNING => l.str(b"running"),
            TASK_READY if t.flags & TASK_FLAG_IDLE != 0 => l.str(b"idle: runs when nothing else can"),
            TASK_READY => l.str(b"ready to run"),
            TASK_SLEEPING => l.str(b"sleeping"),
            TASK_JOINING => l.str(b"waiting for task ").dec(d).str(b" to end"),
            TASK_SENDING => l.str(b"waiting to send on endpoint ").dec(d),
            TASK_RECEIVING => l.str(b"waiting for a message on endpoint ").dec(d),
            TASK_AWAITING_REPLY => l.str(b"waiting for process ").dec(d).str(b" to answer"),
            TASK_LOCK => l.str(b"waiting for a lock"),
            TASK_IRQ => l.str(b"waiting for interrupt line ").dec(d),
            TASK_FINISHED => l.str(b"finished"),
            TASK_STARTING => l.str(b"starting"),
            _ => l.str(b"?"),
        };
        l.str(b"\n");
        say(l.as_bytes());
    }
    let mut l = Line::<120>::new();
    l.dec(sys.tasks as u64).str(b" tasks: ").dec(processes).str(b" processes, ");
    l.dec(sys.tasks as u64 - processes).str(b" in the kernel\n");
    say(l.as_bytes());
}

impl Shell {
fn caps(&self) {
    let mut held = 0;
    for slot in 0..MAX_CAPS as u64 {
        let (kind, a, b, c) = match cap_info(slot) {
            Ok(k) => k,
            Err(e) => return error(b"cap_info refused", e),
        };
        if kind == KIND_NONE {
            continue;
        }
        held += 1;
        let mut l = Line::<120>::new();
        l.str(b"slot ");
        right(&mut l, slot, 2);
        l.str(b"  ");
        match kind {
            KIND_CONSOLE => l.str(b"Console: writes to the log"),
            KIND_ENDPOINT => {
                l.str(b"Endpoint ").dec(a).str(b":");
                let names: [(u64, &[u8]); 3] = [(SEND, b" send"), (RECV, b" receive"), (GRANT, b" grant")];
                for (bit, name) in names {
                    if b & bit != 0 {
                        l.str(name);
                    }
                }
                if c != 0 {
                    l.str(b", badge ").dec(c);
                }
                &mut l
            }
            KIND_IO_PORTS => l.str(b"I/O ports from ").dec(a).str(b", ").dec(b),
            KIND_IRQ => l.str(b"Irq: interrupt line ").dec(a),
            KIND_POWER => l.str(b"Power: restarts, switches off"),
            KIND_MEMORY => l.str(b"Memory: ").dec(a).str(if b & 1 != 0 { b" pages, writable" } else { b" pages, read-only" }),
            KIND_FACTORY => {
                l.str(b"Factory: starts");
                let mut first = true;
                for (n, name) in PROGRAM_NAMES.iter().enumerate() {
                    if a & (1 << n) != 0 {
                        l.str(if first { b" " } else { b", " }).str(name);
                        first = false;
                    }
                }
                if first {
                    l.str(b" nothing");
                }
                if a & FACTORY_ENDPOINTS != 0 {
                    l.str(b"; makes endpoints");
                }
                &mut l
            }
            KIND_INFO => l.str(b"Info: reads uptime, memory, the tasks"),
            KIND_PROCESS => {
                l.str(b"Process ").dec(a);
                if let Some(j) = self.jobs.iter().flatten().find(|j| j.id == a) {
                    l.str(b" (").str(PROGRAM_NAMES[j.program as usize]).str(b")");
                }
                l.str(b": asks how it is doing, stops it")
            }
            _ => l.str(b"(a kind this shell doesn't know)"),
        };
        // Entry 25: what it is lent to, and how much is derived from it.
        if let Some(j) = self.jobs.iter().flatten().find(|j| j.loans.contains(&slot)) {
            l.str(b"; lent to ").str(PROGRAM_NAMES[j.program as usize]).str(b" (process ").dec(j.id).str(b")");
        }
        match derived(slot) {
            0 => {}
            1 => {
                l.str(b"; 1 copy made from it");
            }
            n => {
                l.str(b"; ").dec(n).str(b" copies made from it");
            }
        }
        l.str(b"\n");
        say(l.as_bytes());
    }
    let mut l = Line::<120>::new();
    l.dec(held).str(b" of ").dec(MAX_CAPS as u64).str(b" slots hold a capability; nothing else is allowed\n");
    say(l.as_bytes());
}
}

fn uptime() {
    let Some((sys, _)) = snapshot() else { return };
    let hz = sys.ticks_per_second.max(1);
    let seconds = sys.ticks / hz;
    let mut l = Line::<120>::new();
    l.str(b"up ").dec(seconds / 3600).str(b" h ").dec(seconds / 60 % 60).str(b" min ").dec(seconds % 60).str(b" s");
    l.str(b" (").dec(sys.ticks).str(b" timer ticks, ").dec(hz).str(b" a second); ");
    l.dec(sys.switches).str(b" task switches, ").dec(sys.syscalls).str(b" system calls\n");
    say(l.as_bytes());
}

fn mem() {
    let Some((sys, _)) = snapshot() else { return };
    let mib = |frames: u64| frames * PAGE / (1024 * 1024);
    let mut l = Line::<120>::new();
    l.str(b"memory: ").dec(mib(sys.free_frames)).str(b" MiB free of ").dec(mib(sys.usable_frames));
    l.str(b" MiB usable (").dec(sys.free_frames).str(b" of ").dec(sys.usable_frames).str(b" pages of 4 KiB)\n");
    say(l.as_bytes());
}

fn power_off_or_restart(action: u64, what: &[u8]) {
    say(what);
    let e = power(POWER_SLOT, action); // returns only if refused
    error(b"refused", e);
}
