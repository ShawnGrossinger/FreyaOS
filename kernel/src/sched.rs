//! sched.rs — kernel threads and a preemptive round-robin scheduler.
//!
//! A task is a kernel thread: its own stack, its own saved registers, a
//! function to run. The timer (apic.rs) calls on_tick() 100 times a second;
//! on_tick wakes sleepers and switches to the next ready task, so tasks are
//! preempted without having to cooperate. They can also give up the CPU
//! themselves (yield_now, sleep_ticks, join, returning from their function).
//!
//! How a switch works: every task that isn't running is suspended inside a
//! call to `switch_context` (entry.asm), with its callee-saved registers
//! pushed on its own stack and its stack pointer saved in its Task entry.
//! A task preempted by the timer got there from the timer handler, so the
//! rest of its state (every register, SSE included) is in the interrupt
//! frame isr_common built on that same stack. It resumes by returning out
//! through the handler and `iretq`.
//!
//! The scheduler runs inside the timer interrupt, where allocating is
//! forbidden, so it owns no heap memory: tasks live in a fixed table, and
//! stacks are frames mapped into their own region.
//!
//! Stacks: task N gets the 64 KiB slot at TASK_STACKS_BASE + N * 64 KiB.
//! The slot's first page is never mapped (the guard page) and the next 16
//! KiB are the stack. An overflow faults on the guard page, escalates to a
//! double fault on IST1, and idt.rs reports which task overflowed.
//!
//! Task 0 is the boot thread, on the boot stack. It takes its turn like any
//! other task, and main.rs ends it with exit() once boot is done.
//!
//! The idle task is created by init(). It runs only when no other task is
//! ready: it frees the stacks of finished tasks (that can't happen in the
//! interrupt handler, and a task can't free the stack it is running on),
//! then halts until the next interrupt.
//!
//! User processes (process.rs) are tasks too: a task with its own address
//! space, which it enters ring 3 from. On every switch the scheduler loads
//! the next task's address space (CR3) and points TSS.RSP0 and the system
//! call stack at the top of its kernel stack, which is where the CPU lands
//! when that task's ring-3 code is interrupted or makes a system call.
//!
//! All scheduler state is touched with interrupts disabled. On one CPU that
//! is the whole locking story.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::cpu::without_interrupts;
use crate::pmm::{self, PAGE_SIZE};
use crate::cap::{self, Cap, Caps, NO_CAPS};
use crate::ipc::{Message, EMPTY_MESSAGE};
use crate::vmm::{AddressSpace, PageFlags};

pub const MAX_TASKS: usize = 32;

/// Task stacks: PML4 slot 386.
pub const TASK_STACKS_BASE: u64 = 0xFFFF_C100_0000_0000;
const SLOT_SIZE: u64 = 64 * 1024;
const GUARD_SIZE: u64 = PAGE_SIZE;
const STACK_PAGES: u64 = 4;
pub const STACK_SIZE: u64 = STACK_PAGES * PAGE_SIZE; // 16 KiB

pub type TaskId = usize;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Free,
    /// Slot claimed by spawn(), stack not built yet. Never scheduled.
    Creating,
    Ready,
    Running,
    /// Waiting until apic::ticks() reaches this value.
    Sleeping(u64),
    /// Waiting in join() for this task to finish, or until this tick.
    Joining(TaskId, u64),
    /// ipc.rs: offering a message on this endpoint.
    Sending(u8),
    /// ipc.rs: waiting for a message on this endpoint.
    Receiving(u8),
    /// ipc.rs: made a call that this task received; waiting for its reply.
    AwaitingReply(TaskId),
    /// sync.rs: waiting for the mutex with this key (its address).
    MutexWait(usize),
    /// irq.rs: waiting for this interrupt line.
    IrqWait(u8),
    /// Returned or called exit(); its stack is freed by reap().
    Finished,
}

/// How a task ended.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// Its function returned or it called exit; a process's exit code.
    Code(u64),
    /// A process killed by a CPU exception in ring 3 (the vector).
    Fault(u8),
    /// A process stopped by kill().
    Killed,
}

#[derive(Clone, Copy)]
pub(crate) struct Task {
    pub(crate) state: State,
    /// None only for a free slot. (An Option, not "", so an empty Task is
    /// all zero bytes and the table lives in .bss, not in the kernel image.)
    name: Option<&'static str>,
    /// Saved stack pointer while not running.
    rsp: u64,
    entry: Option<fn(u64)>,
    arg: u64,
    /// idt::irq_depth() when this task was switched out.
    irq_depth: u64,
    /// Times this task has been switched in.
    slices: u64,
    /// True while its stack pages are mapped.
    has_stack: bool,
    /// Root of its own address space; 0 for kernel tasks (kernel's).
    space: u64,
    /// What the process may do (cap.rs). Kernel tasks have none.
    pub(crate) caps: Caps,
    /// ipc.rs: the message being sent, or the one received.
    pub(crate) msg: Message,
    /// ipc.rs: the message offered is a call (wants a reply).
    pub(crate) calling: bool,
    /// ipc.rs: who to answer: the caller of the last call received.
    pub(crate) reply_to: Option<TaskId>,
    /// Result handed to a task woken from a blocking system call.
    pub(crate) result: i64,
    /// When it started waiting: the oldest waiter goes first.
    wait_seq: u64,
    /// Set when it finished.
    status: ExitStatus,
    /// kill() asked for it to stop; it does when next returning to ring 3.
    kill: bool,
}

