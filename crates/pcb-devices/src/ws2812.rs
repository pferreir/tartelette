//! `ws2812 <name> gpio=N [count=8] [order=grb|rgb] [reset_us=50] [mhz=125]`:
//! a chain of WS2812 / SK6812 LEDs on one data pin.
//!
//! The data line is decoded from pulse widths alone, the way the LEDs do it:
//! a high pulse shorter than 0.5 us is a 0 bit, longer a 1 bit; 24 bits per
//! LED, MSB first, in `order`. The first LED keeps the first 24 bits, the
//! second the next 24, and so on (the real chain re-times and forwards the
//! rest). The colours show up when the line has stayed low for `reset_us`
//! (the latch), which is detected with a device timer. `mhz` is the core
//! clock, to turn time into cycles.
//!
//! A `latch` event (value = LEDs updated) is emitted at each latch.
use alloc::{boxed::Box, string::String, vec, vec::Vec};
use pcb::{Device, Io, Params};

pub struct Ws2812 {
    pin: usize,
    /// Colour byte order on the wire.
    grb: bool,
    /// Cycles of low level that latch a frame, and the 0/1 pulse boundary.
    reset_cycles: u64,
    threshold: u64,
    /// Bits received since the last latch.
    bits: Vec<bool>,
    rose_at: Option<u64>,
    fell_at: u64,
    /// Latched colours, RGB.
    leds: Vec<[u8; 3]>,
    /// Number of latches so far.
    pub frames: u32,
}

pub fn factory(p: &mut Params) -> Result<Box<dyn Device>, String> {
    let pin = p.pin("gpio")?;
    let count = p.opt_int("count", 1, 4096)?.unwrap_or(8) as usize;
    let grb = match p.opt_str("order") {
        None | Some("grb") => true,
        Some("rgb") => false,
        Some(_) => return Err("order must be grb or rgb".into()),
    };
    let reset_us = p.opt_int("reset_us", 1, 10_000)?.unwrap_or(50) as u64;
    let mhz = p.opt_int("mhz", 1, 1000)?.unwrap_or(125) as u64;
    Ok(Box::new(Ws2812 {
        pin,
        grb,
        reset_cycles: reset_us * mhz,
        threshold: mhz / 2,
        bits: Vec::new(),
        rose_at: None,
        fell_at: 0,
        leds: vec![[0; 3]; count],
        frames: 0,
    }))
}

impl Ws2812 {
    /// Latched colours (RGB) of every LED in the chain.
    pub fn leds(&self) -> &[[u8; 3]] {
        &self.leds
    }

    fn latch(&mut self, io: &mut Io) {
        let mut updated = 0;
        for (led, chunk) in self.leds.iter_mut().zip(self.bits.as_chunks::<24>().0) {
            let byte = |i: usize| chunk[8 * i..8 * i + 8].iter().fold(0u8, |a, &b| a << 1 | b as u8);
            let (a, b, c) = (byte(0), byte(1), byte(2));
            *led = if self.grb { [b, a, c] } else { [a, b, c] };
            updated += 1;
        }
        self.bits.clear();
        self.frames += 1;
        io.emit("latch", updated);
    }
}

impl Device for Ws2812 {
    fn watched_pins(&self) -> u32 {
        1 << self.pin
    }

    fn pin_changed(&mut self, io: &mut Io, _changed: u32) {
        let now = io.cycles();
        if io.level(self.pin) {
            self.rose_at = Some(now);
        } else if let Some(rose) = self.rose_at.take() {
            self.bits.push(now - rose >= self.threshold);
            self.fell_at = now;
            // If nothing follows for `reset_us`, the frame is complete.
            io.wake_at(now + self.reset_cycles);
        }
    }

