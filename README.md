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

## Status

| Area | State |
|---|---|
| Boot | Stage 1 MBR (with a partition table) loads stage 2, which loads the kernel (up to 1 MiB), collects the E820 memory map, sets a graphics mode and enters long mode |
| Screen | A VESA graphics mode with a linear framebuffer, as large as the display shows (read from its EDID) up to 1920x1280; text in the VGA ROM's own 8x16 font, drawn through a write-combining mapping. 80x25 text mode on request. Drawn by a console driver process from the end of boot, with 400 rows of the log to scroll back through |
| Memory | Kernel-owned page tables, NX, a guard page under the kernel stack, a higher-half map of all RAM, a frame allocator, a page-mapping API, a kernel heap |
| Interrupts | A gate for all 256 vectors, the local APIC timer at 100 Hz, the I/O APIC routed from ACPI tables; interrupts nobody set up are survived and reported |
| Tasks | Kernel threads, preemptive round-robin scheduling, sleeping, joining, a mutex |
| User mode | Ring 3 processes in their own address spaces, written in assembly or Rust; `SYSCALL`, SMEP and SMAP where the CPU has them; a fault ends only the process |
| Capabilities and IPC | 32 capability slots per process; synchronous call/reply IPC that can pass capabilities (with GRANT) and badges; signals; memory lent through Memory capabilities; narrower copies (mint) and letting go (drop) |
| Init | The only process the kernel starts: it holds the drivers' capabilities and a Factory capability, starts each driver with exactly what it needs, and starts one again if it ends |
| Drivers | I/O-port, interrupt-line and memory capabilities, and interrupts delivered as signals; a console driver, a PS/2 driver (keyboard and controller) and a mouse driver, all in Rust, running as user processes, started by init |
| Power | Restart and switch-off through ACPI, behind a Power capability (Ctrl+Alt+Del, Ctrl+Alt+End) |
| Not yet | UEFI boot, mouse, disk, network, file system, multiple CPUs |

Every boot runs **88 self-tests** and shows the result on screen. `make test`
checks the disk image and boots **33 configurations** headless, checking each
one.

## Design in brief

- **Microkernel.** Only scheduling, memory, address spaces, interrupts, IPC
  and capabilities live in the kernel. Drivers and services are processes.
- **Capabilities.** Each process has 16 slots. Kinds: `Console` (write to
  the log), `Endpoint` (rights SEND, RECV, GRANT, plus a badge), `IoPorts` (a
  port range), `Irq` (an interrupt line), `Power`, `Memory` (physical
  pages a process may map: writable or not, write-combining or uncached), and
  `Factory` (may start built-in programs and make endpoints). Authority only
  moves by being copied into a process its holder starts, or passed in an IPC
  message through an endpoint with GRANT. An endpoint capability can be copied
  with fewer rights and a badge (mint), and any capability let go of (drop).
- **Init decides, the kernel enforces.** At the end of boot the kernel starts
  one process, init, with every capability the drivers will need. Init mints
  narrower copies, starts the console, PS/2 and mouse drivers with exactly the
  slots each needs, and is signalled when one ends, so it can start it again
  (three times at most; then it lets go of what only that driver used).
- **Drivers own their devices.** The console driver draws the screen through
  three Memory capabilities: the framebuffer (or VGA text memory), the
  kernel's log, and the kernel's status rows, the last two read-only, plus a
  read-only description of the screen and its font. The kernel wakes it with
  signals on its endpoint; the PS/2 driver asks it to scroll with an IPC
  call. The kernel draws the screen until a process maps it, and takes it
  back for a crash or when that process ends.
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
  | 15 | spawn(cap, program, arg, slots, exit_ep): start a built-in program | Factory |
  | 16 | endpoint(cap): a new endpoint | Factory |
  | 17 | mint(cap, rights, badge): a narrower copy | Endpoint |
  | 18 | drop(slot): let go of a capability | anything |
  | 19 | irq_bind(irq, endpoint): the line's interrupts become signals there | Irq, Endpoint with RECV |

## Repository layout

