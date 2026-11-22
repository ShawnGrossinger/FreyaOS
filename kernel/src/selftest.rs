//! selftest.rs — one tally for every boot-time self-test.
//!
//! Each check prints its own description, then calls `verdict()`, which
//! finishes the line with ": PASS" or ": FAIL" and counts it. At the end of
//! boot `summary()` prints the totals, so one line (and one row on screen)
//! says whether everything is up and working. `make test` keys off that
//! line.

use core::sync::atomic::{AtomicU32, Ordering};

static PASSED: AtomicU32 = AtomicU32::new(0);
static FAILED: AtomicU32 = AtomicU32::new(0);

/// The first few failing self-test lines, repeated after the summary so
/// they end up on screen (the T430 has no serial port to read them from).
const KEEP: usize = 4;
static mut FAILED_LINES: [([u8; crate::LINE_MAX], usize); KEEP] = [([0; crate::LINE_MAX], 0); KEEP];

/// End the current serial line with the verdict and count it. Returns
/// `pass` so callers can fold it into their own result.
pub fn verdict(pass: bool) -> bool {
    if pass {
        PASSED.fetch_add(1, Ordering::Relaxed);
        crate::serial_write_str(b": PASS\n");
    } else {
        let n = FAILED.fetch_add(1, Ordering::Relaxed) as usize;
        if let Some(slot) = unsafe { (*(&raw mut FAILED_LINES)).get_mut(n) } {
            slot.1 = crate::current_line(&mut slot.0);
        }
        crate::serial_write_str(b": FAIL\n");
    }
    pass
}

/// Print "[module] self-test <what>: PASS|FAIL" in one call.
pub fn check(module: &[u8], what: &[u8], pass: bool) -> bool {
    crate::serial_write_str(b"[");
    crate::serial_write_str(module);
    crate::serial_write_str(b"] self-test ");
    crate::serial_write_str(what);
    verdict(pass)
}

/// (passed, failed) so far.
pub fn counts() -> (u32, u32) {
    (PASSED.load(Ordering::Relaxed), FAILED.load(Ordering::Relaxed))
}

/// "[kernel] self-tests: N passed, M failed". Returns true if none failed.
pub fn summary() -> bool {
    let (passed, failed) = counts();
    crate::serial_write_str(b"[kernel] self-tests: ");
    crate::serial_write_u64(passed as u64);
    crate::serial_write_str(b" passed, ");
    crate::serial_write_u64(failed as u64);
    crate::serial_write_str(b" failed\n");
    let lines = unsafe { &*(&raw const FAILED_LINES) };
    for (text, len) in lines.iter().take(failed as usize) {
        crate::serial_write_str(b"  FAILED: ");
        crate::serial_write_str(text.get(..*len).unwrap_or(text));
        crate::serial_write_str(b"\n");
    }
    failed == 0
}
