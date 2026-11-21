//! ARMv6-M (Cortex-M0+) core: CPU interpreter, bus trait and the
//! architecture-defined system peripherals. `no_std` (+ alloc), deterministic,
//! knows nothing about any particular chip.
#![no_std]
// Threaded dispatch (`Cpu::run_threaded`): each instruction's handler
// tail-calls the next one's. WebAssembly needs the tail-call proposal for
// that (`-C target-feature=+tail-call`, set in .cargo/config.toml); built
// without it, the run is a loop.
#![cfg_attr(
    any(not(target_arch = "wasm32"), target_feature = "tail-call"),
    feature(explicit_tail_calls),
    allow(incomplete_features)
)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod bus;
pub mod cpu;
pub mod ppb;