const EMPTY: Task = Task {
    state: State::Free,
    name: None,
    rsp: 0,
    entry: None,
    arg: 0,
    irq_depth: 0,
    slices: 0,
    has_stack: false,
    space: 0,
    caps: NO_CAPS,
    msg: EMPTY_MESSAGE,
    calling: false,
    reply_to: None,
    result: 0,
    wait_seq: 0,
    status: ExitStatus::Code(0),
    kill: false,
};

static mut TASKS: [Task; MAX_TASKS] = [EMPTY; MAX_TASKS];
static mut CURRENT: TaskId = 0;
static ENABLED: AtomicBool = AtomicBool::new(false);
static IDLE: AtomicU64 = AtomicU64::new(0); // the idle task's id, once init() ran
static SWITCHES: AtomicU64 = AtomicU64::new(0);
static WAIT_SEQ: AtomicU64 = AtomicU64::new(0);

extern "C" {
    /// entry.asm: push callee-saved registers, store RSP to *save_rsp,
    /// load RSP from load_rsp, pop, ret.
    fn switch_context(save_rsp: *mut u64, load_rsp: u64);
}

/// The task table. Only touched with interrupts disabled (or, for a task's
/// own fields, by that task), and never held across a switch.
fn table() -> &'static mut [Task; MAX_TASKS] {
    unsafe { &mut *(&raw mut TASKS) }
}

fn current_id() -> TaskId {
    unsafe { *(&raw const CURRENT) }
}

/// Task `id`'s entry, for ipc.rs, sync.rs and irq.rs. Interrupts must be
/// off while it is used, and it must not be held across a switch.
pub(crate) fn task(id: TaskId) -> &'static mut Task {
    &mut table()[id % MAX_TASKS]
}

/// The task that has waited longest among those matching `pred`.
pub(crate) fn oldest(pred: impl Fn(&Task) -> bool) -> Option<TaskId> {
    (0..MAX_TASKS).filter(|&i| pred(&table()[i])).min_by_key(|&i| table()[i].wait_seq)
}

/// Put the current task in waiting state `state` and run something else;
/// returns once another task (or an interrupt) has made it Ready again.
///
/// # Safety
/// Interrupts off. `state` must be one that something will end.
pub(crate) unsafe fn block(state: State) {
    let me = current_id();
    let t = &mut table()[me];
    t.state = state;
    t.wait_seq = WAIT_SEQ.fetch_add(1, Ordering::Relaxed);
    schedule();
}

fn slot_base(id: TaskId) -> u64 {
    TASK_STACKS_BASE + id as u64 * SLOT_SIZE
}

fn stack_bottom(id: TaskId) -> u64 {
    slot_base(id) + GUARD_SIZE
}

/// Where the CPU should land when task `id`'s ring-3 code is interrupted.
fn kernel_stack_top(id: TaskId) -> u64 {
    if id == 0 {
        crate::paging::KERNEL_STACK_TOP // the boot thread is on the boot stack
    } else {
        stack_bottom(id) + STACK_SIZE
    }
}

// ===========================================================================
// Setup and task lifetime
// ===========================================================================

/// Make the running boot thread task 0, create the idle task, and start
/// scheduling.
pub fn init() -> Result<(), SpawnError> {
    without_interrupts(|| {
        table()[0] = Task { state: State::Running, name: Some("boot"), slices: 1, ..EMPTY };
        unsafe { *(&raw mut CURRENT) = 0 };
    });
    let idle = spawn("idle", idle_task, 0)?;
    IDLE.store(idle as u64, Ordering::Relaxed);
    ENABLED.store(true, Ordering::Release);
    Ok(())
}

fn idle_id() -> TaskId {
    IDLE.load(Ordering::Relaxed) as TaskId
}

