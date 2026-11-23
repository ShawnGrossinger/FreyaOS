// build.rs — assemble entry.asm and hand the object to the linker, and
// gather the user programs the kernel embeds: NASM ones assembled here,
// Rust ones built by build/Makefile.
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir      = env::var("OUT_DIR").expect("OUT_DIR");

    let asm_src = PathBuf::from(&manifest_dir).join("entry.asm");
    let obj_out = PathBuf::from(&out_dir).join("entry.o");

    println!("cargo:rerun-if-changed=entry.asm");
    println!("cargo:rerun-if-changed=kernel.ld");

    let status = Command::new("nasm")
        .args(["-f", "elf64"])
        .arg(&asm_src)
        .arg("-o")
        .arg(&obj_out)
        .status()
        .expect("failed to spawn nasm — is it on PATH?");

    assert!(status.success(), "nasm failed to assemble entry.asm");

    // Pass the assembled object straight to the linker.
    println!("cargo:rustc-link-arg={}", obj_out.display());

    // User programs (../user/*.asm): flat binaries the kernel embeds with
    // include_bytes! (process.rs). No file system to load them from yet.
    let user_dir = PathBuf::from(&manifest_dir).join("../user");
    println!("cargo:rerun-if-changed=../user/freya.inc");
    for program in ["hello", "probe", "ipc", "driver", "keyboard"] {
        println!("cargo:rerun-if-changed=../user/{program}.asm");
        let status = Command::new("nasm")
            .args(["-f", "bin", "-I"])
            .arg(format!("{}/", user_dir.display()))
            .arg(user_dir.join(format!("{program}.asm")))
            .arg("-o")
            .arg(PathBuf::from(&out_dir).join(format!("{program}.bin")))
            .status()
            .expect("failed to spawn nasm");
        assert!(status.success(), "nasm failed to assemble user/{program}.asm");
    }

    // User programs in Rust (../user/rust, Entry 20): build/Makefile builds
    // them first, as flat images in FREYA_USER_BINS.
    println!("cargo:rerun-if-env-changed=FREYA_USER_BINS");
    let bins = env::var("FREYA_USER_BINS")
        .expect("FREYA_USER_BINS is unset: build with make in build/, which builds the Rust user programs first");
    for program in ["console", "init", "captest"] {
        let src = PathBuf::from(&bins).join(format!("{program}.bin"));
        println!("cargo:rerun-if-changed={}", src.display());
        std::fs::copy(&src, PathBuf::from(&out_dir).join(format!("{program}.bin")))
            .unwrap_or_else(|e| panic!("can't copy {}: {e}", src.display()));
    }
}