```
bootloader/BIOS/   stage1.asm (MBR), stage2.asm (loader, E820, VESA mode, long mode)
bootloader/UEFI/   placeholder for a later UEFI loader
kernel/            Rust kernel: entry.asm, kernel.ld, src/*.rs
user/              user programs in NASM (test programs)
user/rust/         user programs in Rust (init; the console, PS/2 and mouse drivers) and their library
build/             Makefile, test.sh, write-usb.sh
docs/              development journal, roadmap and design notes
```

## Building

Needs NASM, `llvm-objcopy`, Rust **nightly** with `rust-src`, and QEMU. On
Debian or Kali:

```bash
sudo apt install nasm llvm qemu-system-x86
rustup toolchain install nightly --component rust-src
```

Then, from `build/`:

```bash
make            # builds out/freya-os.img (1 MiB disk image); "make build" too
make run        # boots it in QEMU, serial output in the terminal
make test       # every test case, headless; expect "34 passed, 0 failed"
TEXT_MODE=1 make   # an image that stays in 80x25 text mode
```

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
16-colour swatch are at the top right. Everything below row 17 is a log of
everything the kernel prints (7 rows in text mode, 32 at 1280x800). Once boot
is done the console driver draws all of this; the log says so ("process 4
('console') draws the screen from here on", after init's "started the console
driver (process 4), the PS/2 driver (process 5) and the mouse driver (process
6)"). A failed
self-test is repeated there, so a photo of the screen is a usable report on a
machine without a serial port.

Keys, handled by the PS/2 driver:

- **Ctrl+Alt+Del** restarts.
- **Ctrl+Alt+End** switches off (ACPI S5).
- **PageUp** / **PageDown** scroll the log back and forward a page at a time,
  through the last 400 rows. A bar across the top of the log says how far back
  the view is; lines that arrive meanwhile don't move it.
- Typing a line and pressing **Enter** echoes it back (and returns the log to
  its newest line first).

The mouse (on the T430, the TrackPoint and the touchpad) moves a pointer, a
cell drawn with its colours swapped. Holding the middle button while moving
scrolls the log, the way a ThinkPad's TrackPoint scrolls; a wheel, if there
is one, scrolls 3 rows a notch.

If the kernel ever stops on a CPU exception or a panic, it takes the screen
back from the console driver, and the top of the screen shows the exception,
every register, and the log lines from just before it.

## Tests

`build/test.sh` builds each case into `build/out/test/<case>/`, boots it
headless and checks its serial log:

- `image`: the disk image's layout (not booted)
- `boot-128M`, `boot-4G`: normal boots
- `dirty-ram`: RAM starts full of random bytes, as on a real machine
- `bios-if-on`: the bootloader behaves like a BIOS that leaves interrupts on
- `keyboard`, `restart`, `shutdown`, `restart-q35`: keys pressed through
  QEMU's monitor
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
  the monitor; the console driver's notes must follow every step, typing
  must still work, and a screenshot must show the pointer where it says
- `kill-console`, `kill-console-often`, `kill-init`: the console driver is
  killed, and init must start it again; killed four times, after which init
  must leave it stopped and let go of it; or killed after init itself: the
  kernel must take the screen back and the keyboard must still work
- one case per built-in fault demo, each of which must be caught and reported
  (the page-fault and panic cases also check the crash screen in a screenshot)

The fault demos can also be run by hand: `FREYA_DEMO=<name> make run`, with
`<name>` one of `ud2`, `page_fault`, `stack_overflow`, `task_stack_overflow`,
`smap`, `smep`, `nx`, `use_after_unmap`, `write_read_only`,
`heap_double_free`, `panic`; `log_flood`, `kill_console`,
`kill_console_often` and `kill_init` exercise the console driver and init.

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

If the screen stays dark or garbled in graphics mode, hold the power button,
then boot again holding **Shift** from the moment you choose the stick until
the FreyaOS screen appears: stage 2 then stays in text mode, and the log says
why. `TEXT_MODE=1 make usb DEV=/dev/sdX` writes a stick that always does.

## Documentation

- `docs/FreyaOS Development Journal.docx`: every change, with the evidence
  for it, what went wrong, lessons learned, and where the project stands
  against the roadmap.
- `docs/Freya OS-AI Development Roadmap.docx`: the phase plan.

## Next

The mouse on the T430. Then one of: the capability layer (revoking what was
handed out; capabilities for memory and address spaces), a first shell, or
read-only disks on a spare machine.
