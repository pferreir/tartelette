//! A speaker behind a coupling capacitor and an amplifier, on a DAC output
//! or on a GPIO: what firmware streams to the DAC as samples, or toggles on
//! the pin as PWM or PDM (from PIO, say), is heard as sound.
//!
//! The sound itself is made on the host side (the runtime's synth), since
//! the core does no I/O. On a DAC it follows the DAC's `dac` events. On a
//! GPIO the part does the low-pass filtering a speaker's RC filter would:
//! it averages the pin's level over windows of 1/`rate` seconds, timed by
//! the core clock, and emits `level` (0..65535, the fraction of the window
//! the pin was high) at the end of each window whose average differs from
//! the last one. A steady PWM duty, or a pin left alone, costs nothing.
//!
//! Host actions: `mute`, `unmute`, and `volume` (0..100 percent). Emits
//! `volume` with what is heard (0 while muted) whenever that changes.
use alloc::{boxed::Box, format, string::String};

use pcb::{Device, Io, Kind, Param};

pub enum Source {
    /// A DAC, as the `dac` key names it: `PART` or `PART.CHANNEL` (the
    /// host checks it against the board).
    Dac(String),
    Pin(Pwm),
}

pub struct Speaker {
    pub source: Source,
    /// 0..=100.
    pub volume: i64,
    pub muted: bool,
}

/// A pin's level averaged over fixed windows of the core clock.
pub struct Pwm {
    pub pin: usize,
    /// Whether `mhz` fixes the clock, and its cycles a second.
    fixed: bool,
    cycles_per_sec: u64,
    rate: u64,
    /// The windows are counted from window `origin.0`, which starts at cycle
    /// `origin.1` (both 0 until the clock changes); the window before it
    /// started at `before`.
    origin: (u64, u64),
    before: u64,
    high: bool,
    /// The window being measured, the cycle it was last looked at, and the
    /// cycles it was high until then.
    window: u64,
    since: u64,
    acc: u64,
    /// The last level emitted, 0..=65535.
    pub level: u16,
    timer: bool,
}

const FULL: u64 = 65535;

pub const KIND: Kind = Kind {
    name: "speaker",
    summary: "speaker playing a DAC output or a GPIO's PWM as sound",
    bus: None,
    params: &[
        Param::str(
            "dac",
            "the DAC it plays: a dac8165 channel (dac.A) or a pcm5102 (pcm, pcm.L, pcm.R); or gpio",
        )
        .optional(),
        Param::shared_pin("gpio", "the pin it plays, as PWM or PDM; or dac").optional(),
        Param::int("volume", 0, 100, "the starting volume, in percent").default_int(100),
        Param::int(
            "rate",
            1000,
            192_000,
            "on a gpio: how often the pin's average level is taken, in Hz",
        )
        .default_int(48_000),
        Param::int(
            "mhz",
            1,
            1000,
            "on a gpio: core clock, to turn time into cycles (default: the chip's clk_sys)",
        )
        .optional(),
    ],
    build: |p| {
        let volume = p.int("volume");
        let (rate, mhz) = (p.opt_int("rate"), p.opt_int("mhz"));
        let source = match (p.opt_str("dac").map(String::from), p.opt_pin("gpio")) {
            (Some(_), Some(_)) => return Err("gpio is only for a speaker without dac".into()),
            (None, None) => return Err("give the speaker a dac or a gpio".into()),
            (Some(dac), None) => {
                if rate.is_some() || mhz.is_some() {
                    let key = if rate.is_some() { "rate" } else { "mhz" };
                    return Err(format!("{key} is only for a speaker on a gpio"));
                }
                if dac.is_empty() || dac.starts_with('.') {
                    return Err(format!("dac {dac:?}: expected PART or PART.CHANNEL"));
                }
                Source::Dac(dac)
            }
            (None, Some(pin)) => Source::Pin(Pwm::new(
                pin,
                mhz.is_some(),
                mhz.unwrap_or(125) as u64 * 1_000_000,
                rate.unwrap_or(48_000) as u64,
            )),
        };
        Ok(Box::new(Speaker {
            source,
            volume,
            muted: false,
        }))
    },
};

impl Speaker {
    /// The volume heard, 0..=100.
    pub fn level(&self) -> i64 {
        if self.muted { 0 } else { self.volume }
    }
}

impl Pwm {
    fn new(pin: usize, fixed: bool, cycles_per_sec: u64, rate: u64) -> Self {
        Pwm {
            pin,
            fixed,
            cycles_per_sec,
            rate,
            origin: (0, 0),
            before: 0,
            high: false,
            window: 0,
            since: 0,
            acc: 0,
            level: 0,
            timer: false,
        }
    }

