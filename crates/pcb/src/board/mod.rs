//! The PCB around the chip: devices wired to GPIO pins and buses, described
//! by a small line-based text file.
//!
//! ```text
//! # comments start with '#'
//! board my-pcb
//! i2c1   sda=2 scl=3                       # optional: documents the bus wiring
//! led    status gpio=25                    # <kind> <name> key=value...
//! button user   gpio=2 active=low
//! mcp23008 expander bus=i2c1 addr=0x20 int=16
//! st7789 screen bus=spi0 cs=9 dc=13 rst=17   # SPI: cs=N is the chip-select pin
//! panel  screen at=0,0 span=2,2            # optional: UI layout, see [`panel`]
//! ```
//!
//! Every line after `board` is `<kind> <name> key=value...`. The kind selects
//! a factory from the [`Registry`] (the `pcb-devices` crate has the
//! stock ones), the name identifies the part to the host. Devices that sit on a
//! bus say which with `bus=`.
//!
//! The board never does I/O itself: devices queue [`BoardEvent`]s for the
//! host to drain, and the host pokes devices with [`Board::input`].
pub mod panel;
pub mod params;
pub mod registry;

use crate::device::{Device, I2cDevice, I2cTarget, Io, Pins, SpiTarget};
use alloc::{boxed::Box, format, string::String, vec::Vec};
pub use panel::PanelItem;
pub use params::Params;
pub use registry::{Factory, Registry};

/// Something observable happened on the board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardEvent {
    /// Core cycle count when it happened.
    pub cycles: u64,
    /// Index into [`Board::parts`].
    pub part: usize,
    /// What happened, as the device words it (`"on"`, `"pressed"`, ...).
    pub label: &'static str,
    /// New state or value (1/0 for switches and LEDs).
    pub value: i64,
}

/// One device on the board.
pub struct Part {
    pub name: String,
    pub kind: String,
    pub device: Box<dyn Device>,
    /// Pins this part wants change notifications for.
    watch: u32,
    /// On an SPI bus: the chip-select pin (None = permanently selected).
    cs: Option<usize>,
}

/// A bus declared in the board file, with its wiring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusDecl {
    pub name: String,
    pub pins: Vec<(String, usize)>,
}

#[derive(Default)]
pub struct Board {
    pub name: String,
    pub parts: Vec<Part>,
    pub buses: Vec<BusDecl>,
    /// Where a host UI should put each part's controls (`panel` lines).
    pub panel: Vec<PanelItem>,
    /// Events since the host last took them.
    pub events: Vec<BoardEvent>,
    /// 7-bit address -> part index, per I2C controller.
    i2c: [Vec<(u8, usize)>; 2],
    /// Device addressed by the transaction in progress, per I2C controller
    /// (a transaction can span many instructions).
    i2c_current: [Option<usize>; 2],
    /// Part indices wired to each SPI controller.
    spi: [Vec<usize>; 2],
    /// Pending device wake-ups: (cycle, part index).
    timers: Vec<(u64, usize)>,
}

impl core::fmt::Debug for Board {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let parts: Vec<_> = self.parts.iter().map(|p| (&p.kind, &p.name)).collect();
        f.debug_struct("Board").field("name", &self.name).field("parts", &parts).finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line number.
    pub line: usize,
    pub msg: String,
}

/// Outcome of [`Board::input`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputResult {
    Done,
    NoSuchPart,
    /// The part exists but doesn't know that action.
    Unsupported,
}

/// Names a bus line may declare itself with, and the pin keys it allows.
const BUS_KINDS: &[(&str, &[&str])] = &[
    ("i2c0", &["sda", "scl"]),
    ("i2c1", &["sda", "scl"]),
    ("spi0", &["sck", "mosi", "miso"]),
    ("spi1", &["sck", "mosi", "miso"]),
];

fn bus_index(bus: &str, prefix: &str) -> Option<usize> {
    match bus.strip_prefix(prefix)? {
        "0" => Some(0),
        "1" => Some(1),
        _ => None,
    }
}

/// Is `pin` low (asserted for an active-low chip select)?
fn asserted(pins: &dyn Pins, cs: Option<usize>) -> bool {
    cs.is_none_or(|p| !pins.level(p))
}

