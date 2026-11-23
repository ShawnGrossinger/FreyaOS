//! display.rs — the screen, in text mode or graphics mode.
//!
//! Stage 2 either leaves the BIOS's 80x25 VGA text mode or sets a VESA
//! graphics mode with a linear framebuffer (Entry 18; see setup_display in
//! stage2.asm). Either way the rest of the kernel writes character cells:
//! put(row, col, character, attribute), with the VGA's 16-colour attributes.
//!
//! - Text mode: straight into VGA memory at 0xB8000, 80x25.
//! - Graphics mode: each cell is an 8x16 glyph from the VGA ROM's own font,
//!   which stage 2 copied out (to 0x1000) before leaving real mode, so no
//!   font ships with FreyaOS. The grid is width/8 by height/16: 128x48 at
//!   1024x768, up to MAX_COLS x MAX_ROWS.
//!
//! What should be on screen is kept in `cells`. In graphics mode `drawn`
//! records what the framebuffer actually shows, so redrawing (a scrolling
//! log, a ticking clock) skips cells that haven't changed: the framebuffer
//! is only ever written, never read back. It is mapped write-combining
//! (PAT entry 4), which lets the CPU send the pixels in bursts.
//!
//! Until attach() has mapped the framebuffer (just after the frame
//! allocator is up), graphics-mode output only goes into `cells`; attach()
//! then draws it all. Anything that goes wrong before that point shows on
//! serial only, so stage 2 stays in text mode if Shift is held while it
//! boots, or if the image was built with TEXT_MODE=1.
//!
//! Since Entry 20 the screen belongs to the console driver, a process,
//! from the moment it maps the screen (console.rs, Entry 21): hand_over()
//! stops the kernel drawing,
//! and the driver draws instead, from the system log (log.rs) and from the
//! kernel's status rows, which it reads in `cells` (PANEL_ROWS of them, lent
//! read-only). The kernel keeps `cells` up to date all the same, so that
//! reclaim() can take the screen back and draw it whole: on a crash, a
//! panic, a restart or switch-off, or if the console driver ends.

use crate::boot::{self, DisplayInfo};
use crate::cpu;

pub const MAX_COLS: usize = 240; // 1920 pixels
pub const MAX_ROWS: usize = 80; // 1280 pixels
const TEXT_COLS: usize = 80;
const TEXT_ROWS: usize = 25;
const GLYPH_W: usize = 8;
const GLYPH_H: usize = 16;
/// Rows 0 to 17: the kernel's status rows, which the console driver shows
/// as they are (main.rs fills them); the log is below.
pub const PANEL_ROWS: usize = 18;

/// The CGA/VGA palette the attribute bytes index (0-7 dim, 8-15 bright).
const PALETTE: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00), (0x00, 0x00, 0xAA), (0x00, 0xAA, 0x00), (0x00, 0xAA, 0xAA),
    (0xAA, 0x00, 0x00), (0xAA, 0x00, 0xAA), (0xAA, 0x55, 0x00), (0xAA, 0xAA, 0xAA),
    (0x55, 0x55, 0x55), (0x55, 0x55, 0xFF), (0x55, 0xFF, 0x55), (0x55, 0xFF, 0xFF),
    (0xFF, 0x55, 0x55), (0xFF, 0x55, 0xFF), (0xFF, 0xFF, 0x55), (0xFF, 0xFF, 0xFF),
];

#[derive(Clone, Copy)]
struct Framebuffer {
    va: u64,
    phys: u64,
    width: usize,
    height: usize,
    pitch: usize,
    bytes_per_pixel: usize,
    /// PALETTE packed into this mode's pixel format.
    colours: [u32; 16],
}

struct Screen {
    cols: usize,
    rows: usize,
    /// Stage 2 set a graphics mode (so VGA text memory isn't shown).
    graphics: bool,
    info: Option<DisplayInfo>,
    /// Some once the framebuffer is mapped.
    fb: Option<Framebuffer>,
    write_combining: bool,
    /// The console driver draws the screen; the kernel only keeps `cells`.
    console_draws: bool,
    drawn: [[u16; MAX_COLS]; MAX_ROWS],
}

