//! Microchip MCP23008 8-bit I2C port expander, and what is wired to its pins.
//!
//! Modelled: all eleven registers, the address pointer (with byte mode via
//! `IOCON.SEQOP` and wrap-around), input polarity, pull-ups, interrupt-on-
//! change against the previous value or `DEFVAL`, the `INTF`/`INTCAP`
//! capture, and the INT output (polarity and open-drain). Interrupts are
//! evaluated whenever a pin or register changes, not continuously.
//!
//! The board file can say what each pin is wired to (`pins = [...]`): a
//! momentary or latching switch (normally open or closed, to GND or VCC,
//! with contact bounce), an LED (sourced or sunk), and an external pull
//! resistor. The pin levels follow from that, and the model warns once when
//! the firmware's setup can't work with the wiring (an LED pin left as an
//! input, a switch on an output, a switch input with nothing pulling it).
//!
//! Host actions: `gp0`..`gp7` with value 1 / 0 drive the pin high / low from
//! outside (a probe, overriding whatever is wired), -1 releases it.
//! `sw0`..`sw7`, or the pin's `name`, actuate a wired switch: 1 presses it
//! (or turns a latching one on), 0 releases it. A `pins` event (value = the
//! eight pin voltages) is emitted whenever any pin voltage changes, and a
//! named pin emits its name (1 / 0) when its switch is actuated or released
//! or its LED goes on or off.
use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
};

use pcb::{BusType, Device, I2cDevice, Io, Kind, Param, Params};

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

/// What is wired to one pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Wire {
    /// Nothing the model knows of: the pin is only what the firmware and
    /// `gpN` make it.
    #[default]
    Bare,
    Switch(Switch),
    Led {
        /// Lit while the pin outputs high (an LED to GND), else while it
        /// outputs low (an LED from VCC, sunk by the pin).
        active_high: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Switch {
    /// Stays where it is put (a toggle) rather than springing back.
    pub latching: bool,
    /// Normally closed: the contact opens when actuated.
    pub nc: bool,
    /// Closing the contact connects the pin to VCC (else to GND).
    pub to_vcc: bool,
}

/// One pin's wiring, as the board file gives it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PinWiring {
    pub wire: Wire,
    pub name: Option<&'static str>,
    /// An external pull resistor: `Some(true)` up, `Some(false)` down.
    pub pull: Option<bool>,
    /// How long the switch contact chatters after it moves, in µs.
    pub bounce_us: u64,
}

pub struct Mcp23008 {
    addr: u8,
    /// The `mhz` key if set, and the cycles a µs takes at the clock.
    mhz: Option<u64>,
    cycles_per_us: u64,
    int_pin: Option<usize>,
    wiring: [PinWiring; 8],
    // Masks derived from `wiring` (bit n = GPn).
    switches: u8,
    nc: u8,
    to_vcc: u8,
    pulled: u8,
    pull_up: u8,
    leds: u8,
    leds_high: u8,

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
    /// Pins driven from outside by `gpN`, and the level they are driven to.
    ext_drive: u8,
    ext_level: u8,
    /// Switches pressed / turned on.
    actuated: u8,
    /// Switch contacts closed right now (they lag `actuated` while bouncing).
    contact: u8,
    /// Per pin, while its contact bounces: when the next flip is due and
    /// when it settles (cycles).
    next_flip: [Option<u64>; 8],
    settles_at: [u64; 8],
    rng: u32,
    /// GPIO register value at the last evaluation (for change detection).
    prev: u8,
    /// Pin voltages and lit LEDs last reported to the host.
    reported: u8,
    reported_lit: u8,
    ptr: u8,
    /// The next written byte is the register address.
    expect_ptr: bool,
    /// The firmware has set IODIR, and changed IODIR or GPPU since the
    /// wiring was last checked against them.
    configured: bool,
    unchecked: bool,
    /// Pins already warned about, per kind of problem.
    warned: [u8; 4],
}

