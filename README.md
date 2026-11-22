# FreyaOS (revival build)

This repo is a **BIOS-first** bootstrap for FreyaOS:

- `bootloader/BIOS/stage1.asm` — 512-byte MBR loader
- `bootloader/BIOS/stage2.asm` — loads the kernel, switches to long mode, jumps to 0x0010_0000
- `kernel/src/main.rs` — minimal `no_std` Rust kernel that prints to VGA and halts

## Dependencies

### Required (build)

- NASM (`nasm`)
- LLVM objcopy (`llvm-objcopy`)
- Rust nightly + rust-src

### Recommended (run)

- QEMU (`qemu-system-x86_64`)

## Build & run

From the `build/` directory:

```bash
make clean
make
make run
```

If everything is wired correctly you should see:

- Stage1 prints: `Stage 1: OK`
- Stage2 prints: `Stage 2: OK Kernel loaded`
- Kernel prints (VGA): `Hello from FreyaOS kernel!`

## Notes

- The `Makefile` computes:
  - Stage2 sector count (so Stage1 loads the whole Stage2)
  - Kernel start LBA and kernel sector count (so Stage2 can read the kernel)
- UEFI support is still WIP (`bootloader/UEFI/UEFILoader.rs`).
