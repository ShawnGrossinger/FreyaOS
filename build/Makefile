# ---------------------------------------------------------------------------------------------
# FreyaOS Build (BIOS path)
#
# Layout in final raw image:
#   LBA 0 : stage1.bin (512 bytes, MBR)
#   LBA 1..: stage2.bin (variable size)
#   next.. : kernel.bin (flat binary)
#
# Stage1 needs to know Stage2's sector count.
# Stage2 needs to know kernel start LBA and sector count.
# This Makefile computes those values and assembles with NASM defines.
# ---------------------------------------------------------------------------------------------

NASM    ?= nasm
OBJCOPY ?= llvm-objcopy

SECTOR  := 512
# stage2 loads at most 1 MiB of kernel (it must end below stage2's page
# tables at 0x200000) and refuses anything bigger at boot. Catch it here.
KERNEL_MAX_SECTORS := 2048
OUTDIR  := out
BOOTDIR := ../bootloader/BIOS
KERNDIR := ../kernel

IMG       := $(OUTDIR)/freya-os.img
STAGE1    := $(OUTDIR)/stage1.bin
STAGE2PRE := $(OUTDIR)/stage2.pre.bin
STAGE2    := $(OUTDIR)/stage2.bin
KERNELELF := $(OUTDIR)/kernel.elf
KERNELBIN := $(OUTDIR)/kernel.bin

.PHONY: all build clean run trace debug test rehearse usb usb-clear

all: $(IMG)

# Another name for it.
build: all

$(OUTDIR):
	mkdir -p $(OUTDIR)

# -------------------------
# Kernel build
# -------------------------

