//! syscall.rs — how ring 3 asks the kernel for something.
//!
//! A process executes `syscall` with a number in RAX and arguments in RDI,
//! RSI, RDX (then R10, R8, R9). The CPU jumps to syscall_entry (entry.asm),
//! which switches to the task's kernel stack, saves every register and the
//! SSE state, and calls syscall_dispatch below. The result comes back in
//! RAX; RCX and R11 are destroyed (SYSCALL uses them); nothing else changes.
//!
//! Zero Ambient Authority: a process can do nothing by default. Each system
//! call that acts on the world names a capability (a slot in the process's
//! capability table, see cap.rs), and the kernel refuses if that slot
//! doesn't hold the right kind with the right rights. exit and yield act
//! only on the caller; reply answers a call the caller received.
//!
//!   0 exit(code)                       -> never returns
//!   1 yield()                          -> 0
//!   2 write(console, ptr, len)         -> bytes written
//!   3 send(endpoint, w0..w3, xfer)     -> 0                     (ipc.rs)
//!   4 recv(endpoint)                   -> 0 + message registers
//!   5 call(endpoint, w0..w3, xfer)     -> 0 + reply registers
//!   6 reply(-, w0..w3, xfer)           -> 0
//!   7 port_in(ports, port, width)      -> value                 (irq.rs)
//!   8 port_out(ports, port, width, v)  -> 0
//!   9 irq_wait(irq)                    -> 0 once the line has fired
//!  10 irq_ack(irq)                     -> 0 (the line unmasked: armed)
//!  11 power(power, action)             -> never returns (0 restart, 1 off)
//!  12 (console_scroll, Entry 18; retired in Entry 20: the console driver
//!      process scrolls its own view)       -> bad-syscall
//!  13 map(memory, address)             -> 0                     (mem.rs)
//!  14 note(console, ptr, len)          -> bytes: like write, to the serial
//!                                         port only (test notes)
//!  15 spawn(factory, program, arg, slots, exit_ep) -> the new process's id
//!                                                          (factory.rs)
//!  16 endpoint(factory)                -> slot of a new endpoint capability
//!  17 mint(endpoint, rights, badge)    -> slot of a narrower copy  (cap.rs)
//!  18 drop(slot)                       -> 0: the capability let go of
//!  19 irq_bind(irq, endpoint)          -> 0: the line signals there  (irq.rs)
//!  20 signal(endpoint)                 -> 0: the capability's badge set as
//!                                         signal bits there, never blocking
//!                                         (ipc.rs, Entry 23)
//!  21 info(info, ptr, len)             -> bytes: uptime, memory, tasks
//!                                                             (info.rs)
//!  22 cap_info(slot)                   -> kind, and in RDI, RSI, RDX what
//!                                         the caller's own slot holds
//!  23 kill(process)                    -> 0: the process stops (factory.rs,
//!                                         Entry 24)
//!  24 status(process)                  -> 0 running, or how it ended, the
//!                                         code or vector in RDI
//!  25 copy(slot)                       -> slot of a copy, derived from it
//!                                         (cap.rs, Entry 25)
//!  26 revoke(slot)                     -> how many capabilities derived
//!                                         from it were removed, everywhere
//!  27 sleep(ticks)                     -> 0, that many timer ticks later
//!                                         (acts on the caller only)
//!  28 heap(budget, pages)              -> where the heap starts, once it
//!                                         is that many pages long (mem.rs,
//!                                         Entry 26)
//!
//! cap_info's R10 (Entry 25): how many capabilities are derived from it.
//! For a Budget (Entry 26): RDI its limit in pages, RSI the pages in use by
//! it and everything copied from it, RDX its own heap's pages.
//!
//! spawn's R9 (Entry 24): flags in (bit 0: hand back a Process capability)
//! and that capability's slot out (-1 for none).
//!
//! Message registers: send/call take the four words in RSI, RDX, R10, R8
//! and in R9 the slot of a capability to pass along (-1 for none). recv and
//! call return the badge in RDI, the words in RSI, RDX, R10, R8 and in R9
//! the slot a received capability landed in (-1 for none).
//!
//! The kernel never trusts a pointer from a process. A buffer must lie in
//! user space, and every page of it must be mapped for ring 3 in the
//! caller's address space; the kernel then copies it through the physical
//! map, never through the user address itself. A buffer the kernel writes
//! (info) must also be writable, and the caller's own: never memory it
//! only borrows.

