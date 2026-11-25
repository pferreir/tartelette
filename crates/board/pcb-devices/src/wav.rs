//! A WAV player whose output is an analog level on a pin, for the ADC to
//! sample: a line-level signal biased to mid-rail, as from a microphone
//! preamp or an audio input's coupling network. The pin's digital level is
//! left alone.
//!
//! Each sample `s` (-1..1) becomes `bias + gain * s` volts, `bias` and
//! `gain` in millivolts; while stopped the pin sits at `bias`. The position
//! follows the core clock (`mhz`), and the level at a given cycle is
//! interpolated between the two samples around it, so any sample rate works
//! and the result doesn't depend on when or how often the ADC looks.
//!
//! The host gives it a file with [`Wav::load`] (the runtime reads `file`,
//! relative to the board file, or one picked on the front panel). PCM
//! (8, 16, 24 or 32 bits) and 32-bit float, mono or multi-channel;
//! `channel` picks the left or right channel or their average.
//!
//! Host actions: `play`, `stop` (pauses where it is), `rewind` (back to
//! the start, playing on if it was). Emits `playing` (1 / 0), including
//! when it reaches the end of a file that doesn't loop. With `autoplay`, it
//! plays from power-up; the host plays a file it loads later, too.
use alloc::{boxed::Box, format, string::String, vec::Vec};

use pcb::{Device, Io, Kind, Param};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Left,
    Right,
    Mix,
}

pub struct Wav {
    pub pin: usize,
    /// The `file` key, for the host to load.
    pub file: Option<String>,
    bias: f32,
    gain: f32,
    channel: Channel,
    pub looping: bool,
    /// Play from power-up, and each file as it is loaded.
    pub autoplay: bool,
    /// Whether `mhz` fixes the clock, and its cycles a second.
    fixed: bool,
    cycles_per_sec: u64,
    /// The loaded file's name, mono samples and rate.
    pub name: Option<String>,
    samples: Vec<i16>,
    rate: u32,
    /// Cycles of the file played before `since`, and the cycle playback
    /// last started at (None while stopped).
    played: u64,
    since: Option<u64>,
}

pub const KIND: Kind = Kind {
    name: "wav",
    summary: "WAV file player: an audio signal on a pin for the ADC",
    bus: None,
    params: &[
        Param::pin(
            "gpio",
            "the pin the signal is on (GPIO26 to 29 reach the ADC)",
        ),
        Param::str(
            "file",
            "the WAV file to load, relative to the board file (the front panel can load one too)",
        )
        .optional(),
        Param::int("bias", 0, 3300, "the level of silence, in millivolts").default_int(1650),
        Param::int(
            "gain",
            0,
            10_000,
            "how far a full-scale sample swings from bias, in millivolts",
        )
        .default_int(1650),
        Param::choice(
            "channel",
            &["left", "right", "mix"],
            "the channel of a stereo file to play",
        )
        .default_str("mix"),
        Param::choice("loop", &["off", "on"], "start over at the end").default_str("off"),
        Param::choice(
            "autoplay",
            &["off", "on"],
            "play from power-up, and each file as it is loaded",
        )
        .default_str("off"),
        Param::int(
            "mhz",
            1,
            1000,
            "core clock, to turn time into cycles (default: the chip's clk_sys)",
        )
        .optional(),
    ],
    build: |p| {
        let channel = match p.str("channel") {
            "left" => Channel::Left,
            "right" => Channel::Right,
            _ => Channel::Mix,
        };
        Ok(Box::new(Wav {
            pin: p.pin("gpio"),
            file: p.opt_str("file").map(String::from),
            bias: p.int("bias") as f32 / 1000.0,
            gain: p.int("gain") as f32 / 1000.0,
            channel,
            looping: p.str("loop") == "on",
            autoplay: p.str("autoplay") == "on",
            fixed: p.opt_int("mhz").is_some(),
            cycles_per_sec: p.opt_int("mhz").unwrap_or(125) as u64 * 1_000_000,
            name: None,
            samples: Vec::new(),
            rate: 1,
            played: 0,
            since: None,
        }))
    },
};

