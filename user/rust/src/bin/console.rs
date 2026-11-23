//! console — the console driver: draws the screen, as a process (Entry 20).
//!
//! The kernel lends it, through capabilities that init passes on
//! (kernel/src/console.rs; init started it since Entry 21):
//!
//!   slot 0  Console    to write to the log, and leave notes for test.sh
//!   slot 1  Endpoint   its own (RECV): the kernel's signals, and calls
//!   slot 2  Memory     the screen: a framebuffer, or VGA text memory
//!   slot 3  Memory     the system log, read-only (kernel/src/log.rs)
//!   slot 4  Memory     the kernel's status rows, read-only (display.rs)
//!   slot 5  Memory     the screen's description, read-only: graphics or
//!                      text, the cell grid, the 16 colours in the mode's
//!                      own pixel format, and the ROM font
//!
//! The kernel stops drawing the moment it maps the screen, so it maps that
//! last, just before it draws.
//!
//! The screen is the kernel's status rows (0 to 17) as they are, then the
//! log: everything in the system log, wrapped at the screen's width, the
//! last HISTORY rows kept. The view follows new lines, or stays where
//! PageUp put it: while it is scrolled back, new lines move it one row
//! further back each, so what is on screen doesn't move, until those rows
//! are the oldest kept and fall out (then it shows the oldest left). A grey
//! bar across the log's top row says how far back it is.
//!
//! What it waits for, all on its endpoint:
//!   - signals from the kernel: LOG (the log grew), PANEL (a status row
//!     changed);
//!   - calls: SCROLL (from the PS/2 driver: PageUp, PageDown, Enter),
//!     answered with how many rows back the view is; POINTER (from the
//!     mouse driver, Entry 22): movement, buttons and wheel.
//!
//! The pointer (Entry 22) is a cell drawn with its colours swapped. It
//! appears in the middle of the screen at the mouse's first movement and
//! moves in the mouse's own units, one count to a pixel (a cell is 8 x
//! 16). Holding the middle button turns movement into scrolling, the way a
//! ThinkPad's TrackPoint scrolls: up goes back, a row per 16 counts. The
//! wheel, if there is one, scrolls 3 rows a notch. After each change of
//! cell or buttons it leaves a note: "pointer at column C, row R;
//! buttons: ..." (test.sh's mouse case reads them).
//!
//! Every cell drawn is remembered, so only cells that change are drawn
//! again; the framebuffer is only ever written. After each scroll it leaves
//! a note on serial (not in the log, which would move the view): how far
//! back, how many rows are kept, and the text of the view's top row, which
//! test.sh's scroll and flood cases check.

#![no_std]
#![no_main]

use freya::{Line, Message, MAP_BASE, NO_CAP, SIGNAL_BADGE};

freya::entry!(main);

// ---------------------------------------------------------------------------
// What the kernel says (kernel/src/console.rs)
// ---------------------------------------------------------------------------

const CONSOLE: u64 = 0;
const ENDPOINT: u64 = 1;
const SCREEN: u64 = 2;
const LOG: u64 = 3;
const PANEL: u64 = 4;
const DESCRIPTION: u64 = 5;

const SIGNAL_LOG: u64 = 1 << 0;
const SIGNAL_PANEL: u64 = 1 << 1;

const REQUEST_SCROLL: u64 = freya::CONSOLE_SCROLL;
const REQUEST_POINTER: u64 = freya::CONSOLE_POINTER;
const SCROLL_BACK: u64 = freya::SCROLL_BACK;
const SCROLL_FORWARD: u64 = freya::SCROLL_FORWARD;
const SCROLL_TO_END: u64 = freya::SCROLL_TO_END;