    fn timer(&mut self, io: &mut Io) {
        let idle = io.cycles() >= self.fell_at + self.reset_cycles && !io.level(self.pin);
        if idle && !self.bits.is_empty() {
            self.latch(io);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakePins;
    use pcb::Board;

    const PIN: usize = 14;

    struct Rig {
        board: Board,
        pins: FakePins,
        now: u64,
        last_fall: u64,
    }

    impl Rig {
        fn new(extra: &str) -> Self {
            let text = alloc::format!("ws2812 strip gpio={PIN} {extra}");
            let mut board = Board::parse_with(&text, &crate::registry()).unwrap();
            let mut pins = FakePins::default();
            board.attach(&mut pins);
            Rig { board, pins, now: 0, last_fall: 0 }
        }
        fn edge(&mut self, level: bool) {
            if !level {
                self.last_fall = self.now;
            }
            self.pins.set_chip(PIN, level);
            let changed = self.pins.take_changes();
            self.board.service(&mut self.pins, changed, self.now);
        }
        /// One bit as the firmware's PIO program drives it at 8 MHz from a
        /// 125 MHz core: a 1 is high for 7 of its 10 PIO cycles (~110 core
        /// cycles), a 0 for 2 (~31); every bit lasts ~156 core cycles.
        fn bit(&mut self, one: bool) {
            let high = if one { 110 } else { 31 };
            self.edge(true);
            self.now += high;
            self.edge(false);
            self.now += 156 - high;
        }
        fn byte(&mut self, b: u8) {
            for i in (0..8).rev() {
                self.bit(b >> i & 1 != 0);
            }
        }
        /// Let the line sit low until `cycles` after the last falling edge.
        fn idle_since_fall(&mut self, cycles: u64) {
            self.now = self.last_fall + cycles;
            // Wake-ups in time order up to now.
            self.board.fire_timers(&mut self.pins, self.now);
        }
        fn strip(&self) -> &Ws2812 {
            self.board.device::<Ws2812>("strip").unwrap()
        }
    }

    #[test]
    fn decodes_pulse_widths_and_latches_after_the_reset_gap() {
        let mut r = Rig::new("count=2");
        for b in [0x10, 0x20, 0x30, 0xFF, 0x00, 0x80] {
            r.byte(b);
        }
        assert_eq!(r.strip().leds(), [[0; 3]; 2], "nothing shows before the latch");
        r.idle_since_fall(50 * 125 - 1);
        assert_eq!(r.strip().frames, 0, "gap not long enough");
        r.idle_since_fall(50 * 125);
        // GRB on the wire: G=0x10 R=0x20 B=0x30, then G=0xFF R=0x00 B=0x80.
        assert_eq!(r.strip().leds(), [[0x20, 0x10, 0x30], [0x00, 0xFF, 0x80]]);
        assert_eq!(r.strip().frames, 1);
        let ev: Vec<_> = r.board.events.iter().map(|e| (e.label, e.value)).collect();
        assert_eq!(ev, [("latch", 2)]);
    }

    #[test]
    fn rgb_order_extra_data_and_partial_leds() {
        let mut r = Rig::new("count=1 order=rgb");
        for b in [1, 2, 3, 9, 9, 9, 7] {
            r.byte(b); // a second LED's worth (not in the chain) and a partial one: dropped
        }
        r.idle_since_fall(10_000);
        assert_eq!(r.strip().leds(), [[1, 2, 3]]);
        // A short frame leaves later LEDs alone: only whole LEDs update.
        let mut r = Rig::new("count=3");
        for b in [1, 2, 3, 4, 5] {
            r.byte(b);
        }
        r.idle_since_fall(10_000);
        assert_eq!(r.strip().leds(), [[2, 1, 3], [0; 3], [0; 3]]);
    }

    #[test]
    fn a_new_frame_after_the_latch_starts_from_the_first_led() {
        let mut r = Rig::new("count=1");
        for b in [0, 0, 255] {
            r.byte(b);
        }
        r.idle_since_fall(10_000);
        assert_eq!(r.strip().leds(), [[0, 0, 255]]);
        for b in [255, 0, 0] {
            r.byte(b);
        }
        r.idle_since_fall(10_000);
        assert_eq!(r.strip().leds(), [[0, 255, 0]]);
        assert_eq!(r.strip().frames, 2);
    }

    #[test]
    fn a_gap_inside_the_frame_shorter_than_reset_does_not_latch() {
        let mut r = Rig::new("count=1 reset_us=50");
        r.byte(0x11);
        r.now += 1000; // 8 us: a pause between bytes
        r.byte(0x22);
        r.byte(0x33);
        r.idle_since_fall(10_000);
        assert_eq!(r.strip().leds(), [[0x22, 0x11, 0x33]]);
        assert_eq!(r.strip().frames, 1);
    }

    #[test]
    fn timing_parameters_scale_with_the_clock() {
        // At 4 cycles/us a 1 bit must be high for at least 2 cycles; the rig's
        // 110-cycle pulses are 27 us long, so every bit is a 1.
        let mut r = Rig::new("count=1 mhz=4 reset_us=10");
        r.byte(0x00);
        r.byte(0x00);
        r.byte(0x00);
        r.idle_since_fall(10 * 4);
        assert_eq!(r.strip().leds(), [[255; 3]], "latch after 10 us = 40 cycles; every pulse is long at this clock");
    }

    #[test]
    fn factory_rejects_bad_parameters() {
        let reg = crate::registry();
        assert!(Board::parse_with("ws2812 s gpio=1 order=bgr", &reg).is_err());
        assert!(Board::parse_with("ws2812 s count=2", &reg).is_err(), "gpio is required");
        assert!(Board::parse_with("ws2812 s gpio=1 count=0", &reg).is_err());
    }
}
