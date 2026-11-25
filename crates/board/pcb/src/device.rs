//! The device framework: what a part on the PCB has to implement.
//!
//! A *device* is a chip or component wired to the RP2040. Devices never see
//! the chip's registers: they are reached through
//!
//! * **pins** ([`Device::pin_changed`] + [`Io`]) for anything on a GPIO,
//! * **buses** ([`I2cDevice`], [`SpiDevice`]) for transaction-level traffic,
//! * **the host** ([`Device::input`]) for things a user pokes at (buttons, a
//!   simulated sensor value, ...),
//! * **analog levels** ([`Device::analog`]) for voltages the ADC samples.
//!
//! Adding a device is: write a type implementing [`Device`] (and a bus trait
//! if it sits on one), write a `fn(&mut Params) -> Result<Box<dyn Device>, _>`
//! factory, and register it under a `kind` name (see
//! [`Registry::register`](crate::board::Registry::register)). The board file
//! then picks it by name; nothing else changes.
use alloc::{string::String, vec::Vec};
use core::any::Any;

use crate::board::BoardEvent;

/// The chip's pads as the board sees them. Implemented by the host for its
/// microcontroller.
pub trait Pins {
    /// Voltage on the pad.
    fn level(&self, pin: usize) -> bool;
    /// Drive the pad from outside the chip (`Some(level)`) or release it
    /// (`None`, high impedance).
    fn drive(&mut self, pin: usize, level: Option<bool>);
}

/// A device's view of the board while it is being called: the pins it can
/// sense and drive, the current time, and a way to tell the host something
/// happened.
pub struct Io<'a> {
    pins: &'a mut dyn Pins,
    cycles: u64,
    events: &'a mut Vec<BoardEvent>,
    part: usize,
    quiet: bool,
    pending: Option<&'a mut Pending>,
}

impl<'a> Io<'a> {
    pub fn new(
        pins: &'a mut dyn Pins,
        cycles: u64,
        events: &'a mut Vec<BoardEvent>,
        part: usize,
        quiet: bool,
    ) -> Self {
        Self {
            pins,
            cycles,
            events,
            part,
            quiet,
            pending: None,
        }
    }

    /// Let the device schedule wake-ups ([`wake_at`](Self::wake_at)) and
    /// raise warnings ([`warn`](Self::warn)) into `pending`.
    pub fn with_pending(mut self, pending: &'a mut Pending) -> Self {
        self.pending = Some(pending);
        self
    }

    /// Voltage on the pad.
    pub fn level(&self, pin: usize) -> bool {
        self.pins.level(pin)
    }

    /// Drive the pin from this device (`Some(level)`) or release it.
    pub fn drive(&mut self, pin: usize, level: Option<bool>) {
        self.pins.drive(pin, level);
    }

    /// Core cycle count right now.
    pub fn cycles(&self) -> u64 {
        self.cycles
    }

    /// Ask to be called ([`Device::timer`]) at core cycle `cycle`. Always in
    /// the future: a request for now or the past fires on the next cycle.
    pub fn wake_at(&mut self, cycle: u64) {
        let at = cycle.max(self.cycles + 1);
        if let Some(p) = self.pending.as_deref_mut() {
            p.timers.push((at, self.part));
        }
    }

    /// Tell the user about something suspicious in how the firmware uses
    /// the device (a misconfigured pin, say). The host shows it as a
    /// warning, prefixed with the part's name; say each thing once.
    pub fn warn(&mut self, msg: String) {
        if let Some(p) = self.pending.as_deref_mut() {
            p.warnings.push((self.part, msg));
        }
    }

    /// Report something observable to the host (e.g. `"lit"`, 1). Events are
    /// timestamped and queued in [`Board::events`](crate::board::Board::events).
    pub fn emit(&mut self, label: &'static str, value: i64) {
        if !self.quiet {
            self.events.push(BoardEvent {
                cycles: self.cycles,
                part: self.part,
                label,
                value,
            });
        }
    }
}

/// What devices left for the board to handle after a call: wake-ups as
/// (cycle, part index), and warnings as (part index, text). Hosts and
/// tests that call devices directly give one to [`Io::with_pending`].
#[derive(Default)]
pub struct Pending {
    pub timers: Vec<(u64, usize)>,
    pub warnings: Vec<(usize, String)>,
}

