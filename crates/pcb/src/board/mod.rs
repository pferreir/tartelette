//! The PCB around the chip: devices wired to GPIO pins and buses, described
//! by a TOML board file.
//!
//! ```toml
//! name = "my-pcb"
//!
//! [buses]                   # the chip's bus controllers in use, and their pins
//! i2c1 = { sda = 2, scl = 3 }
//! spi0 = { sck = 18, mosi = 19 }
//!
//! [parts.status]            # [parts.<name>]: one device
//! kind = "led"
//! gpio = 25
//!
//! [parts.expander]
//! kind = "mcp23008"
//! bus = "i2c1"
//! addr = 0x20
//! int = 16
//!
//! [parts.screen]
//! kind = "st7789"
//! bus = "spi0"
//! cs = 9                    # SPI: the chip-select pin (none: always selected)
//! dc = 13
//!
//! [panel]                   # optional: UI layout, see [`panel`]
//! screen = { at = [0, 0], span = [2, 2] }
//!
//! [[flash]]                 # optional: what the flash holds before the firmware
//! addr = 0x101ff000
//! u16 = [63985, 57689, 51389]
//! ```
//!
//! `kind` selects a [`Kind`] from the [`Registry`] (the `pcb-devices` crate
//! has the stock ones), which says what other keys the part takes; the name
//! identifies the part to the host. Parts are attached in file order.
//! Devices that sit on a bus say which with `bus`, and the bus must be
//! declared under `[buses]`.
//!
//! Every GPIO a board names is claimed by its user: two parts (or a part and
//! a bus) on the same GPIO is an error, unless both declare the pin
//! shareable ([`Ty::SharedPin`]: an LED watching a line, an open-drain
//! interrupt).
//!
//! A `[[flash]]` entry puts values into the flash at `addr` (in the XIP
//! window, `0x10000000..0x11000000`), as whoever made the board would have
//! programmed them: factory settings such as calibration. The values are one
//! of `u8`, `u16` or `u32`, little endian. The host loads them under the
//! firmware ([`Board::flash`]).
//!
//! A shield ([`Board::add_shield`]) is a file in the same format whose parts,
//! buses and panel items go on top of the board's. Its panel items replace
//! the board's for the parts they name, and board items they cover move to
//! the right, out of the way.
//!
//! The board never does I/O itself: devices queue [`BoardEvent`]s for the
//! host to drain, and the host pokes devices with [`Board::input`].
mod doc;
pub mod panel;
pub mod params;
pub mod registry;
pub mod spec;

use alloc::{boxed::Box, format, string::String, vec::Vec};

use doc::{Key, Node, Span, V};
pub use panel::PanelItem;
pub use params::Params;
pub use registry::{PinCheck, Registry};
pub use spec::{BusType, Kind, Need, Param, Ty};

use crate::device::{Device, I2cDevice, I2cTarget, Io, Pending, Pins, SpiTarget};

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

/// A bus declared under `[buses]`, with its wiring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusDecl {
    pub name: String,
    /// (signal, GPIO), e.g. `("sck", 18)`.
    pub pins: Vec<(String, usize)>,
}

/// Who uses a GPIO.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinUse {
    pub pin: usize,
    /// `expander.int`, `spi0.sck`, ...
    pub user: String,
    pub shared: bool,
}

#[derive(Default)]
pub struct Board {
    pub name: String,
    /// Names of the shields added on top.
    pub shields: Vec<String>,
    pub parts: Vec<Part>,
    pub buses: Vec<BusDecl>,
    /// Every GPIO the board files name, and who for.
    pub pins: Vec<PinUse>,
    /// Where a host UI should put each part's controls (`[panel]`).
    pub panel: Vec<PanelItem>,
    /// Bytes the flash holds before the firmware goes on, as (address, data),
    /// from `[[flash]]`.
    pub flash: Vec<(u32, Vec<u8>)>,
    /// Events since the host last took them.
    pub events: Vec<BoardEvent>,
    /// 7-bit address -> part index, per I2C controller.
    i2c: [Vec<(u8, usize)>; 2],
    /// Device addressed by the transaction in progress, per I2C controller
    /// (a transaction can span many instructions).
    i2c_current: [Option<usize>; 2],
    /// Part indices wired to each SPI controller.
    spi: [Vec<usize>; 2],
    /// Device wake-ups and warnings not yet handled.
    pending: Pending,
}

impl core::fmt::Debug for Board {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let parts: Vec<_> = self.parts.iter().map(|p| (&p.kind, &p.name)).collect();
        f.debug_struct("Board")
            .field("name", &self.name)
            .field("parts", &parts)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line and column in the file that failed.
    pub line: usize,
    pub col: usize,
    pub msg: String,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.msg)
    }
}

/// Outcome of [`Board::input`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputResult {
    Done,
    NoSuchPart,
    /// The part exists but doesn't know that action.
    Unsupported,
}

/// The buses a board may declare, and the signals each one has.
pub const BUS_KINDS: &[(&str, &[&str])] = &[
    ("i2c0", &["sda", "scl"]),
    ("i2c1", &["sda", "scl"]),
    ("spi0", &["sck", "mosi", "miso"]),
    ("spi1", &["sck", "mosi", "miso"]),
];

const TOP_KEYS: &[&str] = &["name", "buses", "parts", "panel", "flash"];

/// The flash's XIP window, where `[[flash]]` entries may write.
const FLASH_WINDOW: core::ops::Range<i64> = 0x1000_0000..0x1100_0000;

/// The keys of a `[[flash]]` entry.
const FLASH_KEYS: &[&str] = &["addr", "u8", "u16", "u32"];

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

fn hint<'a>(word: &str, options: impl IntoIterator<Item = &'a str>) -> String {
    spec::suggest(word, options)
        .map(|s| format!(" (did you mean {s}?)"))
        .unwrap_or_default()
}

fn entries(what: &str, node: Node) -> Result<Vec<(Key, Node)>, doc::Error> {
    match node.v {
        V::Table(e) => Ok(e),
        v => Err(doc::err(
            &node.span,
            format!("{what} must be a table, not {}", v.what()),
        )),
    }
}

fn string(what: &str, node: Node) -> Result<(String, Span), doc::Error> {
    match node.v {
        V::Str(s) => Ok((s, node.span)),
        v => Err(doc::err(
            &node.span,
            format!("{what} must be a string, not {}", v.what()),
        )),
    }
}