const PIN_KEYS: &[Param] = &[
    Param::int("pin", 0, 7, "which pin, GP0..GP7"),
    Param::choice(
        "as",
        &["switch", "toggle", "led"],
        "what is wired to it: a momentary switch, a latching one, or an LED",
    )
    .optional(),
    Param::str(
        "name",
        "a name for it: an action for its switch, an event for its LED or switch",
    )
    .optional(),
    Param::choice(
        "contact",
        &["no", "nc"],
        "switch: normally open (closes when actuated) or normally closed (default no)",
    )
    .optional(),
    Param::choice(
        "to",
        &["gnd", "vcc"],
        "switch: what the closed contact connects the pin to (default gnd)",
    )
    .optional(),
    Param::int(
        "bounce_us",
        0,
        100_000,
        "switch: how long the contact chatters after it moves (default 0)",
    )
    .optional(),
    Param::choice(
        "initial",
        &["off", "on"],
        "toggle: its position at power-on (default off)",
    )
    .optional(),
    Param::choice(
        "lit",
        &["high", "low"],
        "led: the level that lights it; low for an LED from VCC sunk by the pin (default high)",
    )
    .optional(),
    Param::choice(
        "pull",
        &["up", "down"],
        "an external pull resistor on the pin",
    )
    .optional(),
];

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
        Param::tables("pins", PIN_KEYS, "what is wired to each pin").optional(),
        Param::int(
            "mhz",
            1,
            1000,
            "core clock, to turn bounce_us into cycles (default: the chip's clk_sys)",
        )
        .optional(),
    ],
    build: |p| {
        let addr = p.int("addr") as u8;
        let int_pin = p.opt_pin("int");
        let mhz = p.opt_int("mhz").map(|m| m as u64);
        let mut wiring: [PinWiring; 8] = Default::default();
        let mut initial = 0;
        let mut seen = 0u8;
        for (i, mut t) in p.tables("pins").into_iter().enumerate() {
            let n = t.int("pin") as usize;
            if seen & 1 << n != 0 {
                return Err(format!("pins[{i}]: GP{n} is listed twice"));
            }
            seen |= 1 << n;
            let (w, on) = pin_wiring(&mut t).map_err(|e| format!("pins[{i}]: {e}"))?;
            if let Some(name) = w.name
                && wiring.iter().any(|o| o.name == Some(name))
            {
                return Err(format!("pins[{i}]: the name {name:?} is used twice"));
            }
            initial |= (on as u8) << n;
            wiring[n] = w;
        }
        let mut m = Mcp23008::new(addr, int_pin, wiring, initial);
        (m.mhz, m.cycles_per_us) = (mhz, mhz.unwrap_or(125));
        Ok(Box::new(m))
    },
};

/// One `pins` entry, and whether its switch starts actuated.
fn pin_wiring(t: &mut Params) -> Result<(PinWiring, bool), String> {
    let what = t.opt_str("as").map(str::to_string);
    let is = |k: &[&str]| what.as_deref().is_some_and(|w| k.contains(&w));
    // Keys that only make sense for some kinds of wiring.
    let only = |t: &mut Params, key: &str, kinds: &[&str], what: &str| -> Result<(), String> {
        let given = match key {
            "bounce_us" => t.opt_int(key).is_some(),
            _ => t.opt_str(key).is_some(),
        };
        if given && !is(kinds) {
            return Err(format!("{key} is only for {what}"));
        }
        Ok(())
    };
    let sw = ["switch", "toggle"];
    only(t, "contact", &sw, "switches")?;
    only(t, "to", &sw, "switches")?;
    only(t, "bounce_us", &sw, "switches")?;
    only(t, "initial", &["toggle"], "toggles (as = \"toggle\")")?;
    only(t, "lit", &["led"], "LEDs")?;
    let wire = match what.as_deref() {
        None => Wire::Bare,
        Some("led") => Wire::Led {
            active_high: t.opt_str("lit") != Some("low"),
        },
        Some(kind) => Wire::Switch(Switch {
            latching: kind == "toggle",
            nc: t.opt_str("contact") == Some("nc"),
            to_vcc: t.opt_str("to") == Some("vcc"),
        }),
    };
    let name = match t.opt_str("name") {
        None => None,
        Some(n) => {
            let reserved = n == "pins"
                || ["gp", "sw"]
                    .iter()
                    .any(|p| n.strip_prefix(p).is_some_and(|d| d.parse::<u8>().is_ok()));
            if n.is_empty() || n.contains(char::is_whitespace) || n.contains(['=', ':', '@']) {
                return Err(format!("name {n:?} must be one word"));
            }
            if reserved {
                return Err(format!("name {n:?} is taken by the expander's own actions"));
            }
            if matches!(wire, Wire::Bare) {
                return Err("name needs `as`: only a switch or an LED has one".into());
            }
            // Event labels live as long as the program; boards are few.
            Some(&*String::from(n).leak())
        }
    };
    let pull = t.opt_str("pull").map(|p| p == "up");
    let bounce_us = t.opt_int("bounce_us").unwrap_or(0) as u64;
    let on = t.opt_str("initial") == Some("on");
    Ok((
        PinWiring {
            wire,
            name,
            pull,
            bounce_us,
        },
        on,
    ))
}

