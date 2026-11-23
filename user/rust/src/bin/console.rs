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
//! last HISTORY rows kept; and, on the last row, the line being typed
//! (Entry 23). The view follows new lines, or stays where
//! PageUp put it: while it is scrolled back, new lines move it one row
//! further back each, so what is on screen doesn't move, until those rows
//! are the oldest kept and fall out (then it shows the oldest left). A grey
//! bar across the log's top row says how far back it is.
//!
//! What it waits for, all on its endpoint:
//!   - signals from the kernel: LOG (the log grew), PANEL (a status row
//!     changed);
//!   - calls: SCROLL (from the PS/2 driver: PageUp, PageDown; from the
//!     shell: Enter), answered with how many rows back the view is;
//!     POINTER (from the mouse driver, Entry 22): movement, buttons and
//!     wheel; INPUT (from the shell, Entry 23): the line being typed.
//!
//! The shell's lines (Entry 23). The kernel starts every line a process
//! writes with "[user N 'name'] ", so no process can pass its words off as
//! another's; serial keeps that. On screen, lines from the process named
//! shell are shown without it, in bright white, so the shell reads as a
//! terminal. Names are the kernel's (its program table), so only the
//! shell's lines can lose their prefix.
//!
//! The line being typed is the shell's to edit: it sends the whole line
//! with each change (INPUT), and this draws it on the last row, with the
//! cursor after it, and leaves a note: "input row: ..." (test.sh reads
//! it). If this driver is started again, init tells the shell, which sends
//! the line again.
//!
//! The pointer (Entry 22) is a cell drawn with its colours swapped. It
//! appears in the middle of the screen at the mouse's first movement. It
//! moves over a field 640 x 400 counts in every mode, mapped onto the
//! screen's cells (Entry 23): text mode's 80 x 25 cells of 8 x 16, so the
//! mouse feels the same in text mode and in graphics at any resolution
//! (before, graphics moved a pixel a count: 1600 counts across the T430's
//! screen, where text mode needs 640). Holding the middle button turns
//! movement into scrolling, the way a ThinkPad's TrackPoint scrolls: up
//! goes back, a row per 16 counts. The wheel, if there is one, scrolls 3
//! rows a notch. After each change of cell or buttons it leaves a note:
//! "pointer at column C, row R; buttons: ..." (test.sh's mouse case reads
//! them).
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
const REQUEST_INPUT: u64 = freya::CONSOLE_INPUT;
const SCROLL_BACK: u64 = freya::SCROLL_BACK;
const SCROLL_FORWARD: u64 = freya::SCROLL_FORWARD;
const SCROLL_TO_END: u64 = freya::SCROLL_TO_END;

// The pointer.
const MIDDLE_BUTTON: u8 = 1 << 2;
/// The field the pointer moves over, in mouse counts, in every mode.
const FIELD_W: i64 = 640;
const FIELD_H: i64 = 400;
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
/// The shell's lines, and the line being typed: bright white.
const SHELL_ATTR: u8 = 0x0F;
const CURSOR_ATTR: u8 = 0x70;
/// Longest line being typed that is kept (the shell's are shorter).
const MAX_INPUT: usize = 128;

