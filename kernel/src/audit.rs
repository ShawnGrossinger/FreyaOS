//! audit.rs — the kernel's audit chain (Entry 32; manifesto Section V,
//! the first half of Phase B: the chain in software).
//!
//! What it records (audit_chain.rs has each kind's fields):
//!   - processes starting, with BLAKE3 of the program they run (each
//!     program measured once, the first time it starts), and ending
//!   - faults: a process stopped by a CPU exception
//!   - capabilities put in a slot (by whom, for whom, copied from which),
//!     taken out of one, and revokes; a process asked to stop
//!   - once a second, the events that happen thousands of times a second
//!     as counts and one hash: messages delivered (sender, receiver, badge,
//!     words) and task switches (from, to), in the order they happened
//!   - a header at the start of each segment of 256 entries, carrying the
//!     chain's hash just before it, so each segment is sealed by the next
//!
//! Where it lives: in memory, for one boot. There is no storage driver yet,
//! so the last 8 segments (2048 entries) are kept; an older segment is
//! overwritten whole, and what is left of it is its seal: the chain's hash
//! at its end, printed to the log when it is sealed and kept here for the
//! last 64. Entry 33 records each seal in the TPM as well.
//!
//! Who may read it: a holder of the Info capability (init, and the shell),
//! through the audit system call. Nothing can change or remove an entry
//! except the kernel itself, and a change shows: the next header, or the
//! head, no longer matches (audit_chain.rs: Verifier).
//!
//! Anchored (Entry 33): each sealed segment's seal is extended into a TPM
//! 1.2's PCR 15 (tpm.rs), from task context (the boot thread after the
//! self-tests, then the clock task once a second), and the PCR's new
//! value, as the TPM reported it, is recorded in the chain (ANCHOR).
//! A PCR can only be extended, never set, and goes back to zeros only
//! when the machine resets, so the TPM holds a summary of every seal of
//! this boot, in order, that the kernel can't take back.
//!
//! What it isn't yet: witnessed from outside. The PCR is read through this
//! kernel, so a kernel that lies could misreport it; what proves a PCR to
//! someone else is a TPM quote (signed by a key that needs the TPM to be
//! owned: not on the T430) or the External Witness (Phase C) (5.2: the
//! chain is "meaningful only when verification occurs outside the system
//! being audited"). The CPU's time-stamp counter is the only clock until
//! then (5.3: software-only mode); witness_time is 0.

use crate::audit_chain::{self as chain, Broken, Entry, Verifier, SEGMENT_ENTRIES};
use crate::blake3;
use crate::cap::Cap;
use crate::lock::IrqLock;
use crate::sched::{self, TaskId};

/// Segments kept in memory: the open one and the 7 before it.
pub const KEPT_SEGMENTS: u64 = 8;
pub const KEPT: u64 = SEGMENT_ENTRIES * KEPT_SEGMENTS;
/// Seals kept: the chain's hash at the end of each of the last 64 segments.
const SEALS: u64 = 64;
/// Programs whose measurement is remembered.
const PROGRAMS: usize = 16;
/// Entry kinds counted (audit_chain.rs numbers them from 1).
pub const KINDS: usize = 16;

struct Store {
    entries: [Entry; KEPT as usize],
    /// Entries recorded since boot: the next one's number.
    next: u64,
    /// The chain's hash after the last entry (32 zeros before the first).
    head: [u8; 32],
    /// Segment n's seal at n % SEALS.
    seals: [[u8; 32]; SEALS as usize],
    /// How many of each kind since boot.
    counts: [u64; KINDS],
    /// Programs measured: the image's address and length, and its hash.
    programs: [(u64, u64, [u8; 32]); PROGRAMS],
}

// All zeros, so it stays in .bss and out of the kernel image.
static STORE: IrqLock<Store> = IrqLock::new(Store {
    entries: [Entry::EMPTY; KEPT as usize],
    next: 0,
    head: [0; 32],
    seals: [[0; 32]; SEALS as usize],
    counts: [0; KINDS],
    programs: [(0, 0, [0; 32]); PROGRAMS],
});

impl Store {
    /// Link `e` onto the chain; Some((segment, seal)) if it ended a segment.
    fn push(&mut self, e: Entry) -> Option<(u64, [u8; 32])> {
        self.head = chain::link(&self.head, &e);
        let n = self.next;
        self.entries[(n % KEPT) as usize] = e;
        self.next = n + 1;
        if let Some(c) = self.counts.get_mut(e.kind as usize) {
            *c += 1;
        }
        if self.next % SEGMENT_ENTRIES == 0 {
            let segment = n / SEGMENT_ENTRIES;
            self.seals[(segment % SEALS) as usize] = self.head;
            return Some((segment, self.head));
        }
        None
    }
}