static mut SCREEN: Screen = Screen {
    cols: TEXT_COLS,
    rows: TEXT_ROWS,
    graphics: false,
    info: None,
    fb: None,
    write_combining: false,
    console_draws: false,
    drawn: [[0; MAX_COLS]; MAX_ROWS],
};

/// What should be on screen, row by row (MAX_COLS cells a row, whatever the
/// mode). Page-aligned and a whole number of pages: its first pages, the
/// status rows, are lent to the console driver read-only, and lend nothing
/// else. Blank from init() on.
#[repr(C, align(4096))]
struct Cells([[u16; MAX_COLS]; MAX_ROWS]);

static mut CELLS: Cells = Cells([[0; MAX_COLS]; MAX_ROWS]);

fn cells() -> &'static mut [[u16; MAX_COLS]; MAX_ROWS] {
    unsafe { &mut (*(&raw mut CELLS)).0 }
}

/// The VGA ROM's 8x16 font, copied from where stage 2 left it.
static mut FONT: [u8; 256 * GLYPH_H] = [0; 256 * GLYPH_H];

fn screen() -> &'static mut Screen {
    unsafe { &mut *(&raw mut SCREEN) }
}

/// First thing in rust_start: learn the mode, take the font, blank the
/// screen. In text mode it is usable at once.
pub fn init() {
    let s = screen();
    s.info = boot::display_info();
    match s.info {
        Some(info) if info.status == 0 && info.font => {
            s.graphics = true;
            s.cols = (info.width as usize / GLYPH_W).min(MAX_COLS);
            s.rows = (info.height as usize / GLYPH_H).min(MAX_ROWS);
            unsafe {
                let font = &mut *(&raw mut FONT);
                for (i, byte) in font.iter_mut().enumerate() {
                    *byte = ((boot::FONT_ADDR + i) as *const u8).read_volatile();
                }
            }
        }
        _ => {
            s.graphics = false;
            s.cols = TEXT_COLS;
            s.rows = TEXT_ROWS;
            // CRTC register 0x0A (cursor start): bit 5 set = cursor off.
            unsafe {
                crate::outb(0x3D4, 0x0A);
                crate::outb(0x3D5, 0x20);
            }
        }
    }
    clear();
}

/// Physical [start, end) of the framebuffer stage 2 set up, or (0, 0) in
/// text mode.
pub fn framebuffer_range() -> (u64, u64) {
    let s = screen();
    match (s.graphics, s.info) {
        (true, Some(info)) => (info.framebuffer, info.framebuffer + info.pitch as u64 * info.height as u64),
        _ => (0, 0),
    }
}

pub fn cols() -> usize {
    screen().cols
}

pub fn rows() -> usize {
    screen().rows
}

/// One character cell. Out-of-range cells are ignored.
pub fn put(row: usize, col: usize, ch: u8, attr: u8) {
    cpu::without_interrupts(|| {
        let s = screen();
        if row >= s.rows || col >= s.cols {
            return;
        }
        let value = (attr as u16) << 8 | ch as u16;
        let cell = &mut cells()[row][col];
        if row < PANEL_ROWS && *cell != value {
            crate::console::notify(crate::console::SIGNAL_PANEL);
        }
        *cell = value;
        if s.console_draws {
            return;
        }
        if !s.graphics {
            let cell = (0xB8000 + (row * TEXT_COLS + col) * 2) as *mut u16;
            unsafe { cell.write_volatile(value) };
        } else if let Some(fb) = s.fb {
            if s.drawn[row][col] != value {
                unsafe { draw_cell(&fb, row, col, value) };
                s.drawn[row][col] = value;
            }
        }
    });
}

/// Every cell blank (grey on black).
pub fn clear() {
    let (rows, cols) = (rows(), cols());
    for row in 0..rows {
        for col in 0..cols {
            put(row, col, b' ', 0x07);
        }
    }
}