impl Board {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a board file, building devices from the kinds in `registry`.
    pub fn parse_with(text: &str, registry: &Registry) -> Result<Board, ParseError> {
        let mut board = Board::new();
        board.load(text, registry, false)?;
        Ok(board)
    }

    /// Put a shield on the board: its buses, parts and panel items are added
    /// to the board's (errors are about the shield's text).
    pub fn add_shield(&mut self, text: &str, registry: &Registry) -> Result<(), ParseError> {
        self.load(text, registry, true)
    }

    fn load(&mut self, text: &str, registry: &Registry, shield: bool) -> Result<(), ParseError> {
        self.load_doc(text, registry, shield).map_err(|e| {
            let (line, col) = doc::line_col(text, e.span.start);
            ParseError {
                line,
                col,
                msg: e.msg,
            }
        })
    }

    fn load_doc(
        &mut self,
        text: &str,
        registry: &Registry,
        shield: bool,
    ) -> Result<(), doc::Error> {
        let root = doc::parse(text)?;
        let (mut buses, mut parts, mut panel, mut flash) = (None, None, None, None);
        let mut name = None;
        for (key, node) in entries("the file", root)? {
            match key.name.as_str() {
                "name" => name = Some(string("name", node)?.0),
                "buses" => buses = Some(node),
                "parts" => parts = Some(node),
                "panel" => panel = Some(node),
                "flash" => flash = Some(node),
                k => {
                    return Err(doc::err(
                        &key.span,
                        format!(
                            "unknown section {k} (expected {}){}",
                            TOP_KEYS.join(", "),
                            hint(k, TOP_KEYS.iter().copied())
                        ),
                    ));
                }
            }
        }
        match (shield, name) {
            (false, Some(n)) => self.name = n,
            (true, n) => self.shields.push(n.unwrap_or_else(|| "shield".into())),
            (false, None) => {}
        }
        if let Some(node) = buses {
            for (key, node) in entries("buses", node)? {
                self.declare_bus(key, node, registry)?;
            }
        }
        if let Some(node) = parts {
            for (key, node) in entries("parts", node)? {
                self.add_part(key, node, registry)?;
            }
        }
        if let Some(node) = panel {
            self.lay_out(entries("panel", node)?, shield)?;
        }
        if let Some(node) = flash {
            match node.v {
                V::Array(items) => {
                    for item in items {
                        self.add_flash(item)?;
                    }
                }
                v => {
                    return Err(doc::err(
                        &node.span,
                        format!("flash must be an array of tables ([[flash]]), not {}", v.what()),
                    ));
                }
            }
        }
        Ok(())
    }

    /// A `[[flash]]` entry: `addr`, and the values in one of `u8`, `u16`, `u32`.
    fn add_flash(&mut self, node: Node) -> Result<(), doc::Error> {
        let span = node.span.clone();
        let mut addr = None;
        let mut data: Option<Vec<u8>> = None;
        for (key, node) in entries("a flash entry", node)? {
            let width = match key.name.as_str() {
                "addr" => {
                    match node.v {
                        V::Int(n) => addr = Some((n, node.span)),
                        v => {
                            return Err(doc::err(
                                &node.span,
                                format!("addr must be a number, not {}", v.what()),
                            ));
                        }
                    }
                    continue;
                }
                "u8" => 1,
                "u16" => 2,
                "u32" => 4,
                k => {
                    return Err(doc::err(
                        &key.span,
                        format!(
                            "unknown key {k} in a flash entry (expected {}){}",
                            FLASH_KEYS.join(", "),
                            hint(k, FLASH_KEYS.iter().copied())
                        ),
                    ));
                }
            };
            let w = key.name.as_str();
            if data.is_some() {
                return Err(doc::err(
                    &key.span,
                    "a flash entry takes one of u8, u16 and u32",
                ));
            }
            let items = match node.v {
                V::Array(items) => items,
                v => {
                    return Err(doc::err(
                        &node.span,
                        format!("{w} must be an array, not {}", v.what()),
                    ));
                }
            };
            if items.is_empty() {
                return Err(doc::err(&node.span, format!("{w} has no values")));
            }
            let mut bytes = Vec::new();
            for item in items {
                let n = match item.v {
                    V::Int(n) => n,
                    v => {
                        return Err(doc::err(
                            &item.span,
                            format!("{w} values must be numbers, not {}", v.what()),
                        ));
                    }
                };
                if !(0..1i64 << (8 * width)).contains(&n) {
                    return Err(doc::err(&item.span, format!("{n} does not fit in {w}")));
                }
                bytes.extend_from_slice(&n.to_le_bytes()[..width]);
            }
            data = Some(bytes);
        }
        let (addr, addr_span) = addr.ok_or_else(|| doc::err(&span, "flash entry has no addr"))?;
        let data = data.ok_or_else(|| doc::err(&span, "flash entry has no values (u8, u16 or u32)"))?;
        if !FLASH_WINDOW.contains(&addr) || addr + data.len() as i64 > FLASH_WINDOW.end {
            return Err(doc::err(
                &addr_span,
                format!("{addr:#x}: the values do not fit in the flash (0x10000000..0x11000000)"),
            ));
        }
        self.flash.push((addr as u32, data));
        Ok(())
    }

    /// Record that `user` is wired to `pin`.
    fn claim(
        &mut self,
        pin: usize,
        user: String,
        shared: bool,
        span: &Span,
    ) -> Result<(), doc::Error> {
        if let Some(u) = self
            .pins
            .iter()
            .find(|u| u.pin == pin && !(u.shared || shared))
        {
            return Err(doc::err(
                span,
                format!("GPIO {pin} is already used by {}", u.user),
            ));
        }
        self.pins.push(PinUse { pin, user, shared });
        Ok(())
    }