/// The number of the oldest entry kept, given how many there have been:
/// the start of the oldest segment not yet overwritten.
fn first_kept(next: u64) -> u64 {
    if next <= KEPT {
        0
    } else {
        (next - KEPT).div_ceil(SEGMENT_ENTRIES) * SEGMENT_ENTRIES
    }
}

/// Record an event, caused by the task running now. Any context: the
/// chain's lock turns interrupts off; nothing here allocates.
pub fn record(kind: u64, subject: u64, data: [u64; 3], detail: [u8; 32]) {
    let actor = sched::pid(sched::current());
    let sealed = {
        let mut s = STORE.lock();
        // Read under the lock, so the times go up in the chain's order.
        let time = crate::cpu::rdtsc();
        if s.next % SEGMENT_ENTRIES == 0 {
            let header = Entry {
                kind: chain::SEGMENT,
                actor: 0,
                subject: s.next / SEGMENT_ENTRIES,
                data: [chain::FORMAT, chain::HASH_BLAKE3, SEGMENT_ENTRIES],
                cpu_time: time,
                witness_time: 0,
                detail: s.head,
            };
            s.push(header); // a header is never a segment's last entry
        }
        s.push(Entry { kind, actor, subject, data, cpu_time: time, witness_time: 0, detail })
    };
    if let Some((segment, seal)) = sealed {
        // The seal's first copy outside the kernel: build/audit_verify.rs
        // checks a printed chain against these lines.
        let mut l = Out::new();
        l.str(b"[audit] segment ").dec(segment).str(b" sealed: entries ").dec(segment * SEGMENT_ENTRIES);
        l.str(b" to ").dec(segment * SEGMENT_ENTRIES + SEGMENT_ENTRIES - 1).str(b", chain hash ").str(&blake3::hex(&seal));
        l.print();
    }
}

/// One line of output, built first and printed in one call: one call is
/// never split by another task's output (main.rs: serial_write_str), and
/// these lines are read back by a program.
struct Out {
    buf: [u8; 256],
    len: usize,
}

impl Out {
    fn new() -> Self {
        Out { buf: [0; 256], len: 0 }
    }
    fn str(&mut self, s: &[u8]) -> &mut Self {
        let n = s.len().min(self.buf.len() - 1 - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s[..n]);
        self.len += n;
        self
    }
    fn dec(&mut self, mut v: u64) -> &mut Self {
        let mut digits = [0u8; 20];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.str(&digits[i..])
    }
    fn print(&mut self) {
        self.buf[self.len] = b'\n';
        crate::serial_write_str(&self.buf[..self.len + 1]);
    }
}

/// BLAKE3 of a program's image. The first time a program starts it is
/// measured and recorded (PROGRAM); after that the measurement is reused.
/// Task context: hashing a program takes a while, so it is done with
/// interrupts as the caller has them, outside the lock.
pub fn measure(name: &str, image: &'static [u8]) -> [u8; 32] {
    let key = (image.as_ptr() as u64, image.len() as u64);
    let known = STORE.lock().programs.iter().find(|p| (p.0, p.1) == key).map(|p| p.2);
    if let Some(hash) = known {
        return hash;
    }
    let hash = blake3::hash(image);
    let mut words = [0u8; 16];
    let n = name.len().min(16);
    words[..n].copy_from_slice(&name.as_bytes()[..n]);
    let (lo, hi) = words.split_at(8);
    let word = |b: &[u8]| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
    record(chain::PROGRAM, image.len() as u64, [word(lo), word(hi), 0], hash);
    let mut s = STORE.lock();
    if let Some(slot) = s.programs.iter_mut().find(|p| p.1 == 0) {
        *slot = (key.0, key.1, hash);
    }
    hash
}

pub fn process_started(process: u64, program: &[u8; 32], arg: u64, caps: u64) {
    record(chain::PROCESS_START, process, [arg, caps, 0], *program);
}

pub fn process_ended(process: u64, status: sched::ExitStatus) {
    let (how, value) = match status {
        sched::ExitStatus::Code(code) => (chain::END_EXIT, code),
        sched::ExitStatus::Fault(vector) => (chain::END_FAULT, vector as u64),
        sched::ExitStatus::Killed => (chain::END_KILLED, 0),
    };
    record(chain::PROCESS_END, process, [how, value, 0], [0; 32]);
}

/// The running process, stopped by exception `vector` at `rip`; `extra`:
/// CR2 for a page fault, else the error code.
pub fn fault(vector: u64, rip: u64, extra: u64) {
    record(chain::FAULT, sched::pid(sched::current()), [vector, rip, extra], [0; 32]);
}

pub fn cap_made(serial: u32, holder: u64, slot: usize, parent: u32, cap: &Cap) {
    record(chain::CAP_MADE, serial as u64, [holder, slot as u64, parent as u64], cap_detail(cap));
}

pub fn cap_gone(serial: u32, holder: u64, slot: usize, cap: &Cap) {
    record(chain::CAP_GONE, serial as u64, [holder, slot as u64, 0], cap_detail(cap));
}

