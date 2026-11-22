//! A potentiometer wired as a voltage divider between GND and 3V3, its wiper
//! on a pin. It only gives the pin an analog level ([`Device::analog`]) for
//! the ADC to read; the pin's digital level is left alone.
//!
//! Host action `set` with a value in per mille of the travel (0 = GND,
//! 1000 = 3V3); emits `value` with the new position.
use alloc::boxed::Box;

use pcb::{Device, Io, Kind, Param};

/// The supply across the track.
const SUPPLY: f32 = 3.3;
const FULL: i64 = 1000;

pub struct Pot {
    pub pin: usize,
    /// Wiper position, 0..=1000.
    pub value: i64,
}

pub const KIND: Kind = Kind {
    name: "pot",
    summary: "potentiometer: a voltage between GND and 3V3 for the ADC",
    bus: None,
    params: &[
        Param::pin(
            "gpio",
            "the pin the wiper is on (GPIO26 to 29 reach the ADC)",
        ),
        Param::int(
            "value",
            0,
            FULL,
            "the wiper's starting position, in per mille of the travel",
        )
        .default_int(FULL / 2),
    ],
    build: |p| {
        Ok(Box::new(Pot {
            pin: p.pin("gpio"),
            value: p.int("value"),
        }))
    },
};

impl Pot {
    /// The wiper's voltage.
    pub fn volts(&self) -> f32 {
        SUPPLY * self.value as f32 / FULL as f32
    }
}

impl Device for Pot {
    fn input(&mut self, io: &mut Io, action: &str, value: i64) -> bool {
        if action != "set" {
            return false;
        }
        let value = value.clamp(0, FULL);
        if value != self.value {
            self.value = value;
            io.emit("value", value);
        }
        true
    }

    fn analog(&self, pin: usize, _cycles: u64) -> Option<f32> {
        (pin == self.pin).then(|| self.volts())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::testutil::FakePins;

    #[test]
    fn set_moves_the_wiper_and_clamps() {
        let mut pot = Pot {
            pin: 26,
            value: 500,
        };
        assert!((pot.analog(26, 0).unwrap() - 1.65).abs() < 1e-6);
        assert_eq!(pot.analog(27, 0), None);
        let (mut pins, mut events) = (FakePins::default(), Vec::new());
        let mut io = Io::new(&mut pins, 10, &mut events, 0, false);
        assert!(pot.input(&mut io, "set", 2000));
        assert!(!pot.input(&mut io, "turn", 1));
        assert_eq!(pot.analog(26, 0), Some(3.3));
        assert_eq!(events.len(), 1);
        assert_eq!((events[0].label, events[0].value), ("value", 1000));
    }
}
