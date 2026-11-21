//! The stock device models. Each lives in its own file and exposes a
//! [`pcb::Kind`] (`KIND`) declaring its board-file keys; [`register_builtin`]
//! lists them all, so adding a device is one new file plus one entry here. Chip-independent: they only
//! use the [`pcb`] framework.
//!
//! | kind       | what                                                   |
//! |------------|--------------------------------------------------------|
//! | `led`      | LED on a pin                                           |
//! | `button`   | momentary switch to GND or 3V3                         |
//! | `mcp23008` | 8-bit I2C port expander with interrupt output          |
//! | `st7789`   | SPI TFT display controller with frame RAM              |
//! | `ws2812`   | WS2812 / SK6812 LED chain decoded from the data line   |
//! | `dac8165`  | quad 14-bit DAC on a bit-banged serial link            |
//! | `encoder`  | rotary encoder (quadrature contacts)                   |
//! | `sdcard`   | SDHC card in SPI mode over a disk image                |
//! | `cyw43439` | Pico W Wi-Fi/Bluetooth chip on a bit-banged gSPI link  |
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

use pcb::Registry;

pub mod button;
pub mod cyw43439;
pub mod dac8165;
pub mod encoder;
pub mod fat;
pub mod hci;
pub mod led;
pub mod mcp23008;
pub mod net;
pub mod sdcard;
pub mod st7789;
pub mod ws2812;

pub use led::is_lit;

pub fn register_builtin(r: &mut Registry) {
    for kind in [
        led::KIND,
        button::KIND,
        mcp23008::KIND,
        st7789::KIND,
        ws2812::KIND,
        dac8165::KIND,
        encoder::KIND,
        sdcard::KIND,
        cyw43439::KIND,
    ] {
        r.register(kind);
    }
}

/// A registry holding every stock device kind.
pub fn registry() -> Registry {
    let mut r = Registry::empty();
    register_builtin(&mut r);
    r
}

#[cfg(test)]
mod testutil {
    use pcb::Pins;

    /// Pins for device tests: a pad is driven by the board, else by the
    /// "chip" side (`set_chip`), else it floats to `floating`.
    #[derive(Default)]
    pub struct FakePins {
        pub floating: u32,
        chip: [Option<bool>; pcb::NUM_PINS],
        ext: [Option<bool>; pcb::NUM_PINS],
        seen: u32,
    }

    impl FakePins {
        pub fn set_chip(&mut self, pin: usize, level: bool) {
            self.chip[pin] = Some(level);
        }

        /// The chip stops driving the pad (an input again).
        pub fn release_chip(&mut self, pin: usize) {
            self.chip[pin] = None;
        }

        /// Pads whose level moved since the last call.
        pub fn take_changes(&mut self) -> u32 {
            let now = (0..pcb::NUM_PINS).fold(0, |m, p| m | (self.level(p) as u32) << p);
            core::mem::replace(&mut self.seen, now) ^ now
        }
    }

    impl Pins for FakePins {
        fn level(&self, pin: usize) -> bool {
            self.ext[pin]
                .or(self.chip[pin])
                .unwrap_or(self.floating >> pin & 1 != 0)
        }
        fn drive(&mut self, pin: usize, level: Option<bool>) {
            self.ext[pin] = level;
        }
    }
}

/// The stock devices wired through a real [`pcb::Board`].
#[cfg(test)]
mod board_tests {
    use alloc::{boxed::Box, string::String, vec::Vec};

    use pcb::{Board, Device, I2cTarget, InputResult, ParseError, Pins};

    use crate::{is_lit, led::Led, registry, testutil::FakePins};

    fn parse(text: &str) -> Result<Board, ParseError> {
        Board::parse_with(text, &registry())
    }

    const SAMPLE: &str = r#"
# demo
name = "my pcb"

[parts.status]
kind = "led"
gpio = 25   # on-board

[parts.err]
kind = "led"
gpio = 16
active = "low"

[parts.user]
kind = "button"
gpio = 2

[parts.wake]
kind = "button"
gpio = 3
active = "high"
"#;

    #[test]
    fn parses_parts_and_defaults() {
        let b = parse(SAMPLE).unwrap();
        assert_eq!(b.name, "my pcb");
        let names: Vec<_> = b
            .parts
            .iter()
            .map(|p| (p.name.as_str(), p.kind.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("status", "led"),
                ("err", "led"),
                ("user", "button"),
                ("wake", "button")
            ]
        );
        assert!(parse("").unwrap().parts.is_empty());
    }

