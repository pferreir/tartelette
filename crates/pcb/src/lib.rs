//! The PCB around a microcontroller: a device framework and a board-file
//! parser, with no knowledge of any particular chip. The host implements
//! [`Pins`] (and drives the buses) for its chip. `no_std` (+ alloc).
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod board;
pub mod device;

pub use board::{Board, BoardEvent, BoardI2c, BoardSpi, BusDecl, Factory, InputResult, ParseError, Params, Part, Registry};
pub use device::{Device, I2cDevice, I2cTarget, Io, Pins, SpiDevice, SpiTarget};

/// Number of pins a board file may refer to (`gpio=0..NUM_PINS-1`); pin sets
/// are `u32` masks.
pub const NUM_PINS: usize = 30;
