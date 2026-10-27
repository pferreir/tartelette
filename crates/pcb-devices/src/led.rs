//! An LED on a pin: anode on the pin (through a resistor), cathode on GND
//! by default; `active = "low"` wires it to 3V3. It only watches the pin, so
//! it may sit on a line another part uses (an activity LED).
use alloc::boxed::Box;

use pcb::{Board, Device, Io, Kind, Param};

pub struct Led {
    pub pin: usize,
    pub active_high: bool,
    pub lit: bool,
}

pub const KIND: Kind = Kind {
    name: "led",
    summary: "LED on a pin",
    bus: None,
    params: &[
        Param::shared_pin("gpio", "the pin it hangs off"),
        Param::choice("active", &["high", "low"], "the level that lights it").default_str("high"),
    ],
    build: |p| {
        Ok(Box::new(Led {
            pin: p.pin("gpio"),
            active_high: p.str("active") == "high",
            lit: false,
        }))
    },
};

impl Device for Led {
    fn watched_pins(&self) -> u32 {
        1 << self.pin
    }

    fn pin_changed(&mut self, io: &mut Io, _changed: u32) {
        let lit = io.level(self.pin) == self.active_high;
        if lit != self.lit {
            self.lit = lit;
            io.emit(if lit { "on" } else { "off" }, lit as i64);
        }
    }
}

/// State of the `led` part called `name`, if there is one.
pub fn is_lit(board: &Board, name: &str) -> Option<bool> {
    Some(board.device::<Led>(name)?.lit)
}