    #[test]
    fn reports_errors_with_their_place() {
        let e = |t: &str| parse(t).unwrap_err();
        let at =
            e("parts.a = { kind = \"led\", gpio = 1 }\nparts.b = { kind = \"lde\", gpio = 2 }");
        assert_eq!(
            (at.line, at.col, at.msg.as_str()),
            (2, 20, "unknown device kind \"lde\" (did you mean led?)")
        );
        assert_eq!(
            e("parts.a = { kind = \"led\" }").msg,
            "missing gpio (the pin it hangs off)"
        );
        assert_eq!(
            e("parts.a = { kind = \"led\", gpio = 1, active = \"up\" }").msg,
            "active must be high or low"
        );
        assert_eq!(
            e("parts.a = { kind = \"led\", gpio = 1, colour = \"red\" }").msg,
            "unknown parameter colour (led takes gpio, active)"
        );
        assert_eq!(e("parts.a = { kind = \"button\", gpio = 1 }\nparts.b = { kind = \"button\", gpio = 1 }").msg, "GPIO 1 is already used by a.gpio");
        assert_eq!(e("buses.i2c0 = { sda = 0, scl = 1 }\nparts.a = { kind = \"led\", gpio = 1, bus = \"i2c0\" }").msg, "led does not sit on a bus");
        assert_eq!(
            e("parts.a = { kind = \"mcp23008\", bus = \"i2c0\" }").msg,
            "bus i2c0 is not declared: add it under [buses] with its pins"
        );
    }

    /// Every declared key of every kind, with a value the kind accepts.
    fn sample_table(params: &[pcb::Param], next_pin: &mut usize) -> String {
        use pcb::{Need, Ty};
        let fields: Vec<String> = params
            .iter()
            .map(|p| {
                let v = match (p.ty, p.need) {
                    (Ty::Pin | Ty::SharedPin, _) => {
                        *next_pin += 1;
                        alloc::format!("{next_pin}")
                    }
                    (Ty::Int { .. }, Need::DefaultInt(d)) => alloc::format!("{d}"),
                    (Ty::Int { min, .. }, _) => alloc::format!("{min}"),
                    (Ty::Choice(options), _) => alloc::format!("{:?}", options[options.len() - 1]),
                    (Ty::Str, Need::DefaultStr(d)) => alloc::format!("{d:?}"),
                    (Ty::Str, _) => "\"xxxxxxxx\"".into(),
                    (Ty::Tables(inner), _) => alloc::format!("[{}]", sample_table(inner, next_pin)),
                };
                alloc::format!("{} = {v}", p.key)
            })
            .collect();
        alloc::format!("{{ {} }}", fields.join(", "))
    }

    #[test]
    fn every_kind_builds_with_every_key_it_declares() {
        // The board checks (in debug builds) that a device reads every key it was given.
        // Some kinds have keys that exclude each other (an expander pin's
        // `lit` is for LEDs, `contact` for switches): their own tests cover
        // those keys, and here refusing the mix is fine.
        let r = registry();
        let mut pin = 0;
        let mut text = String::new();
        for k in r.kinds() {
            let t = sample_table(k.params, &mut pin);
            let line = alloc::format!("parts.{} = {{ kind = {:?}, {}\n", k.name, k.name, &t[2..]);
            match Board::parse_with(&line, &r) {
                Err(e) if e.msg.contains(" is only for ") => {}
                Err(e) => panic!("{e}\n{line}"),
                Ok(_) => text += &line,
            }
        }
        let b = Board::parse_with(&text, &r).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert!(b.parts.len() >= r.kinds().count() - 1);
    }

    #[test]
    fn custom_kinds_can_be_registered_and_overridden() {
        const WIDGET: pcb::Kind = pcb::Kind {
            name: "widget",
            summary: "does nothing",
            bus: None,
            params: &[],
            build: |_| Ok(Box::new(Nop)),
        };
        struct Nop;
        impl Device for Nop {}
        let mut r = registry();
        assert!(Board::parse_with("parts.w = { kind = \"widget\" }", &r).is_err());
        r.register(WIDGET);
        let b = Board::parse_with("parts.w = { kind = \"widget\" }", &r).unwrap();
        assert_eq!(b.parts[0].kind, "widget");
        assert!(b.device::<Led>("w").is_none());
        r.register(pcb::Kind {
            name: "led",
            ..WIDGET
        }); // override a built-in
        assert!(Board::parse_with("parts.a = { kind = \"led\" }", &r).is_ok());
        assert!(r.kinds().any(|k| k.name == "widget"));
    }

    fn drive_pin(g: &mut FakePins, pin: usize, high: bool) {
        g.set_chip(pin, high);
    }

