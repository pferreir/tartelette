//! A PS/2 mouse on two open-collector lines, CLK and DATA. The mouse pulls a
//! line low or lets it float, so the firmware needs pull-ups (or the board
//! its 10k resistors), and drives the lines open-drain too: a pin the chip
//! drives high can't be pulled low by the mouse.
//!
//! The mouse makes the clock in both directions, a pulse every
//! [`HALF_US`] x 2 (12.5 kHz). Each byte is an 11-bit frame: start (0), eight
//! data bits LSB first, odd parity, stop (1).
//!
//! ```text
//! mouse to host: DATA changes while CLK is high, the host samples on the falling edge
//!   CLK  ‾‾‾‾‾\__/‾‾\__/‾‾\__ ... __/‾‾\__/‾‾‾‾‾
//!   DATA ‾‾‾\_start_X_d0__X_d1_ ... _X_stop_‾‾‾‾
//!
//! host to mouse: the host holds CLK low (>100 us), pulls DATA low and lets CLK go;
//! the mouse clocks the bits in on its rising edges, then pulls DATA low for a
//! last pulse (the acknowledge)
//! ```
//!
//! The host inhibits the mouse by holding CLK low. A byte the mouse was
//! sending is abandoned if that happens before its eleventh clock pulse, and
//! sent again once CLK is released (unless the host sends a command, which
//! empties the mouse's output). After the host releases CLK the mouse waits
//! [`RESUME_US`] before it sends.
//!
//! The protocol is the PS/2 mouse's, after Adam Chapweske's write-ups: stream
//! and remote modes, the sample rate, resolution and scaling commands, the
//! IntelliMouse wheel (ID 3, sample rates 200, 100, 80) and the Explorer
//! with its two extra buttons (ID 4, sample rates 200, 200, 80), as the
//! `model` key allows. Reset (`FF`) answers `FA`, then `AA 00` after a
//! [`BAT_US`] self-test, as the mouse also does on power-up.
//!
//! Host actions, all in screen terms: `move_x` (counts, right is positive),
//! `move_y` (counts, down is positive), `scroll` (notches, up is positive),
//! `press` and `release` (a [`Button`] number). Counts are at the default
//! resolution, 4 per mm: a higher resolution reports more of them, a lower
//! one fewer, keeping the remainder. Motion builds up between packets, which
//! go no faster than the sample rate.
//! (`mhz` is the core clock, to turn microseconds into cycles.)
use alloc::{boxed::Box, collections::VecDeque, format, vec::Vec};

use pcb::{Device, Io, Kind, Param};

/// Half a clock period, in microseconds: 12.5 kHz, inside the 10-16.7 kHz the
/// protocol allows.
pub const HALF_US: u64 = 40;
/// Time between the host releasing CLK and the mouse sending again, and
/// between the bytes it sends.
pub const RESUME_US: u64 = 100;
/// The self-test after power-up and after a reset command.
pub const BAT_US: u64 = 300_000;

/// Button numbers for the `press` and `release` actions, which are also
/// their bits: left, right and middle in the first byte of a packet, the
/// Explorer's fourth and fifth in the fourth (shifted up by 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left = 0,
    Right = 1,
    Middle = 2,
    Back = 3,
    Forward = 4,
}

const ACK: u8 = 0xFA;
const RESEND: u8 = 0xFE;
const BAT_OK: u8 = 0xAA;
const RATES: [u8; 7] = [10, 20, 40, 60, 80, 100, 200];

/// What the mouse can turn into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Model {
    /// Three buttons, ID 0.
    Basic,
    /// Adds a wheel: ID 3 after the sample rate knock 200, 100, 80.
    Wheel,
    /// Adds buttons 4 and 5: ID 4 after 200, 200, 80.
    Explorer,
}

/// What is on the wires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wire {
    /// Lines released. `wire_at` may be a pause before the next byte.
    Idle,
    /// The host holds CLK low.
    Inhibited,
    /// Clocking out `frame` (start, data, parity, stop from bit 0); `bit`
    /// is the one on DATA.
    Send { frame: u16, bit: u8 },
    /// Clocking in a byte: `bits` sampled so far, from bit 0 up.
    Recv { frame: u16, bits: u8 },
    /// DATA held low for one more pulse, to acknowledge `frame`.
    Ack { frame: u16 },
}

/// The next thing to do on the wire, at `wire_at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Put the next bit on DATA (CLK high).
    Setup,
    Fall,
    Rise,
    /// Pull DATA low to acknowledge.
    AckData,
    /// The pause is over: send what there is.
    Resume,
}

/// A command waiting for its argument byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Argument {
    Rate,
    Resolution,
}

pub struct Ps2Mouse {
    clk: usize,
    data: usize,
    model: Model,
    mhz: Option<u64>,
    cycles_per_us: u64,