impl Board {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a board file, building devices from the kinds in `registry`.
    pub fn parse_with(text: &str, registry: &Registry) -> Result<Board, ParseError> {
        let mut board = Board::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            board.parse_line(line, i + 1, registry).map_err(|msg| ParseError { line: i + 1, msg })?;
        }
        Ok(board)
    }

    fn parse_line(&mut self, line: &str, number: usize, registry: &Registry) -> Result<(), String> {
        let mut words = line.split_whitespace();
        let kind = words.next().unwrap_or_default();
        if kind == "board" {
            self.name = words.collect::<Vec<_>>().join(" ");
            return Ok(());
        }
        if kind == "panel" {
            let name = words.next().ok_or("missing part name")?;
            let part = self.part_index(name).ok_or_else(|| format!("no part named {name:?} (declare it before its panel line)"))?;
            let params = Params::new(kind, name).with_words(words)?;
            let item = panel::parse_item(params, part, number, &self.panel)?;
            self.panel.push(item);
            return Ok(());
        }
        if let Some((_, keys)) = BUS_KINDS.iter().find(|(n, _)| *n == kind) {
            return self.declare_bus(kind, keys, words);
        }
        let factory = registry.get(kind).ok_or_else(|| format!("unknown device kind {kind:?}"))?;
        let name = words.next().ok_or("missing part name")?;
        if name.contains('=') {
            return Err("missing part name".into());
        }
        if self.part_index(name).is_some() {
            return Err("duplicate part name".into());
        }
        let mut params = Params::new(kind, name).with_words(words)?;
        let bus = params.opt_str("bus").map(String::from);
        let mut device = factory(&mut params)?;

        let index = self.parts.len();
        let cs = match bus {
            Some(bus) => self.attach_to_bus(&bus, &mut *device, index, &mut params)?,
            None => None,
        };
        params.finish()?;
        self.parts.push(Part { name: name.into(), kind: kind.into(), device, watch: 0, cs });
        Ok(())
    }

    fn declare_bus<'a>(&mut self, name: &str, keys: &[&str], words: impl Iterator<Item = &'a str>) -> Result<(), String> {
        if self.buses.iter().any(|b| b.name == name) {
            return Err(format!("bus {name} declared twice"));
        }
        let mut params = Params::new(name, name).with_words(words)?;
        let mut pins = Vec::new();
        for key in keys {
            if let Some(pin) = params.opt_pin(key)? {
                pins.push((String::from(*key), pin));
            }
        }
        params.finish()?;
        self.buses.push(BusDecl { name: name.into(), pins });
        Ok(())
    }

    /// Wire `device` to `bus`. Returns the chip-select pin for SPI devices.
    fn attach_to_bus(&mut self, bus: &str, device: &mut dyn Device, part: usize, params: &mut Params) -> Result<Option<usize>, String> {
        if let Some(n) = bus_index(bus, "i2c") {
            let addr = device.i2c().ok_or_else(|| format!("this device is not an I2C device (bus={bus})"))?.address();
            if self.i2c[n].iter().any(|(a, _)| *a == addr) {
                return Err(format!("address {addr:#04x} is already used on {bus}"));
            }
            self.i2c[n].push((addr, part));
            Ok(None)
        } else if let Some(n) = bus_index(bus, "spi") {
            if device.spi().is_none() {
                return Err(format!("this device is not an SPI device (bus={bus})"));
            }
            let cs = params.opt_pin("cs")?;
            if let Some(other) = cs.and_then(|c| self.spi[n].iter().find(|&&i| self.parts[i].cs == Some(c))) {
                return Err(format!("chip select {} is already used by {:?}", cs.unwrap_or_default(), self.parts[*other].name));
            }
            self.spi[n].push(part);
            Ok(cs)
        } else {
            Err(format!("unknown bus {bus:?} (expected i2c0, i2c1, spi0 or spi1)"))
        }
    }

    pub fn part_index(&self, name: &str) -> Option<usize> {
        self.parts.iter().position(|p| p.name == name)
    }

    /// The concrete device called `name`, if it is a `T`. For hosts that
    /// want to look inside a device (a display's framebuffer, ...).
    pub fn device<T: Device>(&self, name: &str) -> Option<&T> {
        let any: &dyn core::any::Any = &*self.parts.get(self.part_index(name)?)?.device;
        any.downcast_ref()
    }

    pub fn device_mut<T: Device>(&mut self, name: &str) -> Option<&mut T> {
        let i = self.part_index(name)?;
        let any: &mut dyn core::any::Any = &mut *self.parts[i].device;
        any.downcast_mut()
    }

    /// Plug the board into the pins: devices set the pins they drive, then
    /// everyone samples the resulting levels. No events are emitted.
    pub fn attach(&mut self, pins: &mut dyn Pins) {
        for (i, p) in self.parts.iter_mut().enumerate() {
            p.watch = p.device.watched_pins();
            p.device.attach(&mut Io::new(pins, 0, &mut self.events, i, true).with_timers(&mut self.timers));
        }
        for (i, p) in self.parts.iter_mut().enumerate() {
            if p.watch != 0 {
                p.device.pin_changed(&mut Io::new(pins, 0, &mut self.events, i, true).with_timers(&mut self.timers), p.watch);
            }
            let active = asserted(pins, p.cs);
            if let Some(spi) = p.device.spi() {
                spi.select(&mut Io::new(pins, 0, &mut self.events, i, true).with_timers(&mut self.timers), active);
            }
        }
    }

    /// Tell devices about pads in `changed` that moved.
    pub fn service(&mut self, pins: &mut dyn Pins, changed: u32, cycles: u64) {
        for (i, p) in self.parts.iter_mut().enumerate() {
            let hit = changed & p.watch;
            if hit != 0 {
                p.device.pin_changed(&mut Io::new(pins, cycles, &mut self.events, i, false).with_timers(&mut self.timers), hit);
            }
            if let Some(cs) = p.cs.filter(|&cs| changed >> cs & 1 != 0) {
                let active = !pins.level(cs);
                if let Some(spi) = p.device.spi() {
                    spi.select(&mut Io::new(pins, cycles, &mut self.events, i, false).with_timers(&mut self.timers), active);
                }
            }
        }
    }

    /// Do something to a part from the host side.
    pub fn input(&mut self, pins: &mut dyn Pins, name: &str, action: &str, value: i64, cycles: u64) -> InputResult {
        let Some(i) = self.part_index(name) else { return InputResult::NoSuchPart };
        let mut io = Io::new(pins, cycles, &mut self.events, i, false).with_timers(&mut self.timers);
        if self.parts[i].device.input(&mut io, action, value) {
            InputResult::Done
        } else {
            InputResult::Unsupported
        }
    }

    /// The cycle of the earliest pending device wake-up.
    pub fn next_timer(&self) -> Option<u64> {
        self.timers.iter().map(|t| t.0).min()
    }

    /// Call [`Device::timer`] for every wake-up due at or before `now`, in
    /// time order. Each runs with `io.cycles()` set to the cycle it asked
    /// for. (Wake-ups are always scheduled in the future, so this ends.)
    pub fn fire_timers(&mut self, pins: &mut dyn Pins, now: u64) {
        while let Some(pos) = self.timers.iter().enumerate().filter(|(_, t)| t.0 <= now).min_by_key(|(_, t)| t.0).map(|(i, _)| i) {
            let (at, part) = self.timers.swap_remove(pos);
            let mut io = Io::new(pins, at, &mut self.events, part, false).with_timers(&mut self.timers);
            self.parts[part].device.timer(&mut io);
        }
    }

    /// Press or release a `button`. Returns false if there is no such button.
    pub fn set_button(&mut self, pins: &mut dyn Pins, name: &str, down: bool, cycles: u64) -> bool {
        let action = if down { "press" } else { "release" };
        self.input(pins, name, action, 0, cycles) == InputResult::Done
    }

    /// The wire of I2C controller `n` (0 or 1) as the devices on it see it.
    pub fn i2c_bus<'a>(&'a mut self, n: usize, pins: &'a mut dyn Pins, cycles: u64) -> BoardI2c<'a> {
        BoardI2c {
            parts: &mut self.parts,
            events: &mut self.events,
            timers: &mut self.timers,
            map: &self.i2c[n],
            pins,
            cycles,
            current: &mut self.i2c_current[n],
        }
    }
}

