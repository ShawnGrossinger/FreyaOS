//! audit_chain.rs — what an audit entry is, and what it means for a chain
//! of them to verify (Entry 32; manifesto Section V).
//!
//! Like blake3.rs, plain Rust with nothing from the kernel, built three
//! times: into the kernel (audit.rs records the chain), into the user
//! programs' library (the shell verifies it), and into the host tool that
//! verifies a chain the kernel printed (build/audit_verify.rs). One
//! definition, so the three can't disagree about the format; what makes a
//! verification outside the kernel worth something is where it runs.
//!
//! An entry is 96 bytes, little-endian, in this order:
//!
//!    0  kind          what happened (the constants below)
//!    8  actor         the process or kernel task that caused it: its slot's
//!                     generation << 32 | its task id (0: the kernel at boot)
//!   16  subject       what it happened to: a capability's serial number, a
//!                     process (as actor), a segment's or an interval's number
//!   24  data[0..3]    three numbers whose meaning the kind gives
//!   48  cpu_time      the CPU's time-stamp counter when it was recorded
//!   56  witness_time  the External Witness's clock (Phase C); 0 until then
//!   64  detail        32 bytes the kind gives: a hash, or a capability
//!
//! The manifesto's AuditEntry (event_type, subject, actor, cpu_timestamp,
//! witness_timestamp), with room for what each kind needs to say.
//!
//! Entries are linked: H(n) = BLAKE3(H(n-1) || entry n), so each hash
//! stands for everything recorded before it. The chain is cut into
//! segments of SEGMENT_ENTRIES; each begins with a header entry (kind
//! SEGMENT), the manifesto's AuditChainHeader: its data is the format
//! version, the hash algorithm and the segment's length, its cpu_time when
//! it was created, and its detail the chain's hash just before it, which
//! for segment 0 is 32 zero bytes. So a segment can be checked on its own
//! from its header, and a header that doesn't match the hash before it is
//! a break in the chain.

// Built into three crates (above), and not every one uses every part.
#![allow(dead_code)]

use crate::blake3;

pub const ENTRY_BYTES: usize = 96;
/// Entries in a segment, its header included.
pub const SEGMENT_ENTRIES: u64 = 256;
/// The format of entries and headers described above.
pub const FORMAT: u64 = 1;
/// The hash algorithms a header can name (manifesto 5.4).
pub const HASH_BLAKE3: u64 = 1;

// What happened (an entry's kind).
/// A segment's header (see above). Actor 0; subject: the segment's number.
pub const SEGMENT: u64 = 1;
/// A program measured, the first time it is started: subject its length in
/// bytes, data its name (16 bytes, 0-padded), detail BLAKE3 of its image.
pub const PROGRAM: u64 = 2;
/// A process started: subject the new process, data [its argument, how
/// many capabilities it was given, 0], detail BLAKE3 of its program.
pub const PROCESS_START: u64 = 3;
/// A process ended: subject the process, data [how (END_*), the exit code
/// or the exception's vector, 0].
pub const PROCESS_END: u64 = 4;
/// A process stopped by a CPU exception: subject the process, data [the
/// vector, the instruction's address, CR2 for a page fault or else the
/// error code].
pub const FAULT: u64 = 5;
/// A capability put in a slot: subject its serial number, data [the
/// process holding it, the slot, the serial of the capability it was
/// copied from (0: the kernel made it)], detail the capability (cap_detail).
pub const CAP_MADE: u64 = 6;
/// A capability taken out of its slot (dropped, revoked, moved in a
/// message, or its process gone): subject its serial, data [the process
/// that held it, the slot, 0], detail the capability.
pub const CAP_GONE: u64 = 7;
/// A revoke: subject the serial of the capability revoked from, data [the
/// process holding it, its slot, how many copies it took].
pub const REVOKE: u64 = 8;
/// A process asked to stop: subject the process.
pub const KILL: u64 = 9;
/// One interval of the events that happen thousands of times a second
/// (manifesto 5.1): subject its number (seconds since the timer started),
/// data [messages delivered, task switches, system calls] during it,
/// detail BLAKE3 over each message (sender, receiver, badge, words) and
/// each switch (from, to), in order.
pub const INTERVAL: u64 = 10;

// PROCESS_END's how.
pub const END_EXIT: u64 = 0;
pub const END_FAULT: u64 = 1;
pub const END_KILLED: u64 = 2;

pub fn kind_name(kind: u64) -> &'static str {
    match kind {
        SEGMENT => "segment",
        PROGRAM => "program",
        PROCESS_START => "process start",
        PROCESS_END => "process end",
        FAULT => "fault",
        CAP_MADE => "cap made",
        CAP_GONE => "cap gone",
        REVOKE => "revoke",
        KILL => "kill",
        INTERVAL => "interval",
        _ => "unknown",
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub kind: u64,
    pub actor: u64,
    pub subject: u64,
    pub data: [u64; 3],
    pub cpu_time: u64,
    pub witness_time: u64,
    pub detail: [u8; 32],
}

impl Entry {
    pub const EMPTY: Entry = Entry { kind: 0, actor: 0, subject: 0, data: [0; 3], cpu_time: 0, witness_time: 0, detail: [0; 32] };

    pub fn to_bytes(&self) -> [u8; ENTRY_BYTES] {
        let mut b = [0u8; ENTRY_BYTES];
        let words = [self.kind, self.actor, self.subject, self.data[0], self.data[1], self.data[2], self.cpu_time, self.witness_time];
        for (i, w) in words.iter().enumerate() {
            b[8 * i..8 * i + 8].copy_from_slice(&w.to_le_bytes());
        }
        b[64..96].copy_from_slice(&self.detail);
        b
    }