pub fn revoked(serial: u32, holder: u64, slot: usize, copies: u64) {
    record(chain::REVOKE, serial as u64, [holder, slot as u64, copies], [0; 32]);
}

pub fn kill_asked(process: u64) {
    record(chain::KILL, process, [0; 3], [0; 32]);
}

/// A capability as 32 bytes: byte 0 its kind (info.rs's numbers), then its
/// fields, little-endian. A Memory capability's physical address is in
/// it: cap_info keeps that from processes, but the chain is for whoever
/// audits the system, and replaying it needs every field.
pub fn cap_detail(cap: &Cap) -> [u8; 32] {
    use crate::info::*;
    let mut d = [0u8; 32];
    let kind = match *cap {
        Cap::None => KIND_NONE,
        Cap::Console => KIND_CONSOLE,
        Cap::Endpoint { ep, badge, rights } => {
            d[1] = ep;
            d[2] = rights;
            d[8..16].copy_from_slice(&badge.to_le_bytes());
            KIND_ENDPOINT
        }
        Cap::IoPorts { base, count } => {
            d[2..4].copy_from_slice(&base.to_le_bytes());
            d[4..6].copy_from_slice(&count.to_le_bytes());
            KIND_IO_PORTS
        }
        Cap::Irq { line } => {
            d[1] = line;
            KIND_IRQ
        }
        Cap::Power => KIND_POWER,
        Cap::Memory { base, pages, flags } => {
            d[1] = flags;
            d[4..8].copy_from_slice(&pages.to_le_bytes());
            d[8..16].copy_from_slice(&base.to_le_bytes());
            KIND_MEMORY
        }
        Cap::Factory { rights } => {
            d[4..8].copy_from_slice(&rights.to_le_bytes());
            KIND_FACTORY
        }
        Cap::Info => KIND_INFO,
        Cap::Process { id, generation } => {
            d[1] = id;
            d[4..8].copy_from_slice(&generation.to_le_bytes());
            KIND_PROCESS
        }
        Cap::Budget { pages } => {
            d[4..8].copy_from_slice(&pages.to_le_bytes());
            KIND_BUDGET
        }
    };
    d[0] = kind as u8;
    d
}

// --- The events that happen thousands of times a second (manifesto 5.1):
// counted, and hashed in order, for one interval at a time.

struct Interval {
    hasher: blake3::Hasher,
    messages: u64,
    switches: u64,
    /// System calls made before this interval began.
    calls_before: u64,
}

static INTERVAL: IrqLock<Interval> = IrqLock::new(Interval { hasher: blake3::Hasher::new(), messages: 0, switches: 0, calls_before: 0 });

/// A message delivered from task `from` to task `to` (ipc.rs: deliver).
pub fn note_message(from: TaskId, to: TaskId, badge: u64, words: &[u64; 4]) {
    let mut b = [0u8; 48];
    b[0] = 1;
    b[1] = from as u8;
    b[2] = to as u8;
    b[8..16].copy_from_slice(&badge.to_le_bytes());
    for (i, w) in words.iter().enumerate() {
        b[16 + 8 * i..24 + 8 * i].copy_from_slice(&w.to_le_bytes());
    }
    let mut i = INTERVAL.lock();
    i.hasher.update(&b);
    i.messages += 1;
}

/// A task switch (sched.rs: schedule).
pub fn note_switch(from: TaskId, to: TaskId) {
    let mut i = INTERVAL.lock();
    i.hasher.update(&[2, from as u8, to as u8, 0]);
    i.switches += 1;
}

/// End interval `number` and record it (the clock task, once a second).
pub fn interval(number: u64) {
    let (hash, messages, switches, calls) = {
        let mut i = INTERVAL.lock();
        let calls_now = crate::syscall::counts().0;
        let out = (i.hasher.finalize(), i.messages, i.switches, calls_now - i.calls_before);
        *i = Interval { hasher: blake3::Hasher::new(), messages: 0, switches: 0, calls_before: calls_now };
        out
    };
    record(chain::INTERVAL, number, [messages, switches, calls], hash);
}

// --- Anchoring it in the TPM (Entry 33).

struct Anchoring {
    /// Segments whose seals have been extended (or passed over).
    anchored: u64,
    /// PCR 15 as the TPM last reported it.
    pcr: [u8; 20],
    /// Extends whose result wasn't SHA-1(the PCR before || the seal).
    mismatches: u64,
    /// Seals extended.
    extended: u64,
    /// The TPM failed a command; no more are tried.
    failed: bool,
}

static ANCHORING: IrqLock<Anchoring> = IrqLock::new(Anchoring { anchored: 0, pcr: [0; 20], mismatches: 0, extended: 0, failed: false });
/// One anchor() at a time: the boot thread and the clock task both call it.
static ANCHOR_ONE: crate::sync::Mutex<()> = crate::sync::Mutex::new(());

