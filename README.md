# FreyaOS

FreyaOS is an operating system for x86-64 written from scratch, built around
**zero ambient authority**: a program can do only what it has been handed a
capability for. There is no root user, no global namespace to reach into, and
no way to touch hardware without a capability that names it.

It boots from BIOS with its own two-stage bootloader (NASM) into a `no_std`
Rust **microkernel**. The kernel schedules tasks, manages memory and address
spaces, routes interrupts and passes messages; drivers run as ordinary user
processes holding exactly the capabilities they need.

It runs in QEMU and on real hardware: a ThinkPad T430 (Intel i7-3520M, Ivy
Bridge, 16 GB), booted from a USB stick.

The design it is built toward, and the case that capability systems have
been practical on ordinary hardware all along, is in the manifesto (see
[Documentation](#documentation)).

## Status

| Area | State |
|---|---|
| Boot | Stage 1 MBR (with a partition table) loads stage 2, which loads the kernel (up to 1 MiB), collects the E820 memory map, sets a graphics mode and enters long mode |
| Screen | A VESA graphics mode with a linear framebuffer, as large as the display shows (read from its EDID) up to 1920x1280; text in the VGA ROM's own 8x16 font, drawn through a write-combining mapping. 80x25 text mode on request. Drawn by a console driver process from the end of boot, with 400 rows of the log to scroll back through |
| Memory | Kernel-owned page tables, NX, a guard page under the kernel stack, a higher-half map of all RAM, a frame allocator, a page-mapping API, a kernel heap; heaps for programs, grown through a Budget capability, with Box, Vec and String in Rust programs. Every frame has a state, a kind and an owner, kept in a frame table beside the allocator's bitmap |
| Interrupts | A gate for all 256 vectors, the local APIC timer at 100 Hz, the I/O APIC routed from ACPI tables; interrupts nobody set up are survived and reported |
| Tasks | Kernel threads, preemptive round-robin scheduling, sleeping, joining, a mutex |
| User mode | Ring 3 processes in their own address spaces, written in assembly or Rust; `SYSCALL`, SMEP and SMAP where the CPU has them; a fault ends only the process |
| Capabilities and IPC | 32 capability slots per process; synchronous call/reply IPC that can pass capabilities (with GRANT) and badges; signals, from the kernel or from a process holding a badged capability; memory lent through Memory capabilities; narrower copies (mint) and letting go (drop); every copy remembers its original, so whatever was lent can be taken back (revoke), from every process it reached; memory budgets that nest the same way |
| Init | The only process the kernel starts: it holds the drivers' capabilities and a Factory capability, starts each driver and the shell with exactly what it needs, and starts one again if it ends |
| Drivers | I/O-port, interrupt-line and memory capabilities, and interrupts delivered as signals; a console driver, a PS/2 driver (keyboard and controller) and a mouse driver, all in Rust, running as user processes, started by init |
| Shell | A first shell in Rust: line editing with history, built-in commands (help, echo, ps with each task's memory, caps, uptime, mem by kind, audit, restart, off), programs started and stopped (run, jobs, kill), and what it lent them taken back (revoke), holding nine capabilities and nothing else |
| Power | Restart and switch-off through ACPI, behind a Power capability (Ctrl+Alt+Del, Ctrl+Alt+End, or the shell's restart and off) |
| Trust | Every `unsafe` in the kernel carries the reason it is sound, and the kernel doesn't build without them; state shared between tasks and interrupt handlers is reached only through a lock, and taking one twice halts the kernel, naming the line; the compiler is pinned, and two builds of the same source give the same image, byte for byte |
| Audit | A hash chain (BLAKE3) of what happened: processes starting (with a hash of the program) and ending, faults, every capability made, taken away and revoked, and each second's messages and task switches as counts and a hash; sealed in segments, verified by the kernel, by the shell (`audit`) and by a tool on the build machine. Kept in memory for one boot; each segment's seal extended into a TPM 1.2's PCR 15: QEMU's emulated TPM (`make run-tpm`), and the T430's own, reading and extending PCR 15 and nothing else (run there in Entry 35) |
| Not yet | UEFI boot, disk, network, file system, memory that grows as a program runs, multiple CPUs |

Every boot runs **107 self-tests** and shows the result on screen. `make test`
checks the disk image and every `unsafe`'s reason, runs the kernel's TPM
protocol against model chips stricter than QEMU's, and boots **48
configurations** headless, four of them with an emulated TPM, checking each
one (51 cases in all).

## Design in brief

- **Microkernel.** Only scheduling, memory, address spaces, interrupts, IPC
  and capabilities live in the kernel. Drivers and services are processes.
- **Capabilities.** Each process has 32 slots. Kinds: `Console` (write to
  the log), `Endpoint` (rights SEND, RECV, GRANT, plus a badge), `IoPorts` (a
  port range), `Irq` (an interrupt line), `Power`, `Memory` (physical
  pages a process may map: writable or not, write-combining or uncached),
  `Factory` (may start the built-in programs its rights name, and make
  endpoints if they say so), `Info` (may read uptime, memory and every
  task's state), and `Process` (names one process; may ask how it is doing
  and stop it). spawn hands back a `Process` capability on request, so only
  whoever started a process can stop it. Authority only
  moves by being copied into a process its holder starts, or passed in an IPC
  message through an endpoint with GRANT. An endpoint capability can be copied
  with fewer rights and a badge, and a Factory with fewer rights (mint); any
  capability can be copied as it is (copy) and let go of (drop).
- **What is lent can be taken back.** Every capability remembers the one it
  was copied from: by copy, mint, spawn's slot list or a message. `revoke`
  removes everything derived from a capability, however many hands it went
  through, in every process at once; the holder keeps its own. Dropping a
  capability hands its copies up to its own original, so they can still be
  revoked. A Memory capability revoked or dropped takes its mapping with it,
  and a capability revoked while its message waits, or while its process is
  being made, is never delivered. A process that ends holds nothing.
- **Memory a program can use is a capability too.** A Budget capability
  says how many pages its holder may have: `heap` grows a heap of fresh,
  zeroed pages at a place fixed for its slot, as far as the budget allows,
  and the budget counts every budget copied from it. Init gets one for all
  the memory the kernel can spare, lends the shell 4 MiB, and the shell
  lends a program 1 MiB of that: a program can't take more than its lender
  has left. Revoking a Budget frees its heap; the program's next touch of
  it is a page fault. Rust programs get Box, Vec and String from it
  (`freya::heap`).
- **Every frame has an identity** (the manifesto's Section III). A frame
  table beside the allocator's bitmap gives every frame of memory its state
  (raw, sanitized, typed, or reserved), its kind (page table, kernel stack,
  kernel heap, a process's code, data, stack or heap, ...) and its owner
  (the kernel, or a task named by its slot and generation). Inside the
  kernel a frame is held as a `Frame<Raw>`, `Frame<Sanitized>` or
  `Frame<Typed>`, so the compiler keeps the order: only a zeroed frame can
  be given an owner. A frame comes back from a page table only through a
  check against the table, so a double free, a free by the wrong owner or a
  free of the kernel's own image halts the kernel instead of corrupting
  memory. `ps` shows what each task owns; `mem` what all memory is doing.
- **Unsafe code is where trust is required** (the manifesto's Section 6.4).
  Every `unsafe` block in the kernel has a `// SAFETY: [category] why`
  comment, and every `unsafe fn` a `# Safety` section; no unsafe operation
  hides in an `unsafe fn`'s body without its own block
  (`#![deny(unsafe_op_in_unsafe_fn)]`). `build/unsafe_check.rs` fails the
  build if one is missing, and writes the list: `make unsafe` prints it,
  counted by category (port I/O, device registers, physical memory, page
  tables, the locks' own insides, context switches, ...).
- **Shared state is behind a lock** (Entry 31). What tasks and interrupt
  handlers share (the task table, the endpoints, the capabilities'
  derivation, the frame allocator, the heap, the screen) is reached only
  through an `IrqLock`, whose guard is the one way to the value: interrupts
  are off while it is held, and the compiler keeps every reference inside
  the guard's life. Taking a lock that is already held halts the kernel,
  naming the lock and the line, where before the same mistake would have
  been two references to one piece of memory. Only tables the CPU reads
  itself (GDT, IDT, TSS, the boot page tables) are still `static mut`.
- **What happened is a chain** (Entry 32; the manifesto's Section V). The
  kernel records an entry for each process started (with BLAKE3 of the
  program it runs) or ended, each fault, each capability put in a slot or
  taken out of one, each revoke and kill, and once a second the messages
  and task switches as counts and one hash. Each entry's hash covers the
  one before, so changing, adding, removing or reordering any entry breaks
  every link after it. Every 256 entries the segment is sealed: its hash
  goes into the next segment's header and out to the serial log at once.
  BLAKE3 is written here (`kernel/src/blake3.rs`, checked against the
  official test vectors), and the same code verifies the chain in the
  kernel, in the shell (`audit`) and on the build machine
  (`make verify-audit LOG=...`). For now it lives in memory (the last 2048
  entries) and the kernel that keeps it is the one it audits.
- **The chain is anchored in a TPM** (Entry 33). Each segment's seal is
  extended into PCR 15 of a TPM 1.2, through its TIS registers
  (`kernel/src/tpm.rs`), and the PCR's new value is recorded in the chain.
  A PCR can only be extended, and goes back to zeros only when the machine
  resets: so a kernel that rewrote its whole chain to hide a change (every
  later hash, header, seal and anchor made to match, which the chain alone
  can't tell from the real one) still can't make PCR 15 agree with it, and
  the shell's `audit` says so. Only extend and read are used, both undone
  by a reset: nothing takes ownership or writes the TPM's memory. This
  build drives two TPMs, the ones it has been tried on: QEMU's emulated
  TPM 1.2 (swtpm) and, from Entry 34, the T430's own, an STMicroelectronics
  TPM 1.2 (vendor 0x104a, device 0x0000). On the T430 (Entry 35) both of
  the boot's seals were extended and checked, with four commands sent in
  all, and after a reboot Linux read PCR 15 as zeros again. Any other TPM
  is found, named and left alone. The T430's may be sent TPM_PCRRead and TPM_Extend of PCR 15,
  nothing else, not even TPM_Startup; a self-test checks both rules on
  every boot. The first error, or answer that doesn't check out, stops
  the kernel sending the TPM anything more until the next boot. The
  protocol (`kernel/src/tis.rs`) is also run on the build machine against
  model TPMs stricter than QEMU's: a byte at a time, status bits set late,
  "busy" at first, and broken ones (`make tis-check`). A PCR read through
  the kernel can still be misreported by it: proving the PCR to someone
  else needs a quote, which needs an owned TPM, or Phase C's External
  Witness.
- **Init decides, the kernel enforces.** At the end of boot the kernel starts
  one process, init, with every capability the drivers will need. Init mints
  narrower copies, starts the console, PS/2 and mouse drivers and the shell
  with exactly the slots each needs, and is signalled when one ends, so it can
  start it again (three times at most; then it lets go of what only that one
  used).
- **Drivers own their devices.** The console driver draws the screen through
  three Memory capabilities: the framebuffer (or VGA text memory), the
  kernel's log, and the kernel's status rows, the last two read-only, plus a
  read-only description of the screen and its font. The kernel wakes it with
  signals on its endpoint; the PS/2 driver asks it to scroll with an IPC
  call. The kernel draws the screen until a process maps it, and takes it
  back for a crash or when that process ends.
- **Servers never wait for their clients.** The PS/2 driver answers every
  call at once and signals the mouse driver and the shell (through
  capabilities whose badge is the signal bit) when they have something to
  read; each then asks.
- **System calls** (`SYSCALL`, number in `rax`):

  | # | Call | Needs |
  |---|---|---|
  | 0 | exit(code) | nothing |
  | 1 | yield | nothing |
  | 2 | write(cap, buf, len) | Console |
  | 3-6 | send, recv, call, reply | Endpoint with the right rights |
  | 7-8 | port_in, port_out | IoPorts covering the port |
  | 9-10 | irq_wait, irq_ack | Irq |
  | 11 | power(cap, action) | Power |
  | 12 | (retired: was console_scroll) | |
  | 13 | map(cap, address) | Memory |
  | 14 | note(cap, buf, len): a line on serial only | Console |
  | 15 | spawn(cap, program, arg, slots, exit_ep, flags): start a built-in program, and on request get a Process capability for it | Factory covering the program |
  | 16 | endpoint(cap): a new endpoint | Factory with the endpoints right |
  | 17 | mint(cap, rights, badge): a narrower copy | Endpoint or Factory |
  | 18 | drop(slot): let go of a capability | anything |
  | 19 | irq_bind(irq, endpoint): the line's interrupts become signals there | Irq, Endpoint with RECV |
  | 20 | signal(endpoint): set the capability's badge as signal bits there, without waiting | Endpoint with SEND and a badge |
  | 21 | info(cap, buf, len): uptime, memory and every task, into the caller's buffer | Info |
  | 22 | cap_info(slot): what one of the caller's own slots holds | nothing |
  | 23 | kill(cap): stop a process | Process |
  | 24 | status(cap): running, or how it ended | Process |
  | 25 | copy(slot): a copy, derived from it, to lend | anything |
  | 26 | revoke(slot): remove everything derived from it, in every process | anything |
  | 27 | sleep(ticks): wait that many timer ticks | nothing |
  | 28 | heap(slot, pages): make that Budget's heap so long; returns where it is | Budget |

## Repository layout

```
bootloader/BIOS/   stage1.asm (MBR), stage2.asm (loader, E820, VESA mode, long mode)
bootloader/UEFI/   placeholder for a later UEFI loader
kernel/            Rust kernel: entry.asm, kernel.ld, src/*.rs
user/              user programs in NASM (test programs)
user/rust/         user programs in Rust (init; the console, PS/2 and mouse drivers; the shell; spin, crash, ticker, relay and hog), and their library with its allocator
build/             Makefile, test.sh, write-usb.sh, reproduce.sh, unsafe_check.rs, audit_verify.rs, tis_check.rs
docs/              the manifesto (the design) and the development journal
rust-toolchain.toml the pinned compiler
```

## Building

Needs NASM, `llvm-objcopy`, QEMU, and the Rust nightly the project is
pinned to (`rust-toolchain.toml`: nightly-2026-04-03, rustc commit
55e86c99) with `rust-src`. On Debian or Kali:

```bash
sudo apt install nasm llvm qemu-system-x86
rustup toolchain install nightly-2026-04-03 --profile minimal --component rust-src
```

rustup uses that compiler for everything in the tree, whatever your default
is. Then, from `build/`:

```bash
make            # builds out/freya-os.img (1 MiB disk image); "make build" too
make run        # boots it in QEMU, serial output in the terminal
make test       # every test case, headless; expect "51 passed, 0 failed"
make unsafe     # every unsafe in the kernel, with its category and reason
make reproduce  # builds a second copy elsewhere; the two images must match
make verify-audit LOG=serial.log  # check an audit chain the kernel printed (FREYA_DEMO=audit_dump)
make run-tpm    # as make run, with an emulated TPM 1.2 (sudo apt install swtpm swtpm-tools)
make tis-check  # the kernel's TPM protocol against strict model TPMs, on this machine
TEXT_MODE=1 make   # an image that stays in 80x25 text mode
```

Builds are reproducible: nothing of the builder (directory, home, toolchain
location) gets into the image, so the same source and the same compiler give
the same image, byte for byte. `make test`'s first lines print the compiler
(and whether it is the pinned one) and the image's hash.

`llvm-objcopy` can be swapped for GNU objcopy with `make OBJCOPY=objcopy`.

## What you see

Stage 2 picks the largest graphics mode the video BIOS offers that the display
can show (its EDID says which; without one, at most 1024x768) and whose lines
and framebuffer the kernel can use, so on a
1280x800 screen the kernel has 160x50 character cells, and on the T430's
1600x900 panel 200x56. Holding **Shift** while
stage 2 starts, or building with `TEXT_MODE=1`, keeps the BIOS's 80x25 text
mode instead.

The top of the screen lists each subsystem as OK or FAIL, then the self-test
count, a banner, and an uptime counter; in graphics mode the mode and a
16-colour swatch are at the top right. Below row 17 is a log of everything the
kernel and the processes print (6 rows in text mode, 31 at 1280x800), and on
the last row the line being typed, after the shell's prompt `freya>`. Once
boot is done the console driver draws all of this; the log says so ("process
4 ('console') draws the screen from here on", after init's "started the
console driver (process 4), the PS/2 driver (process 5), the mouse driver
(process 6) and the shell (process 7)"). A failed self-test is repeated there,
so a photo of the screen is a usable report on a machine without a serial
port.

Keys the PS/2 driver handles itself, so they work whatever the shell does:

- **Ctrl+Alt+Del** restarts.
- **Ctrl+Alt+End** switches off (ACPI S5).
- **PageUp** / **PageDown** scroll the log back and forward a page at a time,
  through the last 400 rows. A bar across the top of the log says how far back
  the view is; lines that arrive meanwhile don't move it.

Everything else goes to the shell. Type a line and press **Enter**: the log
goes back to its newest line, the line appears after the prompt, and the
command runs. **Backspace** deletes, **Esc** clears the line, **Up** and
**Down** go through the last 8 lines entered.

| Command | What it does |
|---|---|
| `help` | lists the commands |
| `echo TEXT` | prints TEXT |
| `ps` | every task: what it is doing, how many capabilities it holds, and how much memory it owns |
| `caps` | the shell's own capabilities: everything it may do, what it has lent and to whom, and how many copies are out |
| `uptime` | time since the timer started, task switches, system calls |
| `mem` | memory: free, and what the rest is doing (the kernel, programs, reserved) |
| `audit` | the audit chain: how many entries, the head, every kept entry verified in the shell's own process, counts by kind, and the latest entries in words |
| `run NAME` | start a program in the background: `hello`, `spin` (runs until stopped), `crash` (writes to the kernel's memory, and is stopped), `ticker` (a line a second while it holds the console), `relay` (lends the console on to a ticker of its own) or `hog` (takes memory until its 1 MiB budget says no, then reads it back every second) |
| `jobs` | the programs started from the shell that are still running |
| `kill ID`, `kill NAME` | stop one of them, or every one of that name |
| `revoke ID`, `revoke NAME` | take back what the shell lent it, from it and from anyone it passed it to |
| `restart`, `off` | restart or switch off the machine |

The shell's Factory capability covers only those six programs: `run
console` is refused by the kernel. When a program ends, the shell says how
(its exit code, the CPU exception that stopped it, or `kill`). It can stop
only what it started: `kill 5` gets "this shell holds no capability for it".

A program gets a copy of the shell's console, lent, not the shell's own;
`relay` also gets a Factory that starts only `ticker`. `revoke` takes those
back from every process that has them: `run relay`, then `revoke relay`,
says "3 capabilities" (the console from relay and from its ticker, and
relay's Factory), and both end on their own, since they can't write any
more. Revoking doesn't stop a program: `spin` doesn't notice. When a program
ends, the shell takes back whatever it had passed on, so a relay that is
killed doesn't leave its ticker writing.

`hog` is lent a Budget of 1 MiB, minted from the shell's 4 MiB: `run hog`,
and it says what it holds every 256 KiB, then that its budget won't give
more at 1024 KiB. `caps` shows the shell's Budget with that in use. `revoke
hog` frees its memory, and its next read of it is a page fault, which the
shell reports. Five hogs at once share the shell's 4 MiB: 4096 KiB between
them, the last ones getting less.

The shell's lines are shown in white, without the "[user 7 'shell']" the
kernel puts in front of every line a process writes (serial keeps it).

The mouse (on the T430, the TrackPoint and the touchpad) moves a pointer, a
cell drawn with its colours swapped, at the same speed in every mode. Holding
the middle button while moving scrolls the log, the way a ThinkPad's
TrackPoint scrolls; a wheel, if there is one, scrolls 3 rows a notch.

If the kernel ever stops on a CPU exception or a panic, it takes the screen
back from the console driver, and the top of the screen shows the exception,
every register, and the log lines from just before it.

## Tests

`build/test.sh` builds each case into `build/out/test/<case>/`, boots it
headless and checks its serial log:

- `image`: the disk image's layout (not booted)
- `unsafe`: every `unsafe` in the kernel has its reason (not booted; the
  counts from `make unsafe`'s list)
- `boot-128M`, `boot-4G`: normal boots
- `dirty-ram`: RAM starts full of random bytes, as on a real machine
- `bios-if-on`: the bootloader behaves like a BIOS that leaves interrupts on
- `keyboard`, `restart`, `shutdown`, `restart-q35`: keys pressed through
  QEMU's monitor
- `shell`: the shell's commands and editing keys (Backspace, Esc, Up twice),
  `ps` and `caps` checked line by line, and a screenshot of the line being
  typed and of the shell's last line
- `programs`: `run spin`, the shell still answering while it spins, `kill`
  by number and by name, `kill 5` refused, `hello`'s exit code, `crash`
  stopped by a page fault, `run console` refused
- `revoke`: `run ticker`, `caps` (the copy lent and the copy made from it),
  `revoke 8` (1 capability; the ticker ends with "no capability"), `run
  relay`, `ps` and `caps`, `revoke relay` (3 capabilities, in two
  processes), then a relay killed (the shell takes back its ticker's
  console); no tick after any take-back, and the shell back to its 9
- `heap`: `run hog` until its budget is used up, `caps` (1 MiB of the
  shell's 4 MiB in use), `revoke hog` (its next read a page fault), five
  hogs at once (4096 KiB between them, none over 1024), all killed, and as
  many pages free as before the first
- `scroll`: PageUp and PageDown, checked step by step, and a screenshot whose
  size and colours must match the mode the kernel reported
- `edid-1366`, `edid-1440`, `no-edid`, `cirrus-24bit`: the mode stage 2
  picks for displays of different sizes (or no EDID), and drawing at 24 bits
  per pixel
- `text-mode`, `shift`: the text-mode build, and Shift held while booting
- `held-key`: a key held down through boot (repeating, with the byte timing
  of a real keyboard), then typing: the keyboard must still answer
- `vbe-lies`: a test build of stage 2 believes a video BIOS that lies about
  its modes (`TEST_VBE_LIES=1`) and must pass over every mode it can't use
- `log-flood`: 600 lines, then more four times a second, while the log is
  scrolled back: the view must stay put (checked in the console driver's
  notes and in screenshots), and the history must stop at 400 rows
- `mouse`: QEMU's mouse moved, clicked, wheeled and middle-dragged through
  the monitor; the console driver's notes must follow every step, the shell
  must still answer, and a screenshot must show the pointer where it says
- `audit`, `audit-dump`, `audit_tamper` (Entry 32): the shell's `audit`
  verifying the chain itself; the chain printed and verified by
  `build/audit_verify.rs` against the seals printed while the boot ran; one
  bit of a kept entry changed, which the kernel catches at the next header
  and the tool here at the seal before it
- `tpm`, `tpm-audit`, `tpm_rewrite` (Entry 33), each with its own swtpm: the
  seals extended into PCR 15 and recomputed here with SHA-1; the shell's
  `audit` checking the anchors and the PCR read afresh; the whole chain
  rewritten to hide a change, which verifies on its own and which the TPM,
  the shell and the tool here each catch
- `tis` (Entry 34, not booted): `build/tis_check.rs` runs the kernel's own
  `tis.rs` against four model TPMs, from one like QEMU's to one that takes a
  byte at a time, sets its status late, needs asking twice and answers
  "busy" at first, counting every step TIS doesn't allow; and against nine
  broken ones, each of which must end in the error that names it, with the
  TPM let go
- `tpm_lost` (Entry 34): the TPM stops answering partway through a boot;
  the command that finds out stops it, the next two are refused without a
  register touched, and the chain, the shell and the tool here show it
- `kill-console`, `kill-console-often`, `kill-init`: the console driver is
  killed, and init must start it again; killed four times, after which init
  must leave it stopped and let go of it; or killed after init itself: the
  kernel must take the screen back and the shell must still answer
- `kill-shell`: the shell is killed, and the one init starts again must
  answer
- one case per built-in fault demo, each of which must be caught and reported
  (the page-fault and panic cases also check the crash screen in a screenshot)

The fault demos can also be run by hand: `FREYA_DEMO=<name> make run`, with
`<name>` one of `ud2`, `page_fault`, `stack_overflow`, `task_stack_overflow`,
`smap`, `smep`, `nx`, `use_after_unmap`, `write_read_only`,
`heap_double_free`, `panic`; `log_flood`, `kill_console`,
`kill_console_often`, `kill_init` and `kill_shell` exercise the console
driver, init and the shell. `audit_rewrite` and `tpm_lost` need a TPM:
`FREYA_DEMO=tpm_lost make run-tpm`.

## Running on real hardware

FreyaOS has **no disk driver**: it never writes to any disk, so the machine's
installed system is safe. The bootloader only reads, and only from the USB
stick it booted from.

```bash
cd build
make rehearse                 # boot as the T430 will see it, in QEMU
./write-usb.sh --list         # find the stick
make usb DEV=/dev/sdX         # write it (asks you to type the device name)
```

`write-usb.sh` refuses anything that is not a whole, unmounted USB disk of at
most 256 GiB, never touches the disk the running system is on, and reads the
stick back to check it. `make usb-clear DEV=/dev/sdX` turns the stick back
into an empty FAT32 drive (needs `fdisk` and `dosfstools`).

Boot it with the firmware's one-time boot menu (F12 on a ThinkPad), as a
legacy (BIOS) USB HDD; on UEFI machines that means enabling legacy/CSM boot and
disabling Secure Boot. The full T430 checklist is in the development journal,
Entry 13. Holding the power button is always safe.

The T430's TPM (Entry 34) is read and extended, PCR 15 only, and nothing
else is sent to it: no ownership, no keys, no counters, nothing written to
its memory, and TPM_Startup is left to the BIOS. PCR 15 goes back to zeros
when the machine resets, which Linux shows: after switching off, booting
Linux and running `grep PCR-15 /sys/class/tpm/tpm0/pcrs` should print
zeros. If the TPM is switched off in the BIOS, FreyaOS finds nothing and
the chain simply isn't anchored.

If the screen stays dark or garbled in graphics mode, hold the power button,
then boot again holding **Shift** from the moment you choose the stick until
the FreyaOS screen appears: stage 2 then stays in text mode, and the log says
why. `TEXT_MODE=1 make usb DEV=/dev/sdX` writes a stick that always does.

## Documentation

- `docs/Manifesto v2.docx`: the design FreyaOS is built toward: what it
  defends against, and how, with the architecture as built (Section X) and
  the roadmap from here (Section XI). Where any other document disagrees,
  the manifesto wins. Version 2 (Entry 29) corrected what wasn't feasible in
  version 1: the External Witness is a $5 Raspberry Pi Pico 2 working with
  the T430's TPM, in place of a CM4 wired to the CPU.
- `docs/FreyaOS Development Journal.docx`: every change, with the evidence
  for it, what went wrong, lessons learned, and where the project stands
  against the manifesto.

## Next

The manifesto's Phase A is done: every frame has an identity, frames are
held as typed handles, every `unsafe` is listed with its reason, and builds
repeat byte for byte with the pinned compiler. Entry 31 put the kernel's
shared state behind a lock the compiler checks. Phase B is done: Entry 32
built the audit chain in software, Entry 33 anchored it in QEMU's
emulated TPM 1.2, each segment's seal extended into PCR 15, and Entry 34
does the same with the T430's own TPM 1.2, extend and read only, confirmed
on the T430 in Entry 35. Numbering segments by a counter that only goes up
is left for later: on a TPM 1.2 creating a counter needs ownership, and
the T430's TPM is owned already; clearing it would be a lasting change.

FreyaOS is meant for anyone, on many kinds of machines: x86-64 and ARM64,
BIOS and UEFI, old hardware as well as new, with the same rules and the
same External Witness everywhere. Entry 35 proposes writing that into the
manifesto, with a new Phase P (Portability) before the Witness: the
kernel split into a portable core and a part per architecture, a UEFI
loader, and ARM64 in QEMU. The proposals are tracked changes in
`docs/Manifesto v2.docx`, to be accepted or rejected there.