    /// The first cycle of window `w`.
    fn start(&self, w: u64) -> u64 {
        let (w0, c0) = self.origin;
        if w < w0 {
            return self.before;
        }
        c0 + ((w - w0) as u128 * self.cycles_per_sec as u128 / self.rate as u128) as u64
    }

    /// The window cycle `t` is in.
    fn window_at(&self, t: u64) -> u64 {
        let (w0, c0) = self.origin;
        let w = w0 + ((t - c0) as u128 * self.rate as u128 / self.cycles_per_sec as u128) as u64;
        // Integer division can put t one window early or late.
        if self.start(w + 1) <= t {
            w + 1
        } else if self.start(w) > t {
            w - 1
        } else {
            w
        }
    }

    /// The clock is now `hz`: the window under way ends as it would have,
    /// and the next ones count at the new rate.
    fn set_clock(&mut self, hz: u64) {
        if !self.fixed && hz != self.cycles_per_sec {
            self.before = self.start(self.window);
            self.origin = (self.window + 1, self.start(self.window + 1));
            self.cycles_per_sec = hz;
        }
    }

    /// The level of a window the pin spent all of high or low.
    fn steady(&self) -> u16 {
        if self.high { FULL as u16 } else { 0 }
    }

    /// Measure up to cycle `t`, emitting the last window that ended on the
    /// way if its level is new.
    fn close_to(&mut self, io: &mut Io, t: u64) {
        let end = self.start(self.window + 1);
        if t < end {
            return;
        }
        if self.high {
            self.acc += end - self.since;
        }
        let len = end - self.start(self.window);
        let w = self.window_at(t);
        // Whole windows after this one had the pin as it is now.
        let level = match w > self.window + 1 {
            true => self.steady(),
            false => (self.acc * FULL / len.max(1)).min(FULL) as u16,
        };
        self.window = w;
        self.since = self.start(w);
        self.acc = 0;
        if level != self.level {
            self.level = level;
            io.emit("level", level as i64);
        }
    }

    /// Wake up at the end of the window, to report it.
    fn arm(&mut self, io: &mut Io) {
        if !self.timer {
            self.timer = true;
            io.wake_at(self.start(self.window + 1));
        }
    }

    fn pin_changed(&mut self, io: &mut Io) {
        let t = io.cycles();
        let high = io.level(self.pin);
        self.close_to(io, t);
        if self.high {
            self.acc += t - self.since;
        }
        self.since = t;
        if high != self.high {
            self.high = high;
            self.arm(io);
        }
    }

    fn timer(&mut self, io: &mut Io) {
        self.timer = false;
        let t = io.cycles();
        self.close_to(io, t);
        // The window now open differs from the one reported if the pin
        // moved in it, or has stayed since at another level.
        if self.since > self.start(self.window) || self.level != self.steady() {
            self.arm(io);
        }
    }
}

impl Device for Speaker {
    fn watched_pins(&self) -> u32 {
        match &self.source {
            Source::Pin(pwm) => 1 << pwm.pin,
            Source::Dac(_) => 0,
        }
    }

    fn clock(&mut self, _at: u64, hz: u64) {
        if let Source::Pin(pwm) = &mut self.source {
            pwm.set_clock(hz);
        }
    }

    fn pin_changed(&mut self, io: &mut Io, _changed: u32) {
        if let Source::Pin(pwm) = &mut self.source {
            pwm.pin_changed(io);
        }
    }

    fn timer(&mut self, io: &mut Io) {
        if let Source::Pin(pwm) = &mut self.source {
            pwm.timer(io);
        }
    }