unsafe fn draw_cell(fb: &Framebuffer, row: usize, col: usize, value: u16) {
    let font = &*(&raw const FONT);
    let ch = (value & 0xFF) as usize;
    let attr = (value >> 8) as usize;
    let fg = fb.colours[attr & 0x0F];
    let bg = fb.colours[(attr >> 4) & 0x0F];
    let x0 = col * GLYPH_W;
    let y0 = row * GLYPH_H;
    for gy in 0..GLYPH_H {
        let bits = font[ch * GLYPH_H + gy];
        let line = (fb.va as *mut u8).add((y0 + gy) * fb.pitch + x0 * fb.bytes_per_pixel);
        for gx in 0..GLYPH_W {
            let colour = if bits & (0x80 >> gx) != 0 { fg } else { bg };
            pixel(fb, line.add(gx * fb.bytes_per_pixel), colour);
        }
    }
}

#[inline(always)]
unsafe fn pixel(fb: &Framebuffer, at: *mut u8, colour: u32) {
    if fb.bytes_per_pixel == 4 {
        (at as *mut u32).write_volatile(colour);
    } else {
        at.write_volatile(colour as u8);
        at.add(1).write_volatile((colour >> 8) as u8);
        at.add(2).write_volatile((colour >> 16) as u8);
    }
}

/// (r, g, b) in this mode's pixel format.
fn pack(info: &DisplayInfo, (r, g, b): (u8, u8, u8)) -> u32 {
    let field = |value: u8, (size, pos): (u8, u8)| -> u32 {
        let size = size.min(8) as u32;
        if size == 0 {
            0
        } else {
            ((value as u32) >> (8 - size)) << pos
        }
    };
    field(r, info.red) | field(g, info.green) | field(b, info.blue)
}

const IA32_PAT: u32 = 0x277;
/// PAT memory type 01h: write-combining.
const PAT_WC: u64 = 0x01;

/// After pmm::init: map the framebuffer (write-combining where the CPU has
/// PAT, uncached otherwise) and draw everything written so far. Nothing to
/// do in text mode.
///
/// # Safety
/// Once, after the frame allocator is up.
pub unsafe fn attach() -> Result<(), &'static [u8]> {
    let s = screen();
    let info = match (s.graphics, s.info) {
        (true, Some(info)) => info,
        _ => return Ok(()),
    };
    let bytes_per_pixel = (info.bpp / 8) as usize;
    if !(bytes_per_pixel == 3 || bytes_per_pixel == 4)
        || info.pitch < info.width * info.bpp / 8
        || info.framebuffer == 0
    {
        return Err(b"the mode stage 2 set isn't one this kernel can draw on");
    }

    // PAT entry 4 (selected by bit 7 of a 4 KiB page entry, which nothing
    // else sets) becomes write-combining. CPUID.1:EDX bit 16 = PAT.
    let (_, _, _, edx) = cpu::cpuid(1);
    let flags = if edx & (1 << 16) != 0 {
        let pat = cpu::rdmsr(IA32_PAT);
        cpu::wrmsr(IA32_PAT, (pat & !(0xFF << 32)) | PAT_WC << 32);
        s.write_combining = true;
        crate::vmm::PageFlags::FRAMEBUFFER
    } else {
        crate::vmm::PageFlags::MMIO
    };
    let bytes = info.pitch as u64 * info.height as u64;
    let va = crate::vmm::map_device(info.framebuffer, bytes, flags)
        .map_err(|_| &b"couldn't map the framebuffer"[..])?;

    let mut colours = [0u32; 16];
    for (i, &rgb) in PALETTE.iter().enumerate() {
        colours[i] = pack(&info, rgb);
    }
    let fb = Framebuffer {
        va,
        phys: info.framebuffer,
        width: info.width as usize,
        height: info.height as usize,
        pitch: info.pitch as usize,
        bytes_per_pixel,
        colours,
    };

    // Black first (the area right of and below the cell grid included),
    // then every cell.
    for y in 0..fb.height {
        let line = (fb.va as *mut u8).add(y * fb.pitch);
        for x in 0..fb.width {
            pixel(&fb, line.add(x * bytes_per_pixel), colours[0]);
        }
    }
    cpu::without_interrupts(|| {
        for row in 0..s.rows {
            for col in 0..s.cols {
                let value = cells()[row][col];
                draw_cell(&fb, row, col, value);
                s.drawn[row][col] = value;
            }
        }
        s.fb = Some(fb);
    });
    Ok(())
}