    fn declare_bus(&mut self, key: Key, node: Node, registry: &Registry) -> Result<(), doc::Error> {
        let name = key.name.as_str();
        let Some((_, signals)) = BUS_KINDS.iter().find(|(n, _)| *n == name) else {
            let names: Vec<_> = BUS_KINDS.iter().map(|(n, _)| *n).collect();
            return Err(doc::err(
                &key.span,
                format!(
                    "unknown bus {name} (expected {}){}",
                    names.join(", "),
                    hint(name, names.iter().copied())
                ),
            ));
        };
        let mut pins = Vec::new();
        let mut spans = Vec::new();
        for (sig, value) in entries(name, node)? {
            if !signals.contains(&sig.name.as_str()) {
                return Err(doc::err(
                    &sig.span,
                    format!(
                        "{name} has no signal {} (it has {}){}",
                        sig.name,
                        signals.join(", "),
                        hint(&sig.name, signals.iter().copied())
                    ),
                ));
            }
            let pin = match value.v {
                V::Int(n) if (0..crate::NUM_PINS as i64).contains(&n) => n as usize,
                _ => {
                    return Err(doc::err(
                        &value.span,
                        format!("{} must be a GPIO, 0..{}", sig.name, crate::NUM_PINS - 1),
                    ));
                }
            };
            if let Some(check) = registry.pin_check {
                check(name, &sig.name, pin).map_err(|e| doc::err(&value.span, e))?;
            }
            pins.push((sig.name, pin));
            spans.push(value.span);
        }
        let decl = BusDecl {
            name: name.into(),
            pins,
        };
        if let Some(old) = self.buses.iter().find(|b| b.name == name) {
            // A shield may restate the board's wiring, but not change it.
            return if *old == decl {
                Ok(())
            } else {
                Err(doc::err(
                    &key.span,
                    format!("bus {name} is already declared with other pins"),
                ))
            };
        }
        for ((sig, pin), span) in decl.pins.iter().zip(&spans) {
            self.claim(*pin, format!("{name}.{sig}"), false, span)?;
        }
        self.buses.push(decl);
        Ok(())
    }

    fn add_part(&mut self, key: Key, node: Node, registry: &Registry) -> Result<(), doc::Error> {
        let name = key.name;
        if self.part_index(&name).is_some() {
            return Err(doc::err(
                &key.span,
                format!("there is already a part named {name}"),
            ));
        }
        let mut rest = entries(&name, node)?;
        let mut take = |k: &str| {
            rest.iter()
                .position(|(key, _)| key.name == k)
                .map(|i| rest.remove(i))
        };
        let Some((_, kind_node)) = take("kind") else {
            return Err(doc::err(&key.span, format!("part {name} has no kind")));
        };
        let (kind_name, kind_span) = string("kind", kind_node)?;
        let Some(kind) = registry.get(&kind_name) else {
            return Err(doc::err(
                &kind_span,
                format!(
                    "unknown device kind {kind_name:?}{}",
                    hint(&kind_name, registry.kinds().map(|k| k.name))
                ),
            ));
        };
        let bus = take("bus");
        let cs = if kind.bus == Some(BusType::Spi) {
            take("cs")
        } else {
            None
        };
        let mut params = Params::check(kind.name, &name, kind.params, rest, &key.span)?;
        let mut device =
            (kind.build)(&mut params).map_err(|e| doc::err(&key.span, format!("{name}: {e}")))?;
        debug_assert!(
            params.unread().is_none(),
            "{}: never read {:?}",
            kind.name,
            params.unread()
        );

        let index = self.parts.len();
        let mut cs_pin = None;
        if let Some((_, cs)) = &cs {
            match cs.v {
                V::Int(n) if (0..crate::NUM_PINS as i64).contains(&n) => cs_pin = Some(n as usize),
                _ => {
                    return Err(doc::err(
                        &cs.span,
                        format!("cs must be a GPIO, 0..{}", crate::NUM_PINS - 1),
                    ));
                }
            }
            if bus.is_none() {
                return Err(doc::err(&cs.span, "cs only means something with a bus"));
            }
        }
        if let Some((bus_key, bus)) = bus {
            let Some(want) = kind.bus else {
                return Err(doc::err(
                    &bus_key.span,
                    format!("{} does not sit on a bus", kind.name),
                ));
            };
            let (bus, span) = string("bus", bus)?;
            let Some(n) = bus_index(&bus, want.prefix()) else {
                return Err(doc::err(
                    &span,
                    format!("bus must be {0}0 or {0}1", want.prefix()),
                ));
            };
            if !self.buses.iter().any(|b| b.name == bus) {
                return Err(doc::err(
                    &span,
                    format!("bus {bus} is not declared: add it under [buses] with its pins"),
                ));
            }
            match want {
                BusType::I2c => {
                    let addr = device
                        .i2c()
                        .unwrap_or_else(|| {
                            panic!("{} is declared I2C but has no I2C side", kind.name)
                        })
                        .address();
                    if let Some((_, other)) = self.i2c[n].iter().find(|(a, _)| *a == addr) {
                        return Err(doc::err(
                            &key.span,
                            format!(
                                "address {addr:#04x} is already used on {bus} by {}",
                                self.parts[*other].name
                            ),
                        ));
                    }
                    self.i2c[n].push((addr, index));
                }
                BusType::Spi => {
                    assert!(
                        device.spi().is_some(),
                        "{} is declared SPI but has no SPI side",
                        kind.name
                    );
                    self.spi[n].push(index);
                }
            }
        }
        if let (Some(pin), Some((_, node))) = (cs_pin, &cs) {
            self.claim(pin, format!("{name}.cs"), false, &node.span)?;
        }
        for c in core::mem::take(&mut params.pins) {
            self.claim(c.pin, format!("{name}.{}", c.key), c.shared, &c.span)?;
        }
        self.parts.push(Part {
            name,
            kind: kind.name.into(),
            device,
            watch: 0,
            cs: cs_pin,
        });
        Ok(())
    }

