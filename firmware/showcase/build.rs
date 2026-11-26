//! Linker setup, and the ROM the varvara demo runs.
use std::{env, fs, fs::File, io::Write, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(include_bytes!("memory.x"))
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tlink-rp.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");

    // VARVARA_ROM, or roms/fourtette.rom (from roms/fetch.sh). Without one,
    // the demo builds empty and says so when it starts, so that the other
    // demos still build.
    println!("cargo:rerun-if-env-changed=VARVARA_ROM");
    let rom = env::var_os("VARVARA_ROM")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("roms/fourtette.rom"));
    println!("cargo:rerun-if-changed={}", rom.display());
    // And its directory: a ROM put there with an older date than the last
    // build (copied, or moved back) still changes the directory's.
    if let Some(dir) = rom.parent().filter(|d| d.is_dir()) {
        println!("cargo:rerun-if-changed={}", dir.display());
    }
    let bytes = fs::read(&rom).unwrap_or_else(|_| {
        println!(
            "cargo:warning=varvara: no ROM at {} (run roms/fetch.sh, or set VARVARA_ROM)",
            rom.display()
        );
        Vec::new()
    });
    assert!(
        bytes.len() <= 0xff00,
        "{}: too big for Uxn memory",
        rom.display()
    );
    fs::write(out.join("varvara.rom"), bytes).unwrap();
}
