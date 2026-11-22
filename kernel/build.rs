// build.rs — assemble entry.asm and hand the object to the linker.
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
}