/// Runs only when no other task is ready.
fn idle_task(_: u64) {
    loop {
        reap();
        // `sti; hlt` as a pair can't miss a wakeup: an interrupt is only
        // taken after the instruction following sti, which is the hlt.
        unsafe { asm!("sti", "hlt", options(nomem, nostack)) };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    TableFull,
    OutOfMemory,
}

/// Create a task that runs `entry(arg)` on its own stack. It is ready at
/// once and runs when the scheduler next picks it. Task context only: it
/// allocates frames and maps pages, which interrupt handlers must not do.
pub fn spawn(name: &'static str, entry: fn(u64), arg: u64) -> Result<TaskId, SpawnError> {
    spawn_in(name, entry, arg, 0, NO_CAPS)
}

/// spawn(), for a task that runs in address space `space` (0 = the
/// kernel's) with capabilities `caps`. The task takes over the address
/// space: reap() destroys it when the task is gone. process.rs uses this.
pub fn spawn_in(name: &'static str, entry: fn(u64), arg: u64, space: u64, caps: Caps) -> Result<TaskId, SpawnError> {
    let id = without_interrupts(|| {
        let t = table();
        let id = (1..MAX_TASKS).find(|&i| t[i].state == State::Free)?;
        t[id] = Task { state: State::Creating, name: Some(name), ..EMPTY };
        Some(id)
    })
    .ok_or(SpawnError::TableFull)?;

    // Map the stack. Interrupts can stay on: pmm and vmm protect themselves.
    let mut kernel = AddressSpace::kernel();
    for page in 0..STACK_PAGES {
        let va = stack_bottom(id) + page * PAGE_SIZE;
        let mapped = unsafe {
            match pmm::alloc_frame() {
                Some(frame) => {
                    let ok = kernel.map(va, frame as u64, PageFlags::KERNEL_DATA).is_ok();
                    if !ok {
                        pmm::free_frame(frame);
                    }
                    ok
                }
                None => false,
            }
        };
        if !mapped {
            unsafe { free_stack_pages(id, page) };
            without_interrupts(|| table()[id] = EMPTY);
            return Err(SpawnError::OutOfMemory);
        }
    }

    // Build the first stack frame so the first switch_context into this
    // task "returns" into task_entry. From the 16-byte-aligned top down:
    //   top -  8        0           task_entry's (never used) return address
    //   top - 16        task_entry  what switch_context's `ret` pops
    //   top - 24 .. 64  0 x 6       rbp rbx r12 r13 r14 r15
    // task_entry is entered with RSP = top - 8, as if it had been called.
    let top = stack_bottom(id) + STACK_SIZE;
    unsafe {
        let words = top as *mut u64;
        words.sub(1).write(0);
        words.sub(2).write(task_entry as *const () as u64);
        for i in 3..=8 {
            words.sub(i).write(0);
        }
    }

    without_interrupts(|| {
        cap::retain_all(&caps);
        table()[id] = Task {
            state: State::Ready,
            name: Some(name),
            rsp: top - 8 * 8,
            entry: Some(entry),
            arg,
            has_stack: true,
            space,
            caps,
            ..EMPTY
        };
    });
    Ok(id)
}

/// Unmap and free the first `pages` stack pages of task `id`.
unsafe fn free_stack_pages(id: TaskId, pages: u64) {
    let mut kernel = AddressSpace::kernel();
    for page in 0..pages {
        if let Ok(frame) = kernel.unmap(stack_bottom(id) + page * PAGE_SIZE) {
            pmm::free_frame(frame as usize);
        }
    }
}

/// Every new task starts here: switch_context `ret`s into it.
extern "C" fn task_entry() -> ! {
    let (entry, arg) = without_interrupts(|| {
        let task = &table()[current_id()];
        (task.entry, task.arg)
    });
    // We came from inside schedule(), which runs with interrupts off.
    // Tasks run with them on.
    unsafe { asm!("sti", options(nomem, nostack)) };
    if let Some(f) = entry {
        f(arg);
    }
    exit();
}

/// End the current task and wake anything joining it. reap() frees its
/// stack later. The boot thread (task 0) can exit too: its stack is the
/// boot stack, which is never freed.
pub fn exit() -> ! {
    exit_with(ExitStatus::Code(0))
}

/// exit(), recording how the task ended for join() to report.
pub fn exit_with(status: ExitStatus) -> ! {
    unsafe { asm!("cli", options(nomem, nostack)) };
    let me = current_id();
    let t = table();
    t[me].state = State::Finished;
    t[me].status = status;
    for task in t.iter_mut() {
        if matches!(task.state, State::Joining(id, _) if id == me) {
            task.state = State::Ready;
        }
    }
    crate::ipc::on_exit(me);
    unsafe { schedule() };
    // schedule() never picks a finished task, so this is unreachable.
    crate::halt();
}

/// Free the stacks (and, for a process, the address space) of finished
/// tasks and return their slots. Task context only; the idle task calls it
/// whenever it runs. Each task is reaped with interrupts off (a few
/// microseconds), so nobody ever sees one half freed. The address space is
/// never the active one: the finished task isn't running, and its address
/// space was nobody else's. Returns how many it reaped.
pub fn reap() -> usize {
    let mut reaped = 0;
    for id in 1..MAX_TASKS {
        reaped += without_interrupts(|| {
            let task = table()[id];
            if id == current_id() || task.state != State::Finished || !task.has_stack {
                return 0;
            }
            unsafe {
                free_stack_pages(id, STACK_PAGES);
                if task.space != 0 {
                    AddressSpace::from_root(task.space).destroy();
                }
            }
            cap::release_all(&task.caps);
            table()[id] = EMPTY;
            1
        });
    }
    reaped
}

// ===========================================================================
// Scheduling
// ===========================================================================

/// Pick the next task and switch to it. Interrupts must be off.
///
/// Round robin, starting after the current task: the first Ready task, or
/// the current one again if it is still Running and nobody else is ready.
/// The idle task is never picked by the scan, only when nothing else can
/// run.
unsafe fn schedule() {
    crate::cpu::assert_interrupts_off(b"sched schedule");
    let cur = current_id();
    let idle = idle_id();
    let next = {
        let t = table();
        let mut next = idle;
        for k in 1..=MAX_TASKS {
            let i = (cur + k) % MAX_TASKS;
            if i != idle
                && (t[i].state == State::Ready || (i == cur && t[i].state == State::Running))
            {
                next = i;
                break;
            }
        }
        if next == cur {
            return; // keep running
        }
        if t[cur].state == State::Running {
            t[cur].state = State::Ready;
        }
        t[next].state = State::Running;
        t[next].slices += 1;

        // A task switched out inside the timer handler resumes inside it;
        // a new or yielding task resumes outside any handler.
        t[cur].irq_depth = crate::idt::irq_depth();
        crate::idt::set_irq_depth(t[next].irq_depth);

        // Its address space, and the stack its ring-3 code lands on.
        let space = if t[next].space == 0 { crate::paging::kernel_root() } else { t[next].space };
        if crate::vmm::active_root() != space {
            crate::vmm::switch_to(space);
        }
        crate::gdt::set_kernel_stack(kernel_stack_top(next));
        next
    };

    SWITCHES.fetch_add(1, Ordering::Relaxed);
    *(&raw mut CURRENT) = next;
    let tasks = &raw mut TASKS;
    switch_context(&raw mut (*tasks)[cur].rsp, (*tasks)[next].rsp);
    // Back: a later schedule() has picked this task again.
}

/// Timer tick (interrupt context, interrupts off, EOI already sent).
pub fn on_tick() {
    if !ENABLED.load(Ordering::Acquire) {
        return;
    }
    let now = crate::apic::ticks();
    for task in table().iter_mut() {
        if let State::Sleeping(until) | State::Joining(_, until) = task.state {
            if now >= until {
                task.state = State::Ready;
            }
        }
    }
    unsafe { schedule() };
}

/// Give up the rest of this time slice.
pub fn yield_now() {
    without_interrupts(|| unsafe { schedule() });
}

/// Sleep for at least `ticks` timer ticks. (Not for the idle task, which
/// has to stay runnable; there it just yields.)
pub fn sleep_ticks(ticks: u64) {
    without_interrupts(|| {
        let cur = current_id();
        if cur != idle_id() {
            table()[cur].state = State::Sleeping(crate::apic::ticks() + ticks);
        }
        unsafe { schedule() };
    });
}

/// Block until task `id` has finished (its exit() wakes us) and say how it
/// ended. None if there is no such task (or it was already reaped). Ids
/// are reused once a finished task is reaped, so join a task you spawned,
/// soon: the idle task only reaps while nothing else is ready, and this
/// task is ready from the moment its target exits.
pub fn join(id: TaskId) -> Option<ExitStatus> {
    join_timeout(id, u64::MAX)
}

/// join(), giving up after `ticks` timer ticks (None if it timed out).
pub fn join_timeout(id: TaskId, ticks: u64) -> Option<ExitStatus> {
    without_interrupts(|| {
        let cur = current_id();
        if id >= MAX_TASKS || id == cur || cur == idle_id() {
            return None;
        }
        let deadline = crate::apic::ticks().saturating_add(ticks);
        loop {
            match table()[id].state {
                State::Finished => return Some(table()[id].status),
                State::Free => return None,
                _ if crate::apic::ticks() >= deadline => return None,
                _ => {}
            }
            table()[cur].state = State::Joining(id, deadline);
            unsafe { schedule() };
        }
    })
}

/// Ask process `id` to stop. It does the next time it would return to ring
/// 3 (from an interrupt or a system call). Kernel tasks can't be killed:
/// they might be holding something. Returns false if `id` isn't a live
/// process.
pub fn kill(id: TaskId) -> bool {
    without_interrupts(|| {
        let task = &mut table()[id % MAX_TASKS];
        let alive = !matches!(task.state, State::Free | State::Finished | State::Creating);
        if id < MAX_TASKS && alive && task.space != 0 {
            task.kill = true;
            crate::ipc::interrupt_wait(id); // if it's blocked, wake it
            true
        } else {
            false
        }
    })
}

/// True if the current task has been asked to stop.
pub fn kill_requested() -> bool {
    without_interrupts(|| table()[current_id()].kill)
}

/// The current task's capability in slot `handle`, if it has one.
pub fn current_cap(handle: u64) -> Cap {
    if handle >= cap::MAX_CAPS as u64 {
        return Cap::None;
    }
    without_interrupts(|| {
        let caps = &table()[current_id()].caps;
        caps.get(handle as usize).copied().unwrap_or(Cap::None)
    })
}

/// The mutex with key `key` (its address) and owner word `owner`: take it,
/// or wait in line for it. Ownership is handed straight to the oldest
/// waiter on release, so nobody can barge in and nobody waits forever.
pub(crate) fn mutex_acquire(key: usize, owner: &AtomicU64, contended: &AtomicU64) {
    without_interrupts(|| unsafe {
        if crate::idt::irq_depth() > 0 {
            crate::cpu::fatal(b"mutex locked inside a hardware interrupt handler, key ", key as u64);
        }
        let me = current_id() as u64;
        match owner.load(Ordering::Relaxed) {
            FREE => owner.store(me, Ordering::Relaxed),
            o if o == me => crate::cpu::fatal(b"mutex locked twice by the same task (deadlock), key ", key as u64),
            _ => {
                contended.fetch_add(1, Ordering::Relaxed);
                block(State::MutexWait(key));
                // Woken by mutex_release, which made us the owner.
            }
        }
    });
}

pub(crate) fn mutex_release(key: usize, owner: &AtomicU64) {
    without_interrupts(|| {
        if owner.load(Ordering::Relaxed) != current_id() as u64 {
            crate::cpu::fatal(b"mutex released by a task that doesn't hold it, key ", key as u64);
        }
        match oldest(|t| t.state == State::MutexWait(key)) {
            Some(next) => {
                owner.store(next as u64, Ordering::Relaxed);
                table()[next].state = State::Ready;
            }
            None => owner.store(FREE, Ordering::Relaxed),
        }
    });
}

/// "Nobody" in a mutex's owner word.
pub(crate) const FREE: u64 = u64::MAX;

/// irq.rs, in interrupt context: wake whoever waits for `line`.
pub(crate) fn wake_irq_waiter(line: u8) {
    for t in table().iter_mut() {
        if t.state == State::IrqWait(line) {
            t.state = State::Ready;
        }
    }
}

/// The current task's address space root (the kernel's for kernel tasks).
pub fn current_space() -> u64 {
    let space = without_interrupts(|| table()[current_id()].space);
    if space == 0 { crate::paging::kernel_root() } else { space }
}

// ===========================================================================
// Queries
// ===========================================================================

pub fn current() -> TaskId {
    current_id()
}

pub fn name(id: TaskId) -> &'static str {
    without_interrupts(|| table()[id % MAX_TASKS].name.unwrap_or(""))
}