impl Wav {
    /// Replace the file being played; the player stops at its start.
    pub fn load(&mut self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let (samples, rate) = decode(bytes, self.channel).map_err(|e| format!("{name}: {e}"))?;
        self.samples = samples;
        self.rate = rate;
        self.name = Some(name.into());
        self.played = 0;
        self.since = None;
        Ok(())
    }

    pub fn playing(&self) -> bool {
        self.since.is_some()
    }

    /// The file's length in cycles.
    fn length(&self) -> u64 {
        (self.samples.len() as u128 * self.cycles_per_sec as u128 / self.rate as u128) as u64
    }

    /// Cycles into the file at core cycle `cycles`: wrapped when looping,
    /// held at the end otherwise.
    fn elapsed(&self, cycles: u64) -> u64 {
        let t = self.played + self.since.map_or(0, |s| cycles.saturating_sub(s));
        match self.length() {
            0 => 0,
            len if self.looping => t % len,
            len => t.min(len),
        }
    }

    /// Position and length in seconds at core cycle `cycles`, for a host to
    /// show.
    pub fn position(&self, cycles: u64) -> (f64, f64) {
        let secs = |c: u64| c as f64 / self.cycles_per_sec as f64;
        (secs(self.elapsed(cycles)), secs(self.length()))
    }

    /// The sample value (-1..1) at core cycle `cycles`.
    fn sample(&self, cycles: u64) -> f32 {
        if self.since.is_none() || self.samples.is_empty() {
            return 0.0;
        }
        // Position in samples, with 16 fraction bits.
        let pos = (self.elapsed(cycles) as u128 * self.rate as u128 * 65536
            / self.cycles_per_sec as u128) as u64;
        let (i, frac) = ((pos >> 16) as usize, (pos & 0xFFFF) as f32 / 65536.0);
        let Some(&a) = self.samples.get(i) else {
            return 0.0;
        };
        let b = match self.samples.get(i + 1) {
            Some(&b) => b,
            None if self.looping => self.samples[0],
            None => a,
        };
        (a as f32 + (b as f32 - a as f32) * frac) / 32768.0
    }

    fn set_playing(&mut self, io: &mut Io, play: bool) {
        let now = io.cycles();
        if play == self.playing() {
            return;
        }
        if play {
            if self.samples.is_empty() {
                io.warn("play: no file loaded".into());
                return;
            }
            if !self.looping && self.played >= self.length() {
                self.played = 0;
            }
            self.since = Some(now);
            self.schedule_end(io);
        } else {
            self.played = self.elapsed(now);
            self.since = None;
        }
        io.emit("playing", play as i64);
    }

    /// Wake up when a file that doesn't loop runs out.
    fn schedule_end(&self, io: &mut Io) {
        if let Some(since) = self.since.filter(|_| !self.looping) {
            io.wake_at(since + self.length().saturating_sub(self.played));
        }
    }
}

impl Device for Wav {
    fn attach(&mut self, io: &mut Io) {
        if self.autoplay && !self.samples.is_empty() {
            self.set_playing(io, true);
        }
    }

    fn input(&mut self, io: &mut Io, action: &str, _value: i64) -> bool {
        match action {
            "play" => self.set_playing(io, true),
            "stop" => self.set_playing(io, false),
            "rewind" => {
                self.played = 0;
                if self.since.is_some() {
                    self.since = Some(io.cycles());
                    self.schedule_end(io);
                }
            }
            _ => return false,
        }
        true
    }

    fn timer(&mut self, io: &mut Io) {
        // Wake-ups from before a stop or a rewind find it still playing; one
        // from before a clock change may come early.
        if self.playing() && !self.looping {
            if self.elapsed(io.cycles()) >= self.length() {
                self.set_playing(io, false);
            } else {
                self.schedule_end(io);
            }
        }
    }

    fn clock(&mut self, at: u64, hz: u64) {
        if self.fixed || hz == self.cycles_per_sec {
            return;
        }
        // What has played so far, in cycles of the new clock; playing on
        // from `at`.
        let played = self.elapsed(at);
        self.played = (played as u128 * hz as u128 / self.cycles_per_sec as u128) as u64;
        if self.since.is_some() {
            self.since = Some(at);
        }
        self.cycles_per_sec = hz;
    }

