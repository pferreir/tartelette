//! A test synthesizer: oscillators and drums patched into the board's CV and
//! gate outputs, to hear what a sequencer or drum machine plays.
//!
//! An oscillator follows a CV output (a `dac8165` channel, through the
//! board's output stage) at 1 V/octave with 0 V = C4, and sounds while its
//! gate is high: an `mcp23008` pin, an `led` part, or always (`open`). A drum
//! (kick, snare, hats, clap; synthesized, no samples) plays a hit on each
//! rising edge of its gate, however short the trigger. Voices follow the
//! board's events, which carry the emulated time they happened at, so they
//! render either sample-exact against emulated time ([`Synth::feed`], for a
//! WAV file) or live, a little behind ([`Live`]).
//!
//! Patches name parts and outputs (`saw:dac.A:expander.0`, `kick:expander.0`),
//! so they survive a reset or a reloaded board.
//!
//! The board's `speaker` parts play too, whatever the patches: each plays a
//! DAC output as it is (the firmware streaming samples to the DAC), or the
//! averaged level of a pin it watches (PWM), held between updates and
//! through a coupling capacitor's high-pass.
use std::{collections::VecDeque, fmt, str::FromStr};

use pcb::{Board, BoardEvent};
use pcb_devices::{
    dac8165::{CvScale, Dac8165},
    led::Led,
    mcp23008::{Mcp23008, Wire},
    pcm5102::Pcm5102,
    speaker::{Source, Speaker},
};
#[cfg(test)]
use rp2040_core::system::CYCLES_PER_US;
use rp2040_core::system::Clock;

/// Sample rate of rendered files.
pub const WAV_RATE: u32 = 48_000;
/// Pitch at 0 V.
const C4_HZ: f32 = 261.626;
/// Gate attack and release, against clicks.
const RAMP_SECS: f32 = 0.002;
/// Mix headroom: a few full-level voices still fit.
const MIX: f32 = 0.3;
/// Drums are louder than tones: a hit is short.
const DRUM_MIX: f32 = 0.6;
/// A hit is over after this long.
const HIT_SECS: f32 = 1.5;
/// A speaker's coupling capacitor: the high-pass that takes the DAC's DC
/// level out.
const SPEAKER_HP_HZ: f32 = 20.0;
/// How far behind the emulator live sound plays, and how far ahead of the
/// sound card the emulator may get before it is pulled back.
const LEAD_SECS: f64 = 0.05;
const MAX_AHEAD_SECS: f64 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Sine,
    Triangle,
    Saw,
    Square,
    Kick,
    Snare,
    Hat,
    OpenHat,
    Clap,
}

impl Kind {
    pub const ALL: [Kind; 9] = [
        Kind::Sine,
        Kind::Triangle,
        Kind::Saw,
        Kind::Square,
        Kind::Kick,
        Kind::Snare,
        Kind::Hat,
        Kind::OpenHat,
        Kind::Clap,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Sine => "sine",
            Kind::Triangle => "triangle",
            Kind::Saw => "saw",
            Kind::Square => "square",
            Kind::Kick => "kick",
            Kind::Snare => "snare",
            Kind::Hat => "hat",
            Kind::OpenHat => "openhat",
            Kind::Clap => "clap",
        }
    }

    /// Triggered by its gate, with no CV.
    pub fn is_drum(self) -> bool {
        matches!(
            self,
            Kind::Kick | Kind::Snare | Kind::Hat | Kind::OpenHat | Kind::Clap
        )
    }

    /// One oscillator sample at phase `t` (0..1) for a phase step of `dt`
    /// per sample; the edges of saw and square are smoothed (PolyBLEP)
    /// against aliasing.
    fn sample(self, t: f32, dt: f32) -> f32 {
        match self {
            Kind::Triangle => 1.0 - 4.0 * (t - 0.5).abs(),
            Kind::Saw => 2.0 * t - 1.0 - blep(t, dt),
            Kind::Square => {
                let sq = if t < 0.5 { 1.0 } else { -1.0 };
                sq + blep(t, dt) - blep((t + 0.5).fract(), dt)
            }
            _ => (t * std::f32::consts::TAU).sin(),
        }
    }
}

/// Correction for a unit step at phase 0, spread over a sample either side.
fn blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let t = t / dt;
        t + t - t * t - 1.0
    } else if t > 1.0 - dt {
        let t = (t - 1.0) / dt;
        t * t + t + t + 1.0
    } else {
        0.0
    }
}

/// One voice's wiring, by names: `cv` is `PART.CHANNEL` (`dac.A`; none for
/// a drum), `gate` is `PART.PIN` for an expander pin (its number or name), `PART` for an LED, or
/// `open`. Empty means the board's default.
#[derive(Clone, Debug, PartialEq)]
pub struct Patch {
    pub kind: Kind,
    pub cv: String,
    pub gate: String,
    /// 0..1.
    pub level: f32,
}

impl Default for Patch {
    fn default() -> Self {
        Patch {
            kind: Kind::Saw,
            cv: String::new(),
            gate: String::new(),
            level: 0.8,
        }
    }
}