/// True while task `id` waits in IPC (sending, receiving, or for a reply).
pub fn blocked_in_ipc(id: TaskId) -> bool {
    without_interrupts(|| matches!(task(id).state, State::Sending(_) | State::Receiving(_) | State::AwaitingReply(_)))
}

/// Times task `id` has been switched in.
pub fn slices(id: TaskId) -> u64 {
    without_interrupts(|| table()[id % MAX_TASKS].slices)
}

/// Times the current task has been switched in.
pub fn current_slices() -> u64 {
    without_interrupts(|| table()[current_id()].slices)
}

/// Task switches since the scheduler started.
pub fn switches() -> u64 {
    SWITCHES.load(Ordering::Relaxed)
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Tasks that exist (any state but Free), not counting the boot thread
/// and the idle task.
pub fn live_tasks() -> usize {
    let idle = idle_id();
    without_interrupts(|| {
        (1..MAX_TASKS).filter(|&i| i != idle && table()[i].state != State::Free).count()
    })
}

/// If `addr` is in a task's guard page, that task's id.
pub fn stack_guard_owner(addr: u64) -> Option<TaskId> {
    if addr < TASK_STACKS_BASE || addr >= TASK_STACKS_BASE + MAX_TASKS as u64 * SLOT_SIZE {
        return None;
    }
    let offset = addr - TASK_STACKS_BASE;
    (offset % SLOT_SIZE < GUARD_SIZE).then_some((offset / SLOT_SIZE) as TaskId)
}

pub fn guard_page(id: TaskId) -> u64 {
    slot_base(id)
}

// ===========================================================================
// Self-test
// ===========================================================================

use crate::selftest::{check, verdict};
use crate::{serial_write_str as s, serial_write_u64 as dec};

fn read_rsp() -> u64 {
    let rsp: u64;
    unsafe { asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags)) };
    rsp
}

