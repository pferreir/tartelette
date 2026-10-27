//! Microchip MCP23008 8-bit I2C port expander.
//!
//! Modelled: all eleven registers, the address pointer (with byte mode via
//! `IOCON.SEQOP` and wrap-around), input polarity, pull-ups, interrupt-on-
//! change against the previous value or `DEFVAL`, the `INTF`/`INTCAP`
//! capture, and the INT output (polarity and open-drain). Interrupts are
//! evaluated whenever a pin or register changes, not continuously.
//!
//! Host actions: `gp0`..`gp7` with value 1 / 0 drive the pin high / low from
//! outside (a switch, a sensor), -1 releases it. A `pins` event (value = the
//! eight pin voltages) is emitted whenever any pin voltage changes.
use alloc::boxed::Box;

use pcb::{BusType, Device, I2cDevice, Io, Kind, Param};

const IODIR: u8 = 0x00;
const IPOL: u8 = 0x01;
const GPINTEN: u8 = 0x02;
const DEFVAL: u8 = 0x03;
const INTCON: u8 = 0x04;
const IOCON: u8 = 0x05;
const GPPU: u8 = 0x06;
const INTF: u8 = 0x07;
const INTCAP: u8 = 0x08;
const GPIO: u8 = 0x09;
const OLAT: u8 = 0x0A;
const NUM_REGS: u8 = 0x0B;

const IOCON_SEQOP: u8 = 1 << 5;
const IOCON_ODR: u8 = 1 << 2;
const IOCON_INTPOL: u8 = 1 << 1;
const IOCON_MASK: u8 = 0x36;

pub struct Mcp23008 {
    addr: u8,
    int_pin: Option<usize>,
    iodir: u8,
    ipol: u8,
    gpinten: u8,
    defval: u8,
    intcon: u8,
    iocon: u8,
    gppu: u8,
    intf: u8,
    intcap: u8,
    olat: u8,
    /// Pins driven from outside, and the level they are driven to.
    ext_drive: u8,
    ext_level: u8,
    /// GPIO register value at the last evaluation (for change detection).
    prev: u8,
    /// Pin voltages last reported to the host.
    reported: u8,
    ptr: u8,
    /// The next written byte is the register address.
    expect_ptr: bool,
}

pub const KIND: Kind = Kind {
    name: "mcp23008",
    summary: "8-bit I2C port expander with interrupt output",
    bus: Some(BusType::I2c),
    params: &[
        Param::hex("addr", 0x20, 0x27, "I2C address (A2..A0 pins)").default_int(0x20),
        Param::shared_pin(
            "int",
            "the GPIO its INT output drives (open drain can be shared)",
        )
        .optional(),
    ],
    build: |p| {
        Ok(Box::new(Mcp23008::new(
            p.int("addr") as u8,
            p.opt_pin("int"),
        )))
    },
};

impl Mcp23008 {
    fn new(addr: u8, int_pin: Option<usize>) -> Self {
        Mcp23008 {
            addr,
            int_pin,
            iodir: 0xFF,
            ipol: 0,
            gpinten: 0,
            defval: 0,
            intcon: 0,
            iocon: 0,
            gppu: 0,
            intf: 0,
            intcap: 0,
            olat: 0,
            ext_drive: 0,
            ext_level: 0,
            prev: 0,
            reported: 0,
            ptr: 0,
            expect_ptr: false,
        }
    }

    /// Voltage on each pin: outputs show OLAT, inputs whatever drives them,
    /// else the pull-up (or nothing: low).
    pub fn pin_levels(&self) -> u8 {
        let input = (self.ext_drive & self.ext_level) | (!self.ext_drive & self.gppu);
        (self.iodir & input) | (!self.iodir & self.olat)
    }

    /// What the GPIO register reads.
    fn gpio_value(&self) -> u8 {
        self.pin_levels() ^ self.ipol
    }

    pub fn int_active(&self) -> bool {
        self.intf != 0
    }

