//! `led <name> gpio=N [active=high|low]`: anode on the pin (through a
//! resistor), cathode on GND by default; `active=low` wires it to 3V3.
use pcb::{Board, Device, Io, Params};
use alloc::{boxed::Box, string::String};

pub struct Led {
    pub pin: usize,
    pub active_high: bool,
    pub lit: bool,
}

pub fn factory(p: &mut Params) -> Result<Box<dyn Device>, String> {
    let pin = p.pin("gpio")?;
    let active_high = p.opt_level("active")?.unwrap_or(true);
    Ok(Box::new(Led { pin, active_high, lit: false }))
}

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