/// Kinds of wiring problem, each warned about once per pin.
const LED_INPUT: usize = 0;
const SWITCH_OUTPUT: usize = 1;
const SWITCH_FLOATS: usize = 2;
const SWITCH_STUCK: usize = 3;

impl Mcp23008 {
    fn new(addr: u8, int_pin: Option<usize>, wiring: [PinWiring; 8], initial: u8) -> Self {
        let mask = |f: &dyn Fn(&PinWiring) -> bool| {
            (0..8).fold(0u8, |m, n| m | (f(&wiring[n]) as u8) << n)
        };
        let sw = |w: &PinWiring| match w.wire {
            Wire::Switch(s) => Some(s),
            _ => None,
        };
        let switches = mask(&|w| sw(w).is_some());
        let nc = mask(&|w| sw(w).is_some_and(|s| s.nc));
        let to_vcc = mask(&|w| sw(w).is_some_and(|s| s.to_vcc));
        Mcp23008 {
            addr,
            mhz: None,
            cycles_per_us: 125,
            int_pin,
            switches,
            nc,
            to_vcc,
            pulled: mask(&|w| w.pull.is_some()),
            pull_up: mask(&|w| w.pull == Some(true)),
            leds: mask(&|w| matches!(w.wire, Wire::Led { .. })),
            leds_high: mask(&|w| matches!(w.wire, Wire::Led { active_high: true })),
            wiring,
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
            actuated: initial & switches,
            contact: (initial ^ nc) & switches,
            next_flip: [None; 8],
            settles_at: [0; 8],
            rng: 0x2545_F491,
            prev: 0,
            reported: 0,
            reported_lit: 0,
            ptr: 0,
            expect_ptr: false,
            configured: false,
            unchecked: false,
            warned: [0; 4],
        }
    }

    /// What the board file says is wired to `pin` (0..7).
    pub fn wiring(&self, pin: u8) -> &PinWiring {
        &self.wiring[pin as usize]
    }

    /// Switches pressed (or, for a latching one, turned on); bit n = GPn.
    pub fn actuated(&self) -> u8 {
        self.actuated
    }

    /// LEDs lit: the pin is an output at the LED's active level.
    pub fn lit(&self) -> u8 {
        self.leds & !self.iodir & !(self.olat ^ self.leds_high)
    }

    /// What holds each input pin when nothing drives it: an external pull
    /// resistor, else the internal pull-up, else nothing (floating, which
    /// reads low).
    fn idle_levels(&self) -> u8 {
        (self.pulled & self.pull_up) | (!self.pulled & self.gppu)
    }