use crate::cap::Cap;
use crate::ipc::{self, Message};
use crate::sched::{self, ExitStatus};
use crate::vmm::{self, AddressSpace};
use core::sync::atomic::{AtomicU64, Ordering};

pub const SYS_EXIT: u64 = 0;
pub const SYS_YIELD: u64 = 1;
pub const SYS_WRITE: u64 = 2;
pub const SYS_SEND: u64 = 3;
pub const SYS_RECV: u64 = 4;
pub const SYS_CALL: u64 = 5;
pub const SYS_REPLY: u64 = 6;
pub const SYS_PORT_IN: u64 = 7;
pub const SYS_PORT_OUT: u64 = 8;
pub const SYS_IRQ_WAIT: u64 = 9;
pub const SYS_IRQ_ACK: u64 = 10;
pub const SYS_POWER: u64 = 11;
pub const SYS_MAP: u64 = 13;
pub const SYS_NOTE: u64 = 14;
pub const SYS_SPAWN: u64 = 15;
pub const SYS_ENDPOINT: u64 = 16;
pub const SYS_MINT: u64 = 17;
pub const SYS_DROP: u64 = 18;
pub const SYS_IRQ_BIND: u64 = 19;
pub const SYS_SIGNAL: u64 = 20;
pub const SYS_INFO: u64 = 21;
pub const SYS_CAP_INFO: u64 = 22;
pub const SYS_KILL: u64 = 23;
pub const SYS_STATUS: u64 = 24;
pub const SYS_COPY: u64 = 25;
pub const SYS_REVOKE: u64 = 26;
pub const SYS_SLEEP: u64 = 27;
pub const SYS_HEAP: u64 = 28;

/// Longest sleep: a day.
const MAX_SLEEP_TICKS: u64 = 24 * 3600 * crate::apic::TIMER_HZ;

pub const ERR_BAD_SYSCALL: i64 = -1;
/// write, note: the slot doesn't hold the console capability; map: it
/// doesn't hold a Memory capability; spawn, endpoint: not the Factory;
/// drop: nothing there.
pub const ERR_NO_CAPABILITY: i64 = -2;
pub const ERR_BAD_ADDRESS: i64 = -3;
/// The slot doesn't hold the kind of capability this call needs, or it
/// lacks the right (or doesn't cover the port).
pub const ERR_WRONG_CAPABILITY: i64 = -4;
/// Passing a capability through an endpoint capability without GRANT.
pub const ERR_NO_GRANT: i64 = -5;
/// The process that was to answer ended first.
pub const ERR_PEER_GONE: i64 = -6;
/// Woken because the process was killed (it ends on the way out).
pub const ERR_INTERRUPTED: i64 = -7;
/// reply with no call to answer.
pub const ERR_NO_REPLY: i64 = -8;
/// An argument outside what the call accepts: a port width, an interrupt
/// line the machine doesn't have, a power action that doesn't exist.
pub const ERR_BAD_ARGUMENT: i64 = -9;
/// No frame left for a page table (map), or no room for a new process or
/// endpoint (spawn, endpoint).
pub const ERR_NO_MEMORY: i64 = -10;
/// Every capability slot is taken (endpoint, mint).
pub const ERR_NO_SLOT: i64 = -11;
/// The Budget, or one it was copied from, won't allow that much (Entry 26).
pub const ERR_OVER_BUDGET: i64 = -12;

/// Longest write in one call; longer ones are cut (the return value says
/// how much was written).
const MAX_WRITE: u64 = 256;

const IA32_EFER: u32 = 0xC000_0080;
const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;
const EFER_SCE: u64 = 1 << 0;

/// RFLAGS bits cleared on entry: IF (no interrupts until the stack is
/// switched), TF (no single-stepping into the kernel), DF (the ABI wants it
/// clear), AC, NT.
const FMASK: u64 = (1 << 9) | (1 << 8) | (1 << 10) | (1 << 18) | (1 << 14);