// Results written by test tasks, read by the boot thread after join(). Atomics keep
// it simple; ordering doesn't matter on one CPU.
static OBSERVED_RSP: AtomicU64 = AtomicU64::new(0);
static OBSERVED_ARG: AtomicU64 = AtomicU64::new(0);

static ORDER_LOG: AtomicU64 = AtomicU64::new(0); // one hex digit per entry
static ORDER_LEN: AtomicU64 = AtomicU64::new(0);

const WORKERS: usize = 3;
const WANT_SLICES: u64 = 4;
const MAX_TEST_TICKS: u64 = 200; // 2 s: a worker gives up rather than hang
const MAX_CHUNKS: u64 = 10_000;
const CHUNK_STEPS: u64 = 10_000;

static WORK_CHUNKS: [AtomicU64; WORKERS] = [const { AtomicU64::new(0) }; WORKERS];
static WORK_A: [AtomicU64; WORKERS] = [const { AtomicU64::new(0) }; WORKERS];
static WORK_X: [AtomicU64; WORKERS] = [const { AtomicU64::new(0) }; WORKERS];
static WORK_SLICES: [AtomicU64; WORKERS] = [const { AtomicU64::new(0) }; WORKERS];

static ALLOC_ROUNDS: [AtomicU64; WORKERS] = [const { AtomicU64::new(0) }; WORKERS];
static ALLOC_ERRORS: [AtomicU64; WORKERS] = [const { AtomicU64::new(0) }; WORKERS];
static ALLOC_SLICES: [AtomicU64; WORKERS] = [const { AtomicU64::new(0) }; WORKERS];

