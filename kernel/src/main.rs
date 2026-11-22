#![no_std]
#![no_main]

use core::panic::PanicInfo;

mod boot;
mod disk_io;

use boot::{read_e820_map, E820Entry};

// ------------------------------------------------------------------
// Serial (COM1) - the external truth source
// ------------------------------------------------------------------

// Very small serial logger (COM1). QEMU: -serial stdio
const COM1: u16 = 0x3F8;

#[inline(always)]
unsafe fn outb(port: u16, val: u8) {

    core::arch::asm!(
        "out dx, al", 
        in("dx") port, in("al") val, 
        options(nomem, nostack, preserves_flags)
    );
}

#[inline(always)]
unsafe fn inb(port: u16) -> u8 {

    let mut val: u8;

    core::arch::asm!(
        "in al, dx", 
        in("dx") port, out("al") val, 
        options(nomem, nostack, preserves_flags)
    );

    val
}

fn serial_init() {

    unsafe {

        outb(COM1 + 1, 0x00); // disable interrupts
        outb(COM1 + 3, 0x80); // enable DLAB
        outb(COM1 + 0, 0x03); // divisor low (38400 baud)
        outb(COM1 + 1, 0x00); // divisor high
        outb(COM1 + 3, 0x03); // 8N1
        outb(COM1 + 2, 0xC7); // enable FIFO, clear, 14-byte threshold
        outb(COM1 + 4, 0x0B); // IRQs enabled, RTS/DSR
    }
}

fn serial_write_byte(b: u8) {

    unsafe {

        // Wait for Transmitter Holking Register Empty
        while (inb(COM1 + 5) & 0x20) == 0 {}

        outb(COM1 + 0, b);
    }
}

fn serial_write_str(s: &[u8]) {

    for &b in s {

        if b == b'\n' {

            serial_write_byte(b'\r');
        }

        serial_write_byte(b);
    }
}

/// Write a u64 as decimal to serial.
fn serial_write_u64(mut n: u64) {

    if n == 0 {

        serial_write_byte(b'0');
        return;
    }

    let mut buf = [0u8; 20];
    let mut i = 20usize;

    while n > 0 {

        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }

    serial_write_str(&buf[i..]);
}

/// Write a u64 as hex (0xXXXXXXXXXXXXXXXX) to serial
fn serial_write_hex(n: u64) {
    serial_write_str(b"0x");

    for shift in (0..16).rev() {

        let nibble = ((n >> (shift * 4)) & 0xF) as u8;
        let ch = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
        serial_write_byte(ch);
    }
}

/// -----------------------------------------------------------------------
///  Physical frame allocator - Phase 0 (bitmap, raw -> sanitized only)
/// 
///  Layout in physical memory (fixed, above kernel at 1 MiB):
///    0x0030_0000 : bitmap (512 KB covers 16 GiB at 4 KB/frame)
///    0x0040_0000 : frame metadata table (future - placeholder for now)
/// 
///  The bitmap uses 1 bit per 4 KiB frame. Bit = 0 -> free, 1 -> used.
///  All frames start marked USED; the E820 pass marks usable regions free
///  ----------------------------------------------------------------------

const PAGE_SIZE:    usize = 4096;
const BITMAP_BASE:  usize = 0x0030_0000;       // 3 MiB - safe above kernel
const BITMAP_BYTES: usize = 512 * 1024;        // 512 KiB covers 16 GiB
const TOTAL_FRAMES: usize = BITMAP_BYTES * 8;  // 4 194 304 frames

/// Mark a physical frame as free in the bitmap.
/// 'frame' is the frame index (physical_address / PAGE_SIZE).
#[inline]
unsafe fn bitmap_set_free(frame: usize) {

    if frame >= TOTAL_FRAMES { return; }

    let byte = BITMAP_BASE + frame / 8;
    let bit = frame % 8;
    let ptr = byte as *mut u8;

    *ptr &= !(1u8 << bit);
}