/// What tpm::init found, into the chain (TPM), and PCR 15's value then.
pub fn tpm_found(found: Option<crate::tpm::Found>, pcr: Option<[u8; 20]>) {
    let Some(f) = found else { return };
    let mut detail = [0u8; 32];
    if let Some(v) = pcr {
        detail[..20].copy_from_slice(&v);
        ANCHORING.lock().pcr = v;
    }
    let used = (f.used && pcr.is_some()) as u64;
    record(chain::TPM, crate::tpm::PCR as u64, [(f.vendor as u64) << 16 | f.device as u64, f.revision as u64, used], detail);
}

/// Extend PCR 15 with every seal not yet extended, in order, and record
/// each in the chain. Task context, interrupts on (tpm.rs).
pub fn anchor() {
    if !crate::tpm::in_use() {
        return;
    }
    let _one = ANCHOR_ONE.lock();
    loop {
        let (segment, before, failed) = {
            let a = ANCHORING.lock();
            (a.anchored, a.pcr, a.failed)
        };
        let (sealed, seal) = {
            let s = STORE.lock();
            let sealed = s.next / SEGMENT_ENTRIES;
            (sealed, s.seals[(segment % SEALS) as usize])
        };
        if failed || segment >= sealed {
            return;
        }
        if segment + SEALS < sealed {
            // Sealed so long ago its seal isn't kept: can't be extended.
            ANCHORING.lock().anchored = sealed - SEALS;
            continue;
        }
        let digest = chain::first_20(&seal);
        match crate::tpm::extend(crate::tpm::PCR, &digest) {
            Ok(after) => {
                let right = crate::sha1::extend(&before, &digest) == after;
                {
                    let mut a = ANCHORING.lock();
                    a.anchored = segment + 1;
                    a.pcr = after;
                    a.extended += 1;
                    a.mismatches += (!right) as u64;
                }
                let mut detail = [0u8; 32];
                detail[..20].copy_from_slice(&after);
                record(chain::ANCHOR, segment, [crate::tpm::PCR as u64, 0, 0], detail);
                let mut l = Out::new();
                l.str(b"[tpm] segment ").dec(segment).str(b"'s seal extended into PCR 15: now ").str(&crate::tpm::hex20(&after));
                if !right {
                    l.str(b" (NOT SHA-1 of the value before and the seal)");
                }
                l.print();
            }
            Err(e) => {
                {
                    let mut a = ANCHORING.lock();
                    a.failed = true;
                }
                record(chain::ANCHOR, segment, [crate::tpm::PCR as u64, e.code(), 0], [0; 32]);
                crate::serial_write_str(b"[tpm] segment's seal not extended (");
                crate::tpm::say_error(e);
                crate::serial_write_str(b"): no more anchors this boot\n");
                return;
            }
        }
    }
}

/// The TPM self-test, after the self-tests' seals have been anchored.
/// Every boot, TPM or not, it checks first the rule that keeps this build
/// off real TPMs: QEMU's is driven, through TPM 1.2 only, and Infineon's,
/// STMicroelectronics', Nuvoton's, Atmel's, Broadcom's, Intel's and an
/// unknown vendor's are left alone (tpm.rs: drives).
pub fn selftest_tpm() -> bool {
    use crate::tpm::drives;
    let gate = drives(crate::tpm::QEMU_VENDOR, 0)
        && drives(crate::tpm::QEMU_VENDOR, 2)
        && !drives(crate::tpm::QEMU_VENDOR, 3)
        && [0x15D1u16, 0x104A, 0x1050, 0x1114, 0x14E4, 0x8086, 0x1234].iter().all(|&v| !drives(v, 0) && !drives(v, 2) && !drives(v, 3));
    let Some(f) = crate::tpm::found() else {
        return crate::selftest::check(b"tpm", b"no TPM here: nothing anchored, and nothing claimed to be; this build would drive QEMU's TPM 1.2 and leave any other alone", gate);
    };
    if !f.used {
        crate::serial_write_str(b"[tpm] self-test a TPM found (vendor ");
        crate::serial_write_str(&crate::tpm::hex16(f.vendor));
        crate::serial_write_str(b") and left alone by this build: nothing sent to it but the reads that named it; QEMU's TPM 1.2 would be driven, any other left alone");
        return crate::selftest::verdict(gate);
    }
    let (anchored, pcr, mismatches, extended, failed) = {
        let a = ANCHORING.lock();
        (a.anchored, a.pcr, a.mismatches, a.extended, a.failed)
    };
    let sealed = STORE.lock().next / SEGMENT_ENTRIES;
    let read = crate::tpm::pcr_read(crate::tpm::PCR);
    crate::serial_write_str(b"[tpm] self-test PCR 15: the seals of all ");
    crate::serial_write_u64(sealed);
    crate::serial_write_str(b" sealed segments extended (");
    crate::serial_write_u64(extended);
    crate::serial_write_str(b"), each new value the TPM gave SHA-1 of the one before and the seal (");
    crate::serial_write_u64(mismatches);
    crate::serial_write_str(b" not); read back, it is the last of them; any TPM but QEMU's would be left alone");
    crate::selftest::verdict(gate && !failed && anchored == sealed && extended == sealed && mismatches == 0 && read == Ok(pcr) && sealed > 0)
}

