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

.PHONY: all clean run trace debug test

all: $(IMG)

$(OUTDIR):
	mkdir -p $(OUTDIR)

# -------------------------
# Kernel build
# -------------------------

KERNEL_SRCS := $(shell find $(KERNDIR)/src -name '*.rs') \
			   $(KERNDIR)/entry.asm $(KERNDIR)/kernel.ld $(KERNDIR)/build.rs \
			   $(KERNDIR)/Cargo.toml $(KERNDIR)/x86_64-freya.json \
			   $(wildcard ../user/*.asm ../user/*.inc)

# Build-time fault demo: FREYA_DEMO=<name> make run  (names: see
# run_requested_demo() in kernel/src/main.rs). The stamp file's name holds
# the current choice, so changing FREYA_DEMO rebuilds the kernel even when
# no source changed.
DEMO_STAMP := $(OUTDIR)/.demo-$(if $(FREYA_DEMO),$(FREYA_DEMO),none)

$(DEMO_STAMP): | $(OUTDIR)
	rm -f $(OUTDIR)/.demo-*
	touch $@

$(KERNELELF): $(KERNEL_SRCS) $(DEMO_STAMP) | $(OUTDIR)
	cd $(KERNDIR) && \
	  RUST_TARGET_PATH=$(abspath $(KERNDIR)) \
	  RUSTFLAGS="-C link-arg=--script=kernel.ld" \
	  FREYA_DEMO="$(FREYA_DEMO)" \
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
$(STAGE2PRE): $(BOOTDIR)/stage2.asm | $(OUTDIR)
	$(NASM) -f bin $(BOOTDIR)/stage2.asm -o $(STAGE2PRE)

# Stage2 final build: compute Stage2 sectors, then compute kernel LBA/sectors, then assemble.
$(STAGE2): $(BOOTDIR)/stage2.asm $(STAGE2PRE) $(KERNELBIN) | $(OUTDIR)
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
	$(NASM) -f bin \
	  -D__KERNEL_LBA=$$KERNEL_LBA \
	  -D__KERNEL_SECTORS=$$KERNEL_SECTORS \
	  $(BOOTDIR)/stage2.asm -o $(STAGE2); \
	STAGE2_SIZE2=$$(stat -c%s $(STAGE2)); \
	STAGE2_SECTORS2=$$(( ($$STAGE2_SIZE2 + $(SECTOR) - 1)/$(SECTOR) )); \
	if [ $$STAGE2_SECTORS -ne $$STAGE2_SECTORS2 ]; then \
	  echo "ERROR: stage2 size changed after patching constants. Re-run make."; \
	  exit 1; \
	fi

# Stage1 final build: needs the final Stage2 sector count.
$(STAGE1): $(BOOTDIR)/stage1.asm $(STAGE2) | $(OUTDIR)
	@STAGE2_SIZE=$$(stat -c%s $(STAGE2)); \
	STAGE2_SECTORS=$$(( ($$STAGE2_SIZE + $(SECTOR) - 1)/$(SECTOR) )); \
	echo "[stage1] assembling with __STAGE2_SECTORS=$$STAGE2_SECTORS"; \
	$(NASM) -f bin -D__STAGE2_SECTORS=$$STAGE2_SECTORS $(BOOTDIR)/stage1.asm -o $(STAGE1)

# -------------------------
# Image
# -------------------------

$(IMG): $(STAGE1) $(STAGE2) $(KERNELBIN) | $(OUTDIR)
	cat $(STAGE1) $(STAGE2) $(KERNELBIN) > $(IMG)
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

# Build and boot every test case headless, check each one's output, and
# print a summary. Non-zero exit if anything failed. See test.sh.
test:
	./test.sh

clean:
	rm -rf $(OUTDIR)
	cd $(KERNDIR) && cargo clean