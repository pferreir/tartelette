//! The Bluetooth controller behind a radio chip's HCI transport.
//!
//! [`HciBackend`] is the seam, as [`NetBackend`](crate::net::NetBackend) is
//! for Wi-Fi: the `cyw43439` model only carries HCI packets between the
//! firmware and a backend. A host can bridge them to a real controller (a
//! Linux HCI user channel in the `hcibridge` crate), so the firmware's host
//! stack drives a real radio and real devices. [`virt::VirtualController`]
//! is a controller in software: with no [mock peers](peers) it stands in
//! when there is no radio (the browser), and with some it gives tests a
//! deterministic radio neighbourhood.
use alloc::{string::String, vec::Vec};

pub mod peers;
pub mod virt;

/// H4 packet types (the first byte on a UART transport).
pub const COMMAND: u8 = 1;
pub const ACL: u8 = 2;
pub const EVENT: u8 = 4;

/// A Bluetooth controller. Packets carry no H4 type byte: the type is
/// passed alongside.
pub trait HciBackend: Send {
    /// False when there is no radio behind it (the browser): the firmware
    /// still gets answers to its commands, but finds and reaches nobody.
    fn available(&self) -> bool;
    /// A packet from the firmware (command, ACL data, ...). `now` is the
    /// emulated time in microseconds.
    fn send(&mut self, now: u64, kind: u8, packet: &[u8]);
    /// A packet for the firmware (event, ACL data, ...), if one is waiting.
    /// Polled about every millisecond of emulated time.
    fn recv(&mut self, now: u64) -> Option<(u8, Vec<u8>)>;
    /// Why the radio is unavailable, for the UI.
    fn unavailable_reason(&self) -> String {
        String::new()
    }
    /// Happenings for the board's event queue since the last call.
    fn take_notes(&mut self) -> Vec<(&'static str, i64)> {
        Vec::new()
    }
    /// A host action for the devices behind the backend (`midi` for the
    /// mock MIDI peripherals). False if not understood.
    fn input(&mut self, _now: u64, _action: &str, _value: i64) -> bool {
        false
    }
}