static SLEPT: AtomicU64 = AtomicU64::new(0);

fn probe_task(arg: u64) {
    yield_now(); // once, with interrupts on, as ordinary tasks will
    OBSERVED_RSP.store(read_rsp(), Ordering::Relaxed);
    OBSERVED_ARG.store(arg, Ordering::Relaxed);
}

/// Append `tag` to the log and yield, three times. The append and the
/// switch happen with interrupts off, so a timer tick can't reorder them.
fn order_task(tag: u64) {
    for _ in 0..3 {
        without_interrupts(|| {
            let len = ORDER_LEN.load(Ordering::Relaxed);
            ORDER_LOG.fetch_or(tag << (4 * len), Ordering::Relaxed);
            ORDER_LEN.store(len + 1, Ordering::Relaxed);
            yield_now(); // nested: interrupts stay off until we're back
        });
    }
}

/// Never yields: runs work chunks until it has been switched in
/// WANT_SLICES times, which only preemption can do.
fn busy_task(index: u64) {
    let i = index as usize;
    let mut state = (index + 1, 0.0f64);
    let mut chunks = 0;
    let deadline = crate::apic::ticks() + MAX_TEST_TICKS;
    while current_slices() < WANT_SLICES && crate::apic::ticks() < deadline && chunks < MAX_CHUNKS {
        crate::apic::work(&mut state, CHUNK_STEPS);
        chunks += 1;
    }
    WORK_CHUNKS[i].store(chunks, Ordering::Relaxed);
    WORK_A[i].store(state.0, Ordering::Relaxed);
    WORK_X[i].store(state.1.to_bits(), Ordering::Relaxed);
    WORK_SLICES[i].store(current_slices(), Ordering::Relaxed);
}

/// Where alloc_task N maps its frame each round: its own page in a 2 MiB
/// stretch of the dynamic region that nothing else uses. All three share
/// one page table, so they also race to create and reclaim it.
const ALLOC_TEST_VA: u64 = crate::vmm::KERNEL_DYNAMIC_BASE + 0x20_0000_0000;