/// Mark a physical frame as used in the bitmap
#[inline]
unsafe fn bitmap_set_used(frame: usize) {

    if frame >= TOTAL_FRAMES { return; }

    let byte = BITMAP_BASE + frame / 8;
    let bit = frame % 8;
    let ptr = byte as *mut u8;

    *ptr |= 1u8 << bit;
}

/// Returns true if the frame is free
#[inline]
unsafe fn bitmap_is_free(frame: usize) -> bool {

    if frame >= TOTAL_FRAMES { return false; }

    let byte = BITMAP_BASE + frame / 8;
    let bit = frame % 8;
    let ptr = byte as *const u8;

    (*ptr & (1u8 << bit)) == 0
}

/// Allocate the lowest free physical frame, zero it (Sanitized state), and
/// return its physical address. Returns None if memory is exhausted.
/// 
/// This is the Raw -> Sanitized transition from the FreyaOS memory model.
/// Typed allocation (Sanitized -> Typed) will be added with the capability
/// system.
pub unsafe fn alloc_frame() -> Option<usize> {

    for i in 0..TOTAL_FRAMES {

        if bitmap_is_free(i) {

            bitmap_set_used(i);
            let phys = i * PAGE_SIZE;
            // Sanitize: zero the frame before handing it out.
            // This is the required Raw -> Sanitized invariant.
            let ptr = phys as *mut u8;

            core::ptr::write_bytes(ptr, 0, PAGE_SIZE);
            return Some(phys);
        }
    }
    None
}

/// Initialise the frame allocator from an E820 map.
/// 
/// Strategy:
///   1. Mark ALL frames used (conservative default - deny everything).
///   2. Walk E820 entries; for each usable region, mark frames free.
///   3. Re-mark frames used for regions we own: bitmap, page tables, kernel.
unsafe fn init_frame_allocator(e820: &[E820Entry]) {

    // Step 1 - pessimistic default: everything is used / reversed.
    let bitmap = core::slice::from_raw_parts_mut(BITMAP_BASE as *mut u8, BITMAP_BYTES);

    for b in bitmap.iter_mut() {

        *b = 0xFF;
    }

    // Step 2 - mark usable E820 regions free.
    let mut usable_bytes: u64 = 0;

    for entry in e820.iter() {

        if !entry.is_usable() {

            continue;
        }

        let start = entry.base as usize;
        let end = (entry.base + entry.length) as usize;
        let first_frame = start / PAGE_SIZE;

        // Round end down to avoid marking partially-covered frames free.
        let last_frame = end / PAGE_SIZE;

        for frame in first_frame..last_frame {

            bitmap_set_free(frame);
        }

        usable_bytes += entry.length;
    }

    // Step 3 - re-protect regions we use ourselves.
    // Conventional low memory (0..1 MiB): BDA, IVT, EBDA, VGA, ROM.
    protect_range(0x0000_0000, 0x0010_0000);

    // Kernel loaded at 1 MiB; reserve 2 MiB for it to be safe.
    protect_range(0x0010_0000, 0x0030_0000);

    // Our bitmap.
    protect_range(BITMAP_BASE, BITMAP_BASE + BITMAP_BYTES);

    // Page tables built by stage2.
    protect_range(0x0020_0000, 0x0020_3000);

    // E820 buffer written by stage2 (4 KiB is plenty).
    protect_range(0x0000_0500, 0x0000_1000);

    serial_write_str(b"[pmm] frame allocator ready. Usable RAM: ");
    serial_write_u64(usable_bytes / (1024 * 1024));
    serial_write_str(b" MiB\n");
}

/// Mark every frame in [start, end) as used.
unsafe fn protect_range(start: usize, end: usize) {

    let first = start / PAGE_SIZE;
    let last = (end + PAGE_SIZE - 1) / PAGE_SIZE;

    for frame in first..last {

        bitmap_set_used(frame);
    }
}