/// `WAVE[:CV[:GATE]]` for an oscillator, `DRUM[:GATE]` for a drum.
impl FromStr for Patch {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let mut it = s.split(':');
        let kind = it.next().unwrap_or_default();
        let kind = Kind::ALL
            .into_iter()
            .find(|k| k.name() == kind)
            .ok_or_else(|| {
                let names: Vec<_> = Kind::ALL.iter().map(|k| k.name()).collect();
                format!("{kind:?}: expected one of {}", names.join(", "))
            })?;
        let cv = if kind.is_drum() {
            String::new()
        } else {
            it.next().unwrap_or_default().into()
        };
        let gate = it.next().unwrap_or_default().into();
        if it.next().is_some() {
            return Err("expected WAVE[:CV[:GATE]] or DRUM[:GATE]".into());
        }
        Ok(Patch {
            kind,
            cv,
            gate,
            ..Patch::default()
        })
    }
}

impl fmt::Display for Patch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind.is_drum() {
            true => write!(f, "{}:{}", self.kind.name(), self.gate),
            false => write!(f, "{}:{}:{}", self.kind.name(), self.cv, self.gate),
        }
    }
}

impl Patch {
    /// Fill in the board's defaults for what was left empty.
    pub fn complete(&mut self, board: &Board) {
        if self.kind.is_drum() {
            self.cv.clear();
        } else if self.cv.is_empty() {
            self.cv = cv_outputs(board).into_iter().next().unwrap_or_default();
        }
        if self.gate.is_empty() {
            self.gate = default_gate(board);
        }
    }
}

/// Every CV output on the board, as patch names.
pub fn cv_outputs(board: &Board) -> Vec<String> {
    let mut v = Vec::new();
    for p in board.parts.iter().filter(|p| p.kind == "dac8165") {
        v.extend((b'A'..=b'D').map(|c| format!("{}.{}", p.name, c as char)));
    }
    v
}

/// Every gate on the board, as patch names, `open` first.
pub fn gate_outputs(board: &Board) -> Vec<String> {
    let mut v = vec!["open".to_string()];
    for p in &board.parts {
        match p.kind.as_str() {
            "mcp23008" => v.extend((0..8).map(|n| format!("{}.{n}", p.name))),
            "led" => v.push(p.name.clone()),
            _ => {}
        }
    }
    v
}

/// The expander pins the panel shows as outputs, in panel order: those of
/// `mode = "view"` items, and those wired to LEDs on items without a mode.
fn panel_gates(board: &Board) -> Vec<String> {
    let mut v = Vec::new();
    for i in &board.panel {
        let part = &board.parts[i.part];
        let Some(dev) = board.device::<Mcp23008>(&part.name) else {
            continue;
        };
        let shown = |pin: u8| match i.option("mode") {
            Some(mode) => mode == "view",
            None => matches!(dev.wiring(pin).wire, Wire::Led { .. }),
        };
        let pins = i.option("pins").unwrap_or("0,1,2,3,4,5,6,7");
        v.extend(
            pins.split(',')
                .filter_map(|n| n.parse::<u8>().ok())
                .filter(|&n| n < 8 && shown(n))
                .map(|n| format!("{}.{n}", part.name)),
        );
    }
    v
}

/// The first gate the panel shows, else `open`.
fn default_gate(board: &Board) -> String {
    panel_gates(board)
        .into_iter()
        .next()
        .unwrap_or_else(|| "open".into())
}

/// The board's default patch: a saw on the first CV output and gate, or
/// nothing on a board with a speaker, which is heard anyway.
pub fn default_patches(board: &Board) -> Vec<Patch> {
    if board.parts.iter().any(|p| p.kind == "speaker") {
        return Vec::new();
    }
    let mut p = Patch::default();
    p.complete(board);
    vec![p]
}

/// The voices asked for (`--osc`, `--drums`, or the page's `osc` and
/// `drums`), or else the board's default.
pub fn patches(osc: &[Patch], drums: bool, board: &Board) -> Vec<Patch> {
    let mut v = osc.to_vec();
    if drums {
        v.extend(drum_kit(board));
    }
    if v.is_empty() {
        v = default_patches(board);
    }
    v
}

/// A drum kit on the board's first four gates (as the panel shows them, or
/// else every gate there is): kick, snare, closed and open hat.
pub fn drum_kit(board: &Board) -> Vec<Patch> {
    let mut gates = panel_gates(board);
    if gates.is_empty() {
        gates = gate_outputs(board).split_off(1);
    }
    [Kind::Kick, Kind::Snare, Kind::Hat, Kind::OpenHat]
        .into_iter()
        .zip(gates)
        .map(|(kind, gate)| Patch {
            kind,
            gate,
            ..Patch::default()
        })
        .collect()
}

enum Gate {
    Open,
    Pin { part: usize, pin: u8 },
    Led { part: usize },
}

fn resolve_cv(board: &Board, name: &str) -> Result<(usize, usize, CvScale), String> {
    let (part, ch) = name
        .rsplit_once('.')
        .ok_or_else(|| format!("CV {name:?}: expected PART.CHANNEL, e.g. dac.A"))?;
    let dac = board
        .device::<Dac8165>(part)
        .ok_or_else(|| format!("CV {name:?}: no DAC named {part:?}"))?;
    let ch = match ch {
        "A" | "a" | "0" => 0,
        "B" | "b" | "1" => 1,
        "C" | "c" | "2" => 2,
        "D" | "d" | "3" => 3,
        _ => return Err(format!("CV {name:?}: channels are A..D")),
    };
    Ok((board.part_index(part).unwrap(), ch, dac.cv_scale()))
}