impl Board {
    /// The wire of SPI controller `n` (0 or 1) as the devices on it see it.
    pub fn spi_bus<'a>(&'a mut self, n: usize, pins: &'a mut dyn Pins, cycles: u64) -> BoardSpi<'a> {
        BoardSpi { parts: &mut self.parts, events: &mut self.events, timers: &mut self.timers, members: &self.spi[n], pins, cycles }
    }
}

/// Routes an SPI controller's frames to the devices wired to it whose chip
/// select is asserted.
pub struct BoardSpi<'a> {
    parts: &'a mut Vec<Part>,
    events: &'a mut Vec<BoardEvent>,
    timers: &'a mut Vec<(u64, usize)>,
    members: &'a [usize],
    pins: &'a mut dyn Pins,
    cycles: u64,
}

impl SpiTarget for BoardSpi<'_> {
    fn transfer(&mut self, mosi: u16, bits: u8) -> u16 {
        // MISO is open-collector style: floating (nobody selected) reads high,
        // and several selected devices AND together.
        let mut miso = u16::MAX;
        for &i in self.members {
            let part = &mut self.parts[i];
            if !asserted(&*self.pins, part.cs) {
                continue;
            }
            if let Some(spi) = part.device.spi() {
                miso &= spi.transfer(&mut Io::new(self.pins, self.cycles, self.events, i, false).with_timers(self.timers), mosi, bits);
            }
        }
        if bits >= 16 { miso } else { miso & ((1 << bits) - 1) }
    }
}