    /// Voltage on each pin: outputs show OLAT (fighting a closed switch,
    /// they win), inputs whatever drives them: a `gpN` probe, else a closed
    /// switch contact, else the pull-ups.
    pub fn pin_levels(&self) -> u8 {
        let closed = self.contact & self.switches;
        let wired = (closed & self.to_vcc) | (!closed & self.idle_levels());
        let input = (self.ext_drive & self.ext_level) | (!self.ext_drive & wired);
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
        let lit = self.lit();
        let changed = lit ^ core::mem::replace(&mut self.reported_lit, lit);
        self.emit_named(io, changed, lit);
    }

    /// Emit the named pins among `changed` with their bit in `state`.
    fn emit_named(&self, io: &mut Io, changed: u8, state: u8) {
        for n in (0..8).filter(|n| changed >> n & 1 != 0) {
            if let Some(name) = self.wiring[n].name {
                io.emit(name, (state >> n & 1) as i64);
            }
        }
    }

    /// Press / release (turn on / off) the switch on pin `n`.
    fn actuate(&mut self, io: &mut Io, n: usize, on: bool) {
        let bit = 1 << n;
        if (self.actuated & bit != 0) == on {
            return;
        }
        self.actuated ^= bit;
        self.emit_named(io, bit, self.actuated);
        // The contact moves at once; with bounce it then chatters until
        // `bounce` after the last movement.
        self.contact = self.contact & !bit | (self.actuated ^ self.nc) & bit;
        let bounce = self.wiring[n].bounce_us * self.cycles_per_us;
        if bounce > 0 {
            self.settles_at[n] = io.cycles() + bounce;
            if self.next_flip[n].is_none() {
                self.schedule_flip(io, n);
            }
        }
        self.evaluate(io);
    }

    /// Pick when the bouncing contact on pin `n` flips next: a random
    /// fraction of its bounce time away, but not past when it settles.
    fn schedule_flip(&mut self, io: &mut Io, n: usize) {
        // xorshift32: the same chatter on every run.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        let bounce = self.wiring[n].bounce_us * self.cycles_per_us;
        let span = (bounce / 4 - bounce / 16).max(1);
        let gap = (bounce / 16 + self.rng as u64 % span).max(1);
        let at = (io.cycles() + gap).min(self.settles_at[n]);
        self.next_flip[n] = Some(at);
        io.wake_at(at);
    }

    /// Warn about wiring the firmware's pin setup can't work with, once per
    /// pin and problem. Runs when the firmware uses the pins after setting
    /// them up (setup order varies, so not during it).
    fn check_wiring(&mut self, io: &mut Io) {
        if !(self.configured && self.unchecked) {
            return;
        }
        self.unchecked = false;
        let inputs = self.iodir;
        let held = self.pulled | self.gppu;
        let idle = self.idle_levels();
        let problems = [
            self.leds & inputs,
            self.switches & !inputs,
            self.switches & inputs & !held,
            // The closed contact pulls to the level the pull-up/down holds.
            self.switches & inputs & held & !(idle ^ self.to_vcc),
        ];
        for (kind, mask) in problems.into_iter().enumerate() {
            let fresh = mask & !self.warned[kind];
            self.warned[kind] |= fresh;
            for n in (0..8).filter(|n| fresh >> n & 1 != 0) {
                let msg = self.problem(kind, n);
                io.warn(msg);
            }
        }
    }