    wire: Wire,
    step: Step,
    /// Wake-ups asked for: the wire's next step, the end of the self-test,
    /// the next packet the sample rate allows.
    wire_at: Option<u64>,
    bat_at: Option<u64>,
    report_at: Option<u64>,
    /// The mouse is pulling CLK or DATA low.
    clk_low: bool,
    data_low: bool,
    /// Bytes to send, and the last message sent, for `FE`.
    queue: VecDeque<u8>,
    last: Vec<u8>,
    /// Said once: the chip drives a line high while the mouse pulls it low.
    warned: bool,

    // Settings.
    id: u8,
    reporting: bool,
    remote: bool,
    wrap: bool,
    scale2: bool,
    resolution: u8,
    rate: u8,
    /// The last three sample rates set, oldest first, for the ID knocks.
    knock: [u8; 3],
    argument: Option<Argument>,

    // Motion.
    /// Motion not reported yet, in counts at 8 per mm (the highest
    /// resolution); y is up, as PS/2 has it.
    dx: i64,
    dy: i64,
    /// Wheel notches not reported yet, positive towards the user.
    dz: i64,
    /// Buttons held (bit per [`Button`]) and as last reported.
    buttons: u8,
    reported: u8,
    /// No packet before this cycle (the sample rate).
    next_report: u64,
}

pub const KIND: Kind = Kind {
    name: "ps2mouse",
    summary: "PS/2 mouse on clock and data lines",
    bus: None,
    params: &[
        Param::pin("clk", "the clock line"),
        Param::pin("data", "the data line"),
        Param::choice(
            "model",
            &["basic", "wheel", "explorer"],
            "three buttons; a wheel (IntelliMouse); a wheel and five buttons (Explorer)",
        )
        .default_str("wheel"),
        Param::int(
            "mhz",
            1,
            1000,
            "core clock, to turn time into cycles (default: the chip's clk_sys)",
        )
        .optional(),
    ],
    build: |p| {
        let model = match p.str("model") {
            "basic" => Model::Basic,
            "explorer" => Model::Explorer,
            _ => Model::Wheel,
        };
        let mhz = p.opt_int("mhz").map(|m| m as u64);
        Ok(Box::new(Ps2Mouse::new(
            p.pin("clk"),
            p.pin("data"),
            model,
            mhz,
        )))
    },
};

impl Ps2Mouse {
    fn new(clk: usize, data: usize, model: Model, mhz: Option<u64>) -> Self {
        Ps2Mouse {
            clk,
            data,
            model,
            mhz,
            cycles_per_us: mhz.unwrap_or(125),
            wire: Wire::Idle,
            step: Step::Resume,
            wire_at: None,
            bat_at: None,
            report_at: None,
            clk_low: false,
            data_low: false,
            queue: VecDeque::new(),
            last: Vec::new(),
            warned: false,
            id: 0,
            reporting: false,
            remote: false,
            wrap: false,
            scale2: false,
            resolution: 2,
            rate: 100,
            knock: [0; 3],
            argument: None,
            dx: 0,
            dy: 0,
            dz: 0,
            buttons: 0,
            reported: 0,
            next_report: 0,
        }
    }

    /// The device ID the mouse answers `F2` with: 0, 3 (wheel) or 4
    /// (Explorer).
    pub fn id(&self) -> u8 {
        self.id
    }

    pub fn model(&self) -> Model {
        self.model
    }

    /// The host has enabled data reporting (`F4`).
    pub fn reporting(&self) -> bool {
        self.reporting
    }

    /// Buttons held, a bit per [`Button`].
    pub fn buttons(&self) -> u8 {
        self.buttons
    }

    fn us(&self, us: u64) -> u64 {
        us * self.cycles_per_us
    }

    /// Ask for a wake-up `us` from now; returns the cycle to keep, which
    /// [`Device::timer`] is called with.
    fn wake(&self, io: &mut Io, us: u64) -> u64 {
        let at = (io.cycles() + self.us(us)).max(io.cycles() + 1);
        io.wake_at(at);
        at
    }

    fn next(&mut self, io: &mut Io, step: Step, us: u64) {
        self.step = step;
        self.wire_at = Some(self.wake(io, us));
    }

    fn set_clk(&mut self, io: &mut Io, low: bool) {
        self.clk_low = low;
        io.drive(self.clk, low.then_some(false));
        self.check(io, self.clk, low);
    }

    fn set_data(&mut self, io: &mut Io, low: bool) {
        self.data_low = low;
        io.drive(self.data, low.then_some(false));
        self.check(io, self.data, low);
    }

    fn check(&mut self, io: &mut Io, pin: usize, low: bool) {
        if low && io.level(pin) && !self.warned {
            self.warned = true;
            let line = if pin == self.clk { "CLK" } else { "DATA" };
            io.warn(format!(
                "{line} (GPIO {pin}) stays high while the mouse pulls it low: \
                 the chip should drive it open-drain"
            ));
        }
    }

