//! Texas Instruments PCM5102A, a stereo I2S audio DAC with its own clock
//! (no MCLK needed). It watches BCK and, on each rising edge, takes a bit
//! from DIN and the channel from LRCK, as the chip does: standard I2S, MSB
//! first, each word starting one BCK after LRCK changes (low = left, high =
//! right), any word length from 16 to 32 bits.
//!
//! Each frame, as its right word ends, emits `sample` with both channels'
//! top 16 bits, `left << 16 | right` (each as unsigned 16 bits). A
//! `speaker` part on the DAC plays them.
use pcb::{Device, Io, Kind, Param};

pub struct Pcm5102 {
    bck: usize,
    lrck: usize,
    din: usize,
    bck_high: bool,
    /// LRCK at the last rising BCK edge.
    lr: bool,
    /// The word being shifted in, and its bits so far.
    acc: u32,
    bits: u32,
    /// The latest samples, left and right.
    pub left: i16,
    pub right: i16,
    /// Frames received, and the cycles between the last two.
    pub frames: u64,
    last_frame: u64,
    pub frame_cycles: u64,
    /// clk_sys, to tell the sample rate.
    pub hz: u64,
}

pub const KIND: Kind = Kind {
    name: "pcm5102",
    summary: "PCM5102A stereo I2S audio DAC",
    bus: None,
    params: &[
        Param::pin("bck", "bit clock"),
        Param::pin("lrck", "word clock (LRCK, WS): low for the left channel"),
        Param::pin("din", "serial data"),
    ],
    build: |p| {
        Ok(alloc::boxed::Box::new(Pcm5102 {
            bck: p.pin("bck"),
            lrck: p.pin("lrck"),
            din: p.pin("din"),
            bck_high: false,
            lr: false,
            acc: 0,
            bits: 0,
            left: 0,
            right: 0,
            frames: 0,
            last_frame: 0,
            frame_cycles: 0,
            hz: 125_000_000,
        }))
    },
};

impl Pcm5102 {
    /// The sample rate, from the time between frames.
    pub fn rate(&self) -> Option<f64> {
        (self.frame_cycles > 0).then(|| self.hz as f64 / self.frame_cycles as f64)
    }

    /// A finished word of `bits` bits, as its top 16.
    fn word(acc: u32, bits: u32) -> i16 {
        match bits {
            0 => 0,
            1..32 => ((acc << (32 - bits)) >> 16) as u16 as i16,
            _ => (acc >> 16) as u16 as i16,
        }
    }
}

impl Device for Pcm5102 {
    fn watched_pins(&self) -> u32 {
        1 << self.bck
    }

    fn clock(&mut self, _at: u64, hz: u64) {
        self.hz = hz;
    }

    fn pin_changed(&mut self, io: &mut Io, _changed: u32) {
        let high = io.level(self.bck);
        let rose = high && !self.bck_high;
        self.bck_high = high;
        if !rose {
            return;
        }
        let bit = io.level(self.din) as u32;
        let lr = io.level(self.lrck);
        if lr == self.lr {
            self.acc = self.acc << 1 | bit;
            self.bits += 1;
            return;
        }
        // LRCK changed a bit ago: this is the last bit of the word before.
        let word = Self::word(self.acc << 1 | bit, self.bits + 1);
        (self.acc, self.bits) = (0, 0);
        if self.lr {
            self.right = word;
            let now = io.cycles();
            if self.frames > 0 {
                self.frame_cycles = now - self.last_frame;
            }
            (self.frames, self.last_frame) = (self.frames + 1, now);
            io.emit(
                "sample",
                (self.left as u16 as i64) << 16 | self.right as u16 as i64,
            );
        } else {
            self.left = word;
        }
        self.lr = lr;
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use pcb::Pins;

    use super::*;
    use crate::testutil::FakePins;

    /// Clock frames of 16-bit words out as a PIO I2S program does: data and
    /// LRCK change while BCK is low, LRCK a bit before each word's MSB.
    fn send(dac: &mut Pcm5102, frames: &[(i16, i16)]) -> Vec<(u64, i64)> {
        let mut pins = FakePins::default();
        let mut events = Vec::new();
        let mut t = 0;
        let mut edge = |dac: &mut Pcm5102, pins: &mut FakePins, bck: bool, t: u64| {
            pins.set_chip(0, bck);
            let mut io = Io::new(pins, t, &mut events, 0, false);
            dac.pin_changed(&mut io, 1);
        };
        // LRCK low to start: left, a bit before the first word.
        pins.set_chip(1, false);
        let mut bits = Vec::new();
        for &(l, r) in frames {
            for (lr, w) in [(false, l), (true, r)] {
                for i in (0..16).rev() {
                    bits.push((lr, (w as u16 >> i) & 1 != 0));
                }
            }
        }
        // One more left word to end the last right one.
        bits.extend((0..16).map(|_| (false, false)));
        for (n, &(lr, bit)) in bits.iter().enumerate() {
            // LRCK leads its word by a bit.
            let next_lr = bits.get(n + 1).map_or(lr, |b| b.0);
            pins.set_chip(1, next_lr);
            pins.set_chip(2, bit);
            edge(dac, &mut pins, false, t);
            edge(dac, &mut pins, true, t + 5);
            t += 10;
        }
        let _ = pins.level(0);
        events.iter().map(|e| (e.cycles, e.value)).collect()
    }

    fn dac() -> Pcm5102 {
        Pcm5102 {
            bck: 0,
            lrck: 1,
            din: 2,
            bck_high: false,
            lr: false,
            acc: 0,
            bits: 0,
            left: 0,
            right: 0,
            frames: 0,
            last_frame: 0,
            frame_cycles: 0,
            hz: 1_000,
        }
    }

    #[test]
    fn decodes_standard_i2s_frames() {
        let mut d = dac();
        let ev = send(&mut d, &[(0x1234, -2), (-32768, 32767)]);
        let frames: Vec<(i16, i16)> = ev
            .iter()
            .map(|e| ((e.1 >> 16) as u16 as i16, e.1 as u16 as i16))
            .collect();
        assert_eq!(frames, [(0x1234, -2), (-32768, 32767)]);
        assert_eq!(d.frame_cycles, 320, "32 bits of 10 cycles");
        assert_eq!(d.rate(), Some(1_000.0 / 320.0));
    }

    #[test]
    fn keeps_the_top_16_bits_of_longer_words() {
        assert_eq!(Pcm5102::word(0x12_3456, 24), 0x1234);
        assert_eq!(Pcm5102::word(0xFFFF_0000, 32), -1);
        assert_eq!(Pcm5102::word(0x8000, 16), -32768);
    }
}
