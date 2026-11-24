//! pic.rs — the legacy 8259 interrupt controllers, moved out of the way.
//!
//! Every PC still has two cascaded 8259 PICs (or an emulation of them).
//! The BIOS leaves them delivering IRQ 0-7 on vectors 8-15, which collide
//! with CPU exceptions: the old timer on IRQ 0 would arrive as vector 8 and
//! look exactly like a double fault.
//!
//! FreyaOS takes interrupts from the local APIC instead (apic.rs), so the
//! PICs only need to be made harmless: re-initialise them onto vectors
//! 0x20-0x2F, clear of the exceptions, then mask every line. Remapping
//! even though they end up masked means that if one ever does fire, it
//! cannot masquerade as an exception.

use crate::{inb, outb};

const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// First vector of each PIC after remapping (8 vectors each).
pub const PIC1_VECTOR_BASE: u8 = 0x20;
pub const PIC2_VECTOR_BASE: u8 = 0x28;

const ICW1_INIT: u8 = 0x10; // start initialisation
const ICW1_ICW4: u8 = 0x01; // an ICW4 will follow
const ICW4_8086: u8 = 0x01; // 8086 mode (not the 8080 call-instruction mode)

/// A write to an unused port: gives an old PIC time to settle between
/// initialisation words.
fn io_wait() {
    // SAFETY: [port] 0x80 is the POST diagnostic port, which no device acts
    // on; writing it only takes time, which is the point.
    unsafe { outb(0x80, 0) };
}

/// Re-initialise both PICs onto vectors 0x20-0x2F and mask all 16 lines.
pub fn remap_and_mask() {
    // SAFETY: [port] the two 8259 PICs' command and data ports, which only
    // this file drives, written in the order the 8259's initialisation
    // sequence requires. The result masks every line: the PICs are then
    // silent and the APICs deliver interrupts instead.
    unsafe {
        // ICW1: begin initialisation, ICW4 will follow.
        outb(PIC1_COMMAND, ICW1_INIT | ICW1_ICW4);
        io_wait();
        outb(PIC2_COMMAND, ICW1_INIT | ICW1_ICW4);
        io_wait();
        // ICW2: vector offsets.
        outb(PIC1_DATA, PIC1_VECTOR_BASE);
        io_wait();
        outb(PIC2_DATA, PIC2_VECTOR_BASE);
        io_wait();
        // ICW3: the second PIC hangs off line 2 of the first.
        outb(PIC1_DATA, 1 << 2);
        io_wait();
        outb(PIC2_DATA, 2);
        io_wait();
        // ICW4: 8086 mode.
        outb(PIC1_DATA, ICW4_8086);
        io_wait();
        outb(PIC2_DATA, ICW4_8086);
        io_wait();
        // OCW1: mask every line.
        outb(PIC1_DATA, 0xFF);
        outb(PIC2_DATA, 0xFF);
    }
}

/// The two interrupt mask registers (0xFF = every line masked).
pub fn masks() -> (u8, u8) {
    // SAFETY: [port] reading a PIC's data port outside an initialisation
    // sequence returns its mask register and changes nothing.
    unsafe { (inb(PIC1_DATA), inb(PIC2_DATA)) }
}
