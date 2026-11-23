//! console.rs — the kernel's side of the console driver (Entries 20, 21).
//!
//! The screen used to be the kernel's: it drew the log, scrolled it back,
//! and kept the status rows. Now a process does that (user/rust/src/bin/
//! console.rs), with nothing but capabilities. Since Entry 21 init starts
//! it, not the kernel; the kernel only prepares what it needs (prepare)
//! and lends all of it to init, which passes it on:
//!
//!   slot 0  Console    to write to the log and leave notes for test.sh
//!   slot 1  Endpoint   with RECV: scroll requests from the keyboard
//!                      driver, and the kernel's signals
//!   slot 2  Memory     the screen: the framebuffer, or VGA text memory
//!   slot 3  Memory     the system log (log.rs), read-only
//!   slot 4  Memory     the kernel's status rows (display.rs), read-only
//!   slot 5  Memory     the screen's description, read-only: the mode, the
//!                      cell grid, the colours in the mode's pixel format,
//!                      and (from offset 0x1000) the ROM font
//!
//! The kernel tells it when to look again by signalling its endpoint:
//! SIGNAL_LOG when the log has grown, SIGNAL_PANEL when a status row has
//! changed. Printing happens everywhere, interrupt handlers included, so
//! notify() only notes the bits; kick() sends them, from the timer tick
//! (at most 10 ms later) and at the end of a write system call (at once).
//! The kernel keeps a reference to the endpoint for good, so its number is
//! never given to another endpoint while the kernel signals it; signals
//! sent while no driver runs wait there for the next one.
//!
//! Who draws: the kernel draws the boot screen until a process maps the
//! screen (on_map, from mem.rs), and takes the screen back
//! (display::reclaim) when that process ends, or to show a crash, a panic,
//! a restart or switch-off. So the kernel needn't know which process init
//! chose, or how often init has started it again.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::cap::{Cap, GRANT, RECV, SEND};
use crate::{display, ipc, log, sched};

/// Signal bits (user/rust/src/bin/console.rs has the same).
pub const SIGNAL_LOG: u64 = 1 << 0;
pub const SIGNAL_PANEL: u64 = 1 << 1;

// Requests to the console driver are its business and the keyboard's
// (user/freya.inc: CONSOLE_SCROLL): the kernel only passes the calls on.

const NONE: u32 = u32::MAX;
static ENDPOINT: AtomicU32 = AtomicU32::new(NONE);
/// The process drawing the screen, if any.
static TASK: AtomicUsize = AtomicUsize::new(usize::MAX);
static PENDING: AtomicU64 = AtomicU64::new(0);
/// The physical address of the screen's memory (u64::MAX before prepare).
static SCREEN_BASE: AtomicU64 = AtomicU64::new(u64::MAX);

/// The screen's description (user/rust/src/bin/console.rs: StartInfo, the
/// same), at the start of DESCRIPTION.
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
const DESCRIPTION_BYTES: usize = 0x2000;

/// The description and the font: two pages of their own in .bss, so
/// lending them lends nothing else of the kernel's.
#[repr(C, align(4096))]
struct Description([u8; DESCRIPTION_BYTES]);
static mut DESCRIPTION: Description = Description([0; DESCRIPTION_BYTES]);

/// What prepare() made for the console driver, to give to init.
pub struct ConsoleCaps {
    /// The driver's endpoint, with every right (init mints RECV for the
    /// driver and SEND for whoever may ask it to scroll).
    pub endpoint: Cap,
    pub screen: Cap,
    pub log: Cap,
    pub panel: Cap,
    pub description: Cap,
}

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

/// Make what a console driver needs: its endpoint (a reference the kernel
/// keeps for good, and the creator's, which the caller lets go of with
/// ipc::release_created once it has made capabilities from it) and the
/// screen's description. None with no screen to lend.
pub fn prepare() -> Option<ConsoleCaps> {
    let screen = display::screen_memory()?;
    let Cap::Memory { base: screen_base, .. } = screen else {
        return None;
    };
    let ep = ipc::create()?;
    ipc::retain(ep);

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
    let block = unsafe { &mut (*(&raw mut DESCRIPTION)).0 };
    let size = core::mem::size_of::<StartInfo>();
    unsafe {
        core::ptr::copy_nonoverlapping(&info as *const StartInfo as *const u8, block.as_mut_ptr(), size);
    }
    block[FONT_OFFSET..FONT_OFFSET + display::font().len()].copy_from_slice(display::font());

    SCREEN_BASE.store(screen_base, Ordering::Relaxed);
    ENDPOINT.store(ep as u32, Ordering::Relaxed);
    Some(ConsoleCaps {
        endpoint: Cap::Endpoint { ep, badge: 0, rights: SEND | RECV | GRANT },
        screen,
        log: Cap::Memory { base: log::phys(), pages: log::LOG_PAGES, flags: 0 },
        panel: display::panel_memory(),
        // The kernel image is in the identity map: the address is physical.
        description: Cap::Memory { base: (&raw const DESCRIPTION) as u64, pages: (DESCRIPTION_BYTES / 4096) as u32, flags: 0 },
    })
}

/// mem.rs: the current process has just mapped memory starting at `base`.
/// If that is the screen, it draws from now on.
pub fn on_map(base: u64) {
    if base != SCREEN_BASE.load(Ordering::Relaxed) {
        return;
    }
    let me = sched::current();
    TASK.store(me, Ordering::Relaxed);
    display::hand_over();
    // One call, so the line is printed whole: this runs with interrupts on,
    // and another process's line could otherwise land inside it (Entry 17).
    let mut line = crate::idt::Line::new();
    line.str(b"[console] process ").dec(me as u64).str(b" ('").str(sched::name(me).as_bytes());
    line.str(b"') draws the screen from here on\n");
    crate::serial_write_str(line.as_bytes());
}

/// sched::exit_with: if the process drawing the screen has ended, the
/// kernel takes the screen back. (Interrupts are off, so the line below
/// is printed whole although it takes several calls.)
pub fn on_exit(id: usize) {
    if TASK.load(Ordering::Relaxed) != id {
        return;
    }
    TASK.store(usize::MAX, Ordering::Relaxed);
    display::reclaim();
    crate::serial_write_str(b"[console] the console driver (process ");
    crate::serial_write_u64(id as u64);
    crate::serial_write_str(b") ended: the kernel draws the screen again\n");
}

/// The process drawing the screen, if any (for the demos).
pub fn task() -> Option<usize> {
    let id = TASK.load(Ordering::Relaxed);
    (id != usize::MAX).then_some(id)
}