fn resolve_gate(board: &Board, name: &str) -> Result<Gate, String> {
    if name == "open" {
        return Ok(Gate::Open);
    }
    if let Some((part, pin)) = name.rsplit_once('.')
        && let Some(dev) = board.device::<Mcp23008>(part)
    {
        let named = (0..8).find(|&n| dev.wiring(n).name == Some(pin));
        return match named.map_or_else(|| pin.parse::<u8>(), Ok) {
            Ok(pin @ 0..8) => Ok(Gate::Pin {
                part: board.part_index(part).unwrap(),
                pin,
            }),
            _ => Err(format!(
                "gate {name:?}: expander pins are 0..7 or a pin's name"
            )),
        };
    }
    if board.device::<Led>(name).is_some() {
        return Ok(Gate::Led {
            part: board.part_index(name).unwrap(),
        });
    }
    Err(format!(
        "gate {name:?}: expected open, EXPANDER.PIN or an LED part"
    ))
}

/// A `speaker` part, wired to a DAC output or listening to its own pin.
struct SpeakerOut {
    part: usize,
    feed: Feed,
    /// The DAC code, the pin's level or the sample, as -1..1.
    x: f32,
    gain: f32,
    /// High-pass state: last input, last output.
    hp: (f32, f32),
}

/// What a speaker plays, by part index.
#[derive(Clone, Copy, PartialEq)]
enum Feed {
    /// A dac8165 channel (0 = A).
    Dac(usize, usize),
    /// A pcm5102's left, right, or both mixed.
    I2s(usize, I2sChannel),
    /// The speaker's own pin: it reports the level.
    Pin,
}

#[derive(Clone, Copy, PartialEq)]
enum I2sChannel {
    Left,
    Right,
    Mix,
}

impl I2sChannel {
    /// The channel's sample of a `sample` event, as -1..1.
    fn take(self, value: i64) -> f32 {
        let (l, r) = ((value >> 16) as u16 as i16, value as u16 as i16);
        let s = match self {
            I2sChannel::Left => l as f32,
            I2sChannel::Right => r as f32,
            I2sChannel::Mix => (l as f32 + r as f32) / 2.0,
        };
        s / 32768.0
    }
}

fn dac_level(code: u16) -> f32 {
    code as f32 / 8192.0 - 1.0
}

fn pin_level(level: u16) -> f32 {
    level as f32 / 32767.5 - 1.0
}

/// A speaker's `dac` key on `board`: the feed and the level it starts at.
fn resolve_dac(board: &Board, speaker: &str, key: &str) -> Result<(Feed, f32), String> {
    // A part's whole name is the mix; else PART.CHANNEL.
    let (name, ch) = match board.part_index(key) {
        Some(_) => (key, None),
        None => match key.rsplit_once('.') {
            Some((n, c)) => (n, Some(c)),
            None => (key, None),
        },
    };
    let bad = |what: &str| format!("speaker {speaker}: {key:?}: {what}");
    let at = board.part_index(name).ok_or_else(|| bad("no such part"))?;
    if let Some(dac) = board.device::<Dac8165>(name) {
        let ch = match ch.map(str::to_ascii_uppercase).as_deref() {
            Some("A") => 0,
            Some("B") => 1,
            Some("C") => 2,
            Some("D") => 3,
            _ => return Err(bad("a dac8165 takes a channel, A to D (dac.A)")),
        };
        return Ok((Feed::Dac(at, ch), dac_level(dac.code(ch))));
    }
    if let Some(pcm) = board.device::<Pcm5102>(name) {
        let ch = match ch.map(str::to_ascii_uppercase).as_deref() {
            None => I2sChannel::Mix,
            Some("L") => I2sChannel::Left,
            Some("R") => I2sChannel::Right,
            _ => return Err(bad("a pcm5102's channels are L and R")),
        };
        let x = ch.take((pcm.left as u16 as i64) << 16 | pcm.right as u16 as i64);
        return Ok((Feed::I2s(at, ch), x));
    }
    Err(bad("not a dac8165 or a pcm5102"))
}

/// The board's speakers, at the levels and volumes they start from; an
/// error names a speaker whose `dac` isn't a DAC on the board.
fn speakers(board: &Board) -> Result<Vec<SpeakerOut>, String> {
    let mut v = Vec::new();
    for (part, p) in board.parts.iter().enumerate() {
        let Some(spk) = board.device::<Speaker>(&p.name) else {
            continue;
        };
        let (feed, x) = match &spk.source {
            Source::Dac(key) => resolve_dac(board, &p.name, key)?,
            Source::Pin(pwm) => (Feed::Pin, pin_level(pwm.level)),
        };
        v.push(SpeakerOut {
            part,
            feed,
            x,
            gain: spk.level() as f32 / 100.0,
            hp: (x, 0.0),
        });
    }
    Ok(v)
}

impl SpeakerOut {
    /// The level `e` gives this speaker, if it is about what it plays.
    fn level_in(&self, e: &BoardEvent) -> Option<f32> {
        match (self.feed, e.label) {
            (Feed::Dac(at, ch), "dac") if e.part == at && (e.value >> 16) as usize == ch => {
                Some(dac_level(e.value as u16))
            }
            (Feed::I2s(at, ch), "sample") if e.part == at => Some(ch.take(e.value)),
            (Feed::Pin, "level") if e.part == self.part => Some(pin_level(e.value as u16)),
            _ => None,
        }
    }
}