    fn drive_int(&self, io: &mut Io) {
        let Some(pin) = self.int_pin else { return };
        let active = self.int_active();
        let level = if self.iocon & IOCON_ODR != 0 {
            // Open drain: pulls low when active, floats otherwise.
            active.then_some(false)
        } else {
            Some(active == (self.iocon & IOCON_INTPOL != 0))
        };
        io.drive(pin, level);
    }

    /// Re-derive the interrupt state and outputs after anything changed.
    fn evaluate(&mut self, io: &mut Io) {
        let now = self.gpio_value();
        let differs = if self.intcon != 0 {
            now ^ self.defval
        } else {
            now ^ self.prev
        };
        let new = differs & self.gpinten & self.iodir;
        if new != 0 {
            if self.intf == 0 {
                self.intcap = now;
            }
            self.intf |= new;
        }
        self.prev = now;
        self.drive_int(io);
        let levels = self.pin_levels();
        if levels != self.reported {
            self.reported = levels;
            io.emit("pins", levels as i64);
        }
    }

    fn clear_interrupt(&mut self, io: &mut Io) {
        self.intf = 0;
        self.drive_int(io);
    }

    fn read_reg(&mut self, io: &mut Io, reg: u8) -> u8 {
        match reg {
            IODIR => self.iodir,
            IPOL => self.ipol,
            GPINTEN => self.gpinten,
            DEFVAL => self.defval,
            INTCON => self.intcon,
            IOCON => self.iocon,
            GPPU => self.gppu,
            INTF => self.intf,
            INTCAP => {
                let v = self.intcap;
                self.clear_interrupt(io);
                v
            }
            GPIO => {
                let v = self.gpio_value();
                self.clear_interrupt(io);
                v
            }
            OLAT => self.olat,
            _ => 0,
        }
    }

    fn write_reg(&mut self, io: &mut Io, reg: u8, v: u8) {
        match reg {
            IODIR => self.iodir = v,
            IPOL => self.ipol = v,
            GPINTEN => self.gpinten = v,
            DEFVAL => self.defval = v,
            INTCON => self.intcon = v,
            IOCON => self.iocon = v & IOCON_MASK,
            GPPU => self.gppu = v,
            GPIO | OLAT => self.olat = v,
            _ => return, // INTF / INTCAP are read-only, the rest reserved
        }
        self.evaluate(io);
    }

    fn advance(&mut self) {
        if self.iocon & IOCON_SEQOP == 0 {
            self.ptr = (self.ptr + 1) % NUM_REGS;
        }
    }
}

impl Device for Mcp23008 {
    fn attach(&mut self, io: &mut Io) {
        self.prev = self.gpio_value();
        self.reported = self.pin_levels();
        self.drive_int(io);
    }

    fn input(&mut self, io: &mut Io, action: &str, value: i64) -> bool {
        let Some(n) = action
            .strip_prefix("gp")
            .and_then(|n| n.parse::<u8>().ok())
            .filter(|n| *n < 8)
        else {
            return false;
        };
        let bit = 1 << n;
        match value {
            -1 => self.ext_drive &= !bit,
            0 | 1 => {
                self.ext_drive |= bit;
                self.ext_level = self.ext_level & !bit | (value as u8) << n;
            }
            _ => return false,
        }
        self.evaluate(io);
        true
    }

    fn i2c(&mut self) -> Option<&mut dyn I2cDevice> {
        Some(self)
    }
}

impl I2cDevice for Mcp23008 {
    fn address(&self) -> u8 {
        self.addr
    }

    fn start(&mut self, _io: &mut Io, read: bool) -> bool {
        if !read {
            self.expect_ptr = true;
        }
        true
    }

    fn write(&mut self, io: &mut Io, byte: u8) -> bool {
        if self.expect_ptr {
            self.ptr = byte;
            self.expect_ptr = false;
        } else {
            let reg = self.ptr;
            self.write_reg(io, reg, byte);
            self.advance();
        }
        true
    }

    fn read(&mut self, io: &mut Io, _last: bool) -> u8 {
        let v = self.read_reg(io, self.ptr);
        self.advance();
        v
    }

