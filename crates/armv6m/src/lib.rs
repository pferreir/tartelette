//! ARMv6-M (Cortex-M0+) core: CPU interpreter, bus trait and the
//! architecture-defined system peripherals. `no_std` (+ alloc), deterministic,
//! knows nothing about any particular chip.
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod bus;
pub mod cpu;
pub mod ppb;