// ===========================================================================
// Handing the screen to the console driver, and taking it back (Entry 20)
// ===========================================================================

/// The console driver draws from now on.
pub fn hand_over() {
    cpu::without_interrupts(|| screen().console_draws = true);
}

/// The kernel draws again, starting with the whole screen: whatever the
/// console driver left there is overwritten with `cells`.
pub fn reclaim() {
    cpu::without_interrupts(|| {
        let s = screen();
        if !s.console_draws {
            return;
        }
        s.console_draws = false;
        for row in 0..s.rows {
            for col in 0..s.cols {
                let value = cells()[row][col];
                if !s.graphics {
                    let cell = (0xB8000 + (row * TEXT_COLS + col) * 2) as *mut u16;
                    unsafe { cell.write_volatile(value) };
                } else if let Some(fb) = s.fb {
                    unsafe { draw_cell(&fb, row, col, value) };
                    s.drawn[row][col] = value;
                }
            }
        }
    });
}

/// A Memory capability for the screen: the framebuffer (write-combining if
/// the kernel's own mapping is) or VGA text memory (uncached, one page).
/// None if graphics mode never got a framebuffer mapped.
pub fn screen_memory() -> Option<crate::cap::Cap> {
    use crate::cap::{Cap, MEM_UNCACHED, MEM_WRITE, MEM_WRITE_COMBINING};
    let s = screen();
    if !s.graphics {
        return Some(Cap::Memory { base: 0xB8000, pages: 1, flags: MEM_WRITE | MEM_UNCACHED });
    }
    let fb = s.fb?;
    let pages = ((fb.pitch * fb.height) as u64).div_ceil(4096) as u32;
    let caching = if s.write_combining { MEM_WRITE_COMBINING } else { MEM_UNCACHED };
    Some(Cap::Memory { base: fb.phys, pages, flags: MEM_WRITE | caching })
}

/// A read-only Memory capability for the status rows in `cells`.
pub fn panel_memory() -> crate::cap::Cap {
    let bytes = (PANEL_ROWS * MAX_COLS * 2) as u64;
    crate::cap::Cap::Memory { base: (&raw const CELLS) as u64, pages: bytes.div_ceil(4096) as u32, flags: 0 }
}

/// What the console driver needs to know to draw (console.rs puts it in
/// the screen's description).
pub struct Geometry {
    pub graphics: bool,
    pub cols: u32,
    pub rows: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bytes_per_pixel: u32,
    pub colours: [u32; 16],
}

pub fn geometry() -> Geometry {
    let s = screen();
    let fb = s.fb.filter(|_| s.graphics);
    Geometry {
        graphics: fb.is_some(),
        cols: s.cols as u32,
        rows: s.rows as u32,
        width: fb.map_or(0, |fb| fb.width as u32),
        height: fb.map_or(0, |fb| fb.height as u32),
        pitch: fb.map_or(0, |fb| fb.pitch as u32),
        bytes_per_pixel: fb.map_or(0, |fb| fb.bytes_per_pixel as u32),
        colours: fb.map_or([0; 16], |fb| fb.colours),
    }
}

/// The ROM font the kernel draws with (256 glyphs of 16 bytes).
pub fn font() -> &'static [u8; 256 * GLYPH_H] {
    unsafe { &*(&raw const FONT) }
}