/// Hammer the frame allocator, the page tables and the heap alongside the
/// other workers. Each round takes a frame, maps it at this task's address,
/// takes a heap block, stamps both with a value unique to (task, round),
/// does some work so preemption has a chance to land in between, checks the
/// stamps (the frame through the physical map, so the check also proves the
/// mapping points at the right frame), and gives everything back. If two
/// tasks were ever handed the same frame or block, one sees the other's
/// stamp.
fn alloc_task(index: u64) {
    use core::alloc::{GlobalAlloc, Layout};
    let i = index as usize;
    let mut rounds = 0u64;
    let mut errors = 0u64;
    let mut state = (index + 7, 0.0f64);
    let deadline = crate::apic::ticks() + MAX_TEST_TICKS;
    while current_slices() < WANT_SLICES && crate::apic::ticks() < deadline && rounds < MAX_CHUNKS * 10 {
        let stamp = ((index + 1) << 56) | rounds;
        let size = 64 + (rounds % 8) as usize * 64;
        let layout = Layout::from_size_align(size, 16).unwrap_or(Layout::new::<u64>());
        unsafe {
            let Some(frame) = pmm::alloc_frame() else {
                errors += 1;
                break;
            };
            let va = ALLOC_TEST_VA + index * PAGE_SIZE;
            let mut kernel = AddressSpace::kernel();
            if kernel.map(va, frame as u64, PageFlags::KERNEL_DATA).is_err() {
                pmm::free_frame(frame);
                errors += 1;
                break;
            }
            let page = va as *mut u64;
            let check = crate::paging::phys_to_virt(frame as u64) as *const u64;
            let block = crate::heap::GLOBAL.alloc(layout) as *mut u64;
            if block.is_null() {
                let _ = kernel.unmap(va);
                pmm::free_frame(frame);
                errors += 1;
                break;
            }
            let last = size / 8 - 1;
            page.write_volatile(stamp);
            page.add(511).write_volatile(stamp);
            block.write_volatile(stamp);
            block.add(last).write_volatile(stamp);

            crate::apic::work(&mut state, 500);

            let intact = check.read_volatile() == stamp
                && check.add(511).read_volatile() == stamp
                && block.read_volatile() == stamp
                && block.add(last).read_volatile() == stamp;
            if !intact {
                errors += 1;
            }
            crate::heap::GLOBAL.dealloc(block as *mut u8, layout);
            if kernel.unmap(va) != Ok(frame as u64) {
                errors += 1;
            }
            pmm::free_frame(frame);
        }
        rounds += 1;
    }
    ALLOC_ROUNDS[i].store(rounds, Ordering::Relaxed);
    ALLOC_ERRORS[i].store(errors, Ordering::Relaxed);
    ALLOC_SLICES[i].store(current_slices(), Ordering::Relaxed);
}

fn sleep_task(ticks: u64) {
    let before = crate::apic::ticks();
    sleep_ticks(ticks);
    SLEPT.store(crate::apic::ticks() - before, Ordering::Relaxed);
}

/// Frames in use by the heap, so a test can tell "stack frames leaked" from
/// "the heap grew" (it never shrinks).
pub fn free_frames_and_heap() -> u64 {
    pmm::free_frames() + crate::heap::stats().mapped / PAGE_SIZE
}

/// Spawn, or record a failed check and return None.
fn spawn_for_test(name: &'static str, entry: fn(u64), arg: u64) -> Option<TaskId> {
    match spawn(name, entry, arg) {
        Ok(id) => Some(id),
        Err(_) => {
            check(b"sched", b"spawn a test task", false);
            None
        }
    }
}