/// Whether `e` is the sound itself (a speaker on a pin reporting its level,
/// an I2S DAC's samples): tens of thousands a second, not something to log.
pub fn is_sound(board: &Board, e: &BoardEvent) -> bool {
    let kind = board.parts[e.part].kind.as_str();
    matches!(
        (kind, e.label),
        ("speaker", "level") | ("pcm5102", "sample")
    )
}

/// Check that each speaker on `board` on a DAC names one.
pub fn check(board: &Board) -> Result<(), String> {
    speakers(board).map(|_| ())
}

/// Pitch for a CV.
pub fn hz(volts: f32) -> f32 {
    C4_HZ * volts.exp2()
}

/// `C4 +3¢` for a CV.
pub fn note_name(volts: f32) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let semis = volts * 12.0 + 60.0;
    let n = semis.round();
    let cents = ((semis - n) * 100.0).round() as i32;
    let n = n as i32;
    let name = format!(
        "{}{}",
        NAMES[n.rem_euclid(12) as usize],
        n.div_euclid(12) - 1
    );
    match cents {
        0 => name,
        c => format!("{name} {c:+}¢"),
    }
}

/// What a patch hears right now, read off the board (for display): the CV
/// (none for a drum) and the gate.
pub fn probe(board: &Board, p: &Patch) -> Option<(Option<f32>, bool)> {
    let volts = match p.kind.is_drum() {
        true => None,
        false => {
            let (part, ch, _) = resolve_cv(board, &p.cv).ok()?;
            Some(
                board
                    .device::<Dac8165>(&board.parts[part].name)?
                    .cv_volts(ch),
            )
        }
    };
    let open = match resolve_gate(board, &p.gate).ok()? {
        Gate::Open => true,
        Gate::Pin { part, pin } => {
            let x = board.device::<Mcp23008>(&board.parts[part].name)?;
            x.pin_levels() >> pin & 1 != 0
        }
        Gate::Led { part } => board.device::<Led>(&board.parts[part].name)?.lit,
    };
    Some((volts, open))
}

struct Voice {
    kind: Kind,
    level: f32,
    cv: Option<(usize, usize, CvScale)>,
    gate: Gate,
    volts: f32,
    open: bool,
    phase: f32,
    env: f32,
    /// A drum's hit in progress: samples since the trigger.
    hit: Option<u32>,
    /// A drum's noise source (xorshift, so renders repeat) and high-pass
    /// filter state (last input, last output).
    noise: u32,
    hp: (f32, f32),
}

impl Voice {
    /// The next drum sample, `t` seconds into the hit.
    fn drum(&mut self, t: f32, rate: f32) -> f32 {
        let decay = |tau: f32| (-t / tau).exp();
        let tone = |v: &mut Voice, hz: f32| {
            v.phase = (v.phase + hz / rate).fract();
            (v.phase * std::f32::consts::TAU).sin()
        };
        match self.kind {
            Kind::Kick => {
                let hz = 48.0 + 120.0 * decay(0.03);
                tone(self, hz) * decay(0.25)
            }
            Kind::Snare => {
                let body = tone(self, 185.0) * decay(0.05) * 0.6;
                body + self.hissed(1500.0, rate) * decay(0.13) * 0.9
            }
            Kind::Hat => self.hissed(7000.0, rate) * decay(0.035),
            Kind::OpenHat => self.hissed(7000.0, rate) * decay(0.3),
            Kind::Clap => {
                // Three quick claps, then the room.
                let env = if t < 0.03 {
                    (-(t % 0.01) / 0.006).exp()
                } else {
                    0.7 * (-(t - 0.03) / 0.12).exp()
                };
                self.hissed(1200.0, rate) * env
            }
            _ => 0.0,
        }
    }

    /// White noise, high-passed at about `hz`.
    fn hissed(&mut self, hz: f32, rate: f32) -> f32 {
        self.noise ^= self.noise << 13;
        self.noise ^= self.noise >> 17;
        self.noise ^= self.noise << 5;
        let x = self.noise as f32 / u32::MAX as f32 * 2.0 - 1.0;
        let rc = 1.0 / (std::f32::consts::TAU * hz);
        let a = rc / (rc + 1.0 / rate);
        self.hp.1 = a * (self.hp.1 + x - self.hp.0);
        self.hp.0 = x;
        self.hp.1
    }
}

/// The oscillators, rendering mono samples.
pub struct Synth {
    voices: Vec<Voice>,
    speakers: Vec<SpeakerOut>,
    rate: u32,
    /// Samples rendered by [`Synth::feed`] so far.
    pos: u64,
    /// What turns the events' cycles into time.
    clock: Clock,
}