/// For the boot log: what stage 2 chose, and why.
pub fn describe() {
    use crate::{serial_write_str as w, serial_write_u64 as dec};
    let s = screen();
    w(b"[display] ");
    match s.info {
        None => w(b"text mode, 80x25 (stage 2 left no display information)"),
        Some(info) => {
            if s.graphics {
                w(b"VESA graphics ");
                dec(info.width as u64);
                w(b"x");
                dec(info.height as u64);
                w(b", ");
                dec(info.bpp as u64);
                w(b"-bit, mode 0x");
                hex16(info.mode);
                w(b", framebuffer at 0x");
                hex32(info.framebuffer as u32);
                w(if s.write_combining { b" (write-combining)" } else { b" (uncached)" });
                w(b"; ");
                dec(s.cols as u64);
                w(b"x");
                dec(s.rows as u64);
                w(b" text cells in the VGA ROM font");
            } else {
                w(b"text mode, 80x25: ");
                w(match info.status {
                    1 => b"built with TEXT_MODE=1",
                    2 => b"Shift held while booting",
                    3 => b"no VBE 2.0, so no linear framebuffer",
                    4 => b"no usable mode in the card's list",
                    5 => b"the card refused the chosen mode",
                    6 => b"stage 2 couldn't copy the ROM font",
                    _ => b"stage 2 said nothing usable",
                });
            }
            // Only if stage 2 asked the card (not with TEXT_MODE=1 or Shift).
            if info.vbe_version != 0 {
                w(b"; ");
                dec(info.modes_listed as u64);
                w(b" modes listed (VBE ");
                dec((info.vbe_version >> 8) as u64);
                w(b".");
                dec(((info.vbe_version >> 4) & 0xF) as u64);
                w(b"), display's own resolution ");
                if info.native.0 == 0 {
                    w(b"unknown (no EDID: modes up to 1024x768)");
                } else {
                    dec(info.native.0 as u64);
                    w(b"x");
                    dec(info.native.1 as u64);
                    w(b" (EDID)");
                }
            }
        }
    }
    w(b"\n");
}

/// A one-line summary for the right side of the status area (graphics
/// mode only: in text mode there is no room). Returns its length in `out`.
pub fn status_note(out: &mut [u8; 64]) -> usize {
    let s = screen();
    let info = match (s.graphics, s.info) {
        (true, Some(info)) => info,
        _ => return 0,
    };
    let mut text = Text { out, len: 0 };
    text.str(b"Screen: ");
    text.num(info.width);
    text.str(b"x");
    text.num(info.height);
    text.str(b", ");
    text.num(info.bpp);
    text.str(b"-bit VESA");
    text.len
}

struct Text<'a> {
    out: &'a mut [u8; 64],
    len: usize,
}

impl Text<'_> {
    fn str(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.len < self.out.len() {
                self.out[self.len] = b;
                self.len += 1;
            }
        }
    }

    fn num(&mut self, mut v: u32) {
        let mut digits = [0u8; 10];
        let mut d = digits.len();
        loop {
            d -= 1;
            digits[d] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.str(&digits[d..]);
    }
}

/// The 16 colours as solid cells, a visible check that the pixel format is
/// read right (graphics mode; in text mode the same cells show the same).
pub fn draw_swatch(row: usize, col: usize) {
    for i in 0..16u8 {
        put(row, col + i as usize, 0xDB, i); // 0xDB: full block in the VGA font
    }
}

/// For vmm's MMIO self-test: (physical page, an existing view of it) to
/// alias in graphics mode; None in text mode (the VGA buffer then).
pub fn mmio_test_target() -> Option<(u64, u64)> {
    screen().fb.map(|fb| (fb.phys & !0xFFF, fb.va & !0xFFF))
}