    fn input(&mut self, io: &mut Io, action: &str, value: i64) -> bool {
        let before = self.level();
        match action {
            "mute" => self.muted = true,
            "unmute" => self.muted = false,
            "volume" => self.volume = value.clamp(0, 100),
            _ => return false,
        }
        if self.level() != before {
            io.emit("volume", self.level());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::testutil::FakePins;

    #[test]
    fn takes_a_dac_or_a_gpio() {
        let err = |t: &str| {
            let text = format!("parts.s = {{ kind = \"speaker\", {t} }}");
            pcb::Board::parse_with(&text, &crate::registry())
                .err()
                .map(|e| e.msg)
        };
        assert_eq!(err("dac = \"dac.A\""), None);
        assert_eq!(err("gpio = 3, rate = 8000"), None);
        assert!(err("").unwrap().contains("a dac or a gpio"));
        assert!(
            err("dac = \"d.A\", gpio = 3")
                .unwrap()
                .contains("gpio is only for")
        );
        assert!(
            err("dac = \"d.A\", rate = 8000")
                .unwrap()
                .contains("rate is only for")
        );
    }

    #[test]
    fn mutes_and_sets_the_volume() {
        let mut s = Speaker {
            source: Source::Dac("dac.A".into()),
            volume: 80,
            muted: false,
        };
        let act = |s: &mut Speaker, action: &str, value: i64| {
            let (mut pins, mut events) = (FakePins::default(), Vec::new());
            let mut io = Io::new(&mut pins, 0, &mut events, 0, false);
            assert!(s.input(&mut io, action, value));
            events.iter().map(|e| e.value).collect::<Vec<_>>()
        };
        assert_eq!(act(&mut s, "mute", 0), [0]);
        assert_eq!(
            act(&mut s, "volume", 50),
            [] as [i64; 0],
            "muted: not heard"
        );
        assert_eq!(act(&mut s, "unmute", 0), [50]);
        assert_eq!(act(&mut s, "volume", 500), [100]);
        assert_eq!(s.level(), 100);
    }

    /// Drives a speaker's pin through a list of (cycle, level) edges, firing
    /// its wake-ups in order, and returns the `level` events.
    fn drive(pwm: &mut Pwm, edges: &[(u64, bool)], until: u64) -> Vec<(u64, i64)> {
        drive_on(pwm, edges, until, &mut Vec::new())
    }

    /// [`drive`], with the wake-ups still due carried in `timers`.
    fn drive_on(
        pwm: &mut Pwm,
        edges: &[(u64, bool)],
        until: u64,
        timers: &mut Vec<u64>,
    ) -> Vec<(u64, i64)> {
        let mut pins = FakePins::default();
        pins.set_chip(pwm.pin, pwm.high);
        let mut events = Vec::new();
        let mut step = |pwm: &mut Pwm, t: u64, edge: Option<bool>, timers: &mut Vec<u64>| {
            if let Some(level) = edge {
                pins.set_chip(pwm.pin, level);
            }
            let mut pending = pcb::Pending::default();
            let mut io = Io::new(&mut pins, t, &mut events, 0, false).with_pending(&mut pending);
            match edge {
                Some(_) => pwm.pin_changed(&mut io),
                None => pwm.timer(&mut io),
            }
            timers.extend(pending.timers.iter().map(|&(t, _)| t));
        };
        for &(t, level) in edges.iter().chain([(until, false)].iter()) {
            while let Some(i) = timers.iter().position(|&w| w <= t) {
                let w = timers.remove(i);
                step(pwm, w, None, timers);
            }
            if t < until {
                step(pwm, t, Some(level), timers);
            }
        }
        events.iter().map(|e| (e.cycles, e.value)).collect()
    }

    #[test]
    fn windows_follow_a_clock_change() {
        // 1 MHz, windows of 100 cycles; then 2 MHz, windows of 200.
        let mut pwm = Pwm::new(3, false, 1_000_000, 10_000);
        let mut timers = Vec::new();
        let ev = drive_on(&mut pwm, &[(50, true)], 120, &mut timers);
        assert_eq!(ev, [(100, 32767)]);
        pwm.set_clock(2_000_000);
        // The window under way (100..200) ends as before, at 50 % again
        // (nothing new to say); then 200..400 is high for its last quarter.
        let ev = drive_on(&mut pwm, &[(150, false), (350, true)], 1000, &mut timers);
        assert_eq!(ev, [(400, 16383), (600, 65535)]);
    }

    #[test]
    fn averages_the_pin_over_each_window() {
        // 1 MHz core, windows of 100 cycles.
        let mut pwm = Pwm::new(3, true, 1_000_000, 10_000);
        // A 25 % duty PWM, period 20 cycles, for 1000 cycles; then low.
        let edges: Vec<_> = (0..50)
            .flat_map(|n| [(n * 20, true), (n * 20 + 5, false)])
            .collect();
        let ev = drive(&mut pwm, &edges, 3000);
        assert_eq!(ev, [(100, 16383), (1100, 0)], "reported once, then low");

        // A square wave with a 400-cycle period: windows alternate.
        let mut pwm = Pwm::new(3, true, 1_000_000, 10_000);
        let edges: Vec<_> = (0..4)
            .flat_map(|n| [(n * 400, true), (n * 400 + 200, false)])
            .collect();
        let ev = drive(&mut pwm, &edges, 2000);
        let levels: Vec<_> = ev.iter().map(|e| e.1).collect();
        assert_eq!(levels, [65535, 0, 65535, 0, 65535, 0, 65535, 0]);
        assert_eq!(ev[1].0, 300, "at the end of the first low window");

        // Held high across many windows, then a long gap.
        let mut pwm = Pwm::new(3, true, 1_000_000, 10_000);
        let ev = drive(&mut pwm, &[(50, true), (100_050, false)], 200_000);
        assert_eq!(
            ev,
            [(100, 32767), (200, 65535), (100_100, 32767), (100_200, 0)]
        );
    }
}