impl Synth {
    /// Oscillators for `patches` (completed with the board's defaults),
    /// starting from the board's present outputs.
    pub fn new(board: &Board, patches: &[Patch], rate: u32) -> Result<Self, String> {
        let mut voices = Vec::new();
        for (i, p) in patches.iter().enumerate() {
            let mut p = p.clone();
            p.complete(board);
            let cv = match p.kind.is_drum() {
                true => None,
                false => Some(resolve_cv(board, &p.cv)?),
            };
            let gate = resolve_gate(board, &p.gate)?;
            let (volts, open) = probe(board, &p).unwrap_or_default();
            voices.push(Voice {
                kind: p.kind,
                level: p.level.clamp(0.0, 1.0),
                cv,
                gate,
                volts: volts.unwrap_or_default(),
                // A drum on an open gate never triggers; one already high at
                // the start waits for the next edge.
                open,
                phase: 0.0,
                env: 0.0,
                hit: None,
                noise: 0x9E37_79B9 ^ (i as u32 + 1).wrapping_mul(0x85EB_CA6B),
                hp: (0.0, 0.0),
            });
        }
        Ok(Synth {
            voices,
            speakers: speakers(board)?,
            rate,
            pos: 0,
            clock: Clock::DEFAULT,
        })
    }

    /// Whether `e` changes a CV or gate an oscillator listens to, or what a
    /// speaker plays.
    fn hears(&self, e: &BoardEvent) -> bool {
        self.speakers
            .iter()
            .any(|s| s.level_in(e).is_some() || (e.label == "volume" && e.part == s.part))
            || self.voices.iter().any(|v| {
                v.cv.is_some_and(|cv| {
                    e.label == "dac" && e.part == cv.0 && (e.value >> 16) as usize == cv.1
                }) || match v.gate {
                    Gate::Open => false,
                    Gate::Pin { part, .. } => e.label == "pins" && e.part == part,
                    Gate::Led { part } => matches!(e.label, "on" | "off") && e.part == part,
                }
            })
    }

    /// Take a board event into the oscillators' state.
    pub fn apply(&mut self, e: &BoardEvent) {
        for s in &mut self.speakers {
            if let Some(x) = s.level_in(e) {
                s.x = x;
            } else if e.label == "volume" && e.part == s.part {
                s.gain = e.value as f32 / 100.0;
            }
        }
        for v in &mut self.voices {
            let was_open = v.open;
            match (e.label, &v.gate, v.cv) {
                ("dac", _, Some((part, ch, scale)))
                    if e.part == part && (e.value >> 16) as usize == ch =>
                {
                    v.volts = scale.volts(e.value as u16);
                }
                ("pins", &Gate::Pin { part, pin }, _) if e.part == part => {
                    v.open = e.value >> pin & 1 != 0;
                }
                ("on" | "off", &Gate::Led { part }, _) if e.part == part => {
                    v.open = e.label == "on";
                }
                _ => {}
            }
            if v.kind.is_drum() && v.open && !was_open {
                v.hit = Some(0);
                v.phase = 0.0;
            }
        }
    }

    /// Mix the oscillators into `out` (overwriting it).
    pub fn render(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        let rate = self.rate as f32;
        let ramp = 1.0 / (RAMP_SECS * rate);
        for v in &mut self.voices {
            if v.kind.is_drum() {
                let Some(start) = v.hit else { continue };
                let n = out.len() as u32;
                for (i, s) in out.iter_mut().enumerate() {
                    let t = (start + i as u32) as f32 / rate;
                    *s += v.drum(t, rate) * v.level * DRUM_MIX;
                }
                v.hit = Some(start + n).filter(|&k| (k as f32) < HIT_SECS * rate);
                continue;
            }
            let dt = hz(v.volts).clamp(1.0, rate * 0.45) / rate;
            let target = if v.open { 1.0 } else { 0.0 };
            for s in out.iter_mut() {
                v.env += (target - v.env).clamp(-ramp, ramp);
                if v.env > 0.0 {
                    *s += v.kind.sample(v.phase, dt) * v.env * v.level * MIX;
                }
                v.phase = (v.phase + dt).fract();
            }
        }
        let rc = 1.0 / (std::f32::consts::TAU * SPEAKER_HP_HZ);
        let a = rc / (rc + 1.0 / rate);
        for spk in &mut self.speakers {
            for s in out.iter_mut() {
                spk.hp.1 = a * (spk.hp.1 + spk.x - spk.hp.0);
                spk.hp.0 = spk.x;
                *s += spk.hp.1 * spk.gain;
            }
        }
        for s in out.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
    }

    /// Sample index for emulated time `cycles`.
    fn sample_at(&self, cycles: u64) -> u64 {
        (self.clock.seconds(cycles) * self.rate as f64).max(0.0) as u64
    }

    /// Render up to emulated time `cycles` (on the clock of the last
    /// [`feed`](Self::feed)) onto `out`.
    pub fn advance_to(&mut self, cycles: u64, out: &mut Vec<f32>) {
        let end = self.sample_at(cycles);
        if end > self.pos {
            let start = out.len();
            out.resize(start + (end - self.pos) as usize, 0.0);
            self.render(&mut out[start..]);
            self.pos = end;
        }
    }

    /// Render up to each event and take it in, in emulated time.
    pub fn feed(&mut self, events: &[BoardEvent], clock: Clock, out: &mut Vec<f32>) {
        self.clock = clock;
        for e in events {
            if self.hears(e) {
                self.advance_to(e.cycles, out);
                self.apply(e);
            }
        }
    }
}

