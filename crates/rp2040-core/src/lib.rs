//! RP2040 emulator core: deterministic, I/O-free, `no_std` (+ alloc) so the
//! exact same code runs natively and in WebAssembly.
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub use armv6m::{bus, cpu, ppb};

pub mod elf;
pub mod periph;
pub mod system;
