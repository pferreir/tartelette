//! Linker setup, and where the Pico W radio firmware blobs come from.
//!
//! The blobs (`43439A0.bin`, `43439A0_clm.bin`, `nvram_rp2040.bin`) are Infineon's and not in
//! this repository: point `CYW43_FIRMWARE_DIR` at a directory holding them
//! (Embassy keeps them in its `cyw43-firmware/` directory).
use std::{env, fs::File, io::Write, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    File::create(out.join("memory.x")).unwrap().write_all(include_bytes!("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-env-changed=CYW43_FIRMWARE_DIR");
    let dir = env::var("CYW43_FIRMWARE_DIR").expect("set CYW43_FIRMWARE_DIR");
    for f in ["43439A0.bin", "43439A0_clm.bin", "nvram_rp2040.bin"] {
        assert!(PathBuf::from(&dir).join(f).exists(), "{dir}/{f} not found: set CYW43_FIRMWARE_DIR");
    }
    println!("cargo:rustc-env=CYW43_FIRMWARE_DIR={dir}");
    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tlink-rp.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");
}