    /// Add `[panel]` items; each part named replaces its earlier items. A
    /// shield's items push the earlier ones they cover out of the way.
    fn lay_out(&mut self, entries: Vec<(Key, Node)>, shield: bool) -> Result<(), doc::Error> {
        let mut new: Vec<(PanelItem, Span)> = Vec::new();
        for (key, node) in entries {
            let Some(part) = self.part_index(&key.name) else {
                let names = self.parts.iter().map(|p| p.name.as_str());
                return Err(doc::err(
                    &key.span,
                    format!("no part named {}{}", key.name, hint(&key.name, names)),
                ));
            };
            let span = node.span.clone();
            new.extend(
                panel::parse_items(part, node)?
                    .into_iter()
                    .map(|i| (i, span.clone())),
            );
        }
        self.panel
            .retain(|old| !new.iter().any(|(n, _)| n.part == old.part));
        let earlier = if shield {
            core::mem::take(&mut self.panel)
        } else {
            Vec::new()
        };
        for (item, span) in new {
            if let Some(o) = self.panel.iter().find(|o| o.overlaps(&item)) {
                return Err(doc::err(
                    &span,
                    format!("overlaps the panel item of {}", self.parts[o.part].name),
                ));
            }
            self.panel.push(item);
        }
        // What's left are the board's items under a shield: keep those in
        // the clear, move the others past the right edge of everything.
        for mut item in earlier {
            if self.panel.iter().any(|o| o.overlaps(&item)) {
                item.col = self
                    .panel
                    .iter()
                    .chain(core::iter::once(&item))
                    .map(|o| o.col + o.cols)
                    .max()
                    .unwrap_or(0);
                while self.panel.iter().any(|o| o.overlaps(&item)) {
                    item.col += 1;
                }
            }
            self.panel.push(item);
        }
        Ok(())
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

    /// Every device that is a `T`, with its part name.
    pub fn devices_mut<T: Device>(&mut self) -> impl Iterator<Item = (&str, &mut T)> {
        self.parts.iter_mut().filter_map(|p| {
            let any: &mut dyn core::any::Any = &mut *p.device;
            Some((p.name.as_str(), any.downcast_mut()?))
        })
    }

    /// Plug the board into the pins: devices set the pins they drive, then
    /// everyone samples the resulting levels. No events are emitted.
    pub fn attach(&mut self, pins: &mut dyn Pins) {
        for (i, p) in self.parts.iter_mut().enumerate() {
            p.watch = p.device.watched_pins();
            p.device.attach(
                &mut Io::new(pins, 0, &mut self.events, i, true).with_pending(&mut self.pending),
            );
        }
        for (i, p) in self.parts.iter_mut().enumerate() {
            if p.watch != 0 {
                p.device.pin_changed(
                    &mut Io::new(pins, 0, &mut self.events, i, true)
                        .with_pending(&mut self.pending),
                    p.watch,
                );
            }
            let active = asserted(pins, p.cs);
            if let Some(spi) = p.device.spi() {
                spi.select(
                    &mut Io::new(pins, 0, &mut self.events, i, true)
                        .with_pending(&mut self.pending),
                    active,
                );
            }
        }
    }

    /// Tell devices about pads in `changed` that moved.
    pub fn service(&mut self, pins: &mut dyn Pins, changed: u32, cycles: u64) {
        for (i, p) in self.parts.iter_mut().enumerate() {
            let hit = changed & p.watch;
            if hit != 0 {
                p.device.pin_changed(
                    &mut Io::new(pins, cycles, &mut self.events, i, false)
                        .with_pending(&mut self.pending),
                    hit,
                );
            }
            if let Some(cs) = p.cs.filter(|&cs| changed >> cs & 1 != 0) {
                let active = !pins.level(cs);
                if let Some(spi) = p.device.spi() {
                    spi.select(
                        &mut Io::new(pins, cycles, &mut self.events, i, false)
                            .with_pending(&mut self.pending),
                        active,
                    );
                }
            }
        }
    }

    /// Do something to a part from the host side.
    pub fn input(
        &mut self,
        pins: &mut dyn Pins,
        name: &str,
        action: &str,
        value: i64,
        cycles: u64,
    ) -> InputResult {
        let Some(i) = self.part_index(name) else {
            return InputResult::NoSuchPart;
        };
        let mut io =
            Io::new(pins, cycles, &mut self.events, i, false).with_pending(&mut self.pending);
        if self.parts[i].device.input(&mut io, action, value) {
            InputResult::Done
        } else {
            InputResult::Unsupported
        }
    }

    /// Warnings devices raised since the last call ([`Io::warn`]), each
    /// prefixed with its part's name.
    pub fn take_warnings(&mut self) -> Vec<String> {
        core::mem::take(&mut self.pending.warnings)
            .into_iter()
            .map(|(part, w)| format!("{}: {w}", self.parts[part].name))
            .collect()
    }

    /// The cycle of the earliest pending device wake-up.
    pub fn next_timer(&self) -> Option<u64> {
        self.pending.timers.iter().map(|t| t.0).min()
    }

    /// Call [`Device::timer`] for every wake-up due at or before `now`, in
    /// time order. Each runs with `io.cycles()` set to the cycle it asked
    /// for. (Wake-ups are always scheduled in the future, so this ends.)
    pub fn fire_timers(&mut self, pins: &mut dyn Pins, now: u64) {
        while let Some(pos) = self
            .pending
            .timers
            .iter()
            .enumerate()
            .filter(|(_, t)| t.0 <= now)
            .min_by_key(|(_, t)| t.0)
            .map(|(i, _)| i)
        {
            let (at, part) = self.pending.timers.swap_remove(pos);
            let mut io =
                Io::new(pins, at, &mut self.events, part, false).with_pending(&mut self.pending);
            self.parts[part].device.timer(&mut io);
        }
    }

    /// Press or release a `button`. Returns false if there is no such button.
    pub fn set_button(&mut self, pins: &mut dyn Pins, name: &str, down: bool, cycles: u64) -> bool {
        let action = if down { "press" } else { "release" };
        self.input(pins, name, action, 0, cycles) == InputResult::Done
    }

    /// The wire of I2C controller `n` (0 or 1) as the devices on it see it.
    pub fn i2c_bus<'a>(
        &'a mut self,
        n: usize,
        pins: &'a mut dyn Pins,
        cycles: u64,
    ) -> BoardI2c<'a> {
        BoardI2c {
            parts: &mut self.parts,
            events: &mut self.events,
            pending: &mut self.pending,
            map: &self.i2c[n],
            pins,
            cycles,
            current: &mut self.i2c_current[n],
        }
    }
}

impl Board {
    /// The wire of SPI controller `n` (0 or 1) as the devices on it see it.
    pub fn spi_bus<'a>(
        &'a mut self,
        n: usize,
        pins: &'a mut dyn Pins,
        cycles: u64,
    ) -> BoardSpi<'a> {
        BoardSpi {
            parts: &mut self.parts,
            events: &mut self.events,
            pending: &mut self.pending,
            members: &self.spi[n],
            pins,
            cycles,
        }
    }
}

/// Routes an SPI controller's frames to the devices wired to it whose chip
/// select is asserted.
pub struct BoardSpi<'a> {
    parts: &'a mut Vec<Part>,
    events: &'a mut Vec<BoardEvent>,
    pending: &'a mut Pending,
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
                miso &= spi.transfer(
                    &mut Io::new(self.pins, self.cycles, self.events, i, false)
                        .with_pending(self.pending),
                    mosi,
                    bits,
                );
            }
        }
        if bits >= 16 {
            miso
        } else {
            miso & ((1 << bits) - 1)
        }
    }
}

