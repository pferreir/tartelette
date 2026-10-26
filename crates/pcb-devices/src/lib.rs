//! The stock device models. Each lives in its own file and exposes a
//! `factory` for the registry; [`register_builtin`] lists them all, so adding
//! a device is one new file plus one line here. Chip-independent: they only
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
//! | `cyw43439` | Pico W Wi-Fi chip on a bit-banged gSPI link            |
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

use pcb::Registry;

pub mod button;
pub mod led;
pub mod mcp23008;
pub mod st7789;
pub mod ws2812;
pub mod dac8165;
pub mod encoder;
pub mod fat;
pub mod sdcard;
pub mod cyw43439;
pub mod net;

pub use led::is_lit;

pub fn register_builtin(r: &mut Registry) {
    r.register("led", led::factory);
    r.register("button", button::factory);
    r.register("mcp23008", mcp23008::factory);
    r.register("st7789", st7789::factory);
    r.register("ws2812", ws2812::factory);
    r.register("dac8165", dac8165::factory);
    r.register("encoder", encoder::factory);
    r.register("sdcard", sdcard::factory);
    r.register("cyw43439", cyw43439::factory);
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
            self.ext[pin].or(self.chip[pin]).unwrap_or(self.floating >> pin & 1 != 0)
        }
        fn drive(&mut self, pin: usize, level: Option<bool>) {
            self.ext[pin] = level;
        }
    }
}

/// The stock devices wired through a real [`pcb::Board`].
#[cfg(test)]
mod board_tests {
    use crate::testutil::FakePins;
    use crate::{is_lit, led::Led, registry};
    use alloc::{boxed::Box, string::String, vec::Vec};
    use pcb::{Board, BusDecl, Device, I2cTarget, InputResult, Params, ParseError, Pins};

    fn parse(text: &str) -> Result<Board, ParseError> {
        Board::parse_with(text, &registry())
    }

    const SAMPLE: &str = "\
# demo
board my pcb
led status gpio=25   # on-board
led err gpio=16 active=low

button user gpio=2
button wake gpio=3 active=high
";

    #[test]
    fn parses_parts_and_defaults() {
        let b = parse(SAMPLE).unwrap();
        assert_eq!(b.name, "my pcb");
        let names: Vec<_> = b.parts.iter().map(|p| (p.name.as_str(), p.kind.as_str())).collect();
        assert_eq!(names, [("status", "led"), ("err", "led"), ("user", "button"), ("wake", "button")]);
        assert!(parse("").unwrap().parts.is_empty());
    }

    #[test]
    fn reports_errors_with_line_numbers() {
        let e = |t: &str| parse(t).unwrap_err();
        assert_eq!(e("led a gpio=1\nfoo b").line, 2);
        assert_eq!(e("foo b").msg, "unknown device kind \"foo\"");
        assert_eq!(e("led").msg, "missing part name");
        assert_eq!(e("led a").msg, "missing gpio=N");
        assert_eq!(e("led a gpio=x").msg, "gpio must be a number");
        assert_eq!(e("led a gpio=30").msg, "gpio must be 0..29");
        assert_eq!(e("led a gpio=1 active=up").msg, "active must be high or low");
        assert_eq!(e("led a gpio=1 colour=red").msg, "unknown parameter colour");
        assert_eq!(e("led a gpio=1\nbutton a gpio=2").msg, "duplicate part name");
    }