    fn problem(&self, kind: usize, n: usize) -> String {
        let pin = match self.wiring[n].name {
            Some(name) => format!("GP{n} ({name})"),
            None => format!("GP{n}"),
        };
        let to_vcc = self.to_vcc >> n & 1 != 0;
        match kind {
            LED_INPUT => format!("{pin} drives an LED but is set as an input: the LED stays dark"),
            SWITCH_OUTPUT => format!(
                "{pin} has a switch wired to it but is set as an output: closing the switch shorts it"
            ),
            SWITCH_FLOATS if to_vcc => format!(
                "{pin}: nothing pulls the switch input down (it has no internal pull-down; \
                 add `pull = \"down\"`): it floats while the switch is open"
            ),
            SWITCH_FLOATS => format!(
                "{pin}: nothing pulls the switch input up (enable it in GPPU, or add \
                 `pull = \"up\"`): it floats while the switch is open"
            ),
            SWITCH_STUCK => {
                let level = if to_vcc { "high" } else { "low" };
                format!(
                    "{pin}: the switch pulls it {level}, where its pull resistor already holds \
                     it: it reads {level} whether the switch is open or closed"
                )
            }
            _ => unreachable!(),
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
                self.check_wiring(io);
                let v = self.intcap;
                self.clear_interrupt(io);
                v
            }
            GPIO => {
                self.check_wiring(io);
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
            IODIR => {
                self.iodir = v;
                self.configured = true;
                self.unchecked = true;
            }
            IPOL => self.ipol = v,
            GPINTEN => self.gpinten = v,
            DEFVAL => self.defval = v,
            INTCON => self.intcon = v,
            IOCON => self.iocon = v & IOCON_MASK,
            GPPU => {
                self.gppu = v;
                self.unchecked = true;
            }
            GPIO | OLAT => {
                self.olat = v;
                self.check_wiring(io);
            }
            _ => return, // INTF / INTCAP are read-only, the rest reserved
        }
        self.evaluate(io);
    }

    fn advance(&mut self) {
        if self.iocon & IOCON_SEQOP == 0 {
            self.ptr = (self.ptr + 1) % NUM_REGS;
        }
    }

    /// The switch pin an action names: `swN` or a switch's `name`.
    fn switch_for(&self, action: &str) -> Option<usize> {
        let n = match action
            .strip_prefix("sw")
            .and_then(|n| n.parse::<usize>().ok())
        {
            Some(n) => n,
            None => self.wiring.iter().position(|w| w.name == Some(action))?,
        };
        (n < 8 && self.switches >> n & 1 != 0).then_some(n)
    }
}

impl Device for Mcp23008 {
    fn clock(&mut self, _at: u64, hz: u64) {
        self.cycles_per_us = self.mhz.unwrap_or(hz / 1_000_000);
    }

    fn attach(&mut self, io: &mut Io) {
        self.prev = self.gpio_value();
        self.reported = self.pin_levels();
        self.reported_lit = self.lit();
        self.drive_int(io);
    }

