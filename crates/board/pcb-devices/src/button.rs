//! A momentary switch between the pin and GND (`active = "low"`, the
//! default) or 3V3. Host actions: `press`, `release`.
use alloc::boxed::Box;

use pcb::{Device, Io, Kind, Param};

pub struct Button {
    pub pin: usize,
    pub active_high: bool,
    pub pressed: bool,
}

pub const KIND: Kind = Kind {
    name: "button",
    summary: "momentary switch to GND or 3V3",
    bus: None,
    params: &[
        Param::pin("gpio", "the pin the switch pulls"),
        Param::choice(
            "active",
            &["high", "low"],
            "the level it pulls the pin to when pressed",
        )
        .default_str("low"),
    ],
    build: |p| {
        Ok(Box::new(Button {
            pin: p.pin("gpio"),
            active_high: p.str("active") == "high",
            pressed: false,
        }))
    },
};

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