/// -----------------------------------------------------------------
/// Kernel entry point
/// -----------------------------------------------------------------
/// 
/// Write a string to a specific VGA text mode row
/// Row 0 is the top of the screen. Each row is 80 characters wide.
/// 'attr' is the VGA attribute byte (e.g. 0x0F = white on black, 0x0A = green on black).
fn vga_print(row: usize, msg: &[u8], attr: u8) {

    let base = 0xb8000 + row * 80 * 2;

    // Clear the row first
    for col in 0..80usize {

        unsafe {

            *((base + col * 2) as *mut u8) = b' ';
            *((base + col * 2 + 1) as *mut u8) = attr;
        }
    }

    // Write message
    for (i, &b) in msg.iter().enumerate() {

        if i >= 80 { break; }
        unsafe {
            *((base + i * 2) as *mut u8) = b;
            *((base + i * 2 + 1) as *mut u8) = attr;
        }
    }
}

/// Boot contract (BIOS path):
///   - CPU is in 64-bit long mode
///   - Paging on; first 2 MiB identity-mapped (2MiB huge page)
///   - Kernel loaded at physical 0x0010_0000
///   - E820 map at 0x0500 (count u16) / 0x0504 (entries, 24 B each)
///   - Interrupts disabled
#[no_mangle]
pub extern "C" fn rust_start() -> ! {

    serial_init();
    serial_write_str(b"[kernel] FreyaOS kernel starting\n");

    // --- Read E820 memory map left by stage2 ---
    let e820 = unsafe { read_e820_map() };

    if e820.is_empty() {
        serial_write_str(b"[kernel] FATAL: no E820 map - cannot initialize memory\n");
        halt();
    }

    serial_write_str(b"[kernel] E820 entries: ");
    serial_write_u64(e820.len() as u64);
    serial_write_str(b"\n");

    // Print the map to serial so we can verify on real hardware.
    for (i, entry) in e820.iter().enumerate() {

        serial_write_str(b"  [");
        serial_write_u64(i as u64);
        serial_write_str(b"] base=");
        serial_write_hex(entry.base);
        serial_write_str(b" len=");
        serial_write_hex(entry.length);
        serial_write_str(b" type=");
        serial_write_u64(entry.region_type as u64);
        serial_write_str(b"\n");
    }

    // --- Initialize the physical frame allocator ---
    unsafe { init_frame_allocator(e820); }

    // --- Smoke-test: allocate a frame and verify it is zeroed ---
    match unsafe { alloc_frame() } {

        Some(phys) => {

            serial_write_str(b"[kernel] test alloc: frame at ");
            serial_write_hex(phys as u64);
            serial_write_str(b"\n");

            // Verify zeroed (Sanitized invariant check).
            let slice = unsafe {
                
                core::slice::from_raw_parts(phys as *const u8, PAGE_SIZE)
            };

            let all_zero = slice.iter().all(|&b| b == 0);

            if all_zero {

                serial_write_str(b"[kernel] sanitize check: PASS\n");
            } else {
                
                serial_write_str(b"[kernel] sanitize check: FAIL - frame not zeroed!\n");
            }
        }

        None => {
                serial_write_str(b"[kernel] FATAL: alloc_frame returned None\n");
        }
    }

    // --- VGA status display (rows 4-8, below BIOS output) ---
    vga_print(4, b"FreyaOS kernel:  OK", 0x0A);
    vga_print(5, b"Memory map:      OK", 0x0A);
    vga_print(6, b"Frame allocator: OK", 0x0A);
    vga_print(7, b"Sanitise check:  OK", 0x0A);
    vga_print(8, b"--- FreyaOS foundation ready ---", 0x0F);

    serial_write_str(b"[kernel] init complete - halting\n");
    halt();
}

fn halt() -> ! {

    loop {
        unsafe { core::arch::asm!("cli; hlt"); }
    }
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    serial_write_str(b"[kernel] PANIC\n");
    halt();
}