/// Routes an I2C controller's traffic to the devices wired to it.
pub struct BoardI2c<'a> {
    parts: &'a mut Vec<Part>,
    events: &'a mut Vec<BoardEvent>,
    pending: &'a mut Pending,
    map: &'a [(u8, usize)],
    pins: &'a mut dyn Pins,
    cycles: u64,
    current: &'a mut Option<usize>,
}

impl BoardI2c<'_> {
    fn with_current<R>(&mut self, f: impl FnOnce(&mut dyn I2cDevice, &mut Io) -> R) -> Option<R> {
        let i = (*self.current)?;
        let mut io =
            Io::new(self.pins, self.cycles, self.events, i, false).with_pending(self.pending);
        Some(f(self.parts[i].device.i2c()?, &mut io))
    }
}

impl I2cTarget for BoardI2c<'_> {
    fn start(&mut self, addr: u8, read: bool) -> bool {
        *self.current = self.map.iter().find(|(a, _)| *a == addr).map(|(_, i)| *i);
        self.with_current(|d, io| d.start(io, read))
            .unwrap_or(false)
    }
    fn write(&mut self, byte: u8) -> bool {
        self.with_current(|d, io| d.write(io, byte))
            .unwrap_or(false)
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
    use alloc::string::ToString;

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

    const PROBE: Kind = Kind {
        name: "probe",
        summary: "watches and drives a pin",
        bus: None,
        params: &[Param::pin("gpio", "the pin")],
        build: |p| Ok(Box::new(Probe { pin: p.pin("gpio") })),
    };
    const SHARED_PROBE: Kind = Kind {
        name: "tap",
        params: &[Param::shared_pin("gpio", "the pin")],
        ..PROBE
    };
    const CELL: Kind = Kind {
        name: "cell",
        summary: "one-byte I2C memory",
        bus: Some(BusType::I2c),
        params: &[Param::hex("addr", 0, 127, "address").default_int(0x20)],
        build: |p| {
            Ok(Box::new(Cell {
                addr: p.int("addr") as u8,
                val: 0,
                writes: 0,
            }))
        },
    };
    const ECHO: Kind = Kind {
        name: "echo",
        summary: "SPI echo",
        bus: Some(BusType::Spi),
        params: &[],
        build: |_| Ok(Box::new(Echo { last: 0 })),
    };

    fn registry() -> Registry {
        let mut r = Registry::empty();
        r.register(PROBE)
            .register(SHARED_PROBE)
            .register(CELL)
            .register(ECHO);
        r
    }

    fn parse(text: &str) -> Result<Board, ParseError> {
        Board::parse_with(text, &registry())
    }

    fn err(text: &str) -> String {
        parse(text).unwrap_err().msg
    }

    const BUSES: &str = "buses.i2c0 = { sda = 0, scl = 1 }\nbuses.i2c1 = { sda = 2, scl = 3 }\nbuses.spi0 = { sck = 18, mosi = 19 }\nbuses.spi1 = { sck = 10, mosi = 11 }\n";

    fn with_buses(parts: &str) -> String {
        format!("{BUSES}{parts}")
    }

    const SAMPLE: &str = r#"
# demo
name = "my pcb"

[buses]
i2c0 = { sda = 0, scl = 1 }

[parts.status]
kind = "probe"
gpio = 25   # on-board

[parts.err]
kind = "probe"
gpio = 16

[parts.sensor]
kind = "cell"
bus = "i2c0"
"#;

    #[test]
    fn parses_parts_in_file_order() {
        let b = parse(SAMPLE).unwrap();
        assert_eq!(b.name, "my pcb");
        let names: Vec<_> = b
            .parts
            .iter()
            .map(|p| (p.name.as_str(), p.kind.as_str()))
            .collect();
        assert_eq!(
            names,
            [("status", "probe"), ("err", "probe"), ("sensor", "cell")]
        );
        assert!(parse("").unwrap().parts.is_empty());
        // Inline tables keep file order too, whatever the names.
        let b = parse(
            "parts.zz = { kind = \"probe\", gpio = 1 }\nparts.aa = { kind = \"probe\", gpio = 2 }",
        )
        .unwrap();
        assert_eq!(b.parts[0].name, "zz");
    }

    #[test]
    fn reports_errors_with_line_and_column() {
        let e = parse("parts.a = { kind = \"probe\", gpio = 1 }\nparts.b = { kind = \"foo\" }")
            .unwrap_err();
        assert_eq!(
            (e.line, e.col, e.msg.as_str()),
            (2, 20, "unknown device kind \"foo\"")
        );
        assert_eq!(e.to_string(), "2:20: unknown device kind \"foo\"");
        assert_eq!(
            err("parts.a = { kind = \"prob\" }"),
            "unknown device kind \"prob\" (did you mean probe?)"
        );
        assert_eq!(err("parts.a = { gpio = 1 }"), "part a has no kind");
        assert_eq!(
            err("parts.a = { kind = \"probe\" }"),
            "missing gpio (the pin)"
        );
        assert_eq!(
            err("parts.a = { kind = \"probe\", gpio = 30 }"),
            "gpio must be a GPIO, 0..29"
        );
        assert_eq!(
            err("parts.a = { kind = \"probe\", gpio = 1, colour = 2 }"),
            "unknown parameter colour (probe takes gpio)"
        );
        assert_eq!(err("parts = 3"), "parts must be a table, not a number");
        assert_eq!(
            err("part.a = 1"),
            "unknown section part (expected name, buses, parts, panel, flash) (did you mean parts?)"
        );
        let syntax = parse("parts.a = { kind = ").unwrap_err();
        assert_eq!(
            (syntax.line, syntax.msg.as_str()),
            (1, "unclosed inline table, expected `}`"),
            "TOML syntax errors come through"
        );
    }

    #[test]
    fn pins_are_claimed_once_unless_both_users_share() {
        let probe = |name: &str, pin: usize| {
            format!("parts.{name} = {{ kind = \"probe\", gpio = {pin} }}\n")
        };
        let tap =
            |name: &str, pin: usize| format!("parts.{name} = {{ kind = \"tap\", gpio = {pin} }}\n");
        assert_eq!(
            err(&(probe("a", 4) + &probe("b", 4))),
            "GPIO 4 is already used by a.gpio"
        );
        assert!(parse(&(probe("a", 4) + &tap("b", 4) + &tap("c", 4))).is_ok());
        assert!(parse(&(tap("a", 4) + &probe("b", 4))).is_ok());
        // Bus pins and chip selects count too.
        assert_eq!(
            err(&with_buses(&probe("a", 18))),
            "GPIO 18 is already used by spi0.sck"
        );
        assert_eq!(
            err(&with_buses(
                "parts.a = { kind = \"echo\", bus = \"spi0\", cs = 5 }\nparts.b = { kind = \"echo\", bus = \"spi1\", cs = 5 }"
            )),
            "GPIO 5 is already used by a.cs"
        );
        let b = parse(&(probe("a", 4) + &tap("b", 4))).unwrap();
        assert_eq!(
            b.pins,
            [
                PinUse {
                    pin: 4,
                    user: "a.gpio".into(),
                    shared: false
                },
                PinUse {
                    pin: 4,
                    user: "b.gpio".into(),
                    shared: true
                }
            ]
        );
    }

    #[test]
    fn buses_must_be_declared_and_follow_the_pin_rules() {
        let b = parse(&with_buses(
            "parts.x = { kind = \"cell\", bus = \"i2c1\", addr = 0x21 }",
        ))
        .unwrap();
        assert_eq!(
            b.buses[1],
            BusDecl {
                name: "i2c1".into(),
                pins: [("sda".into(), 2), ("scl".into(), 3)].into()
            }
        );
        assert_eq!(b.buses[2].pins.len(), 2);
        assert_eq!(
            err("buses.i2c1 = { sdx = 2 }"),
            "i2c1 has no signal sdx (it has sda, scl) (did you mean sda?)"
        );
        assert_eq!(
            err("buses.i2c9 = {}"),
            "unknown bus i2c9 (expected i2c0, i2c1, spi0, spi1) (did you mean i2c0?)"
        );
        assert_eq!(
            err("parts.a = { kind = \"cell\", bus = \"i2c0\" }"),
            "bus i2c0 is not declared: add it under [buses] with its pins"
        );
        assert_eq!(
            err(&with_buses(
                "parts.a = { kind = \"probe\", gpio = 4, bus = \"i2c0\" }"
            )),
            "probe does not sit on a bus"
        );
        assert_eq!(
            err(&with_buses("parts.a = { kind = \"cell\", bus = \"spi0\" }")),
            "bus must be i2c0 or i2c1"
        );
        assert_eq!(
            err(&with_buses(
                "parts.a = { kind = \"cell\", bus = \"i2c0\" }\nparts.b = { kind = \"cell\", bus = \"i2c0\" }"
            )),
            "address 0x20 is already used on i2c0 by a"
        );
        assert_eq!(
            err(&with_buses(
                "parts.a = { kind = \"cell\", bus = \"i2c0\", cs = 5 }"
            )),
            "unknown parameter cs (cell takes addr)"
        );
        assert_eq!(
            err("parts.a = { kind = \"echo\", cs = 5 }"),
            "cs only means something with a bus"
        );
        // Same address on different controllers is fine; no bus, no routing.
        assert!(parse(&with_buses("parts.a = { kind = \"cell\", bus = \"i2c0\" }\nparts.b = { kind = \"cell\", bus = \"i2c1\" }")).is_ok());
        assert_eq!(
            parse("parts.a = { kind = \"echo\" }")
                .unwrap()
                .spi_bus(0, &mut FakePins::default(), 0)
                .transfer(1, 8),
            0xFF
        );

        let mut r = registry();
        r.set_pin_check(|bus, sig, pin| {
            if pin % 2 == 0 {
                Ok(())
            } else {
                Err(format!("{bus} {sig} can't be on GPIO {pin}"))
            }
        });
        let e = Board::parse_with("[buses]\nspi0 = { sck = 2, mosi = 3 }", &r).unwrap_err();
        assert_eq!(
            (e.line, e.col, e.msg.as_str()),
            (2, 26, "spi0 mosi can't be on GPIO 3")
        );
    }

    #[test]
    fn flash_entries_become_little_endian_bytes_at_their_address() {
        let b = parse(
            "[[flash]]\naddr = 0x101ff000\nu16 = [1, 0x0203]\n\
             [[flash]]\naddr = 0x10000010\nu32 = [0xA1B2C3D4]\n\
             [[flash]]\naddr = 0x10000020\nu8 = [7, 8, 0xff]\n",
        )
        .unwrap();
        assert_eq!(
            b.flash,
            [
                (0x101f_f000, alloc::vec![1, 0, 3, 2]),
                (0x1000_0010, alloc::vec![0xD4, 0xC3, 0xB2, 0xA1]),
                (0x1000_0020, alloc::vec![7, 8, 0xFF]),
            ]
        );
    }

    #[test]
    fn bad_flash_entries_are_refused() {
        let flash = |body: &str| err(&format!("[[flash]]\n{body}"));
        assert_eq!(
            err("flash = 1"),
            "flash must be an array of tables ([[flash]]), not a number"
        );
        assert_eq!(flash("u8 = [1]"), "flash entry has no addr");
        assert_eq!(
            flash("addr = 0x10000000"),
            "flash entry has no values (u8, u16 or u32)"
        );
        assert_eq!(flash("addr = 0x10000000\nu8 = []"), "u8 has no values");
        assert_eq!(flash("addr = 0x10000000\nu8 = [256]"), "256 does not fit in u8");
        assert_eq!(flash("addr = 0x10000000\nu16 = [-1]"), "-1 does not fit in u16");
        assert_eq!(
            flash("addr = 0x10000000\nu16 = [\"x\"]"),
            "u16 values must be numbers, not a string"
        );
        assert_eq!(
            flash("addr = 0x10000000\nu8 = [1]\nu16 = [1]"),
            "a flash entry takes one of u8, u16 and u32"
        );
        assert_eq!(
            flash("addr = 0x10000000\nu17 = [1]"),
            "unknown key u17 in a flash entry (expected addr, u8, u16, u32) (did you mean u16?)"
        );
        assert!(flash("addr = 0x20000000\nu8 = [1]").contains("do not fit in the flash"));
        assert!(
            flash("addr = 0x10ffffff\nu8 = [1, 2]").contains("do not fit in the flash"),
            "runs off the end"
        );
        let e = parse("name = \"x\"\n[[flash]]\naddr = 0x10000000\nu8 = [300]").unwrap_err();
        assert_eq!(e.line, 4);
    }

    #[test]
    fn custom_kinds_can_be_registered_and_overridden() {
        let mut r = Registry::empty();
        let text = "parts.p = { kind = \"probe\", gpio = 1 }";
        assert!(Board::parse_with(text, &r).is_err());
        r.register(PROBE);
        let b = Board::parse_with(text, &r).unwrap();
        assert_eq!(b.parts[0].kind, "probe");
        assert!(b.device::<Probe>("p").is_some());
        assert!(b.device::<Cell>("p").is_none());
        r.register(Kind {
            name: "probe",
            ..CELL
        }); // override
        assert!(Board::parse_with("parts.a = { kind = \"probe\" }", &r).is_ok());
        assert!(r.kinds().any(|k| k.name == "probe"));
    }

    #[test]
    fn shields_add_parts_and_rearrange_the_panel() {
        let base = "name = \"base\"\nbuses.spi0 = { sck = 18, mosi = 19 }\nparts.led = { kind = \"probe\", gpio = 25 }\npanel.led = { at = [0, 0], span = [2, 2] }";
        let mut b = parse(base).unwrap();
        let shield = "name = \"hat\"\nbuses.spi0 = { sck = 18, mosi = 19 }\nparts.screen = { kind = \"echo\", bus = \"spi0\", cs = 9 }\n[panel]\nscreen = { at = [0, 0], span = [3, 3] }\nled = { at = [3, 0] }";
        b.add_shield(shield, &registry()).unwrap();
        assert_eq!(
            (b.name.as_str(), b.shields.as_slice()),
            ("base", ["hat".to_string()].as_slice())
        );
        assert_eq!(
            b.parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["led", "screen"]
        );
        let at: Vec<_> = b.panel.iter().map(|i| (i.part, i.col, i.row)).collect();
        assert_eq!(at, [(1, 0, 0), (0, 3, 0)]);

        let shield_err = |s: &str| {
            parse(base)
                .unwrap()
                .add_shield(s, &registry())
                .unwrap_err()
                .msg
        };
        assert_eq!(
            shield_err("parts.led = { kind = \"probe\", gpio = 3 }"),
            "there is already a part named led"
        );
        assert_eq!(
            shield_err("parts.x = { kind = \"probe\", gpio = 25 }"),
            "GPIO 25 is already used by led.gpio"
        );
        assert_eq!(
            shield_err("buses.spi0 = { sck = 18 }"),
            "bus spi0 is already declared with other pins"
        );
        assert_eq!(
            shield_err(
                "parts.x = { kind = \"probe\", gpio = 3 }\npanel.x = { at = [0, 0] }\npanel.led = { at = [0, 0] }"
            ),
            "overlaps the panel item of x"
        );

        // Board items a shield covers without naming them move right.
        let mut b = parse(base).unwrap();
        b.add_shield(
            "parts.x = { kind = \"probe\", gpio = 3 }\npanel.x = { at = [1, 1], span = [2, 1] }",
            &registry(),
        )
        .unwrap();
        let at: Vec<_> = b
            .panel
            .iter()
            .map(|i| (b.parts[i.part].name.as_str(), i.col, i.row))
            .collect();
        assert_eq!(at, [("x", 1, 1), ("led", 3, 0)]);
    }

    #[test]
    fn service_reports_only_watched_pins_that_moved() {
        let mut pins = FakePins::default();
        let mut b = parse(
            "parts.a = { kind = \"probe\", gpio = 5 }\nparts.b = { kind = \"probe\", gpio = 6 }",
        )
        .unwrap();
        b.attach(&mut pins);
        assert!(b.events.is_empty(), "attach is quiet");

        pins.chip = 1 << 5 | 1 << 6;
        b.service(&mut pins, 1 << 5, 100);
        b.service(&mut pins, 0, 200);
        b.service(&mut pins, 1 << 7, 300);
        let ev: Vec<_> = b
            .events
            .iter()
            .map(|e| (e.cycles, e.part, e.value))
            .collect();
        assert_eq!(ev, [(100, 0, 1)]);
    }

    #[test]
    fn input_reports_why_it_failed_and_drives_pins() {
        let mut pins = FakePins::default();
        let mut b =
            parse("parts.p = { kind = \"probe\", gpio = 3 }\nparts.c = { kind = \"cell\" }")
                .unwrap();
        assert_eq!(
            b.input(&mut pins, "x", "set", 1, 0),
            InputResult::NoSuchPart
        );
        assert_eq!(
            b.input(&mut pins, "c", "set", 1, 0),
            InputResult::Unsupported
        );
        assert_eq!(
            b.input(&mut pins, "p", "wiggle", 1, 0),
            InputResult::Unsupported
        );
        assert_eq!(b.input(&mut pins, "p", "set", 1, 0), InputResult::Done);
        assert!(pins.level(3));
        assert!(!b.set_button(&mut pins, "p", true, 0)); // no such action on a probe
        assert!(!b.set_button(&mut pins, "nope", true, 0));
    }

    #[test]
    fn i2c_transactions_span_bus_handles_and_route_by_controller() {
        let mut pins = FakePins::default();
        let mut b = parse(&with_buses(
            "parts.e = { kind = \"cell\", bus = \"i2c0\", addr = 0x22 }",
        ))
        .unwrap();
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
        let mut b = parse(&with_buses(
            "parts.a = { kind = \"echo\", bus = \"spi0\", cs = 5 }\nparts.b = { kind = \"echo\", bus = \"spi0\", cs = 6 }\nparts.c = { kind = \"echo\", bus = \"spi1\" }",
        ))
        .unwrap();
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
        // `b` selected too (a wiring mistake): replies AND together, 0x56 & 0x00.
        pins.chip &= !(1 << 6);
        assert_eq!(b.spi_bus(0, &mut pins, 0).transfer(0x78, 8), 0x00);
        // No CS pin at all means permanently selected; other controller unaffected.
        assert_eq!(b.spi_bus(1, &mut pins, 0).transfer(0x9A, 8), 0x00);
        assert_eq!(b.spi_bus(1, &mut pins, 0).transfer(0xBC, 8), 0x9A);
    }

    #[test]
    fn spi_select_edges_are_reported_to_devices() {
        let mut pins = FakePins::default();
        let mut b = parse(&with_buses("parts.a = { kind = \"echo\", bus = \"spi0\", cs = 5 }\nparts.c = { kind = \"echo\", bus = \"spi1\" }")).unwrap();
        pins.chip = 1 << 5;
        b.attach(&mut pins);
        assert!(b.events.is_empty(), "attach is quiet");
        pins.chip = 0;
        b.service(&mut pins, 1 << 5, 100);
        pins.chip = 1 << 5;
        b.service(&mut pins, 1 << 5, 200);
        b.service(&mut pins, 1 << 7, 300); // some other pin
        let ev: Vec<_> = b
            .events
            .iter()
            .map(|e| (e.cycles, e.part, e.label, e.value))
            .collect();
        assert_eq!(ev, [(100, 0, "select", 1), (200, 0, "select", 0)]);
    }

    const TWO: &str =
        "parts.a = { kind = \"probe\", gpio = 1 }\nparts.b = { kind = \"probe\", gpio = 2 }\n";

    #[test]
    fn the_panel_places_parts_on_a_grid() {
        let b = parse(&format!("{TWO}[panel]\nb = {{ at = [1, 0], span = [2, 3], label = \"Big one\", rotate = 90, keys = [\"A\", \"D\"] }}\na = {{ at = [0, 2] }}")).unwrap();
        assert_eq!(b.panel.len(), 2);
        let big = &b.panel[0];
        assert_eq!(
            (big.part, big.col, big.row, big.cols, big.rows),
            (1, 1, 0, 2, 3)
        );
        assert_eq!(big.label.as_deref(), Some("Big one"));
        assert_eq!(
            big.options,
            [
                ("rotate".into(), "90".into()),
                ("keys".into(), "A,D".into())
            ]
        );
        assert_eq!(
            (big.option("rotate"), big.option("nope")),
            (Some("90"), None)
        );
        assert_eq!(
            (
                b.panel[1].part,
                b.panel[1].cols,
                b.panel[1].rows,
                b.panel[1].label.as_deref()
            ),
            (0, 1, 1, None)
        );
        assert!(parse(SAMPLE).unwrap().panel.is_empty());
        let twice = parse(&format!(
            "{TWO}panel.a = [{{ at = [0, 0] }}, {{ at = [5, 5], x = 1 }}]"
        ))
        .unwrap();
        assert_eq!(twice.panel.len(), 2, "a part may appear twice");
    }

    #[test]
    fn panel_errors() {
        assert_eq!(err("panel.c = { at = [0, 0] }"), "no part named c");
        assert_eq!(
            err(&format!("{TWO}panel.aa = {{ at = [0, 0] }}")),
            "no part named aa (did you mean a?)"
        );
        assert_eq!(
            err(&format!("{TWO}panel.a = {{ span = [1, 1] }}")),
            "missing at = [COLUMN, ROW]"
        );
        assert_eq!(
            err(&format!("{TWO}panel.a = {{ at = [1] }}")),
            "at must be [N, M], each 0..63"
        );
        assert_eq!(
            err(&format!("{TWO}panel.a = {{ at = [0, 64] }}")),
            "at must be [N, M], each 0..63"
        );
        assert_eq!(
            err(&format!("{TWO}panel.a = {{ at = [0, 0], span = [0, 1] }}")),
            "span must be [N, M], each 1..64"
        );
        assert_eq!(
            err(&format!(
                "{TWO}panel.a = {{ at = [0, 0], x = {{ y = 1 }} }}"
            )),
            "x must be a number, string, true/false or an array of them, not a table"
        );
        let overlap = parse(&format!(
            "{TWO}[panel]\na = {{ at = [0, 0], span = [2, 2] }}\nb = {{ at = [1, 1] }}"
        ))
        .unwrap_err();
        assert_eq!(
            (overlap.line, overlap.msg.as_str()),
            (5, "overlaps the panel item of a")
        );
        // Touching edges is fine.
        assert!(
            parse(&format!(
                "{TWO}[panel]\na = {{ at = [0, 0], span = [2, 2] }}\nb = {{ at = [2, 0] }}"
            ))
            .is_ok()
        );
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

    const TICKER: Kind = Kind {
        name: "ticker",
        summary: "ticks",
        bus: None,
        params: &[Param::int("period", 1, 1_000_000, "cycles between ticks").default_int(100)],
        build: |p| {
            Ok(Box::new(Ticker {
                period: p.int("period") as u64,
                n: 0,
            }))
        },
    };

    #[test]
    fn device_timers_fire_in_order_at_their_own_cycle() {
        let mut r = registry();
        r.register(TICKER);
        let mut pins = FakePins::default();
        let mut b = Board::parse_with("parts.a = { kind = \"ticker\", period = 100 }\nparts.b = { kind = \"ticker\", period = 150 }", &r).unwrap();
        b.attach(&mut pins);
        assert_eq!(b.next_timer(), Some(100));
        b.fire_timers(&mut pins, 99);
        assert!(b.events.is_empty(), "nothing due yet");
        // Jumping ahead fires everything due, oldest first, each stamped with its own cycle.
        b.fire_timers(&mut pins, 320);
        let ev: Vec<_> = b
            .events
            .iter()
            .map(|e| (e.cycles, e.part, e.value))
            .collect();
        assert_eq!(
            ev,
            [
                (100, 0, 1),
                (150, 1, 1),
                (200, 0, 2),
                (300, 1, 2),
                (300, 0, 3)
            ]
        );
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
        r.register(Kind {
            name: "eager",
            summary: "",
            bus: None,
            params: &[],
            build: |_| Ok(Box::new(Eager)),
        });
        let mut pins = FakePins::default();
        let mut b = Board::parse_with("parts.e = { kind = \"eager\" }", &r).unwrap();
        b.attach(&mut pins);
        assert_eq!(b.next_timer(), Some(1));
        b.fire_timers(&mut pins, 5);
        let at: Vec<i64> = b.events.iter().map(|e| e.value).collect();
        assert_eq!(
            at,
            [1, 2, 3, 4, 5],
            "one per cycle, never stuck at the same instant"
        );
    }

    #[test]
    fn the_json_schema_covers_every_kind() {
        let schema = spec::json_schema(&registry());
        for k in [
            "\"probe\"",
            "\"cell\"",
            "\"echo\"",
            "\"tap\"",
            "\"spi1\"",
            "\"additionalProperties\": false",
        ] {
            assert!(schema.contains(k), "{k} missing");
        }
        assert_eq!(schema.matches('{').count(), schema.matches('}').count());
    }
}