    /// The host is holding CLK low.
    fn host_holds_clk(&self, io: &Io) -> bool {
        !self.clk_low && !io.level(self.clk)
    }

    /// Release both lines and wait for the host to let CLK go.
    fn inhibited(&mut self, io: &mut Io) {
        if self.data_low {
            self.set_data(io, false);
        }
        self.wire = Wire::Inhibited;
        self.wire_at = None;
    }

    /// Start on the next byte if the wire is free and there is one: queued,
    /// or a packet that is due.
    fn pump(&mut self, io: &mut Io) {
        if self.wire != Wire::Idle || self.wire_at.is_some() || self.host_holds_clk(io) {
            return;
        }
        if self.queue.is_empty() && self.bat_at.is_none() && self.wants_report() {
            if io.cycles() < self.next_report {
                self.sample_later(io);
                return;
            }
            let packet = self.packet();
            self.next_report = io.cycles() + self.us(1_000_000 / self.rate as u64);
            self.send(&packet);
        }
        let Some(byte) = self.queue.pop_front() else {
            return;
        };
        let parity = byte.count_ones().is_multiple_of(2) as u16;
        let frame = (byte as u16) << 1 | parity << 9 | 1 << 10;
        self.wire = Wire::Send { frame, bit: 0 };
        self.set_data(io, true);
        self.next(io, Step::Fall, HALF_US / 2);
    }