/// How the kernel starts a line the shell wrote: "[user ", its process
/// number, and this.
const SHELL_HEAD: &[u8] = b"[user ";
const SHELL_TAIL: &[u8] = b" 'shell'] ";
const SHELL_PREFIX_MAX: usize = 6 + 3 + 10;

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
    /// Each history row's colours: the shell's, or the log's.
    row_attr: [u8; HISTORY],
    /// The row being written, in ring order.
    newest: usize,
    /// Rows started since this driver began (the first counts as 1).
    total: usize,
    col: usize,
    /// Rows scrolled back from the newest; 0 = following.
    back: usize,
    /// Bytes of the system log read so far.
    read: u64,
    /// At the start of a line, looking for the shell's prefix: the bytes
    /// that match it so far, held back until it is clear whether they do.
    line_start: bool,
    pending: [u8; SHELL_PREFIX_MAX],
    pending_len: usize,

    /// The line being typed (INPUT), once the shell has sent one.
    typing: bool,
    input: [u8; MAX_INPUT],
    input_len: usize,

    /// The pointer: shown once the mouse first moves; where it is on the
    /// 640 x 400 field, and the cell that holds it.
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
    c.row_attr[0] = LOG_ATTR;
    c.line_start = true;
    if !map_all(&[(SCREEN, SCREEN_AT)]) {
        return 2;
    }

    // Everything the log holds so far, then the whole screen.
    read_log(c);
    draw_panel(c);
    draw_log(c);
    draw_input(c);
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
        REQUEST_INPUT => {
            input(c, msg.words[1], [msg.words[2], msg.words[3]]);
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
    let first = !c.pointer;
    if first {
        c.pointer = true;
        c.px = FIELD_W / 2;
        c.py = FIELD_H / 2;
    }
    let mut rows = 0;
    if buttons & MIDDLE_BUTTON != 0 {
        c.scroll_carry += dy;
        rows = c.scroll_carry / COUNTS_PER_ROW;
        c.scroll_carry -= rows * COUNTS_PER_ROW;
    } else {
        c.scroll_carry = 0;
        c.px = (c.px + dx).clamp(0, FIELD_W - 1);
        c.py = (c.py - dy).clamp(0, FIELD_H - 1);
    }
    rows -= wheel * ROWS_PER_NOTCH;
    let cell = (c.py as usize * c.rows / FIELD_H as usize, c.px as usize * c.cols / FIELD_W as usize);
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
        draw_input(c);
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

/// The log's rows: below the status rows, above the input row.
fn log_rows(c: &Console) -> usize {
    c.rows - c.log_top - 1
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
    if c.line_start {
        // Hold the line's first bytes back while they could be the shell's
        // prefix; drop them if they are, put them in if they aren't.
        c.pending[c.pending_len] = b;
        c.pending_len += 1;
        match shell_prefix(&c.pending[..c.pending_len]) {
            Some(false) => return,
            Some(true) => {
                c.line_start = false;
                c.pending_len = 0;
                c.row_attr[c.newest] = SHELL_ATTR;
                return;
            }
            None => {
                c.line_start = false;
                let held = c.pending;
                let n = c.pending_len;
                c.pending_len = 0;
                for &h in &held[..n] {
                    put_byte(c, h);
                }
                return;
            }
        }
    }
    put_byte(c, b);
}

/// Some(true): `p` is the whole of the shell's prefix; Some(false): it is
/// the start of it; None: it isn't.
fn shell_prefix(p: &[u8]) -> Option<bool> {
    let n = p.len().min(SHELL_HEAD.len());
    if p[..n] != SHELL_HEAD[..n] {
        return None;
    }
    let rest = &p[n..];
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    if rest.is_empty() || (digits == rest.len() && digits <= 3) {
        return Some(false); // more digits may follow
    }
    if digits == 0 || digits > 3 {
        return None;
    }
    let tail = &rest[digits..];
    let n = tail.len().min(SHELL_TAIL.len());
    if tail[..n] != SHELL_TAIL[..n] {
        return None;
    }
    Some(tail.len() == SHELL_TAIL.len())
}

fn put_byte(c: &mut Console, b: u8) {
    match b {
        b'\r' => {}
        b'\n' => {
            new_row(c, false);
            c.line_start = true;
        }
        _ => {
            if c.col >= c.cols {
                new_row(c, true); // wrap
            }
            let (row, col) = (c.newest, c.col);
            c.history[row][col] = if (0x20..0x7F).contains(&b) { b } else { b'.' };
            c.col += 1;
        }
    }
}

/// A new row: after a newline, or (`wrapped`) the rest of a long line,
/// which keeps its line's colours.
fn new_row(c: &mut Console, wrapped: bool) {
    let attr = if wrapped { c.row_attr[c.newest] } else { LOG_ATTR };
    c.newest = (c.newest + 1) % HISTORY;
    c.history[c.newest] = [b' '; MAX_COLS];
    c.row_attr[c.newest] = attr;
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

/// Where in the history the row shown on screen row `r` of the log is,
/// if there is one.
fn view_index(c: &Console, r: usize) -> Option<usize> {
    let age = log_rows(c) - 1 - r + c.back;
    (age < kept(c)).then(|| (c.newest + HISTORY - age) % HISTORY)
}

/// The history row shown on screen row `r` of the log, if there is one.
fn view_row(c: &Console, r: usize) -> Option<&[u8; MAX_COLS]> {
    view_index(c, r).map(|i| &c.history[i])
}

fn draw_log(c: &mut Console) {
    let rows = log_rows(c);
    let cols = c.cols;
    for r in 0..rows {
        let index = view_index(c, r);
        let attr = index.map_or(LOG_ATTR, |i| c.row_attr[i]);
        for col in 0..cols {
            let ch = index.map_or(b' ', |i| c.history[i][col]);
            put(c, c.log_top + r, col, ch, attr);
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

// ---------------------------------------------------------------------------
// The line being typed (Entry 23)
// ---------------------------------------------------------------------------

/// INPUT: `bytes` (16 of them) go at the offset in `place`'s bits 0 to
/// 15, in a line `place`'s bits 16 to 31 long. The shell sends a line from
/// its start, 16 bytes a call: the call with its last bytes leaves a note.
fn input(c: &mut Console, place: u64, bytes: [u64; 2]) {
    let offset = (place & 0xFFFF) as usize;
    let len = ((place >> 16) & 0xFFFF) as usize;
    c.typing = true;
    c.input_len = len.min(MAX_INPUT);
    for i in 0..freya::INPUT_CHUNK {
        let at = offset + i;
        if at < c.input_len {
            c.input[at] = (bytes[i / 8] >> (8 * (i % 8))) as u8;
        }
    }
    draw_input(c);
    if offset + freya::INPUT_CHUNK >= len {
        let mut line = Line::<160>::new();
        line.str(b"input row: ").str(&c.input[..c.input_len]).str(b"\n");
        freya::note(CONSOLE, line.as_bytes());
    }
}

/// The last row: the line being typed, and the cursor after it (the line's
/// end if it is wider than the screen); blank until there is a shell.
fn draw_input(c: &mut Console) {
    let row = c.rows - 1;
    let cols = c.cols;
    let start = (c.input_len + 1).saturating_sub(cols);
    for col in 0..cols {
        let at = start + col;
        let (ch, attr) = match at {
            _ if !c.typing => (b' ', LOG_ATTR),
            _ if at < c.input_len => (c.input[at], SHELL_ATTR),
            _ if at == c.input_len => (b' ', CURSOR_ATTR),
            _ => (b' ', LOG_ATTR),
        };
        let ch = if (0x20..0x7F).contains(&ch) { ch } else { b'.' };
        put(c, row, col, ch, attr);
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
