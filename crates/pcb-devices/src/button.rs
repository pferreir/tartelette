//! `button <name> gpio=N [active=high|low]`: momentary switch between the pin
//! and GND (`active=low`, the default) or 3V3. Host actions: `press`,
//! `release`.
use pcb::{Device, Io, Params};
use alloc::{boxed::Box, string::String};

pub struct Button {
    pub pin: usize,
    pub active_high: bool,
    pub pressed: bool,
}

pub fn factory(p: &mut Params) -> Result<Box<dyn Device>, String> {
    let pin = p.pin("gpio")?;
    let active_high = p.opt_level("active")?.unwrap_or(false);
    Ok(Box::new(Button { pin, active_high, pressed: false }))
}

impl Device for Button {
    fn attach(&mut self, io: &mut Io) {
        self.pressed = false;
        io.drive(self.pin, None);
    }

    fn input(&mut self, io: &mut Io, action: &str, _value: i64) -> bool {
        let down = match action {
            "press" => true,
            "release" => false,
            _ => return false,
        };
        if self.pressed != down {
            self.pressed = down;
            io.drive(self.pin, down.then_some(self.active_high));
            io.emit(if down { "pressed" } else { "released" }, down as i64);
        }
        true
    }
}
