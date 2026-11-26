//! The Varvara audio devices (ports 0x30-0x6f, one per voice): four voices
//! playing samples from Uxn memory, at a MIDI pitch, under an ADSR envelope.
//! See <https://wiki.xxiivv.com/site/varvara.html#audio>; the arithmetic is
//! uxn2's, at our own sample rate.
//!
//! A sample of up to 256 bytes is one cycle of a waveform, played at the
//! note's frequency; a longer one is a recording, played at its own speed
//! for middle C (note 60), as if made at 44.1 kHz.

pub const VOICES: usize = 4;

/// Per octave from the top (note 96), the 12 notes' phase steps.
const ADVANCES: [u32; 12] = [
    0x80000, 0x879c8, 0x8facd, 0x9837f, 0xa1451, 0xaadc1, 0xb504f, 0xbfc88, 0xcb2ff, 0xd7450,
    0xe411f, 0xf1a1c,
];

#[derive(Clone, Copy)]
struct Voice {
    addr: u16,
    len: u16,
    i: u16,
    count: u32,
    advance: u32,
    period: u32,
    age: u32,
    a: u32,
    d: u32,
    s: u32,
    r: u32,
    left: i32,
    right: i32,
    repeat: bool,
}

impl Voice {
    const fn new() -> Self {
        Self {
            addr: 0,
            len: 0,
            i: 0,
            count: 0,
            advance: 0,
            period: 0,
            age: 0,
            a: 0,
            d: 0,
            s: 0,
            r: 0,
            left: 0,
            right: 0,
            repeat: false,
        }
    }

    fn playing(&self) -> bool {
        self.advance != 0 && self.period != 0
    }

    /// The level, 0 to 0x888, `age` samples into the note.
    fn envelope(&mut self, age: u32) -> i32 {
        if self.r == 0 {
            return 0x888;
        }
        if age < self.a {
            return (0x888 * age / self.a) as i32;
        }
        if age < self.d {
            return (0x444 * (2 * self.d - self.a - age) / (self.d - self.a)) as i32;
        }
        if age < self.s {
            return 0x444;
        }
        if age < self.r {
            return (0x444 * (self.r - age) / (self.r - self.s)) as i32;
        }
        self.advance = 0;
        0
    }
}

pub struct Audio {
    rate: u32,
    voices: [Voice; VOICES],
    /// Voices that ended since the last `take_done`, a bit each.
    done: u8,
}

impl Audio {
    pub const fn new(rate: u32) -> Self {
        Self {
            rate,
            voices: [Voice::new(); VOICES],
            done: 0,
        }
    }

    /// The pitch port of voice `n` was written: start its note. `d` is its 16
    /// ports.
    pub fn start(&mut self, n: usize, d: &[u8]) {
        let v = &mut self.voices[n];
        let pitch = (d[0xf] & 0x7f) as u32;
        let adsr = u16::from_be_bytes([d[0x8], d[0x9]]) as u32;
        let addr = u16::from_be_bytes([d[0xc], d[0xd]]);
        let len = (u16::from_be_bytes([d[0xa], d[0xb]]) as u32).min(0x10000 - addr as u32);
        v.addr = addr;
        v.len = len as u16;
        v.left = (d[0xe] >> 4) as i32;
        v.right = (d[0xe] & 0xf) as i32;
        v.repeat = d[0xf] & 0x80 == 0;
        if pitch >= 108 || len == 0 {
            v.advance = 0;
            return;
        }
        v.advance = ADVANCES[(pitch % 12) as usize] >> (8 - pitch / 12);
        let step = self.rate / 15;
        v.a = step * (adsr >> 12);
        v.d = step * (adsr >> 8 & 0xf) + v.a;
        v.s = step * (adsr >> 4 & 0xf) + v.d;
        v.r = step * (adsr & 0xf) + v.s;
        v.age = 0;
        v.i = 0;
        v.count = 0;
        let note_period = self.rate * 0x4000 / 11025;
        v.period = if len <= 0x100 {
            note_period * 337 / 2 / len
        } else {
            note_period
        };
    }

    /// The output port: the voice's loudness, left and right, a nibble each.
    pub fn vu(&mut self, n: usize) -> u8 {
        let v = &mut self.voices[n];
        if !v.playing() {
            return 0;
        }
        let e = v.envelope(v.age);
        let side = |vol: i32| {
            if vol == 0 {
                0
            } else {
                (1 + e * vol / 0x800).min(0xf) as u8
            }
        };
        side(v.left) << 4 | side(v.right)
    }

    pub fn position(&self, n: usize) -> u16 {
        self.voices[n].i
    }

    pub fn take_done(&mut self) -> u8 {
        core::mem::take(&mut self.done)
    }

    /// Adds the voices into `out`, mono: one voice at full volume reaches
    /// about ±8.4 million. `mem` reads Uxn memory, where the samples are.
    pub fn render(&mut self, mem: impl Fn(u16) -> u8, out: &mut [i32]) {
        for (n, v) in self.voices.iter_mut().enumerate() {
            if !v.playing() {
                continue;
            }
            // The envelope once a block: at the finest it moves in steps of
            // 1/15 s, and blocks are a few ms. (No divisions per sample: the
            // M0+ has none in hardware.)
            let gain = v.envelope(v.age) * (v.left + v.right);
            for o in out.iter_mut() {
                if v.advance == 0 {
                    break;
                }
                v.count += v.advance;
                if v.count >= v.period {
                    let steps = if v.count < 4 * v.period {
                        let mut k = 0;
                        while v.count >= v.period {
                            v.count -= v.period;
                            k += 1;
                        }
                        k
                    } else {
                        let k = v.count / v.period;
                        v.count %= v.period;
                        k
                    };
                    v.i = v.i.wrapping_add(steps as u16);
                }
                if v.i >= v.len {
                    if !v.repeat {
                        v.advance = 0;
                        break;
                    }
                    v.i %= v.len;
                }
                v.age += 1;
                *o += (mem(v.addr.wrapping_add(v.i)) ^ 0x80) as i8 as i32 * gain;
            }
            if v.advance == 0 {
                self.done |= 1 << n;
            }
        }
    }
}