// The pointer.
const MIDDLE_BUTTON: u8 = 1 << 2;
/// Mouse counts (up or down) per row scrolled with the middle button held.
const COUNTS_PER_ROW: i64 = 16;
/// Rows a notch of the wheel scrolls.
const ROWS_PER_NOTCH: i64 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
struct StartInfo {
    magic: [u8; 8],
    version: u32,
    graphics: u32,
    cols: u32,
    rows: u32,
    log_top: u32,
    panel_stride: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bytes_per_pixel: u32,
    colours: [u32; 16],
    log_data_bytes: u32,
    _reserved: u32,
}

const FONT: *const u8 = (DESCRIPTION_AT + 0x1000) as *const u8;
const GLYPH_W: usize = 8;
const GLYPH_H: usize = 16;

// Where it maps what it was lent: 4 GiB apart, inside the map window.
const SCREEN_AT: u64 = MAP_BASE;
const LOG_AT: u64 = MAP_BASE + 0x1_0000_0000;
const PANEL_AT: u64 = MAP_BASE + 0x2_0000_0000;
const DESCRIPTION_AT: u64 = MAP_BASE + 0x3_0000_0000;
const LOG_DATA_OFFSET: u64 = 4096;

// ---------------------------------------------------------------------------
// State (all in .bss: zeroed, filled in by main)
// ---------------------------------------------------------------------------

const MAX_COLS: usize = 240;
const MAX_ROWS: usize = 80;
const TEXT_COLS: usize = 80;
/// Rows of log kept for scrolling back.
const HISTORY: usize = 400;
const LOG_ATTR: u8 = 0x07;
const BAR_ATTR: u8 = 0x70;

struct Console {
    info: StartInfo,
    cols: usize,
    rows: usize,
    log_top: usize,
    /// What each cell on screen shows (attribute << 8 | character), plus
    /// one: 0 means "not drawn yet" (every real cell has a non-zero
    /// attribute byte or character once +1 is added).
    drawn: [[u32; MAX_COLS]; MAX_ROWS],

    history: [[u8; MAX_COLS]; HISTORY],
    /// The row being written, in ring order.
    newest: usize,
    /// Rows started since this driver began (the first counts as 1).
    total: usize,
    col: usize,
    /// Rows scrolled back from the newest; 0 = following.
    back: usize,
    /// Bytes of the system log read so far.
    read: u64,

    /// The pointer: shown once the mouse first moves; where it is, in
    /// pixels (text mode: as if a cell were 8 x 16 pixels too), and the
    /// cell that holds it.
    pointer: bool,
    px: i64,
    py: i64,
    pointer_row: usize,
    pointer_col: usize,
    buttons: u8,
    /// Movement with the middle button held, not yet a whole row.
    scroll_carry: i64,
}

static mut STATE: Console = unsafe { core::mem::zeroed() };

fn state() -> &'static mut Console {
    unsafe { &mut *(&raw mut STATE) }
}

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

fn main(_arg: u64) -> u64 {
    let c = state();
    if !map_all(&[(DESCRIPTION, DESCRIPTION_AT), (LOG, LOG_AT), (PANEL, PANEL_AT)]) {
        return 2;
    }
    c.info = unsafe { (DESCRIPTION_AT as *const StartInfo).read() };
    let info = c.info;
    if &info.magic != b"FREYACON" || info.version != 1 {
        freya::write(CONSOLE, b"console: no usable screen description\n");
        return 1;
    }
    c.cols = (info.cols as usize).min(MAX_COLS);
    c.rows = (info.rows as usize).min(MAX_ROWS);
    c.log_top = (info.log_top as usize).min(c.rows.saturating_sub(2));
    c.total = 1;
    c.history[0] = [b' '; MAX_COLS];
    if !map_all(&[(SCREEN, SCREEN_AT)]) {
        return 2;
    }

    // Everything the log holds so far, then the whole screen.
    read_log(c);
    draw_panel(c);
    draw_log(c);
    let mut line = Line::<96>::new();
    line.str(b"started: ").dec(c.cols as u64).str(b"x").dec(c.rows as u64).str(b" cells, ");
    line.str(if info.graphics != 0 { b"graphics" } else { b"text mode" });
    line.str(b", ").dec(c.total.min(HISTORY) as u64).str(b" rows of log so far\n");
    freya::note(CONSOLE, line.as_bytes());

    loop {
        let msg = match freya::recv(ENDPOINT) {
            Ok(m) => m,
            Err(e) => return e as u64,
        };
        if msg.badge == SIGNAL_BADGE {
            handle_signals(c, msg.words[0]);
        } else {
            handle_call(c, &msg);
        }
    }
}