// --- Reading it: the audit system call, and the kernel's own checks.

/// The reply's header: see read().
pub const HEADER_BYTES: usize = 96 + 8 * KINDS + 72;
const READ_VERSION: u32 = 2;

/// audit(info, from, ptr, len): with an Info capability, the chain's state
/// and as many kept entries as fit, from entry number `from` (or the
/// oldest kept, if `from` is older). Returns the bytes written.
///
///   header   0  "FREYAAUD"             32  entries a segment
///            8  version (1)            40  the first entry in this reply
///           12  bytes an entry (96)    48  entries in this reply
///           16  entries since boot     56  hash algorithm (1: BLAKE3)
///           24  the oldest kept        64  the head: the chain's hash
///                                          after the last entry (32 bytes)
///           96  how many of each kind since boot (16 counts of 8 bytes)
///          224  the TPM (version 2, Entry 33): 0 none, 1 found and not
///               used, 2 anchoring, 3 anchoring stopped by an error (4)
///          228  its vendor << 16 | device (4); 232 segments anchored (8)
///          240  the PCR the chain is anchored in (4), 244 its value as
///               the TPM last reported it (20)
///          264  1 if what follows was read from the TPM for this reply (a
///               reply starting at the oldest entry kept), 265 PCR 15 so
///               read (20); 285 to 296 zeros
///
/// then the entries, 96 bytes each (audit_chain.rs). The header is read
/// once, at the start: the entries go up to its count and no further, so
/// the last of them hashes to its head even if more are recorded while
/// the reply is copied. An entry overwritten meanwhile ends the reply.
pub fn read(slot: u64, from: u64, ptr: u64, len: u64) -> i64 {
    use crate::syscall::{copy_to_user, ERR_BAD_ARGUMENT, ERR_WRONG_CAPABILITY};
    if sched::current_cap(slot) != Cap::Info {
        return ERR_WRONG_CAPABILITY;
    }
    if len < HEADER_BYTES as u64 {
        return ERR_BAD_ARGUMENT;
    }
    // PCR 15 read from the TPM now (Entry 33), when the reply starts at
    // the oldest entry kept: with anchor() held off, so the chain's last
    // anchor and the PCR are of the same moment.
    let _no_anchoring = ANCHOR_ONE.lock();
    let fresh = if crate::tpm::in_use() && from <= first_kept(STORE.lock().next) { crate::tpm::pcr_read(crate::tpm::PCR).ok() } else { None };
    let (next, head, counts) = {
        let s = STORE.lock();
        (s.next, s.head, s.counts)
    };
    let start = from.max(first_kept(next)).min(next);
    let room = (len - HEADER_BYTES as u64) / chain::ENTRY_BYTES as u64;
    let mut sent = 0;
    while sent < room && start + sent < next {
        let n = start + sent;
        let e = {
            let s = STORE.lock();
            if n < first_kept(s.next) { None } else { Some(s.entries[(n % KEPT) as usize]) }
        };
        let Some(e) = e else { break };
        if let Err(err) = copy_to_user(ptr + HEADER_BYTES as u64 + sent * chain::ENTRY_BYTES as u64, &e.to_bytes()) {
            return err;
        }
        sent += 1;
    }
    let mut h = [0u8; HEADER_BYTES];
    h[0..8].copy_from_slice(b"FREYAAUD");
    h[8..12].copy_from_slice(&READ_VERSION.to_le_bytes());
    h[12..16].copy_from_slice(&(chain::ENTRY_BYTES as u32).to_le_bytes());
    h[16..24].copy_from_slice(&next.to_le_bytes());
    h[24..32].copy_from_slice(&first_kept(next).to_le_bytes());
    h[32..40].copy_from_slice(&SEGMENT_ENTRIES.to_le_bytes());
    h[40..48].copy_from_slice(&start.to_le_bytes());
    h[48..56].copy_from_slice(&sent.to_le_bytes());
    h[56..64].copy_from_slice(&chain::HASH_BLAKE3.to_le_bytes());
    h[64..96].copy_from_slice(&head);
    for (k, c) in counts.iter().enumerate() {
        h[96 + 8 * k..104 + 8 * k].copy_from_slice(&c.to_le_bytes());
    }
    let (anchored, pcr, failed) = {
        let a = ANCHORING.lock();
        (a.anchored, a.pcr, a.failed)
    };
    let (state, id) = match crate::tpm::found() {
        None => (0u32, 0u32),
        Some(f) if !f.used => (1, (f.vendor as u32) << 16 | f.device as u32),
        Some(f) => (if failed { 3 } else { 2 }, (f.vendor as u32) << 16 | f.device as u32),
    };
    h[224..228].copy_from_slice(&state.to_le_bytes());
    h[228..232].copy_from_slice(&id.to_le_bytes());
    h[232..240].copy_from_slice(&anchored.to_le_bytes());
    h[240..244].copy_from_slice(&crate::tpm::PCR.to_le_bytes());
    h[244..264].copy_from_slice(&pcr);
    if let Some(v) = fresh {
        h[264] = 1;
        h[265..285].copy_from_slice(&v);
    }
    if let Err(err) = copy_to_user(ptr, &h) {
        return err;
    }
    (HEADER_BYTES as u64 + sent * chain::ENTRY_BYTES as u64) as i64
}