    fn stop(&mut self, _io: &mut Io) {
        self.expect_ptr = false;
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use pcb::{BoardEvent, Pins};

    use super::*;
    use crate::testutil::FakePins;

    struct Rig {
        gpio: FakePins,
        events: Vec<BoardEvent>,
        dev: Mcp23008,
    }

    impl Rig {
        fn new(extra: &str) -> Self {
            let sep = if extra.is_empty() { "" } else { ", " };
            let text = alloc::format!("parts.x = {{ kind = \"mcp23008\"{sep}{extra} }}");
            // Take the device off the board, as a concrete one for direct access.
            let any: Box<dyn core::any::Any> = pcb::Board::parse_with(&text, &crate::registry())
                .unwrap()
                .parts
                .remove(0)
                .device;
            let dev = *any.downcast::<Mcp23008>().unwrap();
            let mut r = Rig {
                gpio: FakePins::default(),
                events: Vec::new(),
                dev,
            };
            let mut io = Io::new(&mut r.gpio, 0, &mut r.events, 0, true);
            r.dev.attach(&mut io);
            r
        }
        fn io(&mut self) -> (&mut Mcp23008, Io<'_>) {
            (
                &mut self.dev,
                Io::new(&mut self.gpio, 7, &mut self.events, 0, false),
            )
        }
        fn write(&mut self, reg: u8, v: u8) {
            let (d, mut io) = self.io();
            d.start(&mut io, false);
            d.write(&mut io, reg);
            d.write(&mut io, v);
            d.stop(&mut io);
        }
        fn read(&mut self, reg: u8) -> u8 {
            let (d, mut io) = self.io();
            d.start(&mut io, false);
            d.write(&mut io, reg);
            d.start(&mut io, true);
            let v = d.read(&mut io, true);
            d.stop(&mut io);
            v
        }
        fn pin(&mut self, n: u8, v: i64) {
            let (d, mut io) = self.io();
            assert!(d.input(&mut io, &alloc::format!("gp{n}"), v));
        }
    }

    #[test]
    fn reset_state_and_register_file() {
        let mut r = Rig::new("");
        assert_eq!(r.dev.address(), 0x20);
        assert_eq!((r.read(IODIR), r.read(OLAT), r.read(IOCON)), (0xFF, 0, 0));
        for reg in [IODIR, IPOL, GPINTEN, DEFVAL, INTCON, GPPU, OLAT] {
            r.write(reg, 0xA5);
            assert_eq!(r.read(reg), 0xA5, "reg {reg}");
        }
        r.write(IOCON, 0xFF);
        assert_eq!(r.read(IOCON), IOCON_MASK);
        let mut fresh = Rig::new("");
        fresh.write(INTF, 0xFF); // read-only
        fresh.write(INTCAP, 0xFF);
        assert_eq!((fresh.read(INTF), fresh.read(INTCAP)), (0, 0));
    }

    #[test]
    fn pointer_auto_increments_wraps_and_byte_mode_holds() {
        let mut r = Rig::new("addr = 0x25");
        assert_eq!(r.dev.address(), 0x25);
        let (d, mut io) = r.io();
        d.start(&mut io, false);
        d.write(&mut io, OLAT); // pointer
        d.write(&mut io, 0x11); // OLAT, then wraps to IODIR
        d.write(&mut io, 0x0F);
        d.stop(&mut io);
        assert_eq!((r.dev.olat, r.dev.iodir), (0x11, 0x0F));
        r.write(IOCON, IOCON_SEQOP);
        let (d, mut io) = r.io();
        d.start(&mut io, false);
        d.write(&mut io, IPOL);
        d.write(&mut io, 1);
        d.write(&mut io, 2); // no increment: overwrites IPOL
        assert_eq!(r.dev.ipol, 2);
        // Reserved addresses read as 0 and swallow writes.
        r.write(0x0B, 0xFF);
        assert_eq!(r.read(0x0B), 0);
    }

    #[test]
    fn outputs_gpio_writes_and_pin_events() {
        let mut r = Rig::new("");
        r.write(IODIR, 0xF0);
        r.write(GPIO, 0x05); // writing GPIO sets OLAT
        assert_eq!((r.read(OLAT), r.read(GPIO)), (0x05, 0x05));
        r.pin(7, 1);
        assert_eq!(r.read(GPIO), 0x85);
        r.pin(7, -1);
        assert_eq!(r.read(GPIO), 0x05);
        let seen: Vec<_> = r.events.iter().map(|e| (e.label, e.value)).collect();
        assert_eq!(seen, [("pins", 0x05), ("pins", 0x85), ("pins", 0x05)]);
    }

    #[test]
    fn pullups_and_polarity() {
        let mut r = Rig::new("");
        assert_eq!(r.read(GPIO), 0, "floating inputs read low");
        r.write(GPPU, 0x0F);
        assert_eq!(r.read(GPIO), 0x0F);
        r.pin(0, 0); // a switch to ground
        assert_eq!(r.read(GPIO), 0x0E);
        r.write(IPOL, 0x01);
        assert_eq!(r.read(GPIO), 0x0F, "bit 0 inverted: low reads as 1");
    }

    #[test]
    fn interrupt_on_change_captures_and_clears_on_read() {
        let mut r = Rig::new("int = 16");
        // Idle: active-low push-pull driver, INT high.
        assert!(r.gpio.level(16));
        r.write(GPPU, 0xFF);
        r.write(GPINTEN, 0b0000_0100);
        r.pin(2, 0);
        assert!(!r.gpio.level(16), "INT asserted (active low)");
        assert_eq!((r.read(INTF), r.dev.intcap), (0b100, 0b1111_1011));
        // Other pins moving doesn't change the capture, and INTF keeps its flag.
        r.pin(2, 1);
        assert_eq!(r.read(INTF), 0b100);
        assert_eq!(r.read(INTCAP), 0b1111_1011);
        assert!(r.gpio.level(16), "reading INTCAP releases INT");
        assert_eq!(r.read(INTF), 0);
        // Un-enabled pins don't interrupt.
        r.pin(3, 0);
        assert!(r.gpio.level(16));
        // Reading GPIO also clears.
        r.pin(2, 0);
        assert!(!r.gpio.level(16));
        r.read(GPIO);
        assert!(r.gpio.level(16));
    }

    #[test]
    fn interrupt_against_defval_and_int_output_modes() {
        let mut r = Rig::new("int = 5");
        r.write(GPPU, 0xFF);
        r.write(DEFVAL, 0xFF);
        r.write(INTCON, 0xFF);
        r.write(GPINTEN, 0x02);
        assert!(r.gpio.level(5), "pins match DEFVAL: quiet");
        r.pin(1, 0);
        assert!(!r.gpio.level(5));
        r.read(GPIO);
        r.pin(1, 1);
        // Active high.
        r.write(IOCON, IOCON_INTPOL);
        assert!(!r.gpio.level(5), "idle level follows polarity");
        r.pin(1, 0);
        assert!(r.gpio.level(5));
        // Open drain: floats when idle (here: pulled up), pulls low when active.
        r.read(GPIO);
        r.pin(1, 1);
        r.gpio.floating |= 1 << 5; // pull-up
        r.write(IOCON, IOCON_ODR);
        assert!(r.gpio.level(5));
        r.pin(1, 0);
        assert!(!r.gpio.level(5));
        r.read(GPIO);
        assert!(r.gpio.level(5));
    }

    #[test]
    fn host_actions_are_validated() {
        let mut r = Rig::new("");
        let (d, mut io) = r.io();
        assert!(!d.input(&mut io, "gp8", 1));
        assert!(!d.input(&mut io, "gp0", 2));
        assert!(!d.input(&mut io, "press", 1));
    }

    #[test]
    fn factory_rejects_bad_parameters() {
        let e = |t: &str| {
            pcb::Board::parse_with(t, &crate::registry())
                .unwrap_err()
                .msg
        };
        assert_eq!(
            e("parts.x = { kind = \"mcp23008\", addr = 0x28 }"),
            "addr must be 0x20..0x27"
        );
        assert_eq!(
            e("parts.x = { kind = \"mcp23008\", int = 40 }"),
            "int must be a GPIO, 0..29"
        );
    }
}