/// Something on the board. Every method has a do-nothing default so a device
/// only implements the interfaces it actually has.
///
/// `Any` is a supertrait so hosts can reach a concrete device (a display's
/// framebuffer, say) with [`Board::device`](crate::board::Board::device).
pub trait Device: Any + Send {
    /// Pins whose changes [`pin_changed`](Self::pin_changed) wants to hear
    /// about (bit n = GPIO n). Read once when the board is plugged in.
    fn watched_pins(&self) -> u32 {
        0
    }

    /// Called once when the board is plugged in, before any pin change is
    /// reported: set the initial state of pins this device drives.
    fn attach(&mut self, _io: &mut Io) {}

    /// One or more of the [`watched_pins`](Self::watched_pins) changed
    /// voltage; `changed` has only those bits. Also called once after attach
    /// with every watched bit set, so devices can sample the initial levels
    /// (events are suppressed during that call).
    fn pin_changed(&mut self, _io: &mut Io, _changed: u32) {}

    /// The host did something to the device (`action` is device-specific,
    /// e.g. `"press"`). Returns false if the action isn't understood.
    fn input(&mut self, _io: &mut Io, _action: &str, _value: i64) -> bool {
        false
    }

    /// A wake-up requested with [`Io::wake_at`] has come due; `io.cycles()` is
    /// the cycle that was asked for.
    fn timer(&mut self, _io: &mut Io) {}

    /// The chip's clock (clk_sys, which the cycles count) is `hz` from cycle
    /// `at` on: a device that turns time into cycles follows it, unless its
    /// board-file keys fix a rate (`mhz`). Called when the board is plugged
    /// in, and on each change after.
    fn clock(&mut self, _at: u64, _hz: u64) {}

    /// The voltage this device puts on `pin` at core cycle `cycles`, if it
    /// drives that pin with an analog signal. Asked by the chip's ADC when a
    /// conversion samples the pin; must not depend on how often it is asked.
    fn analog(&self, _pin: usize, _cycles: u64) -> Option<f32> {
        None
    }

    /// The I2C face of this device, if it has one.
    fn i2c(&mut self) -> Option<&mut dyn I2cDevice> {
        None
    }

    /// The SPI face of this device, if it has one.
    fn spi(&mut self) -> Option<&mut dyn SpiDevice> {
        None
    }
}

/// An I2C target. The controller has already done the electrical work; the
/// device sees whole bytes. A `false` return is a NACK.
pub trait I2cDevice {
    /// 7-bit bus address this device answers to.
    fn address(&self) -> u8;
    /// START (or repeated START) addressed to this device.
    fn start(&mut self, io: &mut Io, read: bool) -> bool;
    /// Byte written by the controller.
    fn write(&mut self, io: &mut Io, byte: u8) -> bool;
    /// Byte requested by the controller. `last` means the controller will
    /// NACK it and stop.
    fn read(&mut self, io: &mut Io, last: bool) -> u8;
    /// STOP condition while this device was addressed.
    fn stop(&mut self, io: &mut Io);
}

/// The far end of the wire, as an I2C controller sees it. A [`Board`](crate::board::Board)
/// routes the traffic to its [`I2cDevice`]s.
pub trait I2cTarget {
    /// START / repeated START with a 7-bit address. False = nobody ACKed.
    fn start(&mut self, addr: u8, read: bool) -> bool;
    /// False = NACK.
    fn write(&mut self, byte: u8) -> bool;
    /// `last`: the controller will NACK this byte and stop.
    fn read(&mut self, last: bool) -> u8;
    fn stop(&mut self);
}

/// An SPI peripheral. Chip select is the board's business: a device only
/// hears frames while its CS pin (`cs=N` in the board file) is low, and is
/// told about CS edges through [`select`](Self::select). Other pins it needs
/// (D/C, reset, ...) are ordinary device parameters read through [`Io`].
pub trait SpiDevice {
    /// CS was asserted (`true`) or released. Also called once when the board
    /// is plugged in with the initial state.
    fn select(&mut self, io: &mut Io, active: bool);
    /// One frame of `bits` (4..=16) shifted in and out at once. Returns what
    /// the device drives on MISO.
    fn transfer(&mut self, io: &mut Io, mosi: u16, bits: u8) -> u16;
}

/// The far end of the wire, as an SPI controller sees it. A
/// [`Board`](crate::board::Board) routes the frame to the devices whose CS is
/// asserted; with none selected MISO floats high.
pub trait SpiTarget {
    fn transfer(&mut self, mosi: u16, bits: u8) -> u16;
}