static CALLS: AtomicU64 = AtomicU64::new(0);
static BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Serialises processes' console output, so lines don't interleave. A
/// sleeping lock: interrupts stay on while a line is printed.
pub static CONSOLE: crate::sync::Mutex<()> = crate::sync::Mutex::new(());

extern "C" {
    fn syscall_entry();
}

/// What syscall_entry saved, lowest address first (mirror of its pushes).
#[repr(C)]
pub struct SyscallFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub rip: u64,
    pub rflags: u64,
    pub rsp: u64,
}

const _: () = assert!(core::mem::size_of::<SyscallFrame>() == 18 * 8);

// 64-bit SYSRET loads SS = STAR[63:48] + 8 and CS = STAR[63:48] + 16 (both
// RPL 3), and init() sets STAR[63:48] = user data - 8. That only lands on
// the right descriptors if user code sits right after user data.
const _: () = assert!(
    crate::gdt::USER_CODE_SEL == crate::gdt::USER_DATA_SEL + 8,
    "SYSRET needs user code = user data + 8 in the GDT"
);

/// Turn SYSCALL on and point it at syscall_entry. Returns the entry address.
pub fn init() -> Result<u64, &'static [u8]> {
    // CPUID 0x8000_0001 EDX bit 11: SYSCALL/SYSRET in 64-bit mode.
    if crate::cpu::cpuid(0x8000_0001).3 & (1 << 11) == 0 {
        return Err(b"SYSCALL not supported by this CPU");
    }
    // STAR: bits 47:32 = kernel CS (SYSCALL loads CS from it and SS from it
    // + 8); bits 63:48 = the base SYSRET adds to: SS = base + 8, CS = base
    // + 16, both with RPL 3. gdt.rs orders its entries for exactly this.
    let kernel = crate::gdt::KERNEL_CODE_SEL as u64;
    let user_base = (crate::gdt::USER_DATA_SEL as u64 & !3) - 8;
    let entry = syscall_entry as *const () as u64;
    unsafe {
        crate::cpu::wrmsr(IA32_STAR, user_base << 48 | kernel << 32);
        crate::cpu::wrmsr(IA32_LSTAR, entry);
        crate::cpu::wrmsr(IA32_FMASK, FMASK);
        crate::cpu::wrmsr(IA32_EFER, crate::cpu::rdmsr(IA32_EFER) | EFER_SCE);
    }
    Ok(entry)
}

/// System calls made so far, and bytes processes have written.
pub fn counts() -> (u64, u64) {
    (CALLS.load(Ordering::Relaxed), BYTES_WRITTEN.load(Ordering::Relaxed))
}

/// Called by syscall_entry with interrupts on, on the task's kernel stack.
#[no_mangle]
extern "C" fn syscall_dispatch(f: &mut SyscallFrame) {
    crate::cpu::assert_kernel_mxcsr(b"syscall_entry");
    CALLS.fetch_add(1, Ordering::Relaxed);
    let words = [f.rsi, f.rdx, f.r10, f.r8];
    let result = match f.rax {
        SYS_EXIT => sched::exit_with(ExitStatus::Code(f.rdi)),
        SYS_YIELD => {
            sched::yield_now();
            0
        }
        SYS_WRITE => sys_write(f.rdi, f.rsi, f.rdx),
        SYS_SEND => ipc::send(f.rdi, words, f.r9, false).0,
        SYS_RECV => with_message(f, ipc::recv(f.rdi)),
        SYS_CALL => with_message(f, ipc::send(f.rdi, words, f.r9, true)),
        SYS_REPLY => ipc::reply(words, f.r9),
        SYS_PORT_IN => crate::irq::port_io(f.rdi, f.rsi, f.rdx, 0, false),
        SYS_PORT_OUT => crate::irq::port_io(f.rdi, f.rsi, f.rdx, f.r10, true),
        SYS_IRQ_WAIT => crate::irq::wait(f.rdi),
        SYS_IRQ_ACK => crate::irq::ack(f.rdi),
        SYS_POWER => sys_power(f.rdi, f.rsi),
        SYS_MAP => crate::mem::map(f.rdi, f.rsi),
        SYS_NOTE => sys_note(f.rdi, f.rsi, f.rdx),
        SYS_SPAWN => {
            let (id, slot) = crate::factory::spawn(f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9);
            f.r9 = slot;
            id
        }
        SYS_ENDPOINT => crate::factory::endpoint(f.rdi),
        SYS_MINT => crate::cap::mint(f.rdi, f.rsi, f.rdx),
        SYS_DROP => crate::cap::drop_cap(f.rdi),
        SYS_IRQ_BIND => crate::irq::bind(f.rdi, f.rsi),
        SYS_SIGNAL => ipc::signal_through(f.rdi),
        SYS_INFO => crate::info::info(f.rdi, f.rsi, f.rdx),
        SYS_CAP_INFO => {
            let (kind, a, b, c, derived) = crate::info::cap_info(f.rdi);
            if kind >= 0 {
                (f.rdi, f.rsi, f.rdx, f.r10) = (a, b, c, derived);
            }
            kind
        }
        SYS_COPY => crate::cap::copy(f.rdi),
        SYS_REVOKE => crate::cap::revoke(f.rdi),
        SYS_SLEEP => {
            sched::sleep_ticks(f.rdi.min(MAX_SLEEP_TICKS));
            0
        }
        SYS_HEAP => crate::mem::heap(f.rdi, f.rsi),
        SYS_KILL => crate::factory::kill(f.rdi),
        SYS_STATUS => {
            let (how, value) = crate::factory::status(f.rdi);
            if how >= 0 {
                f.rdi = value;
            }
            how
        }
        _ => ERR_BAD_SYSCALL,
    };
    f.rax = result as u64;
    // A kill takes effect on the way back to ring 3.
    if sched::kill_requested() {
        sched::exit_with(ExitStatus::Killed);
    }
}

