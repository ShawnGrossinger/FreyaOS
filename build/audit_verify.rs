//! audit_verify.rs — verify, on the build machine, an audit chain FreyaOS
//! printed to its serial log (Entry 32).
//!
//!   audit_verify <serial log>
//!
//! The kernel prints two things this reads:
//!   - while it runs, a line for each segment it seals, with the chain's
//!     hash at its end: "[audit] segment N sealed: entries A to B, chain
//!     hash H". Those lines leave the machine as they are written, so they
//!     are a record the kernel can't take back afterwards.
//!   - with FREYA_DEMO=audit_dump (or audit_tamper), every entry it still
//!     keeps, in hex, and the head: "[audit] dump: entries F to L, head H",
//!     "[audit] #N <192 hex digits>", ..., "[audit] dump end".
//!
//! It checks that the dumped entries form a chain (audit_chain.rs: the
//! kernel's own definition of one, built here from the same file), that
//! the chain ends at the head, and that at the end of each segment the
//! chain's hash is the seal printed when the segment was sealed: so the
//! entries kept now are the ones that were there then. Exit status 0 if
//! all of it holds, 1 if anything doesn't (saying where), 2 if the log
//! has no dump.
//!
//! What this can't tell: whether the kernel recorded what really happened.
//! A kernel that lied from the start would print a consistent lie. What it
//! can tell is that nothing was changed after it was printed, which is
//! what the TPM (Entry 33) and the External Witness (Phase C) extend to
//! the chain itself.

#[path = "../kernel/src/blake3.rs"]
mod blake3;
#[path = "../kernel/src/audit_chain.rs"]
mod audit_chain;

use audit_chain::{Entry, Verifier, ENTRY_BYTES, SEGMENT_ENTRIES};
use std::collections::BTreeMap;
use std::process::exit;

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn hash32(s: &str) -> Option<[u8; 32]> {
    unhex(s.trim())?.try_into().ok()
}

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: audit_verify <serial log>");
        exit(2);
    };
    let text = match std::fs::read(&path) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            eprintln!("audit_verify: {path}: {e}");
            exit(2);
        }
    };

    let mut seals: BTreeMap<u64, [u8; 32]> = BTreeMap::new();
    let mut dump: Option<(u64, u64, [u8; 32])> = None;
    let mut entries: Vec<(u64, Entry)> = Vec::new();
    let mut bad_lines = 0;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("[audit] segment ") {
            // "N sealed: entries A to B, chain hash H"
            let n = rest.split(' ').next().and_then(|n| n.parse::<u64>().ok());
            let h = rest.rsplit(' ').next().and_then(hash32);
            match (n, h) {
                (Some(n), Some(h)) if rest.contains(" sealed: ") => {
                    seals.insert(n, h);
                }
                _ => bad_lines += 1,
            }
        } else if let Some(rest) = line.strip_prefix("[audit] dump: entries ") {
            // "F to L, head H"
            let mut words = rest.split(' ');
            let first = words.next().and_then(|w| w.parse::<u64>().ok());
            let last = words.nth(1).and_then(|w| w.trim_end_matches(',').parse::<u64>().ok());
            let head = rest.rsplit(' ').next().and_then(hash32);
            match (first, last, head) {
                (Some(f), Some(l), Some(h)) => dump = Some((f, l, h)),
                _ => bad_lines += 1,
            }
        } else if let Some(rest) = line.strip_prefix("[audit] #") {
            let mut parts = rest.split(' ');
            let n = parts.next().and_then(|n| n.parse::<u64>().ok());
            let bytes = parts.next().and_then(unhex);
            match (n, bytes) {
                (Some(n), Some(b)) if b.len() == ENTRY_BYTES => {
                    let mut a = [0u8; ENTRY_BYTES];
                    a.copy_from_slice(&b);
                    entries.push((n, Entry::from_bytes(&a)));
                }
                _ => bad_lines += 1,
            }
        }
    }

    let Some((first, last, head)) = dump else {
        println!("audit_verify: no dump in {path} (build with FREYA_DEMO=audit_dump)");
        exit(2);
    };
    let fail = |what: String| -> ! {
        println!("audit_verify: BROKEN {what}");
        exit(1);
    };
    if bad_lines > 0 {
        fail(format!("in the log itself: {bad_lines} [audit] lines that can't be read"));
    }
    if entries.len() as u64 != last + 1 - first {
        fail(format!("in the log itself: the dump says entries {first} to {last}, but has {}", entries.len()));
    }

    let mut v = Verifier::new();
    let mut seals_checked = 0;
    for &(n, ref e) in &entries {
        if let Err(b) = v.feed(n, e) {
            fail(format!("at entry {}: {}", b.at().unwrap_or(n), b.what()));
        }
        // The first kept header against the seal printed for the segment
        // before it: the oldest entries kept follow what was printed.
        if n == first && n > 0 {
            if let Some(seal) = seals.get(&(n / SEGMENT_ENTRIES - 1)) {
                if *seal != e.detail {
                    fail(format!("at entry {n}: segment {}'s header doesn't link to the seal printed for segment {}", n / SEGMENT_ENTRIES, n / SEGMENT_ENTRIES - 1));
                }
                seals_checked += 1;
            }
        }
        // The end of a segment: the chain must be what was printed then.
        if (n + 1) % SEGMENT_ENTRIES == 0 {
            let segment = n / SEGMENT_ENTRIES;
            match seals.get(&segment) {
                Some(seal) if *seal == v.hash() => seals_checked += 1,
                Some(_) => fail(format!("at entry {n}: the chain's hash at the end of segment {segment} isn't the seal printed when it was sealed")),
                None => fail(format!("at entry {n}: segment {segment} ended, but no seal for it was printed")),
            }
        }
    }
    if let Err(b) = v.finish(&head) {
        fail(format!("at the head: {}", b.what()));
    }
    let segments = (first / SEGMENT_ENTRIES, last / SEGMENT_ENTRIES);
    println!(
        "audit_verify: entries {first} to {last} ({} in segments {} to {}) link from the first's header to the head {}...; {seals_checked} seals printed during the boot match the chain",
        entries.len(),
        segments.0,
        segments.1,
        std::str::from_utf8(&blake3::hex(&head)[..16]).unwrap_or("?"),
    );
}
