//! log.rs — the system log: everything the kernel and its processes print.
//!
//! Every byte that goes to serial (main.rs: serial_write_byte) also goes
//! here, into a 64 KiB ring. The kernel only ever appends; the console
//! driver (Entry 20) reads it through a read-only Memory capability and
//! keeps its own scrollback from it. So the log is complete from the first
//! line of boot, whether or not a console driver is running yet, and a
//! console driver that starts late, or again, loses nothing that is still
//! in the ring.
//!
//! Layout, as the console sees it (user/rust/src/bin/console.rs):
//!
//!   page 0     `written`: bytes appended since boot (u64), at offset 0
//!   pages 1-16 the ring: byte n is at data[n % LOG_DATA_BYTES]
//!
//! The kernel writes the byte, then `written` (release). A reader that
//! loads `written` (acquire) can read every byte below it; if it has fallen
//! more than LOG_DATA_BYTES behind, the oldest of those are overwritten and
//! it skips ahead.

use core::sync::atomic::{AtomicU64, Ordering};

pub const LOG_DATA_BYTES: usize = 64 * 1024;
const PAGE: usize = 4096;
/// Pages a Memory capability for the log covers: the header and the ring.
pub const LOG_PAGES: u32 = (1 + LOG_DATA_BYTES / PAGE) as u32;

#[repr(C, align(4096))]
struct Log {
    written: AtomicU64,
    _rest_of_header: [u8; PAGE - 8],
    data: [u8; LOG_DATA_BYTES],
}

const _: () = assert!(core::mem::size_of::<Log>() == LOG_PAGES as usize * PAGE);

// In .bss: page-aligned and a whole number of pages, so lending these
// pages to the console lends nothing else of the kernel's.
// The lock puts the log first, so its address is the log's (lock.rs).
static LOG: crate::lock::IrqLock<Log> =
    crate::lock::IrqLock::new(Log { written: AtomicU64::new(0), _rest_of_header: [0; PAGE - 8], data: [0; LOG_DATA_BYTES] });

/// Append one byte. The console driver reads the same pages through its
/// own mapping, in another program: `written` is atomic and stored last
/// (Release), so it never counts a byte not yet written. With the log's
/// lock held already (output from a fatal inside push itself), the byte
/// goes only to the other sinks.
pub fn push(b: u8) {
    if let Some(mut log) = LOG.try_lock() {
        let n = log.written.load(Ordering::Relaxed);
        log.data[n as usize % LOG_DATA_BYTES] = b;
        log.written.store(n + 1, Ordering::Release);
    }
}

/// The log's physical address. The kernel image sits in the identity map
/// (paging.rs), so a static's address is its physical address.
pub fn phys() -> u64 {
    LOG.addr()
}