    fn analog(&self, pin: usize, cycles: u64) -> Option<f32> {
        (pin == self.pin).then(|| self.bias + self.gain * self.sample(cycles))
    }
}

/// A WAV file's samples, one channel (or the mix), as 16-bit values, and
/// its sample rate.
pub fn decode(bytes: &[u8], channel: Channel) -> Result<(Vec<i16>, u32), String> {
    let u16le = |b: &[u8], at: usize| u16::from_le_bytes([b[at], b[at + 1]]);
    let u32le = |b: &[u8], at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let mut fmt = None;
    let mut data = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32le(bytes, at + 4) as usize;
        let body = &bytes[at + 8..(at + 8).saturating_add(len).min(bytes.len())];
        match id {
            b"fmt " if body.len() >= 16 => fmt = Some(body),
            b"data" => data = Some(body),
            _ => {}
        }
        // Chunks are padded to an even length.
        at = at
            .saturating_add(8)
            .saturating_add(len)
            .saturating_add(len & 1);
    }
    let fmt = fmt.ok_or("no fmt chunk")?;
    let data = data.ok_or("no data chunk")?;
    let mut tag = u16le(fmt, 0);
    let channels = u16le(fmt, 2) as usize;
    let rate = u32le(fmt, 4);
    let bits = u16le(fmt, 14);
    // WAVE_FORMAT_EXTENSIBLE: the real format starts the sub-format GUID.
    if tag == 0xFFFE && fmt.len() >= 26 {
        tag = u16le(fmt, 24);
    }
    if channels == 0 || rate == 0 {
        return Err("no channels or a zero sample rate".into());
    }
    let read: fn(&[u8]) -> f32 = match (tag, bits) {
        (1, 8) => |b| (b[0] as f32 - 128.0) / 128.0,
        (1, 16) => |b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
        (1, 24) => |b| i32::from_le_bytes([0, b[0], b[1], b[2]]) as f32 / 2_147_483_648.0,
        (1, 32) => |b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        (3, 32) => |b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        _ => return Err(format!("unsupported format {tag} with {bits}-bit samples")),
    };
    let width = bits as usize / 8;
    let pick = |frame: &[u8]| match channel {
        Channel::Left => read(frame),
        Channel::Right => read(&frame[width * channels.min(2).saturating_sub(1)..]),
        Channel::Mix => {
            let n = channels.min(2);
            (0..n).map(|c| read(&frame[c * width..])).sum::<f32>() / n as f32
        }
    };
    let samples = data
        .chunks_exact(width * channels)
        .map(|frame| (pick(frame) * 32768.0).clamp(-32768.0, 32767.0) as i16)
        .collect();
    Ok((samples, rate))
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::testutil::FakePins;

    /// A 16-bit PCM WAV file.
    fn wav(channels: u16, rate: u32, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut f = Vec::new();
        f.extend_from_slice(b"RIFF");
        f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        f.extend_from_slice(b"WAVEfmt ");
        f.extend_from_slice(&16u32.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&channels.to_le_bytes());
        f.extend_from_slice(&rate.to_le_bytes());
        f.extend_from_slice(&(rate * 2 * channels as u32).to_le_bytes());
        f.extend_from_slice(&(2 * channels).to_le_bytes());
        f.extend_from_slice(&16u16.to_le_bytes());
        // An unknown chunk with an odd length, padded, to skip.
        f.extend_from_slice(b"LIST");
        f.extend_from_slice(&3u32.to_le_bytes());
        f.extend_from_slice(&[1, 2, 3, 0]);
        f.extend_from_slice(b"data");
        f.extend_from_slice(&(data.len() as u32).to_le_bytes());
        f.extend_from_slice(&data);
        f
    }

    /// A player on GPIO26 at 1 MHz (one cycle per microsecond).
    fn player(looping: bool) -> Wav {
        Wav {
            pin: 26,
            file: None,
            bias: 1.5,
            gain: 1.0,
            channel: Channel::Mix,
            looping,
            autoplay: false,
            fixed: true,
            cycles_per_sec: 1_000_000,
            name: None,
            samples: Vec::new(),
            rate: 1,
            played: 0,
            since: None,
        }
    }

    fn act(w: &mut Wav, action: &str, at: u64) -> Vec<(&'static str, i64)> {
        let (mut pins, mut events, mut pending) =
            (FakePins::default(), Vec::new(), pcb::Pending::default());
        let mut io = Io::new(&mut pins, at, &mut events, 0, false).with_pending(&mut pending);
        if action == "timer" {
            w.timer(&mut io);
        } else {
            assert!(w.input(&mut io, action, 0));
        }
        events.iter().map(|e| (e.label, e.value)).collect()
    }

    fn close(a: Option<f32>, b: f32) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-3)
    }

    #[test]
    fn decodes_pcm_and_picks_channels() {
        let f = wav(2, 8000, &[16384, -16384, 32767, 0]);
        assert_eq!(
            decode(&f, Channel::Left).unwrap(),
            (vec![16384, 32767], 8000)
        );
        assert_eq!(decode(&f, Channel::Right).unwrap().0, vec![-16384, 0]);
        assert_eq!(decode(&f, Channel::Mix).unwrap().0, vec![0, 16383]);
        let mono = wav(1, 8000, &[100]);
        assert_eq!(decode(&mono, Channel::Right).unwrap().0, vec![100]);
        assert!(decode(b"RIFF\0\0\0\0WAVE", Channel::Mix).is_err());
        assert!(decode(b"hello", Channel::Mix).is_err());
    }

    #[test]
    fn plays_stops_and_rewinds_at_the_core_clock() {
        let mut w = player(false);
        // 1 kHz: a sample every 1000 cycles.
        w.load("t.wav", &wav(1, 1000, &[0, 16384, 32767, -32768]))
            .unwrap();
        assert!(close(w.analog(26, 0), 1.5), "stopped: at bias");
        assert_eq!(w.analog(25, 0), None);
        assert_eq!(act(&mut w, "play", 100), [("playing", 1)]);
        assert!(close(w.analog(26, 1100), 2.0));
        assert!(close(w.analog(26, 1600), 2.25), "interpolated");
        assert_eq!(act(&mut w, "stop", 1600), [("playing", 0)]);
        assert!(close(w.analog(26, 5000), 1.5));
        // Playing on resumes where it stopped.
        act(&mut w, "play", 10_000);
        assert!(close(w.analog(26, 10_500), 2.5));
        act(&mut w, "rewind", 10_500);
        assert!(close(w.analog(26, 10_500), 1.5));
        assert!(close(w.analog(26, 11_500), 2.0));
        assert_eq!(w.position(11_500), (1e-3, 4e-3));
    }

    #[test]
    fn stops_at_the_end_unless_looping() {
        let mut w = player(false);
        w.load("t.wav", &wav(1, 1000, &[0, 32767])).unwrap();
        act(&mut w, "play", 0);
        assert!(act(&mut w, "timer", 1000).is_empty());
        assert_eq!(act(&mut w, "timer", 2000), [("playing", 0)]);
        assert!(!w.playing());
        // Playing again starts over.
        act(&mut w, "play", 3000);
        assert!(close(w.analog(26, 3000), 1.5));

        let mut w = player(true);
        w.load("t.wav", &wav(1, 1000, &[0, 32767])).unwrap();
        act(&mut w, "play", 0);
        assert!(close(w.analog(26, 2000), 1.5), "wrapped to the start");
        assert!(
            close(w.analog(26, 1500), 2.0),
            "the last sample leads into the first"
        );
    }

    #[test]
    fn autoplay_starts_at_power_up() {
        let mut w = player(true);
        w.autoplay = true;
        w.load("t.wav", &wav(1, 1000, &[0, 32767])).unwrap();
        let (mut pins, mut events, mut pending) =
            (FakePins::default(), Vec::new(), pcb::Pending::default());
        w.attach(&mut Io::new(&mut pins, 0, &mut events, 0, true).with_pending(&mut pending));
        assert!(w.playing());
        assert!(close(w.analog(26, 1000), 2.5));
    }

    #[test]
    fn play_without_a_file_warns() {
        let mut w = player(false);
        assert!(act(&mut w, "play", 0).is_empty());
        assert!(!w.playing());
    }
}
