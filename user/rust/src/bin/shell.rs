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
//!
//! No port, no interrupt, no screen, no Factory: it can't start anything
//! or reach a device, and a fault in it ends only it (init starts it
//! again). The keyboard's own keys stay with the PS/2 driver
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
            run(&line[..len]);
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
    b"  restart    restart the machine\n",
    b"  off        switch the machine off\n",
    b"keys: Up/Down earlier lines, Esc clears the line, PgUp/PgDn scroll the log\n",
];

fn run(line: &[u8]) {
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
        b"caps" => caps(),
        b"uptime" => uptime(),
        b"mem" => mem(),
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

fn caps() {
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
            KIND_FACTORY => l.str(b"Factory: starts programs, makes endpoints"),
            KIND_INFO => l.str(b"Info: reads uptime, memory, the tasks"),
            _ => l.str(b"(a kind this shell doesn't know)"),
        };
        l.str(b"\n");
        say(l.as_bytes());
    }
    let mut l = Line::<120>::new();
    l.dec(held).str(b" of ").dec(MAX_CAPS as u64).str(b" slots hold a capability; nothing else is allowed\n");
    say(l.as_bytes());
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