    /// Look at the motion at the next sample, no sooner than the next
    /// cycle, so that inputs which land together go in one packet.
    fn sample_later(&mut self, io: &mut Io) {
        if self.report_at.is_none() {
            let at = self.next_report.max(io.cycles() + 1);
            io.wake_at(at);
            self.report_at = Some(at);
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.queue.extend(bytes);
        self.last = bytes.to_vec();
    }

    fn wants_report(&self) -> bool {
        self.reporting
            && !self.remote
            && !self.wrap
            && (self.counts().iter().any(|&c| c != 0)
                || self.dz != 0
                || self.buttons != self.reported)
    }

    /// Motion to report at the current resolution, as (x, y).
    fn counts(&self) -> [i64; 2] {
        let shift = 3 - self.resolution;
        // Towards zero, so the remainder keeps its sign.
        [self.dx / (1 << shift), self.dy / (1 << shift)]
    }

    /// A movement packet from the motion so far, which it then forgets.
    fn packet(&mut self) -> Vec<u8> {
        let [x, y] = self.counts();
        let shift = 3 - self.resolution;
        self.dx -= x << shift;
        self.dy -= y << shift;
        let (x, y) = if self.scale2 {
            (scaled(x), scaled(y))
        } else {
            (x, y)
        };
        let (x, x_over) = (x.clamp(-256, 255), !(-256..=255).contains(&x));
        let (y, y_over) = (y.clamp(-256, 255), !(-256..=255).contains(&y));
        self.reported = self.buttons;
        let mut first = self.buttons & 7 | 1 << 3;
        first |= ((x < 0) as u8) << 4 | ((y < 0) as u8) << 5;
        first |= (x_over as u8) << 6 | (y_over as u8) << 7;
        let mut p = Vec::from([first, x as u8, y as u8]);
        match self.id {
            3 => {
                let z = self.dz.clamp(-128, 127);
                self.dz -= z;
                p.push(z as u8);
            }
            4 => {
                let z = self.dz.clamp(-8, 7);
                self.dz -= z;
                p.push(z as u8 & 0x0F | (self.buttons >> 3 & 3) << 4);
            }
            _ => self.dz = 0,
        }
        p
    }

    fn forget_motion(&mut self) {
        (self.dx, self.dy, self.dz) = (0, 0, 0);
        self.reported = self.buttons;
    }

    fn defaults(&mut self) {
        self.reporting = false;
        self.scale2 = false;
        self.resolution = 2;
        self.rate = 100;
    }

    /// The self-test: `AA 00` after [`BAT_US`].
    fn self_test(&mut self, io: &mut Io) {
        self.bat_at = Some(self.wake(io, BAT_US));
    }

    /// A byte from the host.
    fn received(&mut self, io: &mut Io, byte: u8) {
        io.emit("received", byte as i64);
        if self.wrap && byte != 0xFF && byte != 0xEC {
            self.queue.push_back(byte);
            return;
        }
        if byte != RESEND {
            // A command empties the output and, unless it reads them, the
            // motion counters.
            self.queue.clear();
            if byte != 0xEB || self.argument.is_some() {
                self.forget_motion();
            }
        }
        if let Some(arg) = self.argument.take() {
            let ok = match arg {
                Argument::Rate if RATES.contains(&byte) => {
                    self.rate = byte;
                    self.knock = [self.knock[1], self.knock[2], byte];
                    match self.knock {
                        [200, 100, 80] if self.model >= Model::Wheel => self.id = 3,
                        [200, 200, 80] if self.model >= Model::Explorer => self.id = 4,
                        _ => {}
                    }
                    true
                }
                Argument::Resolution if byte <= 3 => {
                    self.resolution = byte;
                    true
                }
                _ => false,
            };
            self.send(&[if ok { ACK } else { RESEND }]);
            return;
        }
        match byte {
            0xFF => {
                self.defaults();
                (self.id, self.remote, self.wrap, self.knock) = (0, false, false, [0; 3]);
                self.send(&[ACK]);
                self.self_test(io);
            }
            RESEND => {
                let last = self.last.clone();
                self.queue.extend(last);
            }
            0xF6 => {
                self.defaults();
                self.send(&[ACK]);
            }
            0xF5 | 0xF4 => {
                self.reporting = byte == 0xF4;
                self.send(&[ACK]);
            }
            0xF3 => {
                self.argument = Some(Argument::Rate);
                self.send(&[ACK]);
            }
            0xF2 => self.send(&[ACK, self.id]),
            0xF0 | 0xEA => {
                self.remote = byte == 0xF0;
                self.send(&[ACK]);
            }
            0xEE | 0xEC => {
                self.wrap = byte == 0xEE;
                self.send(&[ACK]);
            }
            0xEB => {
                let mut reply = Vec::from([ACK]);
                reply.extend(self.packet());
                self.send(&reply);
            }
            0xE9 => {
                // Left, middle and right, in that order from bit 2.
                let b = self.buttons;
                let held = (b & 1) << 2 | (b >> 2 & 1) << 1 | (b >> 1 & 1);
                let modes = (self.remote as u8) << 6
                    | (self.reporting as u8) << 5
                    | (self.scale2 as u8) << 4;
                self.send(&[ACK, modes | held, self.resolution, self.rate]);
            }
            0xE8 => {
                self.argument = Some(Argument::Resolution);
                self.send(&[ACK]);
            }
            0xE7 | 0xE6 => {
                self.scale2 = byte == 0xE7;
                self.send(&[ACK]);
            }
            _ => self.send(&[RESEND]),
        }
    }

    /// The wire's next step is due.
    fn wire_step(&mut self, io: &mut Io) {
        match (self.wire, self.step) {
            (Wire::Idle, Step::Resume) => self.pump(io),
            (Wire::Send { frame, bit }, Step::Setup) => {
                self.set_data(io, frame >> bit & 1 == 0);
                self.next(io, Step::Fall, HALF_US / 2);
            }
            (Wire::Recv { bits: 0, .. }, Step::Fall) if io.level(self.data) => {
                // DATA went back up before the first pulse: no start bit
                // after all, only pull-ups being turned on one at a time.
                self.wire = Wire::Idle;
                self.next(io, Step::Resume, RESUME_US);
            }
            (Wire::Send { .. } | Wire::Recv { .. } | Wire::Ack { .. }, Step::Fall) => {
                self.set_clk(io, true);
                self.next(io, Step::Rise, HALF_US);
            }
            (Wire::Send { frame, bit }, Step::Rise) => {
                self.set_clk(io, false);
                if bit == 10 {
                    // Eleven pulses: the byte is the host's even if it holds
                    // CLK now.
                    self.wire = Wire::Idle;
                    if self.host_holds_clk(io) {
                        self.inhibited(io);
                    } else {
                        self.next(io, Step::Resume, RESUME_US);
                    }
                } else if self.host_holds_clk(io) {
                    self.abandon(io, frame);
                } else {
                    self.wire = Wire::Send {
                        frame,
                        bit: bit + 1,
                    };
                    self.next(io, Step::Setup, HALF_US / 2);
                }
            }
            (Wire::Recv { frame, bits }, Step::Rise) => {
                self.set_clk(io, false);
                if self.host_holds_clk(io) {
                    self.inhibited(io);
                    return;
                }
                let frame = frame | (io.level(self.data) as u16) << bits;
                if bits == 9 {
                    self.wire = Wire::Ack { frame };
                    self.next(io, Step::AckData, HALF_US / 2);
                } else {
                    self.wire = Wire::Recv {
                        frame,
                        bits: bits + 1,
                    };
                    self.next(io, Step::Fall, HALF_US);
                }
            }
            (Wire::Ack { .. }, Step::AckData) => {
                self.set_data(io, true);
                self.next(io, Step::Fall, HALF_US / 2);
            }
            (Wire::Ack { frame }, Step::Rise) => {
                self.set_clk(io, false);
                self.set_data(io, false);
                self.wire = Wire::Idle;
                let byte = frame as u8;
                let parity_ok = !(frame & 0x1FF).count_ones().is_multiple_of(2);
                let stop_ok = frame >> 9 & 1 == 1;
                if parity_ok && stop_ok {
                    self.received(io, byte);
                } else {
                    // Ask for it again; this isn't the message `FE` repeats.
                    self.queue.clear();
                    self.queue.push_back(RESEND);
                }
                if self.host_holds_clk(io) {
                    self.inhibited(io);
                } else {
                    self.next(io, Step::Resume, RESUME_US);
                }
            }
            _ => {}
        }
    }

    /// The host took CLK before the eleventh pulse: send the byte again
    /// later.
    fn abandon(&mut self, io: &mut Io, frame: u16) {
        self.queue.push_front((frame >> 1) as u8);
        self.inhibited(io);
    }
}

/// 2:1 scaling, for small movements only (Chapweske's table).
fn scaled(c: i64) -> i64 {
    match c.abs() {
        0..=1 => c,
        2 => c.signum(),
        3 => c,
        4 => 6 * c.signum(),
        5 => 9 * c.signum(),
        _ => 2 * c,
    }
}

impl Device for Ps2Mouse {
    fn watched_pins(&self) -> u32 {
        1 << self.clk | 1 << self.data
    }