fn handle_signals(c: &mut Console, bits: u64) {
    if bits & SIGNAL_LOG != 0 {
        read_log(c);
        draw_log(c);
    }
    if bits & SIGNAL_PANEL != 0 {
        draw_panel(c);
    }
}

fn handle_call(c: &mut Console, msg: &Message) {
    match msg.words[0] {
        REQUEST_SCROLL if msg.words[1] <= SCROLL_TO_END => {
            // Show anything that came in first, so the view moves from
            // where the screen really is.
            read_log(c);
            scroll(c, msg.words[1]);
            draw_log(c);
            note_view(c);
            freya::reply([c.back as u64, 0, 0, 0], NO_CAP);
        }
        REQUEST_POINTER => {
            pointer(c, msg.words[1] as i64, msg.words[2] as i64, msg.words[3]);
            freya::reply([0; 4], NO_CAP);
        }
        _ => {
            freya::reply([freya::ERR_BAD_ARGUMENT as u64, 0, 0, 0], NO_CAP);
        }
    }
}

// ---------------------------------------------------------------------------
// The pointer (Entry 22)
// ---------------------------------------------------------------------------

/// The mouse moved by (dx, dy) (y up), with `word`'s buttons (bits 0 to 2)
/// and wheel (bits 8 to 15, signed; negative is up).
fn pointer(c: &mut Console, dx: i64, dy: i64, word: u64) {
    let buttons = (word & 7) as u8;
    let wheel = (word >> 8) as u8 as i8 as i64;
    let (width, height) = if c.info.graphics != 0 {
        (c.info.width as i64, c.info.height as i64)
    } else {
        ((c.cols * GLYPH_W) as i64, (c.rows * GLYPH_H) as i64)
    };
    let first = !c.pointer;
    if first {
        c.pointer = true;
        c.px = width / 2;
        c.py = height / 2;
    }
    let mut rows = 0;
    if buttons & MIDDLE_BUTTON != 0 {
        c.scroll_carry += dy;
        rows = c.scroll_carry / COUNTS_PER_ROW;
        c.scroll_carry -= rows * COUNTS_PER_ROW;
    } else {
        c.scroll_carry = 0;
        c.px = (c.px + dx).clamp(0, width - 1);
        c.py = (c.py - dy).clamp(0, height - 1);
    }
    rows -= wheel * ROWS_PER_NOTCH;
    let cell = ((c.py as usize) / GLYPH_H, (c.px as usize) / GLYPH_W);
    let moved = first || cell != (c.pointer_row, c.pointer_col);
    let pressed = buttons != c.buttons;
    (c.pointer_row, c.pointer_col) = cell;
    c.buttons = buttons;
    if rows != 0 {
        read_log(c);
        let back = (c.back as i64 + rows).clamp(0, max_back(c) as i64);
        c.back = back as usize;
    }
    if moved || rows != 0 {
        // Only the cells that change are drawn: the pointer's old and new
        // cells, or the view the scroll moved.
        draw_panel(c);
        draw_log(c);
    }
    if rows != 0 {
        note_view(c);
    }
    if moved || pressed {
        let mut line = Line::<96>::new();
        line.str(b"pointer at column ").dec(c.pointer_col as u64).str(b", row ").dec(c.pointer_row as u64);
        line.str(b"; buttons:");
        let names: [&[u8]; 3] = [b" left", b" right", b" middle"];
        for (i, name) in names.iter().enumerate() {
            if buttons & (1 << i) != 0 {
                line.str(name);
            }
        }
        if buttons == 0 {
            line.str(b" none");
        }
        line.str(b"\n");
        freya::note(CONSOLE, line.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// The log
// ---------------------------------------------------------------------------

fn log_rows(c: &Console) -> usize {
    c.rows - c.log_top
}

fn kept(c: &Console) -> usize {
    c.total.min(HISTORY)
}

/// How far back the view can go: rows kept, less one screenful.
fn max_back(c: &Console) -> usize {
    kept(c).saturating_sub(log_rows(c))
}

/// Everything new in the system log, into the history.
fn read_log(c: &mut Console) {
    let header = LOG_AT as *const u64;
    let data = (LOG_AT + LOG_DATA_OFFSET) as *const u8;
    let size = c.info.log_data_bytes as u64;
    let written = unsafe { core::ptr::read_volatile(header) };
    core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
    if written.wrapping_sub(c.read) > size {
        // Fallen behind by more than the ring holds: the oldest of it is
        // gone. Say so, and carry on from the oldest byte still there.
        c.read = written - size;
        for &b in b"\n-- part of the log was lost here --\n" {
            add_byte(c, b);
        }
    }
    while c.read < written {
        let b = unsafe { core::ptr::read_volatile(data.add((c.read % size) as usize)) };
        add_byte(c, b);
        c.read += 1;
    }
}

fn add_byte(c: &mut Console, b: u8) {
    match b {
        b'\r' => {}
        b'\n' => new_row(c),
        _ => {
            if c.col >= c.cols {
                new_row(c); // wrap
            }
            let (row, col) = (c.newest, c.col);
            c.history[row][col] = if (0x20..0x7F).contains(&b) { b } else { b'.' };
            c.col += 1;
        }
    }
}

fn new_row(c: &mut Console) {
    c.newest = (c.newest + 1) % HISTORY;
    c.history[c.newest] = [b' '; MAX_COLS];
    c.total += 1;
    c.col = 0;
    if c.back > 0 {
        // Scrolled back: keep showing the same rows, while they're kept.
        c.back = (c.back + 1).min(max_back(c));
    }
}

fn scroll(c: &mut Console, action: u64) {
    let page = log_rows(c).saturating_sub(1).max(1);
    c.back = match action {
        SCROLL_BACK => (c.back + page).min(max_back(c)),
        SCROLL_FORWARD => c.back.saturating_sub(page),
        _ => 0,
    };
}

/// The history row shown on screen row `r` of the log, if there is one.
fn view_row(c: &Console, r: usize) -> Option<&[u8; MAX_COLS]> {
    let age = log_rows(c) - 1 - r + c.back;
    (age < kept(c)).then(|| &c.history[(c.newest + HISTORY - age) % HISTORY])
}

fn draw_log(c: &mut Console) {
    let rows = log_rows(c);
    let cols = c.cols;
    for r in 0..rows {
        for col in 0..cols {
            let ch = view_row(c, r).map_or(b' ', |row| row[col]);
            put(c, c.log_top + r, col, ch, LOG_ATTR);
        }
    }
    if c.back > 0 {
        let mut bar = Line::<MAX_COLS>::new();
        bar.str(b" -- scrolled back ").dec(c.back as u64).str(b" rows: PageDown to go forward -- ");
        let text = bar.as_bytes();
        for col in 0..cols {
            put(c, c.log_top, col, text.get(col).copied().unwrap_or(b' '), BAR_ATTR);
        }
    }
}

/// For test.sh, on serial only: "view B rows back of K kept, top row: T,
/// bottom row: U", the view's first and last rows (the first is under the
/// bar when scrolled back; its text is still what the history holds).
fn note_view(c: &Console) {
    let mut line = Line::<256>::new();
    line.str(b"view ").dec(c.back as u64).str(b" rows back of ").dec(kept(c) as u64);
    line.str(b" kept, top row: ");
    row_text(c, &mut line, view_row(c, 0));
    line.str(b", bottom row: ");
    row_text(c, &mut line, view_row(c, log_rows(c) - 1));
    // The newest finished row, read straight from the ring rather than
    // through view_row: test.sh places the view's rows from it.
    line.str(b", newest row: ");
    let newest = (kept(c) > 1).then(|| &c.history[(c.newest + HISTORY - 1) % HISTORY]);
    row_text(c, &mut line, newest);
    line.str(b"\n");
    freya::note(CONSOLE, line.as_bytes());
}

/// Characters of a row a note shows: three of them, and the rest of the
/// note, fit the 256 bytes a note may be.
const NOTE_ROW_CHARS: usize = 56;

/// The start of a history row, trailing blanks left out.
fn row_text(c: &Console, line: &mut Line<256>, row: Option<&[u8; MAX_COLS]>) {
    if let Some(row) = row {
        let row = &row[..c.cols.min(NOTE_ROW_CHARS)];
        let end = row.iter().rposition(|&b| b != b' ').map_or(0, |i| i + 1);
        line.str(&row[..end]);
    }
}

/// Map each (slot, address); say which one failed, if one does.
fn map_all(list: &[(u64, u64)]) -> bool {
    for &(slot, at) in list {
        if freya::map(slot, at) != 0 {
            let mut line = Line::<64>::new();
            line.str(b"console: couldn't map capability ").dec(slot).str(b"\n");
            freya::write(CONSOLE, line.as_bytes());
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// The status rows
// ---------------------------------------------------------------------------

fn draw_panel(c: &mut Console) {
    let panel = PANEL_AT as *const u16;
    let stride = c.info.panel_stride as usize;
    for row in 0..c.log_top {
        for col in 0..c.cols {
            let value = unsafe { core::ptr::read_volatile(panel.add(row * stride + col)) };
            put(c, row, col, value as u8, (value >> 8) as u8);
        }
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn put(c: &mut Console, row: usize, col: usize, ch: u8, attr: u8) {
    if row >= c.rows || col >= c.cols {
        return;
    }
    // The pointer's cell: its colours swapped.
    let attr = if c.pointer && (row, col) == (c.pointer_row, c.pointer_col) { attr << 4 | attr >> 4 } else { attr };
    let value = (attr as u32) << 8 | ch as u32;
    if c.drawn[row][col] == value + 1 {
        return;
    }
    c.drawn[row][col] = value + 1;
    if c.info.graphics == 0 {
        let cell = (SCREEN_AT as *mut u16).wrapping_add(row * TEXT_COLS + col);
        unsafe { core::ptr::write_volatile(cell, value as u16) };
        return;
    }
    let info = &c.info;
    let fg = info.colours[(attr & 0x0F) as usize];
    let bg = info.colours[(attr >> 4) as usize];
    let bpp = info.bytes_per_pixel as usize;
    let pitch = info.pitch as usize;
    for gy in 0..GLYPH_H {
        let bits = unsafe { FONT.add(ch as usize * GLYPH_H + gy).read() };
        let line = SCREEN_AT as usize + (row * GLYPH_H + gy) * pitch + col * GLYPH_W * bpp;
        for gx in 0..GLYPH_W {
            let colour = if bits & (0x80 >> gx) != 0 { fg } else { bg };
            let at = line + gx * bpp;
            unsafe {
                if bpp == 4 {
                    core::ptr::write_volatile(at as *mut u32, colour);
                } else {
                    core::ptr::write_volatile(at as *mut u8, colour as u8);
                    core::ptr::write_volatile((at + 1) as *mut u8, (colour >> 8) as u8);
                    core::ptr::write_volatile((at + 2) as *mut u8, (colour >> 16) as u8);
                }
            }
        }
    }
}