    fn input(&mut self, io: &mut Io, action: &str, value: i64) -> bool {
        if let Some(n) = self.switch_for(action) {
            if !matches!(value, 0 | 1) {
                return false;
            }
            self.actuate(io, n, value == 1);
            return true;
        }
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

    fn timer(&mut self, io: &mut Io) {
        let now = io.cycles();
        for n in 0..8 {
            let Some(at) = self.next_flip[n].filter(|&at| at <= now) else {
                continue;
            };
            let bit = 1 << n;
            if at >= self.settles_at[n] {
                self.next_flip[n] = None;
                self.contact = self.contact & !bit | (self.actuated ^ self.nc) & bit;
            } else {
                self.contact ^= bit;
                self.schedule_flip(io, n);
            }
        }
        self.evaluate(io);
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

    use pcb::{BoardEvent, Pending, Pins};

    use super::*;
    use crate::testutil::FakePins;

    struct Rig {
        gpio: FakePins,
        events: Vec<BoardEvent>,
        pending: Pending,
        now: u64,
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
                pending: Pending::default(),
                now: 7,
                dev,
            };
            let mut io = Io::new(&mut r.gpio, 0, &mut r.events, 0, true);
            r.dev.attach(&mut io);
            r
        }
        fn io(&mut self) -> (&mut Mcp23008, Io<'_>) {
            (
                &mut self.dev,
                Io::new(&mut self.gpio, self.now, &mut self.events, 0, false)
                    .with_pending(&mut self.pending),
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
        fn act(&mut self, action: &str, v: i64) -> bool {
            let (d, mut io) = self.io();
            d.input(&mut io, action, v)
        }
        /// Run the device's wake-ups up to `until`, in order; returns the
        /// pin levels after each one.
        fn run_to(&mut self, until: u64) -> Vec<(u64, u8)> {
            let mut seen = Vec::new();
            while let Some(i) = (0..self.pending.timers.len())
                .filter(|&i| self.pending.timers[i].0 <= until)
                .min_by_key(|&i| self.pending.timers[i].0)
            {
                self.now = self.pending.timers.swap_remove(i).0;
                let (d, mut io) = self.io();
                d.timer(&mut io);
                seen.push((self.now, self.dev.pin_levels()));
            }
            self.now = until;
            seen
        }
        fn warnings(&mut self) -> Vec<String> {
            self.pending.warnings.drain(..).map(|(_, w)| w).collect()
        }
        fn named(&self) -> Vec<(&'static str, i64)> {
            self.events
                .iter()
                .filter(|e| e.label != "pins")
                .map(|e| (e.label, e.value))
                .collect()
        }
    }

    #[test]
    fn switches_follow_their_wiring() {
        let mut r = Rig::new(
            "pins = [{ pin = 0, as = \"switch\", name = \"ok\" }, \
                     { pin = 1, as = \"switch\", contact = \"nc\" }, \
                     { pin = 2, as = \"switch\", to = \"vcc\", pull = \"down\" }, \
                     { pin = 3, as = \"toggle\", initial = \"on\", name = \"mute\" }]",
        );
        r.write(GPPU, 0x0B);
        // Released: NO open (pulled up), NC closed to GND, pulled down, toggle on.
        assert_eq!(r.read(GPIO) & 0x0F, 0b0001);
        assert!(r.act("ok", 1));
        assert!(r.act("sw1", 1));
        assert!(r.act("sw2", 1));
        assert!(r.act("mute", 0));
        assert_eq!(r.read(GPIO) & 0x0F, 0b1110);
        assert_eq!(r.dev.actuated(), 0b0111);
        assert!(r.act("ok", 0));
        assert_eq!(r.read(GPIO) & 1, 1);
        // Only named switches report, and only real changes.
        assert!(r.act("ok", 0));
        assert_eq!(r.named(), [("ok", 1), ("mute", 0), ("ok", 0)]);
        // A probe overrides the wiring.
        r.pin(1, 0);
        assert_eq!(r.read(GPIO) >> 1 & 1, 0);
        assert!(!r.act("sw4", 1), "nothing wired to GP4");
        assert!(!r.act("ok", 2));
        assert!(!r.act("nope", 1));
    }

    #[test]
    fn external_pulls_hold_undriven_inputs() {
        let mut r = Rig::new("pins = [{ pin = 5, pull = \"up\" }, { pin = 6, pull = \"down\" }]");
        assert_eq!(r.read(GPIO), 1 << 5);
        r.write(GPPU, 1 << 6);
        assert_eq!(r.read(GPIO), 1 << 5, "the external pull-down wins");
        r.pin(5, 0);
        assert_eq!(r.read(GPIO), 0);
    }

    #[test]
    fn leds_light_only_as_driven_outputs() {
        let mut r = Rig::new(
            "pins = [{ pin = 0, as = \"led\", name = \"gate\" }, \
                     { pin = 1, as = \"led\", lit = \"low\" }]",
        );
        assert_eq!(r.dev.lit(), 0, "inputs: dark");
        r.write(OLAT, 0b10);
        r.write(IODIR, 0xFC);
        assert_eq!(r.dev.lit(), 0b00, "GP0 low, GP1 high: both dark");
        r.write(OLAT, 0b01);
        assert_eq!(r.dev.lit(), 0b11);
        r.write(OLAT, 0b00);
        assert_eq!(r.named(), [("gate", 1), ("gate", 0)]);
    }

    #[test]
    fn bouncing_contacts_chatter_then_settle() {
        let mut r = Rig::new("mhz = 1, pins = [{ pin = 4, as = \"switch\", bounce_us = 1000 }]");
        r.write(GPPU, 0xFF);
        assert!(r.act("sw4", 1));
        assert_eq!(r.dev.pin_levels() >> 4 & 1, 0, "the contact makes at once");
        let seen = r.run_to(5000);
        let levels: Vec<u8> = seen.iter().map(|&(_, l)| l >> 4 & 1).collect();
        assert!(levels.len() > 3, "it chatters: {seen:?}");
        assert!(levels.contains(&1));
        let &(last_at, last) = seen.last().unwrap();
        assert_eq!((last_at, last >> 4 & 1), (7 + 1000, 0), "settles closed");
        assert!(r.pending.timers.is_empty());
        // Every flip counts as a change for the interrupt logic.
        r.write(GPINTEN, 1 << 4);
        assert!(r.act("sw4", 0));
        assert!(r.dev.int_active());
        r.run_to(10_000);
        assert_eq!(r.dev.pin_levels() >> 4 & 1, 1, "settles open");
    }

    #[test]
    fn wiring_warnings_come_once_after_setup() {
        let mut r = Rig::new(
            "pins = [{ pin = 0, as = \"led\", name = \"gate\" }, \
                     { pin = 4, as = \"switch\" }, \
                     { pin = 5, as = \"switch\", to = \"vcc\" }, \
                     { pin = 6, as = \"switch\", pull = \"down\" }, \
                     { pin = 7, as = \"switch\" }]",
        );
        r.read(GPIO);
        assert!(r.warnings().is_empty(), "not set up yet");
        r.write(IODIR, 0x7F); // GP7 an output, GP0 left an input
        r.write(GPPU, 0x10);
        assert!(r.warnings().is_empty(), "still setting up");
        r.read(GPIO);
        let w = r.warnings();
        assert_eq!(w.len(), 4, "{w:?}");
        assert!(w[0].starts_with("GP0 (gate) drives an LED but is set as an input"));
        assert!(w[1].starts_with("GP7 has a switch wired to it but is set as an output"));
        assert!(w[2].starts_with("GP5: nothing pulls the switch input down"));
        assert!(w[3].starts_with("GP6: the switch pulls it low"));
        r.read(GPIO);
        r.write(IODIR, 0x7F);
        r.write(OLAT, 0);
        assert!(r.warnings().is_empty(), "each said once");
    }

    #[test]
    fn pin_wiring_is_validated() {
        let e = |pins: &str| {
            pcb::Board::parse_with(
                &alloc::format!("parts.x = {{ kind = \"mcp23008\", pins = [{pins}] }}"),
                &crate::registry(),
            )
            .unwrap_err()
            .msg
        };
        assert_eq!(e("{ pin = 8 }"), "pins[0]: pin must be 0..7");
        assert_eq!(
            e("{ pin = 1 }, { pin = 1 }"),
            "x: pins[1]: GP1 is listed twice"
        );
        assert_eq!(
            e("{ pin = 1, as = \"led\", contact = \"nc\" }"),
            "x: pins[0]: contact is only for switches"
        );
        assert_eq!(
            e("{ pin = 1, as = \"switch\", initial = \"on\" }"),
            "x: pins[0]: initial is only for toggles (as = \"toggle\")"
        );
        assert_eq!(
            e("{ pin = 1, bounce_us = 5 }"),
            "x: pins[0]: bounce_us is only for switches"
        );
        assert_eq!(
            e("{ pin = 1, as = \"led\", lit = \"low\", name = \"sw3\" }"),
            "x: pins[0]: name \"sw3\" is taken by the expander's own actions"
        );
        assert_eq!(
            e("{ pin = 1, name = \"a\" }"),
            "x: pins[0]: name needs `as`: only a switch or an LED has one"
        );
        assert_eq!(
            e("{ pin = 1, as = \"led\", name = \"a\" }, { pin = 2, as = \"led\", name = \"a\" }"),
            "x: pins[1]: the name \"a\" is used twice"
        );
        assert_eq!(
            e("{ pin = 1, as = \"led\", name = \"a b\" }"),
            "x: pins[0]: name \"a b\" must be one word"
        );
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