/// A synth playing live: the emulator pushes events, the sound card pulls
/// samples. Events play `LEAD_SECS` after they happened, against the
/// sound card's clock; when the emulator runs slower or faster than real
/// time the two are lined up again.
pub struct Live {
    synth: Synth,
    /// Events and the sound card sample to play them at.
    queue: VecDeque<(u64, BoardEvent)>,
    /// Sound card sample minus emulated sample, once lined up.
    offset: Option<i64>,
    /// Samples the sound card has taken.
    played: u64,
    pub volume: f32,
}

impl Live {
    pub fn new(synth: Synth) -> Self {
        Live {
            synth,
            queue: VecDeque::new(),
            offset: None,
            played: 0,
            volume: 0.8,
        }
    }

    pub fn rate(&self) -> u32 {
        self.synth.rate
    }

    /// New oscillators (a reset, a new patch); timing carries on.
    pub fn replace(&mut self, synth: Synth) {
        self.synth = synth;
        self.queue.clear();
    }

    /// Take the events from a stretch of emulation.
    pub fn push(&mut self, events: &[BoardEvent], clock: Clock) {
        self.synth.clock = clock;
        let rate = self.synth.rate as f64;
        let (lead, ahead) = ((LEAD_SECS * rate) as i64, (MAX_AHEAD_SECS * rate) as i64);
        let now = self.played as i64;
        for e in events {
            if !self.synth.hears(e) {
                continue;
            }
            let at = self.synth.sample_at(e.cycles) as i64;
            let t = match self.offset {
                Some(o) if (now..now + ahead).contains(&(at + o)) => at + o,
                _ => {
                    self.offset = Some(now + lead - at);
                    now + lead
                }
            };
            self.queue.push_back((t as u64, e.clone()));
        }
    }

    /// Fill the sound card's buffer.
    pub fn render(&mut self, out: &mut [f32]) {
        let mut i = 0;
        while i < out.len() {
            let now = self.played + i as u64;
            while let Some((_, e)) = self.queue.pop_front_if(|(t, _)| *t <= now) {
                self.synth.apply(&e);
            }
            let next = self.queue.front().map_or(u64::MAX, |(t, _)| *t);
            let end = out.len().min((next - self.played) as usize);
            self.synth.render(&mut out[i..end]);
            i = end;
        }
        self.played += out.len() as u64;
        for s in out {
            *s *= self.volume;
        }
    }
}

