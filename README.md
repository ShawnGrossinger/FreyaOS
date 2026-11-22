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
| Boot | Stage 1 MBR (with a partition table) loads stage 2, which loads the kernel (up to 1 MiB), collects the E820 memory map and enters long mode |
| Memory | Kernel-owned page tables, NX, a guard page under the kernel stack, a higher-half map of all RAM, a frame allocator, a page-mapping API, a kernel heap |
| Interrupts | A gate for all 256 vectors, the local APIC timer at 100 Hz, the I/O APIC routed from ACPI tables; interrupts nobody set up are survived and reported |
| Tasks | Kernel threads, preemptive round-robin scheduling, sleeping, joining, a mutex |
| User mode | Ring 3 processes in their own address spaces, `SYSCALL`, SMEP and SMAP where the CPU has them; a fault ends only the process |
| Capabilities and IPC | 16 capability slots per process; synchronous call/reply IPC that can pass capabilities (with GRANT) and badges |
| Drivers | I/O-port and interrupt-line capabilities; a PS/2 keyboard driver running as a user process |
| Power | Restart and switch-off through ACPI, behind a Power capability (Ctrl+Alt+Del, Ctrl+Alt+End) |
| Not yet | UEFI boot, graphics mode, mouse, disk, network, file system, multiple CPUs |

Every boot runs **79 self-tests** and shows the result on screen. `make test`
builds and boots **20 configurations** headless and checks each one.

## Design in brief

- **Microkernel.** Only scheduling, memory, address spaces, interrupts, IPC
  and capabilities live in the kernel. Drivers and services are processes.
- **Capabilities.** Each process has 16 slots. Kinds: `Console`,
  `Endpoint` (rights SEND, RECV, GRANT, plus a badge), `IoPorts` (a port
  range), `Irq` (an interrupt line) and `Power`. Authority only moves by being
  handed over at spawn time or passed in an IPC message through an endpoint
  with GRANT.
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

## Repository layout

```
bootloader/BIOS/   stage1.asm (MBR), stage2.asm (loader, E820, long mode)
bootloader/UEFI/   placeholder for a later UEFI loader
kernel/            Rust kernel: entry.asm, kernel.ld, src/*.rs
user/              user programs in NASM (keyboard driver, test programs)
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
make            # builds out/freya-os.img (1 MiB disk image)
make run        # boots it in QEMU, serial output in the terminal
make test       # every test case, headless; expect "20 passed, 0 failed"
```

`llvm-objcopy` can be swapped for GNU objcopy with `make OBJCOPY=objcopy`.

## What you see

The top of the screen lists each subsystem as OK or FAIL, then the self-test
count, a banner, and an uptime counter. The bottom seven rows are a log of
everything the kernel prints. A failed self-test is repeated there, so a photo
of the screen is a usable report on a machine without a serial port.

Keys, handled by the keyboard driver:

- **Ctrl+Alt+Del** restarts.
- **Ctrl+Alt+End** switches off (ACPI S5).
- Typing a line and pressing **Enter** echoes it back.

If the kernel ever stops on a CPU exception or a panic, the top of the screen
shows the exception, every register, and the log lines from just before it.

## Tests

`build/test.sh` builds each case into `build/out/test/<case>/`, boots it
headless and checks its serial log:

- `image`: the disk image's layout (not booted)
- `boot-128M`, `boot-4G`: normal boots
- `dirty-ram`: RAM starts full of random bytes, as on a real machine
- `bios-if-on`: the bootloader behaves like a BIOS that leaves interrupts on
- `keyboard`, `restart`, `shutdown`, `restart-q35`: keys pressed through
  QEMU's monitor
- one case per built-in fault demo, each of which must be caught and reported

The fault demos can also be run by hand: `FREYA_DEMO=<name> make run`, with
`<name>` one of `ud2`, `page_fault`, `stack_overflow`, `task_stack_overflow`,
`smap`, `smep`, `nx`, `use_after_unmap`, `write_read_only`,
`heap_double_free`, `panic`.

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

## Documentation

- `docs/FreyaOS Development Journal.docx`: every change, with the evidence
  for it, what went wrong, lessons learned, and where the project stands
  against the roadmap.
- `docs/Freya OS-AI Development Roadmap.docx`: the phase plan.

## Next

Graphics: have the bootloader set a VESA graphics mode and give the kernel a
framebuffer, then a console driver process that owns the screen. After that,
the TrackPoint/touchpad as a PS/2 mouse driver.