/// Graphics mode: a glyph drawn into a cell reads back pixel for pixel
/// (font, pixel format, pitch and mapping all right), and the mapping is
/// the write-combining one. Prints its own line. Text mode: nothing to do.
pub fn selftest() -> bool {
    use crate::{serial_write_str as w, serial_write_u64 as dec};
    let s = screen();
    let fb = match (s.graphics, s.fb) {
        (false, _) => return selftest_text(),
        (true, Some(fb)) => fb,
        // attach() failed, and main.rs has counted that as a failed check.
        (true, None) => return true,
    };
    let (row, col) = (s.rows - 1, s.cols - 1);
    let saved = cells()[row][col];
    put(row, col, b'F', 0x1E); // yellow on blue
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };

    let font = unsafe { &*(&raw const FONT) };
    let mut wrong = 0;
    let mut lit = 0;
    for gy in 0..GLYPH_H {
        let bits = font[b'F' as usize * GLYPH_H + gy];
        for gx in 0..GLYPH_W {
            let at = fb.va as usize + (row * GLYPH_H + gy) * fb.pitch + (col * GLYPH_W + gx) * fb.bytes_per_pixel;
            let seen = unsafe {
                if fb.bytes_per_pixel == 4 {
                    (at as *const u32).read_volatile()
                } else {
                    let p = at as *const u8;
                    p.read_volatile() as u32 | (p.add(1).read_volatile() as u32) << 8 | (p.add(2).read_volatile() as u32) << 16
                }
            };
            let on = bits & (0x80 >> gx) != 0;
            lit += on as u32;
            let expect = fb.colours[if on { 0x0E } else { 0x01 }];
            let mask = if fb.bytes_per_pixel == 4 { 0xFFFF_FFFF } else { 0x00FF_FFFF };
            if seen & mask != expect & mask {
                wrong += 1;
            }
        }
    }
    put(row, col, (saved & 0xFF) as u8, (saved >> 8) as u8);

    let leaf = crate::vmm::kernel_leaf_entry(fb.va & !0xFFF).unwrap_or(0);
    let (pat, pcd, pwt) = crate::vmm::cache_bits(leaf);
    let wc_ok = if s.write_combining {
        let pat_msr = unsafe { cpu::rdmsr(IA32_PAT) };
        pat && !pcd && !pwt && (pat_msr >> 32) & 0x07 == PAT_WC
    } else {
        pcd
    };

    // No frame of the framebuffer may be the allocator's to hand out: the
    // screen would draw on someone's memory. Firmware must mark one in RAM
    // reserved in the E820 map (SeaBIOS does for QEMU's ramfb); this checks
    // it did. Outside RAM, is_frame_free is false anyway.
    let (fb_start, fb_end) = framebuffer_range();
    let mut handed_out = 0;
    let mut frame = fb_start & !0xFFF;
    while frame < fb_end {
        handed_out += crate::pmm::is_frame_free(frame) as u32;
        frame += 4096;
    }

    w(b"[display] self-test a glyph drawn in the last cell reads back pixel for pixel (");
    dec((GLYPH_W * GLYPH_H) as u64);
    w(b" pixels, ");
    dec(lit as u64);
    w(b" lit, ");
    dec(wrong as u64);
    w(b" wrong); framebuffer mapped ");
    w(if s.write_combining { b"write-combining (PAT entry 4)" } else { b"uncached (no PAT)" });
    if handed_out > 0 {
        w(b"; ");
        dec(handed_out as u64);
        w(b" of its frames free to allocate");
    }
    crate::selftest::verdict(wrong == 0 && lit > 0 && wc_ok && handed_out == 0)
}

/// Text mode: a character put in the last cell is in VGA memory where the
/// display adapter reads it (0xB8000 + 2 bytes a cell, row by row).
fn selftest_text() -> bool {
    use crate::serial_write_str as w;
    let s = screen();
    let (row, col) = (s.rows - 1, s.cols - 1);
    let saved = cells()[row][col];
    let at = 0xB8000 + (row * TEXT_COLS + col) * 2;
    put(row, col, b'F', 0x1E);
    let seen = unsafe { (at as *const u16).read_volatile() };
    put(row, col, (saved & 0xFF) as u8, (saved >> 8) as u8);
    let restored = unsafe { (at as *const u16).read_volatile() };
    w(b"[display] self-test a character put in the last cell of text mode is in VGA memory (0x");
    hex16(seen);
    w(b" at 0x");
    hex32(at as u32);
    w(b"), and the old one is back after");
    crate::selftest::verdict(seen == 0x1E00 | b'F' as u16 && restored == saved)
}

fn hex16(v: u16) {
    hex_digits(v as u32, 4);
}

fn hex32(v: u32) {
    hex_digits(v, 8);
}

fn hex_digits(v: u32, digits: u32) {
    for i in (0..digits).rev() {
        let d = ((v >> (i * 4)) & 0xF) as u8;
        crate::serial_write_byte(if d < 10 { b'0' + d } else { b'a' + d - 10 });
    }
}