/// What the kernel's own check of its kept chain found.
pub struct Checked {
    pub first: u64,
    pub entries: u64,
    /// Kept headers whose record of the chain before them matches the
    /// seal kept for the segment before (all but the oldest kept header).
    pub seals_matching: u64,
    pub seals_wrong: u64,
    /// Anchors whose PCR value checked out (audit_chain.rs: Verifier).
    pub anchors: u64,
    /// The PCR value the chain's last anchor records.
    pub pcr: Option<[u8; 20]>,
}

/// Verify every kept entry, from the oldest kept segment's header to the
/// head, and each kept header against the seal kept for the segment
/// before it. Entry by entry under the lock, so interrupts aren't held off
/// for the whole of it.
pub fn verify_kept() -> Result<Checked, Broken> {
    let (next, head) = {
        let s = STORE.lock();
        (s.next, s.head)
    };
    let first = first_kept(next);
    let mut v = Verifier::new();
    let (mut seals_matching, mut seals_wrong) = (0, 0);
    for n in first..next {
        let (e, seal_before) = {
            let s = STORE.lock();
            let seal = n.checked_sub(1).map(|m| s.seals[((m / SEGMENT_ENTRIES) % SEALS) as usize]);
            (s.entries[(n % KEPT) as usize], seal)
        };
        v.feed(n, &e)?;
        if e.kind == chain::SEGMENT && n > first {
            if seal_before == Some(e.detail) { seals_matching += 1 } else { seals_wrong += 1 }
        }
    }
    v.finish(&head)?;
    Ok(Checked { first, entries: next - first, seals_matching, seals_wrong, anchors: v.anchors_checked(), pcr: v.pcr() })
}

/// How many of each kind since boot.
pub fn counts() -> [u64; KINDS] {
    STORE.lock().counts
}

// --- Self-tests and demos.

/// BLAKE3 against its official vectors, and the verifier against chains
/// changed in each way it must notice. Early in boot (needs the heap).
pub fn selftest_mechanics() -> bool {
    let passed = blake3::test_vectors_passed();
    crate::serial_write_str(b"[blake3] self-test BLAKE3's official test vectors, inputs of 0 to 102400 bytes, each hashed at once and in uneven pieces: ");
    crate::serial_write_u64(passed as u64);
    crate::serial_write_str(b" of ");
    crate::serial_write_u64(blake3::TEST_VECTORS.len() as u64);
    let vectors_ok = crate::selftest::verdict(passed == blake3::TEST_VECTORS.len());

    // A test chain of 600 entries (segments 0 and 1 sealed, 2 open), built
    // as record() builds the real one, with times going up.
    let (chain, head) = test_chain(600);
    let fine = chain::verify(0, &chain, &head) == Ok(600);
    let mut ok = fine;
    let mut try_it = |change: &dyn Fn(&mut alloc::vec::Vec<Entry>), first: u64, want: Broken| {
        let mut c = chain.clone();
        change(&mut c);
        let got = chain::verify(first, &c[(first as usize).min(c.len())..], &head);
        let right = got == Err(want);
        if !right {
            crate::serial_write_str(b"[audit] self-test: a change not caught as it should be, wanted at ");
            crate::serial_write_u64(want.at().unwrap_or(u64::MAX));
            crate::serial_write_str(b", got ");
            crate::serial_write_u64(match got {
                Ok(n) => n,
                Err(b) => b.at().unwrap_or(u64::MAX),
            });
            crate::serial_write_str(b"\n");
        }
        ok &= right;
    };
    // One bit of an entry's data: caught at the next header.
    try_it(&|c| c[100].data[0] ^= 1, 0, Broken::Link(256));
    // One bit of an entry after the last header: caught at the head.
    try_it(&|c| c[550].detail[31] ^= 0x80, 0, Broken::Head);
    // Two entries swapped: the second's time is earlier.
    try_it(&|c| c.swap(10, 11), 0, Broken::TimeBackwards(11));
    // An entry removed: the next header comes one too early.
    try_it(&|c| {
        c.remove(100);
    }, 0, Broken::Misplaced(255));
    // A header's link forged.
    try_it(&|c| c[512].detail[0] ^= 1, 0, Broken::Link(512));
    // A whole segment removed, the rest moved down: its header doesn't
    // link (the hash before it is segment 0's seal, not segment 1's).
    try_it(&|c| {
        c.drain(256..512);
    }, 0, Broken::Link(256));
    // Started part-way through a segment: nothing to start from.
    try_it(&|_| {}, 1, Broken::NoHeader(1));
    // A segment's header numbered out of turn, its link left right.
    try_it(&|c| c[256].subject = 7, 0, Broken::SegmentNumber(256));
    let chain_ok = crate::selftest::check(
        b"audit",
        b"a test chain of 600 entries in 3 segments verifies; one bit changed in an entry or a header's link, two entries swapped, one or a whole segment removed, a segment misnumbered, or a start part-way: each caught where it shows",
        ok,
    );
    vectors_ok & chain_ok
}

