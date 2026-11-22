//! power.rs — restarting and switching off, the proper way.
//!
//! Both are reached only through the Power capability (a process asks with
//! the power system call; today only the keyboard driver holds it, so the
//! person at the keyboard decides). Each tries the clean method first and
//! falls back step by step:
//!
//! Restart:
//!   1. the FADT's reset register (on Intel chipsets usually port 0xCF9);
//!   2. the keyboard controller's reset line (0xFE to port 0x64), the
//!      method PCs have answered since the AT;
//!   3. port 0xCF9 directly (full reset through the chipset);
//!   4. a triple fault (an empty IDT, then an exception), which every CPU
//!      turns into a reset.
//!
//! Power off (ACPI "soft off", S5):
//!   1. switch the firmware into ACPI mode if it isn't (write ACPI_ENABLE
//!      to the SMI command port, wait for SCI_EN);
//!   2. write \_S5's SLP_TYP with SLP_EN into PM1a (and PM1b) control.
//!   If the machine is still running after that, the screen says it has
//!   stopped and the power button may be held: FreyaOS writes to no disk,
//!   so cutting the power then loses nothing.
//!
//! The firmware's _PTS method, which ACPI says to run before sleeping, is
//! AML and isn't run (there is no AML interpreter); most machines power off
//! without it. If one doesn't, the fallback message covers it.

use crate::{inb, outb};
use core::arch::asm;

pub const RESTART: u64 = 0;
pub const SHUT_DOWN: u64 = 1;

const SCI_EN: u16 = 1 << 0;
const SLP_EN: u16 = 1 << 13;
const SLP_TYP_SHIFT: u16 = 10;
const SLP_TYP_MASK: u16 = 7 << SLP_TYP_SHIFT;

unsafe fn inw(port: u16) -> u16 {
    let v: u16;
    asm!("in ax, dx", in("dx") port, out("ax") v, options(nomem, nostack, preserves_flags));
    v
}

unsafe fn outw(port: u16, v: u16) {
    asm!("out dx, ax", in("dx") port, in("ax") v, options(nomem, nostack, preserves_flags));
}

fn wait_ms(ms: u64) {
    let mut left = ms;
    while left > 0 {
        let step = left.min(50);
        crate::apic::pit_wait_ms(step);
        left -= step;
    }
}

/// Say what is happening, on screen and serial. Nothing else needs saving:
/// FreyaOS keeps nothing on disk yet.
fn announce(what: &[u8]) {
    crate::serial_write_str(b"[power] ");
    crate::serial_write_str(what);
    crate::serial_write_str(b"\n");
    crate::vga_print(16, what, 0x0E);
}

pub fn restart() -> ! {
    announce(b"Restarting...");
    unsafe {
        asm!("cli", options(nomem, nostack));

        if let Some((port, value)) = crate::acpi::info().and_then(|i| i.reset) {
            crate::serial_write_str(b"[power] restart through the ACPI reset register\n");
            outb(port, value);
            wait_ms(100);
        }

        crate::serial_write_str(b"[power] restart through the keyboard controller\n");
        for _ in 0..100_000 {
            if inb(0x64) & 0x02 == 0 {
                break; // its input buffer is empty: it will take a command
            }
        }
        outb(0x64, 0xFE);
        wait_ms(100);

        crate::serial_write_str(b"[power] restart through port 0xCF9\n");
        outb(0xCF9, 0x02);
        outb(0xCF9, 0x06);
        wait_ms(100);

        crate::serial_write_str(b"[power] restart by triple fault\n");
        let empty: [u8; 10] = [0; 10];
        asm!("lidt [{}]", "int3", in(reg) &empty, options(noreturn));
    }
}

pub fn shut_down() -> ! {
    announce(b"Shutting down...");
    unsafe {
        asm!("cli", options(nomem, nostack));
        match crate::acpi::info() {
            Some(info) if info.s5.is_some() && info.pm1a_control != 0 => {
                let (typ_a, typ_b) = info.s5.unwrap_or((0, 0));
                if inw(info.pm1a_control) & SCI_EN == 0 && info.smi_command != 0 && info.acpi_enable != 0 {
                    crate::serial_write_str(b"[power] switching the firmware to ACPI mode\n");
                    outb(info.smi_command, info.acpi_enable);
                    for _ in 0..60 {
                        if inw(info.pm1a_control) & SCI_EN != 0 {
                            break;
                        }
                        wait_ms(50);
                    }
                }
                crate::serial_write_str(b"[power] ACPI soft off (S5), SLP_TYPa=");
                crate::serial_write_u64(typ_a as u64);
                crate::serial_write_str(b"\n");
                let a = inw(info.pm1a_control) & !SLP_TYP_MASK;
                outw(info.pm1a_control, a | (typ_a as u16) << SLP_TYP_SHIFT | SLP_EN);
                if info.pm1b_control != 0 {
                    let b = inw(info.pm1b_control) & !SLP_TYP_MASK;
                    outw(info.pm1b_control, b | (typ_b as u16) << SLP_TYP_SHIFT | SLP_EN);
                }
                wait_ms(1000);
                crate::serial_write_str(b"[power] still running after S5\n");
            }
            _ => crate::serial_write_str(b"[power] no ACPI soft-off information\n"),
        }
    }
    crate::vga_print(16, b"FreyaOS has stopped. It is safe to switch off (hold the power button).", 0x0E);
    crate::serial_write_str(b"[power] stopped; safe to switch off\n");
    crate::halt();
}