    fn clock(&mut self, _at: u64, hz: u64) {
        self.cycles_per_us = self.mhz.unwrap_or(hz / 1_000_000).max(1);
    }

    fn attach(&mut self, io: &mut Io) {
        let (clk, data, model, mhz) = (self.clk, self.data, self.model, self.mhz);
        let cycles_per_us = self.cycles_per_us;
        *self = Ps2Mouse::new(clk, data, model, mhz);
        self.cycles_per_us = cycles_per_us;
        io.drive(clk, None);
        io.drive(data, None);
        self.self_test(io);
    }

    fn pin_changed(&mut self, io: &mut Io, changed: u32) {
        if changed >> self.clk & 1 == 0 {
            return;
        }
        let held = self.host_holds_clk(io);
        match self.wire {
            // `held` only counts while the mouse leaves CLK alone, so its
            // own pulses are not mistaken for the host's. Past the eleventh
            // falling edge it is pulling CLK low itself.
            Wire::Send { frame, .. } if held => self.abandon(io, frame),
            Wire::Recv { .. } | Wire::Ack { .. } | Wire::Idle if held => self.inhibited(io),
            Wire::Inhibited if !held => {
                if !io.level(self.data) {
                    // Request to send: the host has its start bit on DATA.
                    self.wire = Wire::Recv { frame: 0, bits: 0 };
                    self.next(io, Step::Fall, HALF_US / 2);
                } else {
                    self.wire = Wire::Idle;
                    self.next(io, Step::Resume, RESUME_US);
                }
            }
            _ => {}
        }
    }