/// Runs on the boot thread with interrupts on and the timer ticking. Every
/// test waits with join(), so the boot thread is blocked while tasks run.
pub fn selftest() -> bool {
    let mut ok = true;
    let kernel = AddressSpace::kernel();

    // 1. A task runs on its own stack, inside its slot, above an unmapped
    //    guard page, and gets its argument.
    let Some(id) = spawn_for_test("probe", probe_task, 0xF4E7A) else { return false };
    let guard = guard_page(id);
    let guard_unmapped = kernel.translate(guard).is_none();
    let stack_mapped = (0..STACK_PAGES).all(|p| kernel.translate(stack_bottom(id) + p * PAGE_SIZE).is_some());
    join(id);
    let rsp = OBSERVED_RSP.load(Ordering::Relaxed);
    ok &= check(
        b"sched",
        b"a task runs on its own 16 KiB stack above an unmapped guard page",
        guard_unmapped
            && stack_mapped
            && (stack_bottom(id)..stack_bottom(id) + STACK_SIZE).contains(&rsp)
            && OBSERVED_ARG.load(Ordering::Relaxed) == 0xF4E7A,
    );
    reap();

    // Baseline for the leak check at the end. The page-directory-pointer
    // table for the stack region already exists (the idle task's stack
    // built it) and stays: vmm never frees a PDPT.
    let baseline = free_frames_and_heap();

    // 2. Two tasks that yield to each other alternate: A B A B A B.
    //    Only if no tick lands in the middle: a tick that arrives while
    //    one task has interrupts off is taken at its next sti, right after
    //    it resumes and before it logs, and the round robin then hands the
    //    CPU straight back to the other one (preemption working, not a
    //    yield bug). So start just after a tick, check none arrived during
    //    the exchange (a few dozen microseconds), and retry if one did.
    let mut alternated = false;
    let mut attempts = 0;
    while attempts < 5 {
        attempts += 1;
        ORDER_LOG.store(0, Ordering::Relaxed);
        ORDER_LEN.store(0, Ordering::Relaxed);
        sleep_ticks(1);
        let tick = crate::apic::ticks();
        let pair = without_interrupts(|| {
            (spawn_for_test("order-a", order_task, 0xA), spawn_for_test("order-b", order_task, 0xB))
        });
        let (Some(a), Some(b)) = pair else { return false };
        join(a);
        join(b);
        if crate::apic::ticks() == tick {
            alternated = ORDER_LEN.load(Ordering::Relaxed) == 6 && ORDER_LOG.load(Ordering::Relaxed) == 0xBABABA;
            break;
        }
    }
    s(b"[sched] self-test two tasks yielding to each other alternate A B A B A B (attempt ");
    dec(attempts);
    s(b")");
    ok &= verdict(alternated);

    // 3. Preemption: busy tasks that never yield still take turns, and
    //    their work comes out exactly as if they had run alone.
    let idle_before = without_interrupts(|| table()[idle_id()].slices);
    let mut ids = [0; WORKERS];
    for (w, slot) in ids.iter_mut().enumerate() {
        let Some(id) = spawn_for_test("busy", busy_task, w as u64) else { return false };
        *slot = id;
    }
    for &id in &ids {
        join(id);
    }
    let mut same = true;
    let mut min_slices = u64::MAX;
    let mut steps = 0;
    for w in 0..WORKERS {
        let chunks = WORK_CHUNKS[w].load(Ordering::Relaxed);
        let mut quiet = (w as u64 + 1, 0.0f64);
        // Each chunk with interrupts off, so none of the reference work is
        // interrupted, but ticks still get through between chunks.
        for _ in 0..chunks {
            without_interrupts(|| crate::apic::work(&mut quiet, CHUNK_STEPS));
        }
        same &= quiet.0 == WORK_A[w].load(Ordering::Relaxed) && quiet.1.to_bits() == WORK_X[w].load(Ordering::Relaxed);
        min_slices = min_slices.min(WORK_SLICES[w].load(Ordering::Relaxed));
        steps += chunks * CHUNK_STEPS;
    }
    s(b"[sched] self-test 3 tasks that never yield were each preempted ");
    dec(min_slices.saturating_sub(1));
    s(b" times (");
    dec(steps);
    s(b" steps); their work matches the same work with interrupts off");
    ok &= verdict(min_slices >= WANT_SLICES && same);

    // The busy tasks were always ready, so the idle task had no business
    // running at any point during that test.
    let idle_ran = without_interrupts(|| table()[idle_id()].slices) - idle_before;
    s(b"[sched] self-test the idle task ran ");
    dec(idle_ran);
    s(b" times while other tasks were ready (expect 0)");
    ok &= verdict(idle_ran == 0);

    // 4. The frame allocator and heap shared by preempted tasks.
    let mut ids = [0; WORKERS];
    for (w, slot) in ids.iter_mut().enumerate() {
        let Some(id) = spawn_for_test("alloc", alloc_task, w as u64) else { return false };
        *slot = id;
    }
    for &id in &ids {
        join(id);
    }
    let mut rounds = 0;
    let mut errors = 0;
    let mut min_slices = u64::MAX;
    for w in 0..WORKERS {
        rounds += ALLOC_ROUNDS[w].load(Ordering::Relaxed);
        errors += ALLOC_ERRORS[w].load(Ordering::Relaxed);
        min_slices = min_slices.min(ALLOC_SLICES[w].load(Ordering::Relaxed));
    }
    s(b"[sched] self-test 3 preempted tasks shared the frame allocator, page tables and heap for ");
    dec(rounds);
    s(b" rounds; ");
    dec(errors);
    s(b" errors");
    ok &= verdict(min_slices >= WANT_SLICES && errors == 0 && rounds > 0);

    // 5. Sleep: woken by the timer after the requested ticks, not before.
    let Some(id) = spawn_for_test("sleep", sleep_task, 5) else { return false };
    join(id);
    let slept = SLEPT.load(Ordering::Relaxed);
    s(b"[sched] self-test a task asked to sleep 5 ticks woke after ");
    dec(slept);
    ok &= verdict((5..=7).contains(&slept));

    // 6. Clean-up: every finished task's stack is freed (whether the idle
    //    task or this call got to it first), and the boot thread is back
    //    outside any interrupt handler.
    reap();
    let leaked = baseline as i64 - free_frames_and_heap() as i64;
    s(b"[sched] self-test finished tasks reaped: ");
    dec(live_tasks() as u64);
    s(b" left, ");
    dec(leaked.unsigned_abs());
    s(if leaked < 0 { b" frames gained" } else { b" frames leaked" });
    s(b", irq depth ");
    dec(crate::idt::irq_depth());
    ok &= verdict(live_tasks() == 0 && leaked == 0 && crate::idt::irq_depth() == 0);

    ok
}

/// Demo: a task that recurses until it runs into its guard page.
pub fn demo_task_stack_overflow() {
    match spawn("overflow-demo", |_| { crate::demo_stack_overflow(0); }, 0) {
        Ok(id) => {
            join(id);
        }
        Err(_) => s(b"[demo] could not spawn the overflow task\n"),
    }
}
