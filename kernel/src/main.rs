#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc; // Box, Vec, String, BTreeMap, backed by heap.rs

use core::panic::PanicInfo;

mod apic;
mod acpi;
mod audit;
mod audit_chain;
mod blake3;
mod sha1;
mod tpm;
mod boot;
mod cap;
mod console;
mod cpu;
mod disk_io;
mod display;
mod factory;
mod gdt;
mod heap;
mod idt;
mod info;
mod ipc;
mod irq;
mod lock;
mod log;
mod mem;
mod paging;
mod pic;
mod pmm;
mod power;
mod process;
mod sched;
mod selftest;
mod sync;
mod syscall;
mod vmm;

use boot::read_e820_map;

// ------------------------------------------------------------------
// Serial (COM1) - the external truth source
// ------------------------------------------------------------------

// Very small serial logger (COM1). QEMU: -serial stdio
const COM1: u16 = 0x3F8;

/// Write a byte to an I/O port.
///
/// # Safety
/// The port must belong to a device the caller drives (here, COM1), and the
/// byte must be one that device expects: a port write can reset the machine
/// or reprogram any device.
#[inline(always)]
unsafe fn outb(port: u16, val: u8) {
    // SAFETY: [port] the caller chose the port and the value (# Safety).
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port, in("al") val,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Read a byte from an I/O port.
///
/// # Safety
/// The port must belong to a device the caller drives: some devices act on
/// a read (a status register cleared by reading it, say).
#[inline(always)]
unsafe fn inb(port: u16) -> u8 {
    let mut val: u8;
    // SAFETY: [port] the caller chose the port (# Safety).
    unsafe {
        core::arch::asm!(
            "in al, dx",
            in("dx") port, out("al") val,
            options(nomem, nostack, preserves_flags)
        );
    }
    val
}

/// Set by serial_init if a UART answers at COM1. The T430 has none; on such
/// a machine every serial write is skipped (the screen log still gets it).
static SERIAL_PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn serial_init() {
    // SAFETY: [port] COM1's eight ports (0x3F8-0x3FF) belong to the kernel's
    // serial log and nothing else; the PC's I/O map reserves them for it. On
    // a machine with no UART there (the T430) the probe's writes go nowhere
    // and its reads return 0xFF, so nothing else is written.
    unsafe {
        // A UART's scratch register (base + 7) keeps what is written to
        // it; an empty port reads back 0xFF or 0x00.
        outb(COM1 + 7, 0xA5);
        let first = inb(COM1 + 7) == 0xA5;
        outb(COM1 + 7, 0x5A);
        let present = first && inb(COM1 + 7) == 0x5A;
        SERIAL_PRESENT.store(present, core::sync::atomic::Ordering::Relaxed);
        if !present {
            return;
        }

        outb(COM1 + 1, 0x00); // disable interrupts
        outb(COM1 + 3, 0x80); // enable DLAB
        outb(COM1 + 0, 0x03); // divisor low (38400 baud)
        outb(COM1 + 1, 0x00); // divisor high
        outb(COM1 + 3, 0x03); // 8N1
        outb(COM1 + 2, 0xC7); // enable FIFO, clear, 14-byte threshold
        outb(COM1 + 4, 0x0B); // IRQs enabled, RTS/DSR
    }
}

/// Every byte of output goes to the system log (log.rs, which the console
/// driver shows), to the kernel's own screen log, and, if there is one, to
/// COM1. With interrupts off: log::push needs them off (Entry 30: its new
/// check found serial_write_hex calling here with them on).
fn serial_write_byte(b: u8) {
    cpu::without_interrupts(|| {
        if b != b'\r' {
            log::push(b);
            console::notify(console::SIGNAL_LOG);
        }
        screen_log_byte(b);
        uart_byte(b);
    })
}

/// One byte to the UART, if there is one; nothing else.
fn uart_byte(b: u8) {
    if !SERIAL_PRESENT.load(core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // SAFETY: [port] COM1, found by serial_init: its line status register
    // (base + 5) and its transmit register (base + 0).
    unsafe {
        // Wait for the transmitter holding register to empty, but not
        // forever: a UART that stops answering mustn't hang the kernel.
        let mut patience = 100_000u32;
        while (inb(COM1 + 5) & 0x20) == 0 && patience > 0 {
            patience -= 1;
        }
        outb(COM1 + 0, b);
    }
}

// ------------------------------------------------------------------
// Screen log: the last lines of output, at the bottom of the screen
// ------------------------------------------------------------------
//
// Real hardware like the T430 has no serial port, so everything printed
// is also shown from row 18 to the bottom of the screen: 7 rows in text
// mode, 32 at 1280x800. Long lines wrap at the screen's width.
//
// Since Entry 20 this is the kernel's own copy, for the boot screen and
// for when it takes the screen back (a crash, a panic, power, the console
// driver ending): every byte also goes to the system log (log.rs), from
// which the console driver keeps 400 rows of scrollback and draws the
// screen the rest of the time. The kernel's copy only needs a screenful.
// The line being printed is also kept whole, so a failing self-test can
// be repeated after the summary, where it stays on screen.

const LOG_TOP: usize = display::PANEL_ROWS;
const LOG_ATTR: u8 = 0x07;
const LINE_MAX: usize = 160;
const HISTORY: usize = display::MAX_ROWS;
/// Rows and width of the crash screen's "before it happened" block.
const SNAPSHOT_ROWS: usize = 7;
const SNAPSHOT_COLS: usize = 160;
type Snapshot = [[u8; SNAPSHOT_COLS]; SNAPSHOT_ROWS];

struct ScreenLog {
    history: [[u8; display::MAX_COLS]; HISTORY],
    /// Index of the row being written; rows before it in ring order.
    newest: usize,
    /// Rows started since boot (the first row counts as 1).
    total: usize,
    col: usize,
    line: [u8; LINE_MAX],
    line_len: usize,
}

static SCREEN_LOG: lock::IrqLock<ScreenLog> = lock::IrqLock::new(ScreenLog {
    history: [[b' '; display::MAX_COLS]; HISTORY],
    newest: 0,
    total: 1,
    col: 0,
    line: [0; LINE_MAX],
    line_len: 0,
});

fn log_rows() -> usize {
    display::rows() - LOG_TOP
}

fn screen_log_byte(b: u8) {
    // try_lock: output on a fatal path, from inside the screen log itself,
    // goes to the other sinks rather than halting again (Entry 31).
    if let Some(mut guard) = SCREEN_LOG.try_lock() {
        let log = &mut *guard;
        match b {
            b'\r' => {}
            b'\n' => {
                screen_log_new_row(log);
                log.line_len = 0;
            }
            _ => {
                if log.col >= display::cols() {
                    screen_log_new_row(log); // wrap
                }
                log.history[log.newest][log.col] = b;
                display::put(LOG_TOP + log_rows() - 1, log.col, b, LOG_ATTR);
                log.col += 1;
                if let Some(slot) = log.line.get_mut(log.line_len) {
                    *slot = b;
                    log.line_len += 1;
                }
            }
        }
    }
}

fn screen_log_new_row(log: &mut ScreenLog) {
    log.newest = (log.newest + 1) % HISTORY;
    log.history[log.newest] = [b' '; display::MAX_COLS];
    log.total += 1;
    log.col = 0;
    screen_log_draw(log);
}

/// The log area from history. display::put skips cells that already show
/// the right thing, so this is cheap when little has changed.
fn screen_log_draw(log: &ScreenLog) {
    let rows = log_rows();
    let cols = display::cols();
    let kept = log.total.min(HISTORY);
    for r in 0..rows {
        // Screen row r shows the row (rows - 1 - r) before the newest.
        let age = rows - 1 - r;
        let row = (age < kept).then(|| &log.history[(log.newest + HISTORY - age) % HISTORY]);
        for c in 0..cols {
            let ch = row.map_or(b' ', |row| row[c]);
            display::put(LOG_TOP + r, c, ch, LOG_ATTR);
        }
    }
}

/// The last rows of the log as they are now. A crash report takes this
/// before it prints anything, so the lines that led up to the crash aren't
/// scrolled off by the report itself.
fn screen_log_snapshot() -> Snapshot {
    let mut out = [[b' '; SNAPSHOT_COLS]; SNAPSHOT_ROWS];
    // A crash in the middle of printing leaves the lock held: then the
    // crash screen goes without the lines before it, rather than halting.
    let Some(log) = SCREEN_LOG.try_lock() else { return out };
    let kept = log.total.min(HISTORY);
    for (r, row) in out.iter_mut().enumerate() {
        let age = SNAPSHOT_ROWS - 1 - r;
        if age < kept {
            let src = &log.history[(log.newest + HISTORY - age) % HISTORY];
            row.copy_from_slice(&src[..SNAPSHOT_COLS]);
        }
    }
    out
}

/// Rows 0 to 17 after a fatal error, for machines with no serial port:
/// `lines` (white on red) from the top, then the log as it stood before
/// the error, under a heading. The kernel takes the screen back from the
/// console driver first; the log below shows the end of the full report.
fn crash_screen(lines: &[&[u8]], before: &Snapshot) {
    const RED: u8 = 0x4F;
    const HEADING_ROW: usize = LOG_TOP - SNAPSHOT_ROWS - 1; // 10
    display::reclaim();
    for row in 0..LOG_TOP {
        vga_print(row, b"", 0x07);
    }
    for (row, line) in lines.iter().take(HEADING_ROW).enumerate() {
        vga_print(row, line, RED);
    }
    vga_print(HEADING_ROW, b"Before it happened (the last lines of the log):", 0x0E);
    for (r, line) in before.iter().enumerate() {
        for (c, &ch) in line.iter().enumerate().take(display::cols()) {
            display::put(HEADING_ROW + 1 + r, c, ch, 0x07);
        }
    }
}

/// The line printed so far (up to LINE_MAX bytes), for selftest.rs.
fn current_line(out: &mut [u8; LINE_MAX]) -> usize {
    let log = SCREEN_LOG.lock();
    *out = log.line;
    log.line_len
}

/// Text at a column, leaving the rest of the row as it is.
fn vga_text(row: usize, col: usize, text: &[u8], attr: u8) {
    for (i, &ch) in text.iter().enumerate() {
        display::put(row, col + i, ch, attr);
    }
}

/// Serial only, not the screen log: notes for test.sh that would disturb
/// what's on screen (the log's own scrolling, for one).
fn serial_note(s: &[u8]) {
    cpu::without_interrupts(|| {
        if !SERIAL_PRESENT.load(core::sync::atomic::Ordering::Relaxed) {
            return;
        }
        for &b in s {
            if b == b'\n' {
                uart_byte(b'\r');
            }
            uart_byte(b);
        }
    });
}

// Out of line on purpose: these are called from dozens of places, and
// letting the compiler inline the UART polling loop at each one multiplied
// the kernel's code size.
//
// One call is never split: it runs with interrupts off, so no other task
// can be switched in halfway and print into the middle of it (Entry 17: the
// keyboard driver's first line landed inside "init complete").
#[inline(never)]
fn serial_write_str(s: &[u8]) {
    cpu::without_interrupts(|| {
        for &b in s {
            if b == b'\n' {
                serial_write_byte(b'\r');
            }
            serial_write_byte(b);
        }
    });
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
    // One write, so an interrupt's own output can't land inside the number.
    let mut buf = *b"0x0000000000000000";
    for (i, shift) in (0..16).rev().enumerate() {
        let nibble = ((n >> (shift * 4)) & 0xF) as u8;
        buf[2 + i] = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
    }
    serial_write_str(&buf);
}



/// -----------------------------------------------------------------
/// Kernel entry point
/// -----------------------------------------------------------------
/// 
/// A whole row: `msg` from column 0, the rest of the row blank, all in
/// `attr` (VGA attribute: 0x0F white on black, 0x0A green on black...).
fn vga_print(row: usize, msg: &[u8], attr: u8) {
    for col in 0..display::cols() {
        display::put(row, col, msg.get(col).copied().unwrap_or(b' '), attr);
    }
}

/// Boot contract (BIOS path):
///   - CPU is in 64-bit long mode
///   - Paging on; first 2 MiB identity-mapped (2MiB huge page)
///   - Kernel loaded at physical 0x0010_0000
///   - E820 map at 0x0500 (count u16) / 0x0504 (entries, 24 B each)
///   - FLAGS after the last BIOS call at 0x04F0 (u16)
///   - Display information at 0x0900 ('FDSP' + the mode, boot.rs) and the
///     VGA ROM's 8x16 font at 0x1000 (4 KiB): text mode or a VESA graphics
///     mode with a linear framebuffer (Entry 18)
///   - Interrupts disabled (and _start disables them anyway: Entry 16)
#[no_mangle]
pub extern "C" fn rust_start() -> ! {

    display::init();
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
    serial_write_str(b"[idt] loaded: a gate for every vector (0-255), code selector ");
    serial_write_hex(cs as u64);
    serial_write_str(b", #DF on IST1\n");

    // --- Interrupts were off from the bootloader on (Entry 16) ---
    let bios_left_if = boot::bios_left_interrupts_on();
    serial_write_str(b"[boot] self-test interrupts were off when the kernel started (the BIOS had left them ");
    serial_write_str(if bios_left_if { b"ON after its disk calls; stage 2 turned them off)" } else { b"off)" });
    let entry_ok = selftest::verdict(!boot::entered_with_interrupts_on());

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
    // SAFETY: [phys] the one call, early at boot as read_e820_map asks:
    // stage 2's map is still where it left it, in the low 1 MiB, which
    // nothing writes and the frame allocator never hands out.
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
    // SAFETY: [paging] the one call, before pmm::init, with stage 2's map
    // and `early` sized by physmap_table_bound for that same map.
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
    // SAFETY: [phys] the one call, right after map_physical_memory, with
    // the frames the early page tables took, so it marks them in use.
    let ps = unsafe { pmm::init(e820, early.used()) };
    serial_write_str(b"[pmm] bitmap: ");
    serial_write_u64(ps.bitmap_bytes / 1024);
    serial_write_str(b" KiB at ");
    serial_write_hex(ps.bitmap_phys);
    serial_write_str(b", tracking ");
    serial_write_u64(ps.frames);
    serial_write_str(b" frames (to ");
    serial_write_hex(ps.frames * pmm::PAGE_SIZE);
    serial_write_str(b"); frame table after it: ");
    serial_write_u64(ps.table_bytes / 1024);
    serial_write_str(b" KiB, 8 bytes a frame: its state, kind and owner\n[pmm] reserved: low 1 MiB, kernel ");
    serial_write_hex(kernel_start);
    serial_write_str(b"-");
    serial_write_hex(kernel_end);
    serial_write_str(b", ");
    serial_write_u64((early.used().1 - early.used().0) / pmm::PAGE_SIZE);
    serial_write_str(b" early page-table frames, the bitmap and the table\n[pmm] usable RAM ");
    serial_write_u64(ps.usable_bytes / (1024 * 1024));
    serial_write_str(b" MiB, free ");
    serial_write_u64(ps.free_frames * pmm::PAGE_SIZE / (1024 * 1024));
    serial_write_str(b" MiB\n");

    // --- The screen: map the framebuffer, if stage 2 set a graphics mode ---
    // As early as it can be: it needs the frame allocator for page tables.
    // SAFETY: [mmio] the one call, with the frame allocator and the
    // kernel's page tables (vmm) working, as attach requires.
    let attached = unsafe { display::attach() };
    display::describe();
    let mut display_ok = match attached {
        Ok(()) => true,
        Err(why) => selftest::check(b"display", why, false),
    };

    // SAFETY: [phys] after pmm::init, and before anything else is running
    // that could want the frames the tests borrow.
    let pmm_ok = unsafe { pmm::selftest(e820) };
    // SAFETY: [phys] after map_physical_memory and pmm::init, as it needs.
    let physmap_ok = unsafe { paging::selftest(e820) };

    // --- Page mapping API: map/unmap/translate 4 KiB pages ---
    // SAFETY: [paging] the kernel's page tables and the frame allocator are
    // up, and no task exists yet to be using the addresses it maps.
    let vmm_ok = unsafe { vmm::selftest() };
    display_ok &= display::selftest();

    // --- Kernel heap: Box, Vec, String, BTreeMap ---
    let heap_ok = heap::selftest();
    // The lock types every shared state now lives behind (Entry 31).
    let heap_ok = lock::selftest() & heap_ok;
    // BLAKE3 and the audit chain's verifier (Entry 32), before anything is
    // recorded that depends on them.
    let audit_ok = audit::selftest_mechanics();
    // SHA-1, for checking a TPM 1.2's PCRs (Entry 33).
    let sha1_passed = sha1::test_vectors_passed();
    serial_write_str(b"[sha1] self-test FIPS 180's examples (the empty string, \"abc\", the 448-bit message, a million 'a's): ");
    serial_write_u64(sha1_passed as u64);
    serial_write_str(b" of ");
    serial_write_u64(sha1::TEST_VECTORS as u64);
    let audit_ok = selftest::verdict(sha1_passed == sha1::TEST_VECTORS) & audit_ok;

    // --- Smoke-test: allocate a frame and verify it is zeroed ---
    let sanitize_ok = match pmm::alloc(pmm::Kind::Test, pmm::Owner::Kernel) {
        Some(frame) => {
            serial_write_str(b"[kernel] test alloc: frame at ");
            serial_write_hex(frame.phys());
            serial_write_str(b"\n");

            // Verify zeroed (Sanitized invariant check).
            // SAFETY: [phys] the frame is this code's (`frame` holds it) and
            // the physical map covers all RAM; the slice is dropped before
            // the frame is freed.
            let slice = unsafe { core::slice::from_raw_parts(paging::phys_to_virt(frame.phys()), pmm::PAGE_SIZE as usize) };
            let all_zero = slice.iter().all(|&b| b == 0);
            frame.free();

            selftest::check(b"kernel", b"a freshly allocated frame is all zeros", all_zero)
        }
        None => selftest::check(b"kernel", b"alloc returned a frame", false),
    };

    // --- Interrupts: legacy PICs out of the way, local APIC timer on ---
    // Last, so every allocation and mapping above happened with interrupts
    // off; apic::selftest() turns them on and leaves them on.
    pic::remap_and_mask();
    let apic_info = apic::init(); // masks every local source but the timer

    // --- Firmware tables (ACPI) and the I/O APIC ---
    // ACPI says where the I/O APIC is and how ISA interrupts reach it, and
    // how to switch off and restart. Done BEFORE interrupts are enabled, so
    // that every I/O APIC input is masked first: until then it holds
    // whatever the firmware left there (Entry 15).
    let acpi = acpi::init();
    let mut firmware_ok = acpi::selftest(&acpi);
    let ioapic_ready = match (&acpi, &apic_info) {
        (Ok(info), Ok(_)) => match irq::init_ioapic(info) {
            Ok(setup) => {
                firmware_ok &= irq::selftest_ioapic(&setup);
                true
            }
            Err(why) => {
                firmware_ok &= selftest::check(b"ioapic", why, false);
                false
            }
        },
        _ => false,
    };

    let mut irq_ok = match apic_info {
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
            apic::selftest(&info) & idt::selftest_unexpected()
        }
        Err(why) => selftest::check(b"apic", why, false),
    };

    // --- Scheduler: the boot thread becomes task 0 ---
    // Only with a timer that ticks: sleeping and preemption both run off it,
    // and without ticks the sleep test would wait forever. Ticking is
    // enough: a rate the self-test measured badly fails that test, not the
    // scheduler and everything after it (Entry 22).
    let sched_ok = if !apic::ticking() {
        selftest::check(b"sched", b"scheduler not started: it needs the timer", false)
    } else if sched::init().is_err() {
        selftest::check(b"sched", b"scheduler not started: no memory for the idle task", false)
    } else {
        serial_write_str(b"[sched] scheduler on: ");
        serial_write_u64(sched::MAX_TASKS as u64);
        serial_write_str(b" task slots, ");
        serial_write_u64(sched::STACK_SIZE / 1024);
        serial_write_str(b" KiB stacks at ");
        serial_write_hex(sched::TASK_STACKS_BASE);
        serial_write_str(b", preempted every timer tick\n");
        sched::selftest()
    };

    // --- The TPM (Entry 33): found and named; QEMU's is used to anchor the
    // audit chain. Its waits are counted in timer ticks, so after the
    // scheduler. ---
    if sched::is_enabled() {
        let (found, pcr) = tpm::init();
        audit::tpm_found(found, pcr);
    }

    // --- User mode: system calls on, then processes ---
    let mut user_mode_on = false;
    let user_ok = if !sched::is_enabled() {
        selftest::check(b"user", b"user mode not started: it needs the scheduler", false)
    } else {
        match syscall::init() {
            Err(why) => selftest::check(b"user", why, false),
            Ok(entry) => {
                user_mode_on = true;
                let (smep, smap) = cpu::enable_smep_smap();
                serial_write_str(b"[user] system calls via SYSCALL at ");
                serial_write_hex(entry);
                serial_write_str(b"; user space ");
                serial_write_hex(vmm::USER_BASE);
                serial_write_str(b"-");
                serial_write_hex(vmm::USER_END);
                serial_write_str(if smep { b"; SMEP on" } else { b"; no SMEP on this CPU" });
                serial_write_str(if smap { b", SMAP on\n" } else { b", no SMAP\n" });
                process::selftest()
            }
        }
    };

    // --- Rest of Phase 4: a mutex, IPC, and the driver model ---
    let ipc_ok = if !user_mode_on {
        selftest::check(b"ipc", b"IPC and drivers not tested: they need user mode", false)
    } else {
        let mutex = sync::selftest();
        let ipc = ipc::selftest();
        let factory = factory::selftest();
        let info = info::selftest();
        let caps = cap::selftest();
        let budgets = mem::selftest();
        let drivers = irq::selftest();
        mutex && ipc && factory && info && caps && budgets && drivers
    };

    // --- Frame identity, after every test process has ended (Entry 29):
    // no frame names a task that is gone, and what the frames are doing ---
    sched::reap();
    let owners_ok = pmm::selftest_owners(|id, generation| sched::slot_holds(id as usize, generation));
    pmm::print_kinds();
    // --- The audit chain the boot has recorded (Entry 32), its seals
    // extended into the TPM first (Entry 33) ---
    if sched::is_enabled() {
        audit::anchor();
    }
    let audit_ok = audit::selftest_tpm() & audit_ok;
    let audit_ok = audit::selftest_boot() & audit_ok;

    // --- VGA status display (screen was cleared at entry) ---
    // Steps with no result value either succeed or end in an exception
    // report, so reaching this point means they passed.
    vga_status(0, b"Kernel + screen: ", display_ok);
    vga_status(1, b"Segments + TSS:  ", true);
    vga_status(2, b"Exception table: ", int3_ok && entry_ok);
    vga_status(3, b"Page tables:     ", true);
    vga_status(4, b"Memory map:      ", true);
    vga_status(5, b"Frame allocator: ", pmm_ok && owners_ok);
    // The panel's rows are all taken: the audit chain's self-tests count in
    // the tally below, like every other.
    let _ = audit_ok;
    // Anything the hardware sent that nobody asked for, over the whole boot.
    irq_ok &= idt::selftest_none_unexpected();

    vga_status(6, b"Physical map:    ", physmap_ok);
    vga_status(7, b"Page mapping:    ", vmm_ok);
    vga_status(8, b"Kernel heap:     ", heap_ok);
    vga_status(9, b"Sanitise check:  ", sanitize_ok);
    vga_status(10, b"Timer interrupt: ", irq_ok);
    vga_status(11, b"ACPI + I/O APIC: ", firmware_ok);
    vga_status(12, b"Scheduler:       ", sched_ok);
    vga_status(13, b"User mode:       ", user_ok);
    vga_status(14, b"IPC + drivers:   ", ipc_ok);
    if bios_left_if {
        // Worth seeing on a real machine: this is what Entry 16 was about.
        vga_text(0, 26, b"(the BIOS left interrupts on; stage 2 turns them off)", 0x08);
    }
    // Graphics mode has room on the right: which mode stage 2 chose, and
    // the 16 colours as a check that the pixel format is read right.
    let mut note = [0u8; 64];
    let note_len = display::status_note(&mut note);
    if note_len > 0 && display::cols() >= 100 {
        let col = display::cols() - 36;
        vga_text(1, col, &note[..note_len], 0x07);
        display::draw_swatch(2, col);
    }

    // --- One line that says whether everything is up ---
    let all_ok = selftest::summary();
    let (passed, failed) = selftest::counts();
    vga_summary(15, passed, failed);

    // The uptime line is kept current by a task of its own. The self-tests'
    // last tasks are reaped first, so the clock is task 2 and init process
    // 3 on every boot.
    sched::reap();
    if sched::is_enabled() && sched::spawn("clock", clock_task, 17).is_err() {
        serial_write_str(b"[kernel] could not start the clock task\n");
    }

    // Init (Entry 21): the first process, which starts the rest. The kernel
    // hands it what it used to hand out itself, and the Factory capability
    // to make processes and endpoints with.
    let init = user_mode_on && start_init(ioapic_ready);

    // Build-time demo, if one was requested. The fault demos halt; they run
    // once init's console driver has drawn, so their crash screens show the
    // kernel taking the screen back (Entry 20).
    if option_env!("FREYA_DEMO").is_some_and(|d| !d.is_empty()) && init {
        sched::sleep_ticks(20);
    }
    run_requested_demo();

    if all_ok {
        vga_print(
            16,
            match (init, display::cols() >= 110) {
                (true, true) => b"--- FreyaOS ready ---  Ctrl+Alt+Del: restart   Ctrl+Alt+End: switch off   PageUp/PageDown: scroll the log",
                (true, false) => b"--- FreyaOS ready ---  Ctrl+Alt+Del restart  Ctrl+Alt+End off  PgUp/PgDn scroll",
                (false, _) => b"--- FreyaOS foundation ready ---",
            },
            0x0F,
        );
        serial_write_str(b"[kernel] init complete - boot thread done; scheduler and timer running\n");
    } else {
        vga_print(16, b"--- SELF-TEST FAILURE: the failed tests are at the bottom of the screen ---", 0x0C);
        serial_write_str(b"[kernel] init complete WITH FAILURES\n");
    }

    // Boot is done. With the scheduler running, the boot thread just ends:
    // the idle task and the clock take over. Without it, idle right here.
    if sched::is_enabled() {
        sched::exit();
    }
    loop {
        // SAFETY: [cpu] with no scheduler there is nothing left to run but
        // interrupt handlers: let them in, and wait for the next one.
        unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
    }
}

/// Pages kept out of init's Budget (Entry 26): 4 MiB for the kernel's own
/// needs (task stacks, page tables, endpoints) whatever the heaps take.
const INIT_RESERVE: u64 = 1024;

/// Init's task id, once it runs (for the demos).
static INIT_TASK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(usize::MAX);

/// Start init with everything the drivers it starts will need. Its slots
/// (user/rust/src/bin/init.rs says the same):
///
///   0 Console        1 Factory        2 its own endpoint (exit signals)
///   3 the console driver's endpoint (every right)
///   4 the screen     5 the system log 6 the status rows 7 the screen's
///                                                         description
///   8 Irq 1 (the keyboard)            9 IoPorts 0x60   10 IoPorts 0x64
///  11 Power                         12 Irq 12 (the mouse, Entry 22)
///  13 Info (for the shell, Entry 23)
///  14 Budget (Entry 26): every page free now but INIT_RESERVE, for the
///     heaps of whoever init lends it to
///
/// A slot is left empty if the kernel has nothing to put there (no screen
/// to lend, interrupts not routed); init then can't start what needs it,
/// and says so. Returns whether init started.
fn start_init(ioapic_ready: bool) -> bool {
    use cap::{Cap, RECV, SEND};
    let console = console::prepare();
    let own = ipc::create();
    let console_cap = |pick: fn(&console::ConsoleCaps) -> Cap| console.as_ref().map_or(Cap::None, pick);
    let caps = cap::table(&[
        Cap::Console,
        Cap::Factory { rights: cap::FACTORY_ALL },
        own.map_or(Cap::None, |ep| Cap::Endpoint { ep, badge: 0, rights: SEND | RECV }),
        console_cap(|c| c.endpoint),
        console_cap(|c| c.screen),
        console_cap(|c| c.log),
        console_cap(|c| c.panel),
        console_cap(|c| c.description),
        if ioapic_ready { Cap::Irq { line: 1 } } else { Cap::None },
        Cap::IoPorts { base: 0x60, count: 1 },
        Cap::IoPorts { base: 0x64, count: 1 },
        Cap::Power,
        if ioapic_ready { Cap::Irq { line: 12 } } else { Cap::None },
        Cap::Info,
        Cap::Budget { pages: crate::pmm::free_frames().saturating_sub(INIT_RESERVE).min(u32::MAX as u64) as u32 },
    ]);
    let started = process::spawn(&process::INIT, 0, caps);
    // init's table holds the endpoints now (or nothing does, and they go).
    if let Some(ep) = own {
        ipc::release_created(ep);
    }
    if let Some(Cap::Endpoint { ep, .. }) = console.as_ref().map(|c| c.endpoint) {
        ipc::release_created(ep);
    }
    match started {
        Ok(id) => {
            INIT_TASK.store(id, core::sync::atomic::Ordering::Relaxed);
            // One call: init may already be running and printing (Entry 17).
            let mut line = idt::Line::new();
            line.str(b"[kernel] init (process ").dec(id as u64).str(b") starts the drivers\n");
            serial_write_str(line.as_bytes());
            true
        }
        Err(_) => {
            serial_write_str(b"[kernel] couldn't start init, so no drivers: to switch off, hold the power button (safe: nothing is written to disk)\n");
            false
        }
    }
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

/// Task: keep the uptime line on screen current, waking once a second.
fn clock_task(row: u64) {
    loop {
        let ticks = apic::ticks();
        let mut line = VgaLine::new();
        line.put(b"Uptime:          ")
            .num(ticks / apic::TIMER_HZ)
            .put(b" s  (")
            .num(ticks)
            .put(b" timer ticks, ")
            .num(sched::switches())
            .put(b" task switches)");
        vga_print(row as usize, line.as_bytes(), 0x07);
        // Each second's counts and hash of the messages and task switches
        // in it go into the audit chain (Entry 32).
        crate::audit::interval(ticks / apic::TIMER_HZ);
        // And any segment sealed since, into the TPM (Entry 33).
        crate::audit::anchor();
        // Wake at the start of the next second.
        sched::sleep_ticks(apic::TIMER_HZ - ticks % apic::TIMER_HZ);
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

    match demo {
        // #UD, vector 6
        // SAFETY: [fault] an invalid instruction on purpose: the #UD
        // handler reports it and halts.
        "ud2" => unsafe { core::arch::asm!("ud2") },
        // #PF, not present: above everything that is mapped
        "page_fault" => {
            // SAFETY: [fault] nothing is mapped at 256 MiB in the
            // kernel's space, so this faults (and halts) before reading.
            unsafe { core::ptr::read_volatile(0x1000_0000 as *const u8) };
        }
        // #DF on IST1 after the stack hits its guard page
        "stack_overflow" => {
            demo_stack_overflow(0);
        }
        // The same, on a task's 16 KiB stack instead of the boot stack
        "task_stack_overflow" => sched::demo_task_stack_overflow(),
        // #PF: the kernel reads (SMAP) or runs (SMEP) a process's page.
        // Needs a CPU that has them: QEMU -cpu max, or SMEP on the T430.
        "smap" => process::demo_kernel_touches_user(false),
        "smep" => process::demo_kernel_touches_user(true),
        // #PF, instruction fetch: kernel code through its no-execute
        // physical-map alias
        "nx" => {
            // Without NX the alias is executable, and the call would run
            // the kernel's entry code again instead of faulting.
            if !paging::nx_enabled() {
                serial_write_str(b"[demo] this CPU has no NX: nothing to show\n");
                return;
            }
            // SAFETY: [fault] the kernel's code seen through the physical
            // map, whose pages are no-execute (checked just above):
            // fetching from it faults before a single instruction runs.
            let f: extern "C" fn() = unsafe { core::mem::transmute(paging::phys_to_virt(0x10_0000)) };
            f();
        }
        // #PF, not present: used, unmapped, used again
        // SAFETY: [fault] boot is done with the address it maps; it
        // faults and halts.
        "use_after_unmap" => unsafe { vmm::demo_use_after_unmap() },
        // #PF, protection violation on a write
        // SAFETY: [fault] as above: it faults and halts.
        "write_read_only" => unsafe { vmm::demo_write_read_only() },
        // [heap] FATAL: double free
        "heap_double_free" => heap::demo_double_free(),
        // [lock] FATAL (Entry 31): a lock taken while it is already held
        "lock_twice" => lock::demo_lock_twice(),
        // The audit chain (Entry 32), printed for build/audit_verify.rs to
        // check outside the machine; and one bit of it changed first.
        "audit_dump" => {
            audit::dump();
            return;
        }
        "audit_tamper" => {
            audit::demo_tamper();
            audit::dump();
            return;
        }
        // The whole chain rewritten to hide a change, which only the TPM's
        // PCR can show (Entry 33).
        "audit_rewrite" => {
            audit::demo_rewrite();
            audit::dump();
            return;
        }
        // [pmm] FATAL (Entry 29): a frame dropped while held, and the
        // kernel image's first frame given back as if it were heap
        "lost_frame" => pmm::demo_lost_frame(),
        "free_kernel_image" => pmm::demo_free_kernel_image(),
        // [kernel] PANIC with file and line
        "panic" => panic!("FREYA_DEMO=panic"),
        // Not faults (Entries 20, 21 and 23): the console driver killed, so
        // init must start it again; init and then the console driver
        // killed, or the console driver killed more often than init
        // starts it again, so the kernel must take the screen back and
        // the keyboard driver must carry on without it; and the log
        // flooded past the console's 400 rows, then a line every
        // quarter second.
        "kill_console" => {
            demo_kill_console();
            return;
        }
        "kill_init" => {
            demo_kill_init();
            return;
        }
        "kill_console_often" => {
            demo_kill_console_often();
            return;
        }
        "log_flood" => {
            demo_log_flood();
            return;
        }
        // The shell killed (Entry 23): init starts it again, and
        // typing reaches the new one.
        "kill_shell" => {
            demo_kill_shell();
            return;
        }
        _ => {
            serial_write_str(b"[demo] unknown; known: ud2 page_fault stack_overflow \
                               task_stack_overflow smap smep nx use_after_unmap write_read_only heap_double_free lock_twice panic audit_dump audit_tamper audit_rewrite \
                               kill_console kill_init kill_console_often log_flood kill_shell\n");
            return;
        }
    }
    serial_write_str(b"[demo] returned without faulting\n");
}

/// The process drawing the screen, waiting (up to 3 s) for one to start.
fn console_task() -> Option<sched::TaskId> {
    for _ in 0..300 {
        if console::task().is_some() {
            break;
        }
        sched::sleep_ticks(1);
    }
    console::task()
}

/// Demo: kill the console driver once it draws, and wait until it has
/// ended.
fn demo_kill_console() {
    let Some(id) = console_task() else {
        serial_write_str(b"[demo] no console driver to kill\n");
        return;
    };
    let ended = sched::kill(id) && sched::join_timeout(id, 200).is_some();
    serial_write_str(if ended { b"[demo] console driver killed\n" } else { b"[demo] the console driver didn't end\n" });
}

/// Demo: kill the console driver four times, each time once init has
/// started it again: after the third restart init leaves it stopped, and
/// lets go of its endpoint, so the keyboard driver isn't left waiting.
fn demo_kill_console_often() {
    for n in 1..=4u64 {
        // The kernel forgets who drew when that process ends, so this
        // waits for the next one to take the screen, whatever its id.
        let Some(id) = console_task() else {
            serial_write_str(b"[demo] no console driver to kill\n");
            return;
        };
        let ended = sched::kill(id) && sched::join_timeout(id, 200).is_some();
        let mut line = idt::Line::new();
        line.str(if ended { b"[demo] console driver killed (" } else { b"[demo] the console driver didn't end (" });
        line.dec(n).str(b" of 4)\n");
        serial_write_str(line.as_bytes());
    }
}

/// Demo: kill the shell once it is running (waiting up to 3 s for it).
fn demo_kill_shell() {
    let mut shell = None;
    for _ in 0..300 {
        shell = sched::find("shell");
        if shell.is_some() {
            break;
        }
        sched::sleep_ticks(1);
    }
    let Some(id) = shell else {
        serial_write_str(b"[demo] no shell to kill\n");
        return;
    };
    let ended = sched::kill(id) && sched::join_timeout(id, 200).is_some();
    serial_write_str(if ended { b"[demo] shell killed\n" } else { b"[demo] the shell didn't end\n" });
}

/// Demo: once the console driver draws, kill init, then the console
/// driver: nobody starts it again.
fn demo_kill_init() {
    console_task();
    let id = INIT_TASK.load(core::sync::atomic::Ordering::Relaxed);
    let ended = id != usize::MAX && sched::kill(id) && sched::join_timeout(id, 200).is_some();
    serial_write_str(if ended { b"[demo] init killed\n" } else { b"[demo] init didn't end\n" });
    demo_kill_console();
}

/// Demo: 600 numbered lines at once, then one every 25 ticks for ever (a
/// task of its own).
fn demo_log_flood() {
    for n in 1..=600u64 {
        let mut line = idt::Line::new();
        line.str(b"flood ").dec(n).str(b"\n");
        serial_write_str(line.as_bytes());
    }
    if sched::spawn("ticker", ticker_task, 0).is_err() {
        serial_write_str(b"[demo] could not start the ticker\n");
    }
}

fn ticker_task(_: u64) {
    for n in 1.. {
        sched::sleep_ticks(25);
        let mut line = idt::Line::new();
        line.str(b"tick ").dec(n).str(b"\n");
        serial_write_str(line.as_bytes());
    }
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

/// Demo only: recurse until the stack runs into its guard page. Each frame
/// keeps a 512-byte buffer alive, so the 64 KiB boot stack is used up in
/// ~120 calls and a 16 KiB task stack in ~30. black_box stops the compiler from turning the recursion
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
        // SAFETY: [cpu] stops the CPU for good: interrupts off, then halt
        // (and halt again should an NMI wake it).
        unsafe { core::arch::asm!("cli; hlt"); }
    }
}

#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // File, line and (for messages without {} arguments) the text. Printed
    // without core::fmt, which would add several KiB to the kernel.
    let before = screen_log_snapshot();
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

    let mut place = [0u8; 80];
    let mut len = 0;
    for &b in b"at ".iter().chain(info.location().map(|l| l.file().as_bytes()).unwrap_or(b"?")) {
        if let Some(slot) = place.get_mut(len) {
            *slot = b;
            len += 1;
        }
    }
    let mut digits = [0u8; 10];
    let mut n = info.location().map(|l| l.line()).unwrap_or(0);
    let mut d = digits.len();
    loop {
        d -= 1;
        digits[d] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 || d == 0 {
            break;
        }
    }
    for &b in b":".iter().chain(&digits[d..]) {
        if let Some(slot) = place.get_mut(len) {
            *slot = b;
            len += 1;
        }
    }
    let message = info.message().as_str().map(|m| m.as_bytes()).unwrap_or(b"(message has arguments: see the log below)");
    crash_screen(&[b"!! KERNEL PANIC", &place[..len], message], &before);
    halt();
}