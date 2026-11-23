//! console.rs — the kernel's side of the console driver (Entry 20).
//!
//! The screen used to be the kernel's: it drew the log, scrolled it back,
//! and kept the status rows. Now a process does that (user/rust/src/bin/
//! console.rs), with nothing but capabilities:
//!
//!   slot 0  Console    to write to the log and leave notes for test.sh
//!   slot 1  Endpoint   its own, with RECV: scroll requests from the
//!                      keyboard driver, and the kernel's signals
//!   slot 2  Memory     the screen: the framebuffer, or VGA text memory
//!   slot 3  Memory     the system log (log.rs), read-only
//!   slot 4  Memory     the kernel's status rows (display.rs), read-only
//!
//! Its start block says how to draw: the mode, the cell grid, the colours
//! in the mode's pixel format, and (from offset 0x1000) the ROM font.
//!
//! The kernel tells it when to look again by signalling its endpoint:
//! SIGNAL_LOG when the log has grown, SIGNAL_PANEL when a status row has
//! changed. Printing happens everywhere, interrupt handlers included, so
//! notify() only notes the bits; kick() sends them, from the timer tick
//! (at most 10 ms later) and at the end of a write system call (at once).
//!
//! The kernel still draws the boot screen until the driver starts, and
//! takes the screen back (display::reclaim) to show a crash, a panic, a
//! restart or switch-off, and if the driver ends.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::cap::{table, Cap, RECV};
use crate::{display, ipc, log, process};

/// Signal bits (user/rust/src/bin/console.rs has the same).
pub const SIGNAL_LOG: u64 = 1 << 0;
pub const SIGNAL_PANEL: u64 = 1 << 1;

// Requests to the console driver are its business and the keyboard's
// (user/freya.inc: CONSOLE_SCROLL): the kernel only passes the calls on.

const NONE: u32 = u32::MAX;
static ENDPOINT: AtomicU32 = AtomicU32::new(NONE);
static TASK: AtomicUsize = AtomicUsize::new(usize::MAX);
static PENDING: AtomicU64 = AtomicU64::new(0);

/// The start block (user/rust/src/bin/console.rs: StartInfo, the same).
#[repr(C)]
struct StartInfo {
    magic: [u8; 8],
    version: u32,
    /// 1: draw glyphs on the framebuffer; 0: VGA text memory.
    graphics: u32,
    cols: u32,
    rows: u32,
    /// The first row of the log; the rows above are the kernel's.
    log_top: u32,
    /// Cells per row in the status-row memory (slot 4).
    panel_stride: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bytes_per_pixel: u32,
    colours: [u32; 16],
    log_data_bytes: u32,
    _reserved: u32,
}

const START_MAGIC: [u8; 8] = *b"FREYACON";
const FONT_OFFSET: usize = 0x1000;

/// Something for the console driver to look at: sent with the next kick().
pub fn notify(bits: u64) {
    if ENDPOINT.load(Ordering::Relaxed) != NONE {
        PENDING.fetch_or(bits, Ordering::Relaxed);
    }
}

/// Send what notify() gathered. Any context.
pub fn kick() {
    let ep = ENDPOINT.load(Ordering::Relaxed);
    if ep == NONE {
        return;
    }
    let bits = PENDING.swap(0, Ordering::Relaxed);
    if bits != 0 {
        ipc::signal(ep as u8, bits);
    }
}

/// Start the console driver. Returns its endpoint, whose creator reference
/// the caller still holds: make SEND capabilities for whoever may ask it to
/// scroll, then ipc::release_created. None if it couldn't start (the
/// kernel then keeps drawing).
pub fn start() -> Option<u8> {
    let screen = display::screen_memory()?;
    let ep = ipc::create()?;
    let caps = table(&[
        Cap::Console,
        Cap::Endpoint { ep, badge: 0, rights: RECV },
        screen,
        Cap::Memory { base: log::phys(), pages: log::LOG_PAGES, flags: 0 },
        display::panel_memory(),
    ]);

    let g = display::geometry();
    let info = StartInfo {
        magic: START_MAGIC,
        version: 1,
        graphics: g.graphics as u32,
        cols: g.cols,
        rows: g.rows,
        log_top: display::PANEL_ROWS as u32,
        panel_stride: display::MAX_COLS as u32,
        width: g.width,
        height: g.height,
        pitch: g.pitch,
        bytes_per_pixel: g.bytes_per_pixel,
        colours: g.colours,
        log_data_bytes: log::LOG_DATA_BYTES as u32,
        _reserved: 0,
    };
    let mut block = [0u8; process::START_BLOCK_BYTES];
    let size = core::mem::size_of::<StartInfo>();
    unsafe {
        core::ptr::copy_nonoverlapping(&info as *const StartInfo as *const u8, block.as_mut_ptr(), size);
    }
    block[FONT_OFFSET..FONT_OFFSET + display::font().len()].copy_from_slice(display::font());

    // The endpoint is live before the driver runs, so signals sent while it
    // starts wait there for its first recv.
    ENDPOINT.store(ep as u32, Ordering::Relaxed);
    match process::spawn_with(&process::CONSOLE, 0, caps, &block) {
        Ok(id) => {
            TASK.store(id, Ordering::Relaxed);
            display::hand_over();
            crate::serial_write_str(b"[console] the console driver (process ");
            crate::serial_write_u64(id as u64);
            crate::serial_write_str(b") draws the screen from here on\n");
            Some(ep)
        }
        Err(_) => {
            ENDPOINT.store(NONE, Ordering::Relaxed);
            ipc::release_created(ep);
            crate::serial_write_str(b"[console] couldn't start the console driver: the kernel keeps drawing\n");
            None
        }
    }
}

/// sched::exit_with: if the console driver has ended, the kernel takes the
/// screen back. (Interrupts are off.)
pub fn on_exit(id: usize) {
    if TASK.load(Ordering::Relaxed) != id {
        return;
    }
    TASK.store(usize::MAX, Ordering::Relaxed);
    ENDPOINT.store(NONE, Ordering::Relaxed);
    PENDING.store(0, Ordering::Relaxed);
    display::reclaim();
    crate::serial_write_str(b"[console] the console driver ended: the kernel draws the screen again\n");
}

/// The console driver's task, if it is running (for the demos).
pub fn task() -> Option<usize> {
    let id = TASK.load(Ordering::Relaxed);
    (id != usize::MAX).then_some(id)
}