    #[test]
    fn bus_lines_and_bus_attachment() {
        let b = parse("i2c1 sda=2 scl=3\nspi0 sck=18 mosi=19\nmcp23008 x bus=i2c1 addr=0x21").unwrap();
        assert_eq!(b.buses[0], BusDecl { name: "i2c1".into(), pins: [("sda".into(), 2), ("scl".into(), 3)].into() });
        assert_eq!(b.buses[1].pins.len(), 2);
        let e = |t: &str| parse(t).unwrap_err().msg;
        assert_eq!(e("i2c1 sda=2\ni2c1 sda=4"), "bus i2c1 declared twice");
        assert_eq!(e("i2c1 sdx=2"), "unknown parameter sdx");
        assert_eq!(e("led a gpio=1 bus=i2c0"), "this device is not an I2C device (bus=i2c0)");
        assert_eq!(e("mcp23008 a bus=i2c9"), "unknown bus \"i2c9\" (expected i2c0, i2c1, spi0 or spi1)");
        assert_eq!(
            e("mcp23008 a bus=i2c0\nmcp23008 b bus=i2c0"),
            "address 0x20 is already used on i2c0"
        );
        // Same address on different controllers is fine.
        assert!(parse("mcp23008 a bus=i2c0\nmcp23008 b bus=i2c1").is_ok());
    }

    fn dummy(_: &mut Params) -> Result<Box<dyn Device>, String> {
        struct Nop;
        impl Device for Nop {}
        Ok(Box::new(Nop))
    }

    #[test]
    fn custom_kinds_can_be_registered_and_overridden() {
        let mut r = registry();
        assert!(Board::parse_with("widget w", &r).is_err());
        r.register("widget", dummy);
        let b = Board::parse_with("widget w", &r).unwrap();
        assert_eq!(b.parts[0].kind, "widget");
        assert!(b.device::<Led>("w").is_none());
        r.register("led", dummy); // override a built-in
        assert!(Board::parse_with("led a", &r).is_ok());
        assert!(r.kinds().any(|k| k == "widget"));
    }

    fn drive_pin(g: &mut FakePins, pin: usize, high: bool) {
        g.set_chip(pin, high);
    }

    #[test]
    fn leds_follow_pins_and_queue_events() {
        let mut g = FakePins::default();
        let mut b = parse("led a gpio=5\nled b gpio=6 active=low").unwrap();
        b.attach(&mut g);
        // Pins idle low through the default pull-down.
        assert_eq!((is_lit(&b, "a"), is_lit(&b, "b")), (Some(false), Some(true)));
        assert!(b.events.is_empty());

        drive_pin(&mut g, 5, true);
        drive_pin(&mut g, 6, true);
        let changed = g.take_changes();
        b.service(&mut g, changed, 100);
        assert_eq!((is_lit(&b, "a"), is_lit(&b, "b")), (Some(true), Some(false)));
        // Pins that did not move are not looked at.
        b.service(&mut g, 0, 200);
        drive_pin(&mut g, 5, false);
        let changed = g.take_changes();
        b.service(&mut g, changed, 300);
        let ev: Vec<_> = b.events.iter().map(|e| (e.cycles, e.part, e.value != 0)).collect();
        assert_eq!(ev, [(100, 0, true), (100, 1, false), (300, 0, false)]);
        assert_eq!(is_lit(&b, "nope"), None);
    }

    #[test]
    fn buttons_drive_pins() {
        let mut g = FakePins::default();
        let mut b = parse("button up gpio=2\nbutton hi gpio=3 active=high\nled l gpio=2").unwrap();
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
        let ev: Vec<_> = b.events.iter().map(|e| (e.cycles, e.part, e.label)).collect();
        assert_eq!(ev, [(10, 0, "pressed"), (20, 0, "released"), (30, 1, "pressed")]);
    }

    #[test]
    fn i2c_transactions_span_bus_handles_and_route_by_controller() {
        let mut g = FakePins::default();
        let mut b = parse("mcp23008 e bus=i2c0 addr=0x22").unwrap();
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
        let mut b = parse("led l gpio=1\nbutton b gpio=2").unwrap();
        assert_eq!(b.input(&mut g, "x", "press", 0, 0), InputResult::NoSuchPart);
        assert_eq!(b.input(&mut g, "l", "press", 0, 0), InputResult::Unsupported);
        assert_eq!(b.input(&mut g, "b", "wiggle", 0, 0), InputResult::Unsupported);
        assert_eq!(b.input(&mut g, "b", "press", 0, 0), InputResult::Done);
    }
}