    fn input(&mut self, io: &mut Io, action: &str, value: i64) -> bool {
        match action {
            "move_x" => self.dx += 2 * value,
            "move_y" => self.dy -= 2 * value,
            "scroll" => {
                if self.id != 0 {
                    self.dz -= value
                }
            }
            "press" | "release" => {
                let max = if self.model == Model::Explorer { 4 } else { 2 };
                if !(0..=max).contains(&value) {
                    return false;
                }
                let bit = 1 << value;
                if action == "press" {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
            }
            _ => return false,
        }
        self.sample_later(io);
        true
    }

    fn timer(&mut self, io: &mut Io) {
        let now = io.cycles();
        if self.bat_at == Some(now) {
            self.bat_at = None;
            self.send(&[BAT_OK, 0x00]);
            self.pump(io);
        }
        if self.wire_at == Some(now) {
            self.wire_at = None;
            self.wire_step(io);
        }
        if self.report_at == Some(now) {
            self.report_at = None;
            self.pump(io);
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use pcb::{Board, Pins};

    use super::*;
    use crate::testutil::FakePins;

    const CLK: usize = 16;
    const DATA: usize = 17;

    /// A PS/2 host on the far side of the lines, at 1 MHz (a cycle is a
    /// microsecond). It samples DATA on every falling edge of CLK it didn't
    /// make, and while sending puts each bit on DATA as CLK falls.
    struct Rig {
        board: Board,
        pins: FakePins,
        now: u64,
        last_clk: bool,
        holding_clk: bool,
        rx: (u16, u8),
        got: Vec<u8>,
        bad_frames: usize,
        /// The frame being sent (data, parity) and the falling edges seen.
        tx: Option<(u16, u8)>,
        acked: Vec<bool>,
    }

    impl Rig {
        fn new(extra: &str) -> Self {
            let text = alloc::format!(
                "parts.mouse = {{ kind = \"ps2mouse\", clk = {CLK}, data = {DATA}, mhz = 1{extra} }}"
            );
            let board = Board::parse_with(&text, &crate::registry()).unwrap();
            let mut pins = FakePins::default();
            pins.floating = 1 << CLK | 1 << DATA;
            Self::with(board, pins)
        }

        fn with(mut board: Board, mut pins: FakePins) -> Self {
            board.attach(&mut pins);
            pins.take_changes();
            let last_clk = pins.level(CLK);
            Rig {
                board,
                pins,
                now: 0,
                last_clk,
                holding_clk: false,
                rx: (0, 0),
                got: Vec::new(),
                bad_frames: 0,
                tx: None,
                acked: Vec::new(),
            }
        }

        /// The host pulls `pin` low (`Some(false)`) or lets it go (`None`).
        fn line(&mut self, pin: usize, level: Option<bool>) {
            match level {
                Some(l) => self.pins.set_chip(pin, l),
                None => self.pins.release_chip(pin),
            }
            if pin == CLK {
                self.holding_clk = level.is_some();
            }
            self.sync();
        }

        fn sync(&mut self) {
            let changed = self.pins.take_changes();
            self.board.service(&mut self.pins, changed, self.now);
            let clk = self.pins.level(CLK);
            let fell = self.last_clk && !clk;
            self.last_clk = clk;
            if !fell || self.holding_clk {
                return;
            }
            if let Some((frame, edge)) = self.tx {
                match edge {
                    0..=8 => {
                        let one = frame >> edge & 1 != 0;
                        self.line(DATA, (!one).then_some(false));
                    }
                    9 => self.line(DATA, None),
                    _ => {
                        self.acked.push(!self.pins.level(DATA));
                        self.tx = None;
                        return;
                    }
                }
                self.tx = Some((frame, edge + 1));
                return;
            }
            let (mut frame, mut n) = self.rx;
            frame |= (self.pins.level(DATA) as u16) << n;
            n += 1;
            if n == 11 {
                let ok = frame & 1 == 0
                    && frame >> 10 & 1 == 1
                    && !(frame >> 1 & 0x1FF).count_ones().is_multiple_of(2);
                if ok {
                    self.got.push((frame >> 1) as u8);
                } else {
                    self.bad_frames += 1;
                }
                (frame, n) = (0, 0);
            }
            self.rx = (frame, n);
        }

        fn run_to(&mut self, t: u64) {
            while let Some(next) = self.board.next_timer().filter(|&n| n <= t) {
                self.now = next;
                self.board.fire_timers(&mut self.pins, next);
                self.sync();
            }
            self.now = t;
        }

        fn run(&mut self, us: u64) {
            self.run_to(self.now + us);
        }

        /// Inhibit, request to send, and clock out `byte` with `parity`
        /// (odd if None), then wait for the answer.
        fn send_with(&mut self, byte: u8, parity: Option<bool>) {
            self.line(CLK, Some(false));
            self.rx = (0, 0);
            self.run(120);
            self.line(DATA, Some(false));
            let parity = parity.unwrap_or(byte.count_ones().is_multiple_of(2));
            self.tx = Some((byte as u16 | (parity as u16) << 8, 0));
            self.line(CLK, None);
            self.run(6000);
        }

        fn send(&mut self, byte: u8) {
            self.send_with(byte, None);
        }

        /// Send `bytes` and return what came back.
        fn command(&mut self, bytes: &[u8]) -> Vec<u8> {
            self.got.clear();
            for &b in bytes {
                self.send(b);
            }
            core::mem::take(&mut self.got)
        }

        fn input(&mut self, action: &str, value: i64) {
            let r = self
                .board
                .input(&mut self.pins, "mouse", action, value, self.now);
            assert_eq!(r, pcb::InputResult::Done);
            self.sync();
        }

        fn booted(extra: &str) -> Self {
            let mut r = Rig::new(extra);
            r.run_to(BAT_US + 5000);
            assert_eq!(r.got, [BAT_OK, 0]);
            r.got.clear();
            r
        }

        fn streaming(extra: &str) -> Self {
            let mut r = Rig::booted(extra);
            assert_eq!(r.command(&[0xF4]), [ACK]);
            r
        }

        fn mouse(&self) -> &Ps2Mouse {
            self.board.device::<Ps2Mouse>("mouse").unwrap()
        }
    }

    #[test]
    fn power_up_self_test_reports_aa_00() {
        let mut r = Rig::new("");
        r.run_to(BAT_US - 1);
        assert!(r.got.is_empty());
        r.run_to(BAT_US + 5000);
        assert_eq!(r.got, [BAT_OK, 0]);
        assert_eq!(r.bad_frames, 0);
        // The lines are released after.
        assert!(r.pins.level(CLK) && r.pins.level(DATA));
    }

    #[test]
    fn the_clock_runs_at_12_5_khz() {
        let mut r = Rig::new("");
        let mut falls = Vec::new();
        let mut last = true;
        while let Some(next) = r.board.next_timer().filter(|&n| n <= BAT_US + 2000) {
            r.board.fire_timers(&mut r.pins, next);
            let clk = r.pins.level(CLK);
            if last && !clk {
                falls.push(next);
            }
            last = clk;
        }
        assert_eq!(falls.len(), 22, "two frames of eleven pulses");
        assert!(falls[..11].windows(2).all(|w| w[1] - w[0] == 2 * HALF_US));
    }

    #[test]
    fn commands_are_acknowledged_and_answered() {
        let mut r = Rig::booted("");
        assert_eq!(r.command(&[0xF2]), [ACK, 0]);
        assert_eq!(r.acked, [true]);
        assert_eq!(r.command(&[0xE9]), [ACK, 0, 2, 100]);
        assert_eq!(
            r.command(&[0xF3, 40, 0xE8, 3, 0xF4, 0xE9]),
            [ACK, ACK, ACK, ACK, ACK, ACK, 0x20, 3, 40]
        );
        assert_eq!(r.command(&[0xF6, 0xE9]), [ACK, ACK, 0, 2, 100]);
        assert_eq!(r.command(&[0x42]), [RESEND], "unknown command");
        assert_eq!(r.command(&[0xF3, 99]), [ACK, RESEND], "not a sample rate");
        assert_eq!(r.command(&[0xE8, 4]), [ACK, RESEND], "not a resolution");
        let events: Vec<_> = r.board.events.iter().map(|e| (e.label, e.value)).collect();
        assert_eq!(events[0], ("received", 0xF2));
    }

    #[test]
    fn reset_acknowledges_then_self_tests() {
        let mut r = Rig::booted("");
        r.command(&[0xF3, 200, 0xF3, 100, 0xF3, 80]);
        assert_eq!(r.mouse().id(), 3);
        assert_eq!(r.command(&[0xFF]), [ACK]);
        r.run(BAT_US);
        assert_eq!(r.got, [BAT_OK, 0]);
        assert_eq!(r.command(&[0xF2]), [ACK, 0], "the wheel is off again");
    }

    #[test]
    fn a_bad_parity_bit_asks_for_the_byte_again() {
        let mut r = Rig::booted("");
        r.got.clear();
        r.send_with(0xF2, Some(true));
        assert_eq!(r.got, [RESEND]);
        assert_eq!(r.command(&[0xF2]), [ACK, 0]);
    }

    #[test]
    fn resend_repeats_the_last_message() {
        let mut r = Rig::booted("");
        assert_eq!(r.command(&[0xE9]), [ACK, 0, 2, 100]);
        assert_eq!(r.command(&[RESEND]), [ACK, 0, 2, 100]);
    }

    #[test]
    fn motion_and_buttons_stream_once_enabled() {
        let mut r = Rig::booted("");
        r.input("move_x", 5);
        r.run(20_000);
        assert!(r.got.is_empty(), "reporting is off after power-up");
        let mut r = Rig::streaming("");
        r.input("move_x", 10);
        r.input("move_y", 5); // down on the screen is negative y
        r.run(20_000);
        assert_eq!(r.got, [0x08 | 0x20, 10, (-5i8) as u8]);
        r.got.clear();
        r.input("press", Button::Left as i64);
        r.run(20_000);
        r.input("move_x", -300);
        r.run(20_000);
        r.input("release", Button::Left as i64);
        r.run(20_000);
        assert_eq!(r.got, [0x09, 0, 0, 0x09 | 0x10 | 0x40, 0, 0, 0x08, 0, 0]);
        assert_eq!(r.mouse().buttons(), 0);
        assert_eq!(r.bad_frames, 0);
    }

    #[test]
    fn packets_wait_for_the_sample_rate() {
        let mut r = Rig::streaming("");
        assert_eq!(r.command(&[0xF3, 10]), [ACK, ACK]);
        r.input("move_x", 1);
        let first = r.now;
        r.run(5000);
        assert_eq!(r.got.len(), 3);
        r.input("move_x", 1);
        r.run(90_000);
        assert_eq!(r.got.len(), 3, "a tenth of a second between packets");
        r.input("move_x", 1); // builds up with the last
        r.run_to(first + 105_000);
        assert_eq!(r.got[3..], [0x08, 2, 0]);
    }

    #[test]
    fn resolution_and_scaling_change_the_counts() {
        let mut r = Rig::streaming("");
        assert_eq!(r.command(&[0xE8, 0]), [ACK, ACK]);
        r.input("move_x", 3);
        r.run(20_000);
        assert!(r.got.is_empty(), "less than a count at 1 per mm");
        r.input("move_x", 1);
        r.run(20_000);
        assert_eq!(r.got, [0x08, 1, 0]);
        assert_eq!(r.command(&[0xE8, 3, 0xE7]), [ACK, ACK, ACK]);
        r.input("move_y", -2); // 4 counts at 8 per mm
        r.run(20_000);
        assert_eq!(r.got, [0x08, 0, 6], "4 is 6 at 2:1");
    }

    #[test]
    fn the_wheel_knock_adds_a_fourth_byte() {
        let mut r = Rig::streaming("");
        r.command(&[0xF3, 200, 0xF3, 100, 0xF3, 80]);
        assert_eq!(r.command(&[0xF2]), [ACK, 3]);
        r.input("scroll", 2);
        r.run(20_000);
        assert_eq!(r.got, [0x08, 0, 0, (-2i8) as u8]);
        // Not an Explorer: the second knock leaves it at 3.
        r.command(&[0xF3, 200, 0xF3, 200, 0xF3, 80]);
        assert_eq!(r.command(&[0xF2]), [ACK, 3]);
        let mut r = Rig::streaming(", model = \"basic\"");
        r.command(&[0xF3, 200, 0xF3, 100, 0xF3, 80]);
        assert_eq!(r.command(&[0xF2]), [ACK, 0]);
    }

    #[test]
    fn the_explorer_reports_buttons_four_and_five() {
        let mut r = Rig::streaming(", model = \"explorer\"");
        r.command(&[0xF3, 200, 0xF3, 100, 0xF3, 80]);
        r.command(&[0xF3, 200, 0xF3, 200, 0xF3, 80]);
        assert_eq!(r.command(&[0xF2]), [ACK, 4]);
        r.input("press", Button::Forward as i64);
        r.input("scroll", -1);
        r.run(20_000);
        assert_eq!(r.got, [0x08, 0, 0, 0x20 | 1]);
    }

    #[test]
    fn buttons_the_model_lacks_are_refused() {
        let mut r = Rig::booted("");
        assert_eq!(
            r.board.input(&mut r.pins, "mouse", "press", 3, r.now),
            pcb::InputResult::Unsupported
        );
    }

    #[test]
    fn remote_mode_reports_only_when_asked() {
        let mut r = Rig::streaming("");
        assert_eq!(r.command(&[0xF0]), [ACK]);
        r.input("move_x", 7);
        r.run(20_000);
        assert!(r.got.is_empty());
        assert_eq!(r.command(&[0xEB]), [ACK, 0x08, 7, 0]);
        assert_eq!(r.command(&[0xEB]), [ACK, 0x08, 0, 0]);
    }

    #[test]
    fn wrap_mode_echoes_until_told_to_stop() {
        let mut r = Rig::booted("");
        assert_eq!(
            r.command(&[0xEE, 0x12, 0xF2, 0xEC, 0xF2]),
            [ACK, 0x12, 0xF2, ACK, ACK, 0]
        );
    }

    #[test]
    fn inhibiting_mid_byte_sends_it_again_whole() {
        let mut r = Rig::streaming("");
        r.input("move_x", 1);
        // Into the first byte, then the host holds CLK.
        r.run(5 * 2 * HALF_US);
        assert!(r.rx.1 > 0 && r.rx.1 < 11, "mid-frame");
        r.line(CLK, Some(false));
        r.rx = (0, 0);
        r.run(1000);
        assert!(r.got.is_empty());
        r.line(CLK, None);
        r.run(20_000);
        assert_eq!(r.got, [0x08, 1, 0]);
        assert_eq!(r.bad_frames, 0);
    }

    #[test]
    fn nothing_goes_out_while_the_host_holds_the_clock() {
        let mut r = Rig::streaming("");
        r.line(CLK, Some(false));
        r.input("move_x", 4);
        r.run(50_000);
        assert!(r.got.is_empty());
        r.line(CLK, None);
        r.run(RESUME_US - 1);
        assert_eq!(r.rx.1, 0, "waits before it starts");
        r.run(20_000);
        assert_eq!(r.got, [0x08, 4, 0]);
    }

    #[test]
    fn pull_ups_turned_on_one_by_one_are_no_request_to_send() {
        // Without pull-ups (the pads' reset pull-downs) both lines read low;
        // then the firmware turns on CLK's pull-up, and DATA's a little later.
        let board = Board::parse_with(
            "parts.mouse = { kind = \"ps2mouse\", clk = 16, data = 17, mhz = 1 }",
            &crate::registry(),
        )
        .unwrap();
        let mut r = Rig::with(board, FakePins::default());
        r.run(1000);
        r.pins.floating = 1 << CLK;
        r.sync();
        r.run(5);
        r.pins.floating |= 1 << DATA;
        r.sync();
        r.run_to(BAT_US + 5000);
        assert_eq!(r.got, [BAT_OK, 0]);
        assert!(r.board.events.is_empty(), "nothing received");
    }
}