/// power(slot, action): restart (0) or switch off (1), with the Power
/// capability in `slot`. Doesn't return when it works.
fn sys_power(slot: u64, action: u64) -> i64 {
    if sched::current_cap(slot) != Cap::Power {
        return ERR_WRONG_CAPABILITY;
    }
    match action {
        crate::power::RESTART => crate::power::restart(),
        crate::power::SHUT_DOWN => crate::power::shut_down(),
        _ => ERR_BAD_ARGUMENT,
    }
}

/// A received message goes back in the message registers (only on success:
/// on an error they keep what the process had).
fn with_message(f: &mut SyscallFrame, (result, msg): (i64, Message)) -> i64 {
    if result == 0 {
        f.rdi = msg.badge;
        f.rsi = msg.words[0];
        f.rdx = msg.words[1];
        f.r10 = msg.words[2];
        f.r8 = msg.words[3];
        f.r9 = msg.cap_slot;
    }
    result
}

/// write(handle, ptr, len): print `len` bytes at `ptr` to the system log
/// (serial, the screen), if capability slot `handle` holds the console
/// capability.
fn sys_write(handle: u64, ptr: u64, len: u64) -> i64 {
    let mut buf = [0u8; MAX_WRITE as usize];
    let len = match console_text(handle, ptr, len, &mut buf) {
        Ok(len) => len,
        Err(e) => return e,
    };
    print_from_process(&buf, len);
    BYTES_WRITTEN.fetch_add(len as u64, Ordering::Relaxed);
    crate::console::kick(); // show it now, not at the next tick
    len as i64
}

/// note(handle, ptr, len): the same checks as write, but the text goes to
/// the serial port only, as "[name note] text": how the console driver
/// tells test.sh what it did without putting it in the log it shows.
fn sys_note(handle: u64, ptr: u64, len: u64) -> i64 {
    let mut buf = [0u8; MAX_WRITE as usize];
    let len = match console_text(handle, ptr, len, &mut buf) {
        Ok(len) => len,
        Err(e) => return e,
    };
    let _console = CONSOLE.lock();
    crate::cpu::without_interrupts(|| {
        let mut line_start = true;
        for &b in buf.iter().take(len) {
            if line_start {
                crate::serial_note(b"[");
                crate::serial_note(sched::name(sched::current()).as_bytes());
                crate::serial_note(b" note] ");
                line_start = false;
            }
            match b {
                b'\n' => {
                    crate::serial_note(b"\n");
                    line_start = true;
                }
                0x20..=0x7E => crate::serial_note(&[b]),
                _ => crate::serial_note(b"."),
            }
        }
        if !line_start {
            crate::serial_note(b"\n");
        }
    });
    len as i64
}