/// `n` entries as record() makes them: a header every SEGMENT_ENTRIES,
/// linking to the hash before it. And the head.
fn test_chain(n: u64) -> (alloc::vec::Vec<Entry>, [u8; 32]) {
    let mut v = alloc::vec::Vec::with_capacity(n as usize);
    let mut h = [0u8; 32];
    for i in 0..n {
        let e = if i % SEGMENT_ENTRIES == 0 {
            Entry {
                kind: chain::SEGMENT,
                actor: 0,
                subject: i / SEGMENT_ENTRIES,
                data: [chain::FORMAT, chain::HASH_BLAKE3, SEGMENT_ENTRIES],
                cpu_time: 1000 + i,
                witness_time: 0,
                detail: h,
            }
        } else {
            Entry { kind: chain::CAP_MADE, actor: 3, subject: i, data: [i, i * 7, i * 13], cpu_time: 1000 + i, witness_time: 0, detail: [i as u8; 32] }
        };
        h = chain::link(&h, &e);
        v.push(e);
    }
    (v, h)
}

/// The boot's own chain, after every test process has ended.
pub fn selftest_boot() -> bool {
    let counts = counts();
    let all_kinds = [chain::PROGRAM, chain::PROCESS_START, chain::PROCESS_END, chain::FAULT, chain::CAP_MADE, chain::CAP_GONE, chain::REVOKE, chain::KILL]
        .iter()
        .all(|&k| counts[k as usize] > 0);
    crate::serial_write_str(b"[audit] self-test the boot's own chain: ");
    let verified = match verify_kept() {
        Ok(c) => {
            crate::serial_write_u64(c.entries);
            crate::serial_write_str(b" entries kept (");
            crate::serial_write_u64(c.first);
            crate::serial_write_str(b" to ");
            crate::serial_write_u64(c.first + c.entries - 1);
            crate::serial_write_str(b") verify from the first's header to the head; ");
            crate::serial_write_u64(c.seals_matching);
            crate::serial_write_str(b" headers match the seal of the segment before; ");
            crate::serial_write_u64(c.anchors);
            crate::serial_write_str(b" anchors in the TPM check out");
            c.seals_wrong == 0
        }
        Err(b) => {
            crate::serial_write_str(b"broken: ");
            crate::serial_write_str(b.what().as_bytes());
            false
        }
    };
    crate::serial_write_str(b"; recorded since boot: ");
    let named: [(u64, &[u8]); 8] = [
        (chain::PROGRAM, b" programs measured, "),
        (chain::PROCESS_START, b" processes started, "),
        (chain::PROCESS_END, b" ended, "),
        (chain::FAULT, b" faults, "),
        (chain::CAP_MADE, b" capabilities made, "),
        (chain::CAP_GONE, b" gone, "),
        (chain::REVOKE, b" revokes, "),
        (chain::KILL, b" kills"),
    ];
    for (k, words) in named {
        crate::serial_write_u64(counts[k as usize]);
        crate::serial_write_str(words);
    }
    crate::selftest::verdict(verified && all_kinds)
}

/// Print every kept entry, for a verifier outside (FREYA_DEMO=audit_dump;
/// build/audit_verify.rs reads them back from the serial log).
pub fn dump() {
    let (next, head) = {
        let s = STORE.lock();
        (s.next, s.head)
    };
    let first = first_kept(next);
    let mut l = Out::new();
    l.str(b"[audit] dump: entries ").dec(first).str(b" to ").dec(next.saturating_sub(1)).str(b", head ").str(&blake3::hex(&head));
    l.print();
    for n in first..next {
        let e = STORE.lock().entries[(n % KEPT) as usize];
        let mut l = Out::new();
        l.str(b"[audit] #").dec(n).str(b" ");
        for part in e.to_bytes().chunks(32) {
            let mut p = [0u8; 32];
            p.copy_from_slice(part);
            l.str(&blake3::hex(&p));
        }
        l.print();
    }
    crate::serial_write_str(b"[audit] dump end\n");
}

