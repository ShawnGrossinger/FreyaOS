#![no_std]
#![no_main]

extern crate alloc; // Box, Vec, String, BTreeMap, backed by heap.rs

use core::panic::PanicInfo;

mod apic;
mod boot;
mod disk_io;
mod gdt;
mod heap;
mod idt;
mod paging;
mod pic;
mod pmm;
mod selftest;
mod vmm;

use boot::read_e820_map;

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

// Out of line on purpose: these are called from dozens of places, and
// letting the compiler inline the UART polling loop at each one multiplied
// the kernel's code size.
#[inline(never)]
fn serial_write_str(s: &[u8]) {

    for &b in s {

        if b == b'\n' {

            serial_write_byte(b'\r');
        }

        serial_write_byte(b);
    }
}

/// Write a u64 as decimal to serial.
#[inline(never)]
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
#[inline(never)]
fn serial_write_hex(n: u64) {
    serial_write_str(b"0x");

    for shift in (0..16).rev() {

        let nibble = ((n >> (shift * 4)) & 0xF) as u8;
        let ch = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
        serial_write_byte(ch);
    }
}


/// Blank the 80x25 text screen and hide the BIOS's blinking cursor, so the
/// kernel's status lines aren't interleaved with leftover BIOS output.
fn vga_clear() {
    for cell in 0..80 * 25usize {
        unsafe {
            *((0xb8000 + cell * 2) as *mut u8) = b' ';
            *((0xb8000 + cell * 2 + 1) as *mut u8) = 0x07; // grey on black
        }
    }

    // CRTC register 0x0A (cursor start): bit 5 set = cursor off.
    unsafe {
        outb(0x3D4, 0x0A);
        outb(0x3D5, 0x20);
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

    vga_clear();
    serial_init();
    serial_write_str(b"[kernel] FreyaOS kernel starting\n");

    // --- Segments: the kernel's own GDT + TSS (IST1 = double fault stack) ---
    // Must come before the IDT: the IDT copies CS into its gates, and the
    // #DF gate's IST index needs TR loaded.
    gdt::init();
    let (cs, tr) = gdt::current_selectors();
    serial_write_str(b"[gdt] loaded: cs=");
    serial_write_hex(cs as u64);
    serial_write_str(b" tr=");
    serial_write_hex(tr as u64);
    serial_write_str(b"\n");

    // --- Exceptions: install the IDT before anything else can fault ---
    let cs = idt::init();
    serial_write_str(b"[idt] loaded: 32 exception gates, code selector ");
    serial_write_hex(cs as u64);
    serial_write_str(b", #DF on IST1\n");

    // --- Paging: kernel-owned tables, guard page under the stack ---
    // After the IDT, so a mistake here is reported rather than silent.
    let cr3 = paging::init();
    serial_write_str(b"[paging] cr3=");
    serial_write_hex(cr3);
    serial_write_str(b" stack guard page at ");
    serial_write_hex(paging::STACK_GUARD_PAGE);
    serial_write_str(b"\n");

    let int3_ok = idt::selftest_breakpoint();
    let int3_ok = selftest::check(b"idt", b"int3 handler returns with registers intact", int3_ok);

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

    // --- Physical map: all RAM at PHYS_MAP_BASE + phys (higher half) ---
    // Its page tables come from a small run of frames right after the
    // kernel: the real allocator can't exist yet, because its bitmap is
    // reached through this map.
    let mut early = pmm::EarlyFrames::after_kernel(e820, paging::physmap_table_bound(e820));
    let pm = unsafe { paging::map_physical_memory(e820, &mut early) };
    serial_write_str(b"[paging] physical map at ");
    serial_write_hex(paging::PHYS_MAP_BASE);
    serial_write_str(b": ");
    serial_write_u64(pm.bytes_mapped / (1024 * 1024));
    serial_write_str(b" MiB in ");
    serial_write_u64(pm.huge_pages);
    serial_write_str(b" x 2 MiB pages, ");
    serial_write_u64(pm.table_frames);
    serial_write_str(b" table frames, up to ");
    serial_write_hex(pm.highest_addr);
    serial_write_str(if pm.nx { b", no-execute\n" } else { b", NX unsupported\n" });

    // --- Frame allocator: bitmap sized from E820, placed in RAM ---
    let (kernel_start, kernel_end) = pmm::kernel_image();
    let ps = unsafe { pmm::init(e820, early.used()) };
    serial_write_str(b"[pmm] bitmap: ");
    serial_write_u64(ps.bitmap_bytes / 1024);
    serial_write_str(b" KiB at ");
    serial_write_hex(ps.bitmap_phys);
    serial_write_str(b", tracking ");
    serial_write_u64(ps.frames);
    serial_write_str(b" frames (to ");
    serial_write_hex(ps.frames * pmm::PAGE_SIZE);
    serial_write_str(b")\n[pmm] reserved: low 1 MiB, kernel ");
    serial_write_hex(kernel_start);
    serial_write_str(b"-");
    serial_write_hex(kernel_end);
    serial_write_str(b", ");
    serial_write_u64((early.used().1 - early.used().0) / pmm::PAGE_SIZE);
    serial_write_str(b" early page-table frames, the bitmap\n[pmm] usable RAM ");
    serial_write_u64(ps.usable_bytes / (1024 * 1024));
    serial_write_str(b" MiB, free ");
    serial_write_u64(ps.free_frames * pmm::PAGE_SIZE / (1024 * 1024));
    serial_write_str(b" MiB\n");

    let pmm_ok = unsafe { pmm::selftest(e820) };
    let physmap_ok = unsafe { paging::selftest(e820) };

    // --- Page mapping API: map/unmap/translate 4 KiB pages ---
    let vmm_ok = unsafe { vmm::selftest() };

    // --- Kernel heap: Box, Vec, String, BTreeMap ---
    let heap_ok = heap::selftest();

    // --- Smoke-test: allocate a frame and verify it is zeroed ---
    let sanitize_ok = match unsafe { pmm::alloc_frame() } {

        Some(phys) => {

            serial_write_str(b"[kernel] test alloc: frame at ");
            serial_write_hex(phys as u64);
            serial_write_str(b"\n");

            // Verify zeroed (Sanitized invariant check).
            let slice = unsafe {

                core::slice::from_raw_parts(paging::phys_to_virt(phys as u64), pmm::PAGE_SIZE as usize)
            };

            let all_zero = slice.iter().all(|&b| b == 0);
            unsafe { pmm::free_frame(phys) };

            selftest::check(b"kernel", b"a freshly allocated frame is all zeros", all_zero)
        }

        None => selftest::check(b"kernel", b"alloc_frame returned a frame", false),
    };

    // --- Interrupts: legacy PICs out of the way, local APIC timer on ---
    // Last, so every allocation and mapping above happened with interrupts
    // off; apic::selftest() turns them on and leaves them on.
    pic::remap_and_mask();
    let irq_ok = match apic::init() {
        Ok(info) => {
            serial_write_str(b"[apic] local APIC id ");
            serial_write_u64(info.id as u64);
            serial_write_str(b", version ");
            serial_write_hex(info.version as u64);
            serial_write_str(b", registers at ");
            serial_write_hex(info.phys_base);
            serial_write_str(b"; timer at ");
            serial_write_u64(apic::TIMER_HZ);
            serial_write_str(b" Hz on vector ");
            serial_write_hex(apic::TIMER_VECTOR as u64);
            serial_write_str(b"\n");
            apic::selftest(&info)
        }
        Err(why) => selftest::check(b"apic", why, false),
    };

    // --- VGA status display (screen was cleared at entry) ---
    // Steps with no result value either succeed or end in an exception
    // report, so reaching this point means they passed.
    vga_status(0, b"FreyaOS kernel:  ", true);
    vga_status(1, b"Segments + TSS:  ", true);
    vga_status(2, b"Exception table: ", int3_ok);
    vga_status(3, b"Page tables:     ", true);
    vga_status(4, b"Memory map:      ", true);
    vga_status(5, b"Frame allocator: ", pmm_ok);
    vga_status(6, b"Physical map:    ", physmap_ok);
    vga_status(7, b"Page mapping:    ", vmm_ok);
    vga_status(8, b"Kernel heap:     ", heap_ok);
    vga_status(9, b"Sanitise check:  ", sanitize_ok);
    vga_status(10, b"Timer interrupt: ", irq_ok);

    // --- One line that says whether everything is up ---
    let all_ok = selftest::summary();
    let (passed, failed) = selftest::counts();
    vga_summary(11, passed, failed);

    // Build-time fault demo, if one was requested (halts).
    run_requested_demo();

    if all_ok {
        vga_print(12, b"--- FreyaOS foundation ready ---", 0x0F);
        serial_write_str(b"[kernel] init complete - idle, timer running\n");
    } else {
        vga_print(12, b"--- SELF-TEST FAILURE: see serial log ---", 0x0C);
        serial_write_str(b"[kernel] init complete WITH FAILURES - idle\n");
    }
    idle(13);
}

/// "Self-tests:      N passed, M failed", green if none failed.
fn vga_summary(row: usize, passed: u32, failed: u32) {
    let mut line = VgaLine::new();
    line.put(b"Self-tests:      ")
        .num(passed as u64)
        .put(b" passed, ")
        .num(failed as u64)
        .put(b" failed");
    vga_print(row, line.as_bytes(), if failed == 0 { 0x0A } else { 0x0C });
}

/// Build an 80-column VGA line from text and decimal numbers.
struct VgaLine {
    buf: [u8; 80],
    len: usize,
}

impl VgaLine {
    fn new() -> Self {
        VgaLine { buf: [b' '; 80], len: 0 }
    }

    fn put(&mut self, bytes: &[u8]) -> &mut Self {
        for &b in bytes {
            if self.len < self.buf.len() {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
        self
    }

    fn num(&mut self, mut n: u64) -> &mut Self {
        let mut digits = [0u8; 20];
        let mut count = 0;
        for slot in digits.iter_mut() {
            *slot = b'0' + (n % 10) as u8;
            count += 1;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        for &d in digits.iter().take(count).rev() {
            self.put(&[d]);
        }
        self
    }

    fn as_bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or(&self.buf)
    }
}

/// After boot: sleep until the next interrupt, forever, keeping the uptime
/// line on screen current. `sti; hlt` as a pair cannot miss a wakeup: an
/// interrupt is only taken after the instruction following `sti`, which is
/// the `hlt` itself.
fn idle(row: usize) -> ! {
    let mut shown = u64::MAX;
    loop {
        let ticks = apic::ticks();
        let seconds = ticks / apic::TIMER_HZ;
        if seconds != shown {
            shown = seconds;
            let mut line = VgaLine::new();
            line.put(b"Uptime:          ").num(seconds).put(b" s  (").num(ticks).put(b" timer ticks)");
            vga_print(row, line.as_bytes(), 0x07);
        }
        unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
    }
}

/// Fault demos, chosen at build time:  FREYA_DEMO=<name> make run
/// `make test` builds each one and checks the report it produces.
fn run_requested_demo() {
    let demo = match option_env!("FREYA_DEMO") {
        Some(d) if !d.is_empty() => d,
        _ => return,
    };
    serial_write_str(b"[demo] FREYA_DEMO=");
    serial_write_str(demo.as_bytes());
    serial_write_str(b"\n");

    unsafe {
        match demo {
            // #UD, vector 6
            "ud2" => core::arch::asm!("ud2"),
            // #PF, not present: above everything that is mapped
            "page_fault" => {
                core::ptr::read_volatile(0x1000_0000 as *const u8);
            }
            // #DF on IST1 after the stack hits its guard page
            "stack_overflow" => {
                demo_stack_overflow(0);
            }
            // #PF, instruction fetch: kernel code through its no-execute
            // physical-map alias
            "nx" => {
                let f: extern "C" fn() = core::mem::transmute(paging::phys_to_virt(0x10_0000));
                f();
            }
            // #PF, not present: used, unmapped, used again
            "use_after_unmap" => vmm::demo_use_after_unmap(),
            // #PF, protection violation on a write
            "write_read_only" => vmm::demo_write_read_only(),
            // [heap] FATAL: double free
            "heap_double_free" => heap::demo_double_free(),
            // [kernel] PANIC with file and line
            "panic" => panic!("FREYA_DEMO=panic"),
            _ => {
                serial_write_str(b"[demo] unknown; known: ud2 page_fault stack_overflow nx \
                                   use_after_unmap write_read_only heap_double_free panic\n");
                return;
            }
        }
    }
    serial_write_str(b"[demo] returned without faulting\n");
}

/// One status row: label followed by OK (green) or FAIL (red).
fn vga_status(row: usize, label: &[u8], ok: bool) {
    let (word, attr): (&[u8], u8) = if ok { (b"OK", 0x0A) } else { (b"FAIL", 0x0C) };

    let mut line = [b' '; 80];
    let mut len = 0;
    for &b in label.iter().chain(word.iter()) {
        if len < line.len() {
            line[len] = b;
            len += 1;
        }
    }
    vga_print(row, &line[..len.min(line.len())], attr);
}

/// Demo only: recurse until the kernel stack runs into its guard page.
/// Each frame keeps a 512-byte buffer alive so the 64 KiB stack is used up
/// in ~120 calls. black_box stops the compiler from turning the recursion
/// into a loop or deleting the buffer.
#[inline(never)]
fn demo_stack_overflow(depth: u64) -> u64 {
    let buf = [depth; 64];
    core::hint::black_box(&buf);
    if core::hint::black_box(true) {
        demo_stack_overflow(depth + 1) + buf[0]
    } else {
        0
    }
}

fn halt() -> ! {

    loop {
        unsafe { core::arch::asm!("cli; hlt"); }
    }
}

#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // File, line and (for messages without {} arguments) the text. Printed
    // without core::fmt, which would add ~10 KiB to the kernel.
    serial_write_str(b"\n[kernel] PANIC");
    if let Some(location) = info.location() {
        serial_write_str(b" at ");
        serial_write_str(location.file().as_bytes());
        serial_write_str(b":");
        serial_write_u64(location.line() as u64);
    }
    if let Some(message) = info.message().as_str() {
        serial_write_str(b": ");
        serial_write_str(message.as_bytes());
    }
    serial_write_str(b"\n");
    vga_print(20, b"!! KERNEL PANIC - file and line on COM1", 0x4F);
    halt();
}