//! A USB host for an emulated device: deterministic, I/O-free, `no_std`
//! (+ alloc), like the rest of the emulator.
//!
//! [`Host`] enumerates whatever pulls D+ up on a [`DevicePort`] (reset,
//! address, descriptors, configuration) the way a PC would, then drives the
//! functions it knows: CDC-ACM serial ports, HID interfaces and USB-MIDI
//! streaming interfaces (see [`function`]). Frontends move bytes, reports
//! and messages in and out of those functions and bridge them to the
//! outside world; every transfer also goes to a traffic [`log`].
//!
//! The port is spoken to one transaction at a time (SETUP, IN, OUT tokens and
//! their handshakes), which is the level a device controller works at. Time
//! is the caller's clock (emulated cycles); each transaction takes the bus
//! for as long as it would at full speed.
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod descriptor;
pub mod function;
mod host;
pub mod log;
pub mod midi;
#[cfg(test)]
mod tests;

use alloc::vec::Vec;

pub use host::{DeviceInfo, Host};

/// How a device answered a token when it sent or took no data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handshake {
    /// Not ready; the host tries again later.
    Nak,
    /// The endpoint is halted, or the request is not supported.
    Stall,
    /// No answer: nothing at that address or endpoint.
    Timeout,
}

impl Handshake {
    pub fn name(self) -> &'static str {
        match self {
            Handshake::Nak => "NAK",
            Handshake::Stall => "STALL",
            Handshake::Timeout => "timeout",
        }
    }
}

/// The device side of the cable, one transaction at a time. Endpoint
/// numbers are 0..=15, without the direction bit.
pub trait DevicePort {
    /// Is a full-speed device pulling D+ up?
    fn attached(&self) -> bool;
    /// Signal a bus reset (SE0).
    fn bus_reset(&mut self);
    /// A SETUP token and its 8-byte DATA0 packet to endpoint 0.
    fn setup(&mut self, address: u8, packet: [u8; 8]) -> Result<(), Handshake>;
    /// An IN token: the device's data packet and whether it was DATA1.
    fn token_in(&mut self, address: u8, ep: u8, max: u16) -> Result<(bool, Vec<u8>), Handshake>;
    /// An OUT token and a data packet (DATA1 if `data1`).
    fn token_out(&mut self, address: u8, ep: u8, data1: bool, data: &[u8])
    -> Result<(), Handshake>;
}