/// FREYA_DEMO=audit_rewrite (Entry 33): what a careful attacker with the
/// kernel's memory would do. Change one bit of a kept entry, then make the
/// chain whole again: every later link, header, seal and head recomputed,
/// and every anchor's recorded PCR value recomputed from the new seals, so
/// that the chain verifies on its own. What can't be rewritten is the
/// TPM's PCR 15: it holds the seals that were really extended.
pub fn demo_rewrite() {
    let _no_anchoring = ANCHOR_ONE.lock();
    let target = {
        let mut s = STORE.lock();
        let mut a = ANCHORING.lock();
        let first = first_kept(s.next);
        let target = (first + 17).min(s.next.saturating_sub(1));
        s.entries[(target % KEPT) as usize].data[0] ^= 1;
        // Relink everything kept, from the oldest header's record of what
        // came before it.
        let mut h = s.entries[(first % KEPT) as usize].detail;
        let mut pcr: Option<[u8; 20]> = None;
        for n in first..s.next {
            let i = (n % KEPT) as usize;
            let mut e = s.entries[i];
            if e.kind == chain::SEGMENT && n > first {
                e.detail = h;
                let segment = e.subject - 1;
                s.seals[(segment % SEALS) as usize] = h;
            }
            if e.kind == chain::TPM && e.data[2] == 1 {
                pcr = Some(chain::first_20(&e.detail));
            }
            if e.kind == chain::ANCHOR && e.data[1] == 0 {
                if let Some(before) = pcr {
                    let seal = s.seals[(e.subject % SEALS) as usize];
                    let after = crate::sha1::extend(&before, &chain::first_20(&seal));
                    e.detail[..20].copy_from_slice(&after);
                    pcr = Some(after);
                    a.pcr = after;
                }
            }
            s.entries[i] = e;
            h = chain::link(&h, &e);
        }
        if s.next % SEGMENT_ENTRIES == 0 && s.next > first {
            // The last segment sealed, its next header not yet written.
            let segment = s.next / SEGMENT_ENTRIES - 1;
            s.seals[(segment % SEALS) as usize] = h;
        }
        s.head = h;
        target
    };
    crate::serial_write_str(b"[audit] demo: entry ");
    crate::serial_write_u64(target);
    crate::serial_write_str(b" changed, and every link, header, seal and anchor after it rewritten to match
");
    match verify_kept() {
        Ok(c) => {
            crate::serial_write_str(b"[audit] demo: the rewritten chain verifies on its own: ");
            crate::serial_write_u64(c.entries);
            crate::serial_write_str(b" entries, ");
            crate::serial_write_u64(c.anchors);
            crate::serial_write_str(b" anchors
");
            let read = crate::tpm::pcr_read(crate::tpm::PCR);
            match (read, c.pcr) {
                (Ok(tpm), Some(claimed)) if tpm != claimed => {
                    crate::serial_write_str(b"[audit] demo: caught by the TPM: PCR 15 is ");
                    crate::serial_write_str(&crate::tpm::hex20(&tpm));
                    crate::serial_write_str(b", the chain's last anchor says ");
                    crate::serial_write_str(&crate::tpm::hex20(&claimed));
                    crate::serial_write_str(b"
");
                }
                (Ok(_), Some(_)) => crate::serial_write_str(b"[audit] demo: the TPM's PCR 15 matches the rewritten chain: NOT caught
"),
                _ => crate::serial_write_str(b"[audit] demo: no TPM in use, nothing to compare with: NOT caught
"),
            }
        }
        Err(b) => {
            crate::serial_write_str(b"[audit] demo: the rewrite didn't hold together: ");
            crate::serial_write_str(b.what().as_bytes());
            crate::serial_write_str(b"
");
        }
    }
}

/// FREYA_DEMO=audit_tamper: change one bit of a kept entry, as something
/// with write access to the kernel's memory could, and verify.
pub fn demo_tamper() {
    let target = {
        let mut s = STORE.lock();
        let first = first_kept(s.next);
        // Inside the oldest kept segment, so the break shows at the next
        // header, not only at the head.
        let n = (first + 17).min(s.next.saturating_sub(1));
        s.entries[(n % KEPT) as usize].data[0] ^= 1;
        n
    };
    crate::serial_write_str(b"[audit] demo: one bit of entry ");
    crate::serial_write_u64(target);
    crate::serial_write_str(b" changed in memory, as something that can write the kernel's memory could\n");
    match verify_kept() {
        Ok(_) => crate::serial_write_str(b"[audit] demo: the chain still verifies: the change was NOT caught\n"),
        Err(b) => {
            crate::serial_write_str(b"[audit] chain broken at ");
            match b.at() {
                Some(n) => {
                    crate::serial_write_str(b"entry ");
                    crate::serial_write_u64(n);
                }
                None => crate::serial_write_str(b"the head"),
            }
            crate::serial_write_str(b": ");
            crate::serial_write_str(b.what().as_bytes());
            crate::serial_write_str(b"\n");
        }
    }
}