/// Mono samples as a 16-bit PCM WAV file.
pub fn wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data = samples.len() as u32 * 2;
    let mut v = Vec::with_capacity(44 + data as usize);
    v.extend(b"RIFF");
    v.extend((36 + data).to_le_bytes());
    v.extend(b"WAVEfmt ");
    v.extend(16u32.to_le_bytes());
    v.extend(1u16.to_le_bytes()); // PCM
    v.extend(1u16.to_le_bytes()); // mono
    v.extend(rate.to_le_bytes());
    v.extend((rate * 2).to_le_bytes());
    v.extend(2u16.to_le_bytes());
    v.extend(16u16.to_le_bytes());
    v.extend(b"data");
    v.extend(data.to_le_bytes());
    for s in samples {
        v.extend(((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: &str = r#"
parts.dac = { kind = "dac8165", sdi = 4, cs = 5, clk = 6 }
parts.x = { kind = "mcp23008", bus = "i2c1", addr = 0x20 }
parts.lamp = { kind = "led", gpio = 15 }
buses.i2c1 = { sda = 2, scl = 3 }
panel.x = [{ at = [0, 0], pins = [4, 5] }, { at = [0, 1], pins = [2, 3], mode = "view" }]
"#;

    fn board() -> Board {
        Board::parse_with(BOARD, &pcb_devices::registry()).unwrap()
    }

    fn ev(board: &Board, cycles: u64, part: &str, label: &'static str, value: i64) -> BoardEvent {
        BoardEvent {
            cycles,
            part: board.part_index(part).unwrap(),
            label,
            value,
        }
    }

    /// Zero crossings upwards per second.
    fn pitch(s: &[f32], rate: u32) -> f32 {
        let ups = s.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        ups as f32 * rate as f32 / s.len() as f32
    }

    #[test]
    fn patches_parse_and_take_the_board_defaults() {
        let b = board();
        let mut p: Patch = "square".parse().unwrap();
        p.complete(&b);
        assert_eq!(p.to_string(), "square:dac.A:x.2");
        let p: Patch = "sine:dac.C:lamp".parse().unwrap();
        assert_eq!((p.cv.as_str(), p.gate.as_str()), ("dac.C", "lamp"));
        assert!("buzz".parse::<Patch>().is_err());
        assert!(Synth::new(&b, &["saw:dac.E".parse().unwrap()], WAV_RATE).is_err());
        assert!(Synth::new(&b, &["saw:dac.A:x.9".parse().unwrap()], WAV_RATE).is_err());
        assert!(Synth::new(&b, &["saw:dac.A:nope".parse().unwrap()], WAV_RATE).is_err());
        assert_eq!(gate_outputs(&b).len(), 1 + 8 + 1);
        assert_eq!(cv_outputs(&b), ["dac.A", "dac.B", "dac.C", "dac.D"]);
    }

    #[test]
    fn plays_the_cv_while_the_gate_is_high() {
        let b = board();
        let mut s = Synth::new(&b, &["square:dac.B:x.3".parse().unwrap()], WAV_RATE).unwrap();
        let ms = |n: u64| n * 1000 * CYCLES_PER_US;
        // 1 V on a plain DAC (2.5 V full scale) is code 6553.6: about C5.
        let mut out = Vec::new();
        s.feed(
            &[
                ev(&b, ms(10), "dac", "dac", 1 << 16 | 6554),
                ev(&b, ms(100), "x", "pins", 1 << 3),
                ev(&b, ms(600), "x", "pins", 0),
                ev(&b, ms(700), "dac", "dac", 0), // channel A: not ours
            ],
            Clock::DEFAULT,
            &mut out,
        );
        s.advance_to(ms(800), &mut out);
        assert_eq!(out.len(), 48 * 800);
        let silent = |r: std::ops::Range<usize>| out[r].iter().all(|v| *v == 0.0);
        assert!(silent(0..4800));
        let tone = &out[48 * 150..48 * 550];
        assert!((pitch(tone, WAV_RATE) - 2.0 * C4_HZ).abs() < 3.0);
        assert!(tone.iter().any(|v| v.abs() > 0.2));
        assert!(silent(48 * 610..48 * 800), "released after the ramp");
    }

    #[test]
    fn speakers_on_a_pin_play_its_level() {
        let text = format!("{BOARD}parts.spk = {{ kind = \"speaker\", gpio = 20 }}\n");
        let b = Board::parse_with(&text, &pcb_devices::registry()).unwrap();
        let mut s = Synth::new(&b, &[], WAV_RATE).unwrap();
        // A 500 Hz tone as PWM duty: 75 % and 25 %, switching each ms. The
        // DAC's events are someone else's.
        let ms = |n: u64| n * 1000 * CYCLES_PER_US;
        let mut events: Vec<_> = (0..100)
            .map(|n| {
                ev(
                    &b,
                    ms(n),
                    "spk",
                    "level",
                    if n % 2 == 0 { 49151 } else { 16384 },
                )
            })
            .collect();
        events.push(ev(&b, ms(50), "dac", "dac", 16383));
        events.sort_by_key(|e| e.cycles);
        let mut out = Vec::new();
        s.feed(&events, Clock::DEFAULT, &mut out);
        s.advance_to(ms(100), &mut out);
        let tone = &out[48 * 40..48 * 100];
        assert!((pitch(tone, WAV_RATE) - 500.0).abs() < 20.0);
        let peak = tone.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!((0.45..0.55).contains(&peak), "{peak}");
    }

    #[test]
    fn speakers_play_an_i2s_dacs_samples() {
        let text = format!(
            "{BOARD}parts.pcm = {{ kind = \"pcm5102\", bck = 20, lrck = 21, din = 22 }}\n\
             parts.l = {{ kind = \"speaker\", dac = \"pcm.L\" }}\n\
             parts.mix = {{ kind = \"speaker\", dac = \"pcm\", volume = 0 }}\n"
        );
        let b = Board::parse_with(&text, &pcb_devices::registry()).unwrap();
        let mut s = Synth::new(&b, &[], WAV_RATE).unwrap();
        // A 500 Hz square on the left, silence on the right, at 24 kHz.
        let at = |n: u64| n * 125_000_000 / 24_000;
        let frame = |l: i16, r: i16| (l as u16 as i64) << 16 | r as u16 as i64;
        let events: Vec<_> = (0..2400)
            .map(|n| {
                let l = if n / 24 % 2 == 0 { 16384 } else { -16384 };
                ev(&b, at(n), "pcm", "sample", frame(l, 0))
            })
            .collect();
        let mut out = Vec::new();
        s.feed(&events, Clock::DEFAULT, &mut out);
        s.advance_to(at(2400), &mut out);
        let tone = &out[48 * 40..48 * 100];
        assert!((pitch(tone, WAV_RATE) - 500.0).abs() < 20.0);
        let peak = tone.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(
            (0.45..0.6).contains(&peak),
            "the left half full scale: {peak}"
        );
        let bad = |key: &str| {
            let t = format!(
                "parts.pcm = {{ kind = \"pcm5102\", bck = 20, lrck = 21, din = 22 }}\n\
                 parts.s = {{ kind = \"speaker\", dac = \"{key}\" }}\n"
            );
            check(&Board::parse_with(&t, &pcb_devices::registry()).unwrap()).unwrap_err()
        };
        assert!(bad("pcm.A").contains("channels are L and R"));
        assert!(bad("nope").contains("no such part"));
    }

    #[test]
    fn speakers_play_the_dac_output() {
        let text =
            format!("{BOARD}parts.spk = {{ kind = \"speaker\", dac = \"dac.B\", volume = 50 }}\n");
        let b = Board::parse_with(&text, &pcb_devices::registry()).unwrap();
        assert!(
            default_patches(&b).is_empty(),
            "no default saw with a speaker"
        );
        let mut s = Synth::new(&b, &[], WAV_RATE).unwrap();
        // A 1 kHz square across the whole DAC range, as samples at 8 kHz.
        let at = |n: u64| n * 125 * CYCLES_PER_US;
        let mut events: Vec<_> = (0..800)
            .map(|n| {
                ev(
                    &b,
                    at(n),
                    "dac",
                    "dac",
                    1 << 16 | if n % 8 < 4 { 16383 } else { 0 },
                )
            })
            .collect();
        events.push(ev(&b, at(400), "dac", "dac", 9000)); // channel A: not ours
        events.push(ev(&b, at(600), "spk", "volume", 0));
        events.sort_by_key(|e| e.cycles);
        let mut out = Vec::new();
        s.feed(&events, Clock::DEFAULT, &mut out);
        s.advance_to(at(800), &mut out);
        // After the step from code 0 at power-up has died away.
        let tone = &out[48 * 40..48 * 70];
        assert!((pitch(tone, WAV_RATE) - 1000.0).abs() < 40.0);
        let peak = tone.iter().fold(0f32, |m, v| m.max(v.abs()));
        // Half volume, and the high-pass's droop between edges.
        assert!((0.45..0.55).contains(&peak), "{peak}");
        assert!(out[48 * 76..].iter().all(|v| v.abs() < 0.01), "muted");

        let text = format!("{BOARD}parts.spk = {{ kind = \"speaker\", dac = \"nope.A\" }}\n");
        let b = Board::parse_with(&text, &pcb_devices::registry()).unwrap();
        assert_eq!(
            check(&b).unwrap_err(),
            "speaker spk: \"nope.A\": no such part"
        );
    }

    #[test]
    fn drums_hit_on_rising_gate_edges() {
        let b = board();
        let kit = drum_kit(&b);
        let names: Vec<_> = kit.iter().map(Patch::to_string).collect();
        assert_eq!(
            names,
            ["kick:x.2", "snare:x.3"],
            "as many as the panel's gates"
        );
        let p: Patch = "clap:lamp".parse().unwrap();
        assert_eq!((p.cv.as_str(), p.gate.as_str()), ("", "lamp"));
        assert!("kick:x.2:x.3".parse::<Patch>().is_err());

        let mut s = Synth::new(&b, &kit, WAV_RATE).unwrap();
        let ms = |n: u64| n * 1000 * CYCLES_PER_US;
        let mut out = Vec::new();
        let events = [
            // A 1 ms trigger on the kick's gate still plays a whole hit.
            ev(&b, ms(100), "x", "pins", 1 << 2),
            ev(&b, ms(101), "x", "pins", 0),
            // The snare's gate rises; the kick's staying low is no edge.
            ev(&b, ms(1000), "x", "pins", 1 << 3),
            ev(&b, ms(1500), "x", "pins", 1 << 3 | 1 << 5),
        ];
        s.feed(&events, Clock::DEFAULT, &mut out);
        s.advance_to(ms(2000), &mut out);
        let loud =
            |a: usize, b: usize| out[48 * a..48 * b].iter().fold(0f32, |m, v| m.max(v.abs()));
        assert_eq!(loud(0, 100), 0.0);
        assert!(loud(100, 300) > 0.3, "kick");
        assert!(loud(900, 1000) < 0.05, "decayed");
        assert!(loud(1000, 1100) > 0.1, "snare");
        assert!(loud(1600, 2000) < 0.01, "no retrigger without a new edge");
        // The same events render the same samples, however the rendering
        // is chunked (live, the sound card picks the chunks).
        let mut again = Vec::new();
        let mut s = Synth::new(&b, &kit, WAV_RATE).unwrap();
        for e in &events {
            // Odd-sized chunks up to the event.
            while s.pos + 997 < s.sample_at(e.cycles) {
                let next = (s.pos + 997) * 125_000_000 / WAV_RATE as u64;
                s.advance_to(next, &mut again);
            }
            s.feed(std::slice::from_ref(e), Clock::DEFAULT, &mut again);
        }
        s.advance_to(ms(2000), &mut again);
        assert_eq!(again, out);
    }

    #[test]
    fn notes_are_named() {
        assert_eq!(note_name(0.0), "C4");
        assert_eq!(note_name(0.75), "A4");
        assert_eq!(note_name(-1.0 + 1.0 / 12.0), "C#3");
        assert_eq!(note_name(0.03 / 12.0), "C4 +3¢");
        assert!((hz(0.75) - 440.0).abs() < 0.01);
    }

    #[test]
    fn live_plays_events_a_lead_behind_the_emulator() {
        let b = board();
        let mut live =
            Live::new(Synth::new(&b, &["saw:dac.A:open".parse().unwrap()], 1000).unwrap());
        let mut out = vec![0.0; 10];
        live.render(&mut out);
        // An event at emulated 1 s lines the clocks up: it plays LEAD after now.
        live.push(&[ev(&b, 125_000_000, "dac", "dac", 6554)], Clock::DEFAULT);
        assert_eq!(live.queue[0].0, 10 + 50);
        // A later one keeps the spacing.
        live.push(
            &[ev(&b, 125_000_000 + 125_000 * 20, "dac", "dac", 0)],
            Clock::DEFAULT,
        );
        assert_eq!(live.queue[1].0, 80);
        let mut out = vec![0.0; 60];
        live.render(&mut out);
        assert_eq!(live.queue.len(), 1);
        assert!((live.synth.voices[0].volts - 1.0).abs() < 0.001);
        // Far behind the sound card: lined up again.
        live.push(&[ev(&b, 0, "dac", "dac", 1)], Clock::DEFAULT);
        assert_eq!(live.queue[1].0, 70 + 50);
    }

    #[test]
    fn wav_header() {
        let w = wav(&[0.0, 1.0, -1.0], 8000);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(w.len(), 44 + 6);
        assert_eq!(&w[44..], [0, 0, 0xFF, 0x7F, 0x01, 0x80]);
    }
}