    pub fn from_bytes(b: &[u8; ENTRY_BYTES]) -> Entry {
        let w = |i: usize| {
            let mut x = [0u8; 8];
            x.copy_from_slice(&b[8 * i..8 * i + 8]);
            u64::from_le_bytes(x)
        };
        let mut detail = [0u8; 32];
        detail.copy_from_slice(&b[64..96]);
        Entry { kind: w(0), actor: w(1), subject: w(2), data: [w(3), w(4), w(5)], cpu_time: w(6), witness_time: w(7), detail }
    }
}

/// H(n) = BLAKE3(H(n-1) || entry n).
pub fn link(before: &[u8; 32], entry: &Entry) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(before);
    h.update(&entry.to_bytes());
    h.finalize()
}

/// Where and why a chain failed to verify.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Broken {
    /// The first entry given isn't a segment's header, so there is nothing
    /// to start from.
    NoHeader(u64),
    /// A segment's header where none belongs, or none where one does: an
    /// entry added or taken away.
    Misplaced(u64),
    /// A header whose record of the chain before it is not what the
    /// entries before it hash to: something before it was changed, added,
    /// removed or reordered.
    Link(u64),
    /// A header numbered out of turn: a segment missing or repeated.
    SegmentNumber(u64),
    /// A header in a format or with a hash this verifier doesn't know.
    Format(u64),
    /// A CPU time earlier than the entry before it.
    TimeBackwards(u64),
    /// Every link held, but the last hash isn't the head the chain's
    /// keeper reports: an entry since the last header was changed (or the
    /// head is someone else's).
    Head,
}

impl Broken {
    /// The entry it was found at (None for Head: at the end).
    pub fn at(&self) -> Option<u64> {
        match *self {
            Broken::NoHeader(n) | Broken::Misplaced(n) | Broken::Link(n) | Broken::SegmentNumber(n) | Broken::Format(n) | Broken::TimeBackwards(n) => Some(n),
            Broken::Head => None,
        }
    }

    pub fn what(&self) -> &'static str {
        match self {
            Broken::NoHeader(_) => "the first entry is not a segment's header",
            Broken::Misplaced(_) => "a segment's header out of place: an entry added or taken away",
            Broken::Link(_) => "this header's record of the chain before it doesn't match the entries before it",
            Broken::SegmentNumber(_) => "a segment numbered out of turn",
            Broken::Format(_) => "a header in a format or with a hash not known here",
            Broken::TimeBackwards(_) => "a CPU time earlier than the entry before",
            Broken::Head => "the entries since the last header don't hash to the head reported",
        }
    }
}

/// Verifies a chain fed to it entry by entry, from a segment's header.
/// The first header is taken at its word for what came before it (that
/// can only be checked against a seal kept somewhere else: the boot log,
/// later the TPM); everything after it must follow.
pub struct Verifier {
    hash: [u8; 32],
    next: u64,
    segment: u64,
    time: u64,
    started: bool,
}

impl Verifier {
    pub const fn new() -> Self {
        Verifier { hash: [0; 32], next: 0, segment: 0, time: 0, started: false }
    }

    /// Entry number `n` (counted from the first ever recorded). Numbers
    /// must follow on, from a segment's start.
    pub fn feed(&mut self, n: u64, e: &Entry) -> Result<(), Broken> {
        let is_header = e.kind == SEGMENT;
        if !self.started {
            if !is_header || n % SEGMENT_ENTRIES != 0 {
                return Err(Broken::NoHeader(n));
            }
            self.started = true;
            self.hash = e.detail;
            self.segment = e.subject;
        } else {
            if n != self.next || is_header != (n % SEGMENT_ENTRIES == 0) {
                return Err(Broken::Misplaced(n));
            }
            if is_header {
                if e.detail != self.hash {
                    return Err(Broken::Link(n));
                }
                if e.subject != self.segment + 1 {
                    return Err(Broken::SegmentNumber(n));
                }
                self.segment = e.subject;
            }
            if e.cpu_time < self.time {
                return Err(Broken::TimeBackwards(n));
            }
        }
        if is_header && (e.data[0] != FORMAT || e.data[1] != HASH_BLAKE3 || e.data[2] != SEGMENT_ENTRIES || e.subject != n / SEGMENT_ENTRIES) {
            return Err(Broken::Format(n));
        }
        self.time = e.cpu_time;
        self.hash = link(&self.hash, e);
        self.next = n + 1;
        Ok(())
    }

    /// After the last entry: does the chain end at `head`?
    pub fn finish(&self, head: &[u8; 32]) -> Result<(), Broken> {
        if self.hash == *head { Ok(()) } else { Err(Broken::Head) }
    }

    /// The hash of everything fed so far.
    pub fn hash(&self) -> [u8; 32] {
        self.hash
    }

    /// Entries fed so far, counting from the first's number.
    pub fn next(&self) -> u64 {
        self.next
    }
}

impl Default for Verifier {
    fn default() -> Self {
        Self::new()
    }
}

/// Verify `entries`, numbered from `first`, ending at `head`. Ok: how many.
pub fn verify(first: u64, entries: &[Entry], head: &[u8; 32]) -> Result<u64, Broken> {
    let mut v = Verifier::new();
    for (i, e) in entries.iter().enumerate() {
        v.feed(first + i as u64, e)?;
    }
    v.finish(head)?;
    Ok(entries.len() as u64)
}