/// Routes an I2C controller's traffic to the devices wired to it.
pub struct BoardI2c<'a> {
    parts: &'a mut Vec<Part>,
    events: &'a mut Vec<BoardEvent>,
    timers: &'a mut Vec<(u64, usize)>,
    map: &'a [(u8, usize)],
    pins: &'a mut dyn Pins,
    cycles: u64,
    current: &'a mut Option<usize>,
}

impl BoardI2c<'_> {
    fn with_current<R>(&mut self, f: impl FnOnce(&mut dyn I2cDevice, &mut Io) -> R) -> Option<R> {
        let i = (*self.current)?;
        let mut io = Io::new(self.pins, self.cycles, self.events, i, false).with_timers(self.timers);
        Some(f(self.parts[i].device.i2c()?, &mut io))
    }
}

impl I2cTarget for BoardI2c<'_> {
    fn start(&mut self, addr: u8, read: bool) -> bool {
        *self.current = self.map.iter().find(|(a, _)| *a == addr).map(|(_, i)| *i);
        self.with_current(|d, io| d.start(io, read)).unwrap_or(false)
    }
    fn write(&mut self, byte: u8) -> bool {
        self.with_current(|d, io| d.write(io, byte)).unwrap_or(false)
    }
    fn read(&mut self, last: bool) -> u8 {
        self.with_current(|d, io| d.read(io, last)).unwrap_or(0xFF)
    }
    fn stop(&mut self) {
        self.with_current(|d, io| d.stop(io));
        *self.current = None;
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::SpiDevice;

    /// Levels the test sets by hand plus whatever devices drive on top.
    #[derive(Default)]
    struct FakePins {
        chip: u32,
        ext: [Option<bool>; crate::NUM_PINS],
    }

    impl Pins for FakePins {
        fn level(&self, pin: usize) -> bool {
            self.ext[pin].unwrap_or(self.chip >> pin & 1 != 0)
        }
        fn drive(&mut self, pin: usize, level: Option<bool>) {
            self.ext[pin] = level;
        }
    }

    /// Reports `"high"`/`"low"` when `gpio` moves; the `set` action drives it.
    struct Probe {
        pin: usize,
    }

    impl Device for Probe {
        fn watched_pins(&self) -> u32 {
            1 << self.pin
        }
        fn pin_changed(&mut self, io: &mut Io, _changed: u32) {
            io.emit("level", io.level(self.pin) as i64);
        }
        fn input(&mut self, io: &mut Io, action: &str, value: i64) -> bool {
            if action != "set" {
                return false;
            }
            io.drive(self.pin, Some(value != 0));
            true
        }
    }

    fn probe(p: &mut Params) -> Result<Box<dyn Device>, String> {
        Ok(Box::new(Probe { pin: p.pin("gpio")? }))
    }

    /// I2C device at `addr` (default 0x20) holding one byte: a write stores
    /// its second byte, a read returns it.
    struct Cell {
        addr: u8,
        val: u8,
        writes: u8,
    }

    impl Device for Cell {
        fn i2c(&mut self) -> Option<&mut dyn I2cDevice> {
            Some(self)
        }
    }

    impl I2cDevice for Cell {
        fn address(&self) -> u8 {
            self.addr
        }
        fn start(&mut self, _io: &mut Io, _read: bool) -> bool {
            self.writes = 0;
            true
        }
        fn write(&mut self, _io: &mut Io, byte: u8) -> bool {
            if self.writes == 1 {
                self.val = byte;
            }
            self.writes += 1;
            true
        }
        fn read(&mut self, _io: &mut Io, _last: bool) -> u8 {
            self.val
        }
        fn stop(&mut self, _io: &mut Io) {}
    }

    /// SPI device answering every frame with the previous one, and reporting
    /// its chip select.
    struct Echo {
        last: u16,
    }

    impl Device for Echo {
        fn spi(&mut self) -> Option<&mut dyn SpiDevice> {
            Some(self)
        }
    }

    impl SpiDevice for Echo {
        fn select(&mut self, io: &mut Io, active: bool) {
            io.emit("select", active as i64);
        }
        fn transfer(&mut self, _io: &mut Io, mosi: u16, _bits: u8) -> u16 {
            core::mem::replace(&mut self.last, mosi)
        }
    }

    fn echo(_: &mut Params) -> Result<Box<dyn Device>, String> {
        Ok(Box::new(Echo { last: 0 }))
    }

    fn cell(p: &mut Params) -> Result<Box<dyn Device>, String> {
        let addr = p.opt_int("addr", 0, 127)?.unwrap_or(0x20) as u8;
        Ok(Box::new(Cell { addr, val: 0, writes: 0 }))
    }

    fn registry() -> Registry {
        let mut r = Registry::empty();
        r.register("probe", probe).register("cell", cell).register("echo", echo);
        r
    }

    fn parse(text: &str) -> Result<Board, ParseError> {
        Board::parse_with(text, &registry())
    }

    const SAMPLE: &str = "\
# demo
board my pcb
probe status gpio=25   # on-board
probe err gpio=16

cell sensor bus=i2c0
";

    #[test]
    fn parses_parts_and_defaults() {
        let b = parse(SAMPLE).unwrap();
        assert_eq!(b.name, "my pcb");
        let names: Vec<_> = b.parts.iter().map(|p| (p.name.as_str(), p.kind.as_str())).collect();
        assert_eq!(names, [("status", "probe"), ("err", "probe"), ("sensor", "cell")]);
        assert!(parse("").unwrap().parts.is_empty());
    }

    #[test]
    fn reports_errors_with_line_numbers() {
        let e = |t: &str| parse(t).unwrap_err();
        assert_eq!(e("probe a gpio=1\nfoo b").line, 2);
        assert_eq!(e("foo b").msg, "unknown device kind \"foo\"");
        assert_eq!(e("probe").msg, "missing part name");
        assert_eq!(e("probe a").msg, "missing gpio=N");
        assert_eq!(e("probe a gpio=x").msg, "gpio must be a number");
        assert_eq!(e("probe a gpio=30").msg, "gpio must be 0..29");
        assert_eq!(e("probe a gpio=1 colour=red").msg, "unknown parameter colour");
        assert_eq!(e("probe a gpio=1\ncell a").msg, "duplicate part name");
    }

    #[test]
    fn bus_lines_and_bus_attachment() {
        let b = parse("i2c1 sda=2 scl=3\nspi0 sck=18 mosi=19\ncell x bus=i2c1 addr=0x21").unwrap();
        assert_eq!(b.buses[0], BusDecl { name: "i2c1".into(), pins: [("sda".into(), 2), ("scl".into(), 3)].into() });
        assert_eq!(b.buses[1].pins.len(), 2);
        let e = |t: &str| parse(t).unwrap_err().msg;
        assert_eq!(e("i2c1 sda=2\ni2c1 sda=4"), "bus i2c1 declared twice");
        assert_eq!(e("i2c1 sdx=2"), "unknown parameter sdx");
        assert_eq!(e("probe a gpio=1 bus=i2c0"), "this device is not an I2C device (bus=i2c0)");
        assert_eq!(e("cell a bus=i2c9"), "unknown bus \"i2c9\" (expected i2c0, i2c1, spi0 or spi1)");
        assert_eq!(e("cell a bus=i2c0\ncell b bus=i2c0"), "address 0x20 is already used on i2c0");
        // Same address on different controllers is fine.
        assert!(parse("cell a bus=i2c0\ncell b bus=i2c1").is_ok());
    }

    #[test]
    fn custom_kinds_can_be_registered_and_overridden() {
        let mut r = Registry::empty();
        assert!(Board::parse_with("probe p gpio=1", &r).is_err());
        r.register("probe", probe);
        let b = Board::parse_with("probe p gpio=1", &r).unwrap();
        assert_eq!(b.parts[0].kind, "probe");
        assert!(b.device::<Probe>("p").is_some());
        assert!(b.device::<Cell>("p").is_none());
        r.register("probe", cell); // override
        assert!(Board::parse_with("probe a", &r).is_ok());
        assert!(r.kinds().any(|k| k == "probe"));
    }

    #[test]
    fn service_reports_only_watched_pins_that_moved() {
        let mut pins = FakePins::default();
        let mut b = parse("probe a gpio=5\nprobe b gpio=6").unwrap();
        b.attach(&mut pins);
        assert!(b.events.is_empty(), "attach is quiet");

        pins.chip = 1 << 5 | 1 << 6;
        b.service(&mut pins, 1 << 5, 100);
        b.service(&mut pins, 0, 200);
        b.service(&mut pins, 1 << 7, 300);
        let ev: Vec<_> = b.events.iter().map(|e| (e.cycles, e.part, e.value)).collect();
        assert_eq!(ev, [(100, 0, 1)]);
    }

    #[test]
    fn input_reports_why_it_failed_and_drives_pins() {
        let mut pins = FakePins::default();
        let mut b = parse("probe p gpio=3\ncell c").unwrap();
        assert_eq!(b.input(&mut pins, "x", "set", 1, 0), InputResult::NoSuchPart);
        assert_eq!(b.input(&mut pins, "c", "set", 1, 0), InputResult::Unsupported);
        assert_eq!(b.input(&mut pins, "p", "wiggle", 1, 0), InputResult::Unsupported);
        assert_eq!(b.input(&mut pins, "p", "set", 1, 0), InputResult::Done);
        assert!(pins.level(3));
        assert!(!b.set_button(&mut pins, "p", true, 0)); // no such action on a probe
        assert!(!b.set_button(&mut pins, "nope", true, 0));
    }

    #[test]
    fn i2c_transactions_span_bus_handles_and_route_by_controller() {
        let mut pins = FakePins::default();
        let mut b = parse("cell e bus=i2c0 addr=0x22").unwrap();
        b.attach(&mut pins);
        // Each handle is short-lived (one per instruction); the transaction isn't.
        assert!(b.i2c_bus(0, &mut pins, 0).start(0x22, false));
        assert!(b.i2c_bus(0, &mut pins, 1).write(0x0A));
        assert!(b.i2c_bus(0, &mut pins, 2).write(0x5A));
        b.i2c_bus(0, &mut pins, 3).stop();
        assert!(b.i2c_bus(0, &mut pins, 4).start(0x22, false));
        assert!(b.i2c_bus(0, &mut pins, 5).write(0x0A));
        assert!(b.i2c_bus(0, &mut pins, 6).start(0x22, true));
        assert_eq!(b.i2c_bus(0, &mut pins, 7).read(true), 0x5A);
        b.i2c_bus(0, &mut pins, 8).stop();
        // After STOP nothing is addressed, and unknown addresses / the other controller NACK.
        assert!(!b.i2c_bus(0, &mut pins, 9).write(1));
        assert!(!b.i2c_bus(0, &mut pins, 9).start(0x30, false));
        assert!(!b.i2c_bus(1, &mut pins, 9).start(0x22, false));
    }

    #[test]
    fn spi_frames_reach_only_selected_devices() {
        let mut pins = FakePins::default();
        let mut b = parse("echo a bus=spi0 cs=5\necho b bus=spi0 cs=6\necho c bus=spi1").unwrap();
        // CS lines idle high.
        pins.chip = 1 << 5 | 1 << 6;
        b.attach(&mut pins);
        // Nothing selected on spi0: MISO floats high, masked to the frame size.
        assert_eq!(b.spi_bus(0, &mut pins, 0).transfer(0x12, 8), 0xFF);
        assert_eq!(b.spi_bus(0, &mut pins, 0).transfer(0x12, 12), 0xFFF);
        assert_eq!(b.spi_bus(0, &mut pins, 0).transfer(0x12, 16), 0xFFFF);
        // Device `a` selected: it echoes the previous frame, `b` hears nothing.
        pins.chip &= !(1 << 5);
        b.service(&mut pins, 1 << 5, 10);
        assert_eq!(b.spi_bus(0, &mut pins, 0).transfer(0x34, 8), 0x00);
        assert_eq!(b.spi_bus(0, &mut pins, 0).transfer(0x56, 8), 0x34);
        // `b` selected too (a wiring mistake): replies AND together.
        pins.chip &= !(1 << 6);
        assert_eq!(b.spi_bus(0, &mut pins, 0).transfer(0x78, 8), 0x56 & 0x00);
        // No CS pin at all means permanently selected; other controller unaffected.
        assert_eq!(b.spi_bus(1, &mut pins, 0).transfer(0x9A, 8), 0x00);
        assert_eq!(b.spi_bus(1, &mut pins, 0).transfer(0xBC, 8), 0x9A);
    }

    #[test]
    fn spi_select_edges_are_reported_to_devices() {
        let mut pins = FakePins::default();
        let mut b = parse("echo a bus=spi0 cs=5\necho c bus=spi1").unwrap();
        pins.chip = 1 << 5;
        b.attach(&mut pins);
        assert!(b.events.is_empty(), "attach is quiet");
        pins.chip = 0;
        b.service(&mut pins, 1 << 5, 100);
        pins.chip = 1 << 5;
        b.service(&mut pins, 1 << 5, 200);
        b.service(&mut pins, 1 << 7, 300); // some other pin
        let ev: Vec<_> = b.events.iter().map(|e| (e.cycles, e.part, e.label, e.value)).collect();
        assert_eq!(ev, [(100, 0, "select", 1), (200, 0, "select", 0)]);
    }

    #[test]
    fn panel_lines_place_parts_on_a_grid() {
        let b = parse("probe a gpio=1\nprobe b gpio=2\npanel b at=1,0 span=2,3 label=Big_one rotate=90\npanel a at=0,2").unwrap();
        assert_eq!(b.panel.len(), 2);
        let big = &b.panel[0];
        assert_eq!((big.part, big.col, big.row, big.cols, big.rows), (1, 1, 0, 2, 3));
        assert_eq!(big.label.as_deref(), Some("Big one"));
        assert_eq!(big.options, [("rotate".into(), "90".into())]);
        assert_eq!((big.option("rotate"), big.option("nope"), big.line), (Some("90"), None, 3));
        assert_eq!((b.panel[1].part, b.panel[1].cols, b.panel[1].rows, b.panel[1].label.as_deref()), (0, 1, 1, None));
        assert!(parse(SAMPLE).unwrap().panel.is_empty());
    }

    #[test]
    fn panel_errors() {
        let e = |t: &str| parse(t).unwrap_err();
        assert_eq!(e("panel a at=0,0").msg, "no part named \"a\" (declare it before its panel line)");
        assert_eq!(e("probe a gpio=1\npanel a").msg, "missing at=COL,ROW");
        assert_eq!(e("probe a gpio=1\npanel a at=1").msg, "at must be N,M");
        assert_eq!(e("probe a gpio=1\npanel a at=0,64").msg, "at values must be 0..63");
        assert_eq!(e("probe a gpio=1\npanel a at=0,0 span=0,1").msg, "span values must be 1..64");
        assert_eq!(parse("probe a gpio=1\npanel a at=0,0\npanel a at=5,5 x=1").unwrap().panel.len(), 2, "a part may appear twice");
        let overlap = e("probe a gpio=1\nprobe b gpio=2\npanel a at=0,0 span=2,2\npanel b at=1,1");
        assert_eq!((overlap.line, overlap.msg.as_str()), (4, "overlaps the panel item on line 3"));
        // Touching edges is fine.
        assert!(parse("probe a gpio=1\nprobe b gpio=2\npanel a at=0,0 span=2,2\npanel b at=2,0").is_ok());
    }

    #[test]
    fn spi_attachment_errors() {
        let e = |t: &str| parse(t).unwrap_err().msg;
        assert_eq!(e("cell a bus=spi0"), "this device is not an SPI device (bus=spi0)");
        assert_eq!(e("echo a bus=i2c0"), "this device is not an I2C device (bus=i2c0)");
        assert_eq!(e("echo a bus=spi0 cs=5\necho b bus=spi0 cs=5"), "chip select 5 is already used by \"a\"");
        assert_eq!(e("echo a bus=spi0 cs=40"), "cs must be 0..29");
        assert_eq!(e("cell a bus=i2c0 cs=5"), "unknown parameter cs");
        // The same CS pin on different controllers is a board design problem too, but not ours to police.
        assert!(parse("echo a bus=spi0 cs=5\necho b bus=spi1 cs=5").is_ok());
        // No bus, no routing.
        assert!(parse("echo a").unwrap().spi_bus(0, &mut FakePins::default(), 0).transfer(1, 8) == 0xFF);
    }

    /// Toggles its `level` event every `period` cycles starting at `period`.
    struct Ticker {
        period: u64,
        n: i64,
    }

    impl Device for Ticker {
        fn attach(&mut self, io: &mut Io) {
            io.wake_at(self.period);
        }
        fn timer(&mut self, io: &mut Io) {
            self.n += 1;
            io.emit("tick", self.n);
            io.wake_at(io.cycles() + self.period);
        }
    }

    fn ticker(p: &mut Params) -> Result<Box<dyn Device>, String> {
        Ok(Box::new(Ticker { period: p.opt_int("period", 1, 1_000_000)?.unwrap_or(100) as u64, n: 0 }))
    }

    #[test]
    fn device_timers_fire_in_order_at_their_own_cycle() {
        let mut r = registry();
        r.register("ticker", ticker);
        let mut pins = FakePins::default();
        let mut b = Board::parse_with("ticker a period=100\nticker b period=150", &r).unwrap();
        b.attach(&mut pins);
        assert_eq!(b.next_timer(), Some(100));
        b.fire_timers(&mut pins, 99);
        assert!(b.events.is_empty(), "nothing due yet");
        // Jumping ahead fires everything due, oldest first, each stamped with its own cycle.
        b.fire_timers(&mut pins, 320);
        let ev: Vec<_> = b.events.iter().map(|e| (e.cycles, e.part, e.value)).collect();
        assert_eq!(ev, [(100, 0, 1), (150, 1, 1), (200, 0, 2), (300, 1, 2), (300, 0, 3)]);
        assert_eq!(b.next_timer(), Some(450).min(Some(400)));
    }

    #[test]
    fn wake_requests_for_now_or_the_past_fire_on_the_next_cycle() {
        struct Eager;
        impl Device for Eager {
            fn attach(&mut self, io: &mut Io) {
                io.wake_at(0);
            }
            fn timer(&mut self, io: &mut Io) {
                io.emit("eager", io.cycles() as i64);
                io.wake_at(0);
            }
        }
        let mut r = Registry::empty();
        r.register("eager", |_| Ok(Box::new(Eager)));
        let mut pins = FakePins::default();
        let mut b = Board::parse_with("eager e", &r).unwrap();
        b.attach(&mut pins);
        assert_eq!(b.next_timer(), Some(1));
        b.fire_timers(&mut pins, 5);
        let at: Vec<i64> = b.events.iter().map(|e| e.value).collect();
        assert_eq!(at, [1, 2, 3, 4, 5], "one per cycle, never stuck at the same instant");
    }
}