/// For write and note: the console capability in `handle`, and up to
/// MAX_WRITE bytes at `ptr` copied into `buf`. Returns how many.
fn console_text(handle: u64, ptr: u64, len: u64, buf: &mut [u8; MAX_WRITE as usize]) -> Result<usize, i64> {
    if sched::current_cap(handle) != Cap::Console {
        return Err(ERR_NO_CAPABILITY);
    }
    let len = len.min(MAX_WRITE);
    if len == 0 {
        return Ok(0);
    }
    copy_from_user(ptr, &mut buf[..len as usize])?;
    Ok(len as usize)
}

/// Fill `buf` from the caller's memory at `ptr`, page by page, checking
/// each page is the caller's to read (and not memory it only borrows:
/// vmm::user_page). Through the physical map, never the user address.
pub fn copy_from_user(ptr: u64, buf: &mut [u8]) -> Result<(), i64> {
    let len = buf.len() as u64;
    if len == 0 {
        return Ok(());
    }
    if !vmm::is_user_range(ptr, len) {
        return Err(ERR_BAD_ADDRESS);
    }
    let space = unsafe { AddressSpace::from_root(sched::current_space()) };
    let mut done = 0;
    while done < len {
        let va = ptr + done;
        let Some(frame) = space.user_page(va & !0xFFF, false) else {
            return Err(ERR_BAD_ADDRESS);
        };
        let offset = va & 0xFFF;
        let n = (0x1000 - offset).min(len - done);
        unsafe {
            core::ptr::copy_nonoverlapping(
                crate::paging::phys_to_virt(frame + offset),
                buf.as_mut_ptr().add(done as usize),
                n as usize,
            );
        }
        done += n;
    }
    Ok(())
}

/// Copy `buf` into the caller's memory at `ptr`, page by page, checking
/// each page is the caller's own and writable (vmm::user_page: never a
/// page it only borrows, such as the system log, which it may only read).
/// Through the physical map, never the user address. On a bad page the
/// pages before it may already hold their part.
pub fn copy_to_user(ptr: u64, buf: &[u8]) -> Result<(), i64> {
    let len = buf.len() as u64;
    if len == 0 {
        return Ok(());
    }
    if !vmm::is_user_range(ptr, len) {
        return Err(ERR_BAD_ADDRESS);
    }
    let space = unsafe { AddressSpace::from_root(sched::current_space()) };
    let mut done = 0;
    while done < len {
        let va = ptr + done;
        let Some(frame) = space.user_page(va & !0xFFF, true) else {
            return Err(ERR_BAD_ADDRESS);
        };
        let offset = va & 0xFFF;
        let n = (0x1000 - offset).min(len - done);
        unsafe {
            core::ptr::copy_nonoverlapping(
                buf.as_ptr().add(done as usize),
                crate::paging::phys_to_virt(frame + offset),
                n as usize,
            );
        }
        done += n;
    }
    Ok(())
}

/// Print a process's text, every line prefixed with who wrote it, so a
/// process can't pass its output off as the kernel's (a test harness reads
/// this log). Unprintable bytes become '.'. The console mutex keeps
/// processes in turn; the printing itself runs with interrupts off (at most
/// MAX_WRITE bytes), so no task, the kernel's boot thread included, can be
/// switched in to print into the middle of it (Entry 17).
fn print_from_process(buf: &[u8], len: usize) {
    use crate::{serial_write_byte as byte, serial_write_str as s, serial_write_u64 as dec};
    let _console = CONSOLE.lock();
    crate::cpu::without_interrupts(|| {
        let id = sched::current();
        let mut line_start = true;
        for &b in buf.iter().take(len) {
            if line_start {
                s(b"[user ");
                dec(id as u64);
                s(b" '");
                s(sched::name(id).as_bytes());
                s(b"'] ");
                line_start = false;
            }
            match b {
                b'\n' => {
                    s(b"\n");
                    line_start = true;
                }
                0x20..=0x7E => byte(b),
                _ => byte(b'.'),
            }
        }
        if !line_start {
            s(b"\n");
        }
    });
}