    #[test]
    fn leds_follow_pins_and_queue_events() {
        let mut g = FakePins::default();
        let mut b = parse("parts.a = { kind = \"led\", gpio = 5 }\nparts.b = { kind = \"led\", gpio = 6, active = \"low\" }").unwrap();
        b.attach(&mut g);
        // Pins idle low through the default pull-down.
        assert_eq!(
            (is_lit(&b, "a"), is_lit(&b, "b")),
            (Some(false), Some(true))
        );
        assert!(b.events.is_empty());

        drive_pin(&mut g, 5, true);
        drive_pin(&mut g, 6, true);
        let changed = g.take_changes();
        b.service(&mut g, changed, 100);
        assert_eq!(
            (is_lit(&b, "a"), is_lit(&b, "b")),
            (Some(true), Some(false))
        );
        // Pins that did not move are not looked at.
        b.service(&mut g, 0, 200);
        drive_pin(&mut g, 5, false);
        let changed = g.take_changes();
        b.service(&mut g, changed, 300);
        let ev: Vec<_> = b
            .events
            .iter()
            .map(|e| (e.cycles, e.part, e.value != 0))
            .collect();
        assert_eq!(ev, [(100, 0, true), (100, 1, false), (300, 0, false)]);
        assert_eq!(is_lit(&b, "nope"), None);
    }

    #[test]
    fn buttons_drive_pins() {
        let mut g = FakePins::default();
        let mut b = parse("parts.up = { kind = \"button\", gpio = 2 }\nparts.hi = { kind = \"button\", gpio = 3, active = \"high\" }\nparts.l = { kind = \"led\", gpio = 2 }").unwrap();
        g.floating |= 1 << 2; // pull-up
        b.attach(&mut g);
        assert!(g.level(2));
        assert!(b.set_button(&mut g, "up", true, 10));
        assert!(!g.level(2));
        assert!(b.set_button(&mut g, "up", true, 11)); // already down: no event
        assert!(b.set_button(&mut g, "up", false, 20));
        assert!(g.level(2));
        assert!(b.set_button(&mut g, "hi", true, 30));
        assert!(g.level(3));
        assert!(!b.set_button(&mut g, "nope", true, 0));
        assert!(!b.set_button(&mut g, "l", true, 0)); // not a button
        assert_eq!(is_lit(&b, "up"), None);
        let ev: Vec<_> = b
            .events
            .iter()
            .map(|e| (e.cycles, e.part, e.label))
            .collect();
        assert_eq!(
            ev,
            [(10, 0, "pressed"), (20, 0, "released"), (30, 1, "pressed")]
        );
    }

    #[test]
    fn i2c_transactions_span_bus_handles_and_route_by_controller() {
        let mut g = FakePins::default();
        let mut b = parse("buses.i2c0 = { sda = 0, scl = 1 }\nparts.e = { kind = \"mcp23008\", bus = \"i2c0\", addr = 0x22 }").unwrap();
        b.attach(&mut g);
        // Each handle is short-lived (one per instruction); the transaction isn't.
        assert!(b.i2c_bus(0, &mut g, 0).start(0x22, false));
        assert!(b.i2c_bus(0, &mut g, 1).write(0x0A));
        assert!(b.i2c_bus(0, &mut g, 2).write(0x5A));
        b.i2c_bus(0, &mut g, 3).stop();
        assert!(b.i2c_bus(0, &mut g, 4).start(0x22, false));
        assert!(b.i2c_bus(0, &mut g, 5).write(0x0A));
        assert!(b.i2c_bus(0, &mut g, 6).start(0x22, true));
        assert_eq!(b.i2c_bus(0, &mut g, 7).read(true), 0x5A);
        b.i2c_bus(0, &mut g, 8).stop();
        // After STOP nothing is addressed, and unknown addresses / the other controller NACK.
        assert!(!b.i2c_bus(0, &mut g, 9).write(1));
        assert!(!b.i2c_bus(0, &mut g, 9).start(0x30, false));
        assert!(!b.i2c_bus(1, &mut g, 9).start(0x22, false));
    }

    #[test]
    fn input_reports_why_it_failed() {
        let mut g = FakePins::default();
        let mut b = parse(
            "parts.l = { kind = \"led\", gpio = 1 }\nparts.b = { kind = \"button\", gpio = 2 }",
        )
        .unwrap();
        assert_eq!(b.input(&mut g, "x", "press", 0, 0), InputResult::NoSuchPart);
        assert_eq!(
            b.input(&mut g, "l", "press", 0, 0),
            InputResult::Unsupported
        );
        assert_eq!(
            b.input(&mut g, "b", "wiggle", 0, 0),
            InputResult::Unsupported
        );
        assert_eq!(b.input(&mut g, "b", "press", 0, 0), InputResult::Done);
    }
}