KERNEL_SRCS := $(shell find $(KERNDIR)/src -name '*.rs') \
			   $(KERNDIR)/entry.asm $(KERNDIR)/kernel.ld $(KERNDIR)/build.rs \
			   $(KERNDIR)/Cargo.toml $(KERNDIR)/x86_64-freya.json \
			   $(wildcard ../user/*.asm ../user/*.inc)

# -------------------------
# User programs in Rust (Entry 20)
# -------------------------
# Built before the kernel, which embeds them (kernel/build.rs copies them
# from FREYA_USER_BINS). Each is an ELF linked by user.ld at the fixed user
# addresses, then turned into a flat image like the NASM programs.

USERRS          := ../user/rust
USER_RS_SRCS    := $(shell find $(USERRS)/src -name '*.rs') $(USERRS)/Cargo.toml \
                   $(USERRS)/user.ld $(USERRS)/x86_64-freya-user.json
USER_RS_PROGRAMS := console init captest ps2 mouse shell
USER_RS_STAMP   := $(OUTDIR)/user/.built

$(USER_RS_STAMP): $(USER_RS_SRCS) | $(OUTDIR)
	mkdir -p $(OUTDIR)/user
	cd $(USERRS) && \
	  RUST_TARGET_PATH=$(abspath $(USERRS)) \
	  RUSTFLAGS="-C link-arg=--script=user.ld" \
	  cargo +nightly build \
	    --target x86_64-freya-user.json \
	    --release \
	    -Z build-std=core,compiler_builtins \
	    -Z build-std-features=compiler-builtins-mem \
	    -Z json-target-spec
	for p in $(USER_RS_PROGRAMS); do \
	  $(OBJCOPY) -O binary $(USERRS)/target/x86_64-freya-user/release/$$p $(OUTDIR)/user/$$p.bin || exit 1; \
	done
	touch $@

# Build-time fault demo: FREYA_DEMO=<name> make run  (names: see
# run_requested_demo() in kernel/src/main.rs). The stamp file's name holds
# the current choice, so changing FREYA_DEMO rebuilds the kernel even when
# no source changed.
DEMO_STAMP := $(OUTDIR)/.demo-$(if $(FREYA_DEMO),$(FREYA_DEMO),none)

$(DEMO_STAMP): | $(OUTDIR)
	rm -f $(OUTDIR)/.demo-*
	touch $@

$(KERNELELF): $(KERNEL_SRCS) $(DEMO_STAMP) $(USER_RS_STAMP) | $(OUTDIR)
	cd $(KERNDIR) && \
	  RUST_TARGET_PATH=$(abspath $(KERNDIR)) \
	  RUSTFLAGS="-C link-arg=--script=kernel.ld" \
	  FREYA_DEMO="$(FREYA_DEMO)" \
	  FREYA_USER_BINS=$(abspath $(OUTDIR)/user) \
	  cargo +nightly build \
	    --target x86_64-freya.json \
	    --release \
	    -Z build-std=core,compiler_builtins,alloc \
	    -Z build-std-features=compiler-builtins-mem \
	    -Z json-target-spec
	cp $(KERNDIR)/target/x86_64-freya/release/freya-kernel $(KERNELELF)

$(KERNELBIN): $(KERNELELF)
	$(OBJCOPY) -O binary $(KERNELELF) $(KERNELBIN)
	truncate -s %$(SECTOR) $(KERNELBIN)

# -------------------------
# Bootloaders
# -------------------------

# Stage2 preliminary build (with placeholder kernel constants) so we can compute its size.
# BIOS_LEAVES_IF_ON=1 (test.sh's bios-if-on case): stage2 turns interrupts
# on after its disk calls, as the T430's BIOS leaves them (Entry 16), and
# again after setting the display.
# TEXT_MODE=1: stage2 keeps the BIOS's 80x25 text mode instead of setting a
# VESA graphics mode (Entry 18). Holding Shift while it boots does the same.
# TEST_VBE_LIES=1 (test.sh's vbe-lies case): stage2 believes a video BIOS
# that misdescribes its larger modes, to test the checks it makes before
# setting one (Entry 20).
STAGE2_DEFS := $(if $(BIOS_LEAVES_IF_ON),-D__BIOS_LEAVES_IF_ON,) $(if $(TEXT_MODE),-D__TEXT_MODE,) \
               $(if $(TEST_VBE_LIES),-D__TEST_VBE_LIES,)
# As with FREYA_DEMO: a stamp names the current choice, so switching
# between "make" and "TEXT_MODE=1 make" rebuilds stage2.
STAGE2_STAMP := $(OUTDIR)/.stage2$(if $(BIOS_LEAVES_IF_ON),-bios-if-on,)$(if $(TEXT_MODE),-text-mode,)$(if $(TEST_VBE_LIES),-vbe-lies,)

$(STAGE2_STAMP): | $(OUTDIR)
	rm -f $(OUTDIR)/.stage2*
	touch $@

$(STAGE2PRE): $(BOOTDIR)/stage2.asm $(STAGE2_STAMP) | $(OUTDIR)
	$(NASM) -f bin $(STAGE2_DEFS) $(BOOTDIR)/stage2.asm -o $(STAGE2PRE)

# Stage2 final build: compute Stage2 sectors, then compute kernel LBA/sectors, then assemble.
$(STAGE2): $(BOOTDIR)/stage2.asm $(STAGE2PRE) $(KERNELBIN) $(STAGE2_STAMP) | $(OUTDIR)
	@STAGE2_SIZE=$$(stat -c%s $(STAGE2PRE)); \
	STAGE2_SECTORS=$$(( ($$STAGE2_SIZE + $(SECTOR) - 1)/$(SECTOR) )); \
	KERNEL_LBA=$$(( 1 + $$STAGE2_SECTORS )); \
	KERNEL_SIZE=$$(stat -c%s $(KERNELBIN)); \
	KERNEL_SECTORS=$$(( ($$KERNEL_SIZE + $(SECTOR) - 1)/$(SECTOR) )); \
	echo "[layout] stage2_sectors=$$STAGE2_SECTORS  kernel_lba=$$KERNEL_LBA  kernel_sectors=$$KERNEL_SECTORS (stage2 limit: $(KERNEL_MAX_SECTORS))"; \
	if [ $$KERNEL_SECTORS -gt $(KERNEL_MAX_SECTORS) ]; then \
	  echo "ERROR: kernel is $$KERNEL_SECTORS sectors; stage2 loads at most $(KERNEL_MAX_SECTORS) (1 MiB, below its page tables at 0x200000)."; \
	  exit 1; \
	fi; \
	$(NASM) -f bin $(STAGE2_DEFS) \
	  -D__KERNEL_LBA=$$KERNEL_LBA \
	  -D__KERNEL_SECTORS=$$KERNEL_SECTORS \
	  $(BOOTDIR)/stage2.asm -o $(STAGE2); \
	STAGE2_SIZE2=$$(stat -c%s $(STAGE2)); \
	STAGE2_SECTORS2=$$(( ($$STAGE2_SIZE2 + $(SECTOR) - 1)/$(SECTOR) )); \
	if [ $$STAGE2_SECTORS -ne $$STAGE2_SECTORS2 ]; then \
	  echo "ERROR: stage2 size changed after patching constants. Re-run make."; \
	  exit 1; \
	fi

# Stage1 final build: needs the final Stage2 sector count, and the whole
# image's size for its partition table.
$(STAGE1): $(BOOTDIR)/stage1.asm $(STAGE2) $(KERNELBIN) | $(OUTDIR)
	@STAGE2_SIZE=$$(stat -c%s $(STAGE2)); \
	STAGE2_SECTORS=$$(( ($$STAGE2_SIZE + $(SECTOR) - 1)/$(SECTOR) )); \
	KERNEL_SECTORS=$$(( ($$(stat -c%s $(KERNELBIN)) + $(SECTOR) - 1)/$(SECTOR) )); \
	IMAGE_SECTORS=$$(( (1 + $$STAGE2_SECTORS + $$KERNEL_SECTORS + 2047) / 2048 * 2048 )); \
	echo "[stage1] assembling with __STAGE2_SECTORS=$$STAGE2_SECTORS __IMAGE_SECTORS=$$IMAGE_SECTORS"; \
	$(NASM) -f bin -D__STAGE2_SECTORS=$$STAGE2_SECTORS -D__IMAGE_SECTORS=$$IMAGE_SECTORS $(BOOTDIR)/stage1.asm -o $(STAGE1)

# -------------------------
# Image
# -------------------------

# Padded to a whole MiB, the size the partition table in stage1 claims.
# (Some BIOS disk paths, QEMU's q35 AHCI among them, refuse a disk of a
# few hundred sectors; a USB stick is always far bigger anyway.)
$(IMG): $(STAGE1) $(STAGE2) $(KERNELBIN) | $(OUTDIR)
	cat $(STAGE1) $(STAGE2) $(KERNELBIN) > $(IMG)
	truncate -s %1M $(IMG)
	@echo "Built: $(IMG)"

# Run in QEMU.
# -drive if=ide,media=disk : explicitly emulates an IDE hard disk so that
#   INT 13h EDD (AH=42h) works correctly — the default drive type in some
#   QEMU versions does not expose EDD extensions.
# -m 128M    : enough RAM for the frame allocator bitmap + metadata table.
# -no-reboot / -no-shutdown : keep the window open on triple-fault so you
#   can read the last VGA/serial output before QEMU exits.
#run: $(IMG)
#	qemu-system-x86_64 \
	  -drive format=raw,file=$(IMG),if=ide,media=disk,index=0 \
	  -serial stdio \
	  -m 128M \
	  -no-reboot \
	  -no-shutdown

# The kernel reports its own exceptions on serial and screen, so `run` only
# logs CPU resets (triple faults) and guest errors. With the timer running
# at 100 Hz, `-d int` grows the log by about 8 MB a minute.
run: $(IMG)
	qemu-system-x86_64 \
	  -drive format=raw,file=$(IMG),if=ide,media=disk,index=0 \
	  -serial stdio \
	  -m 128M \
	  -no-reboot \
	  -no-shutdown \
	  -d cpu_reset,guest_errors -D /tmp/qemu-int.log

# Same as run, logging every interrupt and exception. For faults before the
# kernel's IDT exists (stage2, early _start). Grows quickly once the timer
# is on.
trace: $(IMG)
	qemu-system-x86_64 \
	  -drive format=raw,file=$(IMG),if=ide,media=disk,index=0 \
	  -serial stdio \
	  -m 128M \
	  -no-reboot \
	  -no-shutdown \
	  -d int,cpu_reset -D /tmp/qemu-int.log

# Same as run but halts at startup waiting for GDB on localhost:1234.
# In a second terminal: gdb -ex "target remote :1234"
debug: $(IMG)
	qemu-system-x86_64 \
	  -drive format=raw,file=$(IMG),if=ide,media=disk,index=0 \
	  -serial stdio \
	  -m 128M \
	  -no-reboot \
	  -no-shutdown \
	  -s -S

# Boot the image the way the ThinkPad T430 will see it: from a USB stick
# (EHCI), Ivy Bridge CPU, q35 chipset, no serial port. Ctrl+Alt+Del really
# restarts and Ctrl+Alt+End closes the window, as on the laptop, so the keys
# can be practised here first. snapshot=on: the image file is never changed.
rehearse: $(IMG)
	qemu-system-x86_64 \
	  -machine q35 -cpu IvyBridge -m 2G \
	  -drive if=none,id=stick,format=raw,file=$(IMG),snapshot=on \
	  -device usb-ehci,id=ehci \
	  -device usb-storage,bus=ehci.0,drive=stick,bootindex=0 \
	  -serial none $(REHEARSE_ARGS)

# Write the image to a USB stick: make usb DEV=/dev/sdX (see write-usb.sh,
# which refuses anything that isn't an unmounted USB disk and asks first).
usb: $(IMG)
	@[ -n "$(DEV)" ] || { echo "usage: make usb DEV=/dev/sdX"; ./write-usb.sh --list; exit 2; }
	@[ -z "$(FREYA_DEMO)" ] || { echo "FREYA_DEMO=$(FREYA_DEMO) is set: that image crashes on purpose. Unset it first."; exit 2; }
	./write-usb.sh $(IMG) $(DEV)

# Turn the stick back into an empty FAT32 stick: make usb-clear DEV=/dev/sdX
# (same checks as usb; LABEL=NAME to name it, default USB).
usb-clear:
	@[ -n "$(DEV)" ] || { echo "usage: make usb-clear DEV=/dev/sdX"; ./write-usb.sh --list; exit 2; }
	./write-usb.sh --clear $(DEV)

# Build and boot every test case headless, check each one's output, and
# print a summary. Non-zero exit if anything failed. See test.sh.
test:
	./test.sh

clean:
	rm -rf $(OUTDIR)
	cd $(KERNDIR) && cargo clean
	cd $(USERRS) && cargo clean