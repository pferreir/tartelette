//! GROOVEBOX AUDIO: a drum machine and bass synth for a Pico with a Pimoroni
//! Pico Display Pack 2.0 (`boards/groovebox-audio.toml` with the
//! `pico-display-2` shield) and a speaker on GPIO16. A kick, a snare, closed
//! and open hats and a filtered saw bass are synthesized on the Pico, 16
//! steps each, shown as rings with the playhead sweeping round.
//!
//! - **A**: next step; **B**: step on / off (on release)
//! - **X**: next track (on release); **Y**: play / stop (on release)
//! - **X** held + A: previous step
//! - **B** held + A, on the bass ring: the step's note up a semitone
//! - **Y** held + A / B: tempo up / down; Y held + X: next groove
//!
//! The sound: PWM slice 0 drives GPIO16 at 25 kHz (TOP 4999 at 125 MHz, so
//! a sample is a level from 0 to 5000). Each time it wraps it asks the DMA
//! for the next level, which a DMA channel moves into its compare register
//! from a ring of two halves (the register is double-buffered: the level
//! takes effect at the next wrap). When the DMA finishes a half, its
//! interrupt re-arms it on the other half (the read address wraps round the
//! ring) and renders the next 64 samples into the half just played. The
//! sequencer runs in there too, counting samples, so every step lands on
//! its exact sample whatever the screen is doing.
#![no_std]
#![no_main]

use core::{cell::Cell, fmt::Write as _, ptr::addr_of_mut};

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_rp::{
    bind_interrupts,
    gpio::{Input, Level, Output, Pull},
    interrupt,
    interrupt::{InterruptExt, Priority},
    pac,
    peripherals::DMA_CH0,
    pwm::{self, Pwm},
    spi::{self, Spi},
};
use embassy_sync::{
    blocking_mutex::{Mutex, raw::CriticalSectionRawMutex},
    channel::Channel,
};
use embassy_time::{Duration, Ticker, Timer};
use embedded_graphics::mono_font::ascii::{FONT_6X10, FONT_7X13_BOLD, FONT_9X15_BOLD};
use heapless::String;
use libm::{cosf, sinf};
use panic_probe as _;
use portable_atomic::{AtomicBool, AtomicI16, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use showcase::{Canvas, Lcd, W, mix, rgb};

bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
});

// ------------------------------------------------------------- grooves ----

const STEPS: usize = 16;
const TRACKS: usize = 5;
const BASS: usize = 4;
const NAMES: [&str; TRACKS] = ["kick", "snare", "hat", "open", "bass"];
/// Bass notes are semitones above C2.
const NOTE_MAX: u8 = 24;

#[derive(Clone, Copy)]
struct Groove {
    name: &'static str,
    bpm: u16,
    /// A bit per step for each drum, step 0 the lowest.
    drums: [u16; 4],
    bass: u16,
    notes: [u8; STEPS],
}

impl Groove {
    fn bits(&self, track: usize) -> u16 {
        if track == BASS {
            self.bass
        } else {
            self.drums[track]
        }
    }

    fn bits_mut(&mut self, track: usize) -> &mut u16 {
        if track == BASS {
            &mut self.bass
        } else {
            &mut self.drums[track]
        }
    }
}

const fn steps(s: &[u8; 16]) -> u16 {
    let mut v = 0;
    let mut i = 0;
    while i < 16 {
        if s[i] != b'.' {
            v |= 1 << i;
        }
        i += 1;
    }
    v
}

const GROOVES: [Groove; 3] = [
    Groove {
        name: "Low Tide",
        bpm: 112,
        drums: [
            steps(b"x.....x...x....."),
            steps(b"....x.......x..."),
            steps(b"..x...x...x...x."),
            steps(b"...............x"),
        ],
        bass: steps(b"x..x...x..x..x.."),
        notes: [0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 7, 0, 0, 5, 0, 0],
    },
    Groove {
        name: "Paper Moon",
        bpm: 88,
        drums: [
            steps(b"x.......x.x....."),
            steps(b"........x......."),
            steps(b"x.x.x.x.x.x.x.xx"),
            steps(b"......x........."),
        ],
        bass: steps(b"x.....x...x.x..."),
        notes: [9, 0, 0, 0, 0, 0, 12, 0, 0, 0, 4, 0, 7, 0, 0, 0],
    },
    Groove {
        name: "Neon Rush",
        bpm: 136,
        drums: [
            steps(b"x...x...x...x..."),
            steps(b"....x..x....x..."),
            steps(b".x.x.x.x.x.x.x.x"),
            steps(b"..x...x...x...x."),
        ],
        bass: steps(b"xx.xx.xx.xx.x.xx"),
        notes: [2, 14, 0, 2, 14, 0, 5, 17, 0, 5, 17, 0, 7, 0, 19, 9],
    },
];

/// The groove playing, as the screen edits it; the sequencer takes a copy
/// for each block of samples.
static GROOVE: Mutex<CriticalSectionRawMutex, Cell<Groove>> = Mutex::new(Cell::new(GROOVES[0]));
static PLAYING: AtomicBool = AtomicBool::new(true);

fn groove() -> Groove {
    GROOVE.lock(Cell::get)
}

fn edit(f: impl FnOnce(&mut Groove)) {
    GROOVE.lock(|c| {
        let mut g = c.get();
        f(&mut g);
        c.set(g);
    });
}

// What the sound tells the screen.

/// Steps played so far, times 16, plus the step: it changes on each step.
static STEP: AtomicU32 = AtomicU32::new(0);
/// Each voice's envelope (0..32767), once a block.
static LEVELS: [AtomicU16; TRACKS] = [const { AtomicU16::new(0) }; TRACKS];
/// Every 4th sample of the output, round a ring, and where the next goes.
const SCOPE_N: usize = 128;
static SCOPE: [AtomicI16; SCOPE_N] = [const { AtomicI16::new(0) }; SCOPE_N];
static SCOPE_AT: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------- sound path ----

/// Samples per second: one per PWM period.
const RATE: u32 = 25_000;
/// The PWM's period, in 125 MHz cycles; a level `x` is high for x of them.
const PERIOD: u32 = 125_000_000 / RATE;
const MID: u32 = PERIOD / 2;
/// Samples per half of the ring.
const HALF: usize = 64;
/// The DMA channel the sound uses (embassy's drivers have CH0).
const CH: usize = 1;

/// Aligned to its size, for the DMA's address wrap.
#[repr(C, align(512))]
struct Ring([u32; 2 * HALF]);

static mut RING: Ring = Ring([MID; 2 * HALF]);
static mut SYNTH: Synth = Synth::new();

#[interrupt]
fn DMA_IRQ_1() {
    pac::DMA.ints(1).write_value(1 << CH);
    // Carry on round the ring, then refill the half just played.
    pac::DMA
        .ch(CH)
        .al1_trans_count_trig()
        .write_value(HALF as u32);
    // SAFETY: only this handler touches SYNTH and the half of RING that DMA
    // has finished reading, once main has started the DMA.
    let (synth, ring) = unsafe { (&mut *addr_of_mut!(SYNTH), &mut (*addr_of_mut!(RING)).0) };
    let h = synth.half as usize;
    synth.half ^= 1;
    synth.render(&mut ring[h * HALF..(h + 1) * HALF]);
}

/// Start the DMA feeding slice 0's compare register, once per wrap.
fn start_sound() {
    // SAFETY: the DMA interrupt isn't enabled yet.
    let (synth, ring) = unsafe { (&mut *addr_of_mut!(SYNTH), &mut (*addr_of_mut!(RING)).0) };
    synth.render(&mut ring[..HALF]);
    synth.render(&mut ring[HALF..]);

    let dma = pac::DMA;
    let ch = dma.ch(CH);
    ch.read_addr().write_value(ring.as_ptr() as u32);
    ch.write_addr()
        .write_value(pac::PWM.ch(0).cc().as_ptr() as u32);
    ch.trans_count().write_value(HALF as u32);
    dma.inte(1).modify(|v| *v |= 1 << CH);
    interrupt::DMA_IRQ_1.set_priority(Priority::P1);
    // SAFETY: the handler only needs what is set up above.
    unsafe { interrupt::DMA_IRQ_1.enable() };
    ch.ctrl_trig().write(|w| {
        w.set_data_size(pac::dma::vals::DataSize::SizeWord);
        w.set_incr_read(true);
        w.set_incr_write(false);
        // Wrap the read address round the 512-byte ring.
        w.set_ring_size(9);
        w.set_ring_sel(false);
        w.set_treq_sel(pac::dma::vals::TreqSel::PwmWrap0);
        w.set_chain_to(CH as u8);
        w.set_en(true);
    });
}

// --------------------------------------------------------------- synth ----

/// Phase increment per sample for `hz`.
const fn inc(hz: u32) -> u32 {
    (((hz as u64) << 32) / RATE as u64) as u32
}

/// The per-sample factor (Q16) of an envelope falling to 1/e in `ms`.
const fn decay(ms: u32) -> u32 {
    65536 - 65536 / (ms * RATE / 1000)
}

/// Q15 times Q16 decay.
fn fall(env: &mut i32, k: u32) {
    *env = ((*env as u32 * k) >> 16) as i32;
}

/// A sine from a phase, as a parabola per half cycle: close enough, and no
/// table.
fn sine(p: u32) -> i32 {
    let t = ((p >> 16) & 0x7fff) as i32;
    let y = ((t * (32768 - t)) >> 13).min(32767);
    if p & 0x8000_0000 == 0 { y } else { -y }
}

/// C2 (65.406 Hz).
const C2_INC: u32 = 11_236_732;
/// 2^(n/12) in Q16.
const SEMI: [u32; 12] = [
    65536, 69433, 73562, 77936, 82570, 87480, 92682, 98193, 104032, 110218, 116772, 123715,
];

fn note_inc(n: u8) -> u32 {
    let n = n.min(NOTE_MAX) as u32;
    ((C2_INC as u64 * SEMI[(n % 12) as usize] as u64) >> 16) as u32 * (1 << (n / 12))
}

/// The resting level's rise per sample: 50 ms from 0 to the middle.
const REST_STEP: i32 = 32767 / (RATE as i32 / 20);

const KICK_HI: u32 = inc(170);
const KICK_LO: u32 = inc(48);
const SNARE_HZ: u32 = inc(185);
/// The bass filter's one-pole coefficient (Q12), at rest and its sweep.
const CUT_LO: i32 = 180;
const CUT_SWEEP: i32 = 1700;

struct Synth {
    /// The half of the ring the next render goes into.
    half: u8,
    /// The output's resting level, rising from 0 to the middle at power-up
    /// so the speaker doesn't thump (Q15 of MID).
    rest: i32,
    /// Samples into the current step, the step, and steps played.
    count: u32,
    step: u8,
    played: u32,
    noise: u32,
    last_noise: i32,
    kick_phase: u32,
    kick_amp: i32,
    kick_pitch: i32,
    snare_phase: u32,
    snare_tone: i32,
    snare_noise: i32,
    hat: i32,
    open_hat: i32,
    bass_phase: u32,
    bass_inc: u32,
    /// Samples left with the gate open.
    bass_gate: u32,
    bass_amp: i32,
    bass_env: i32,
    bass_y: i32,
}

impl Synth {
    const fn new() -> Self {
        Self {
            half: 0,
            rest: 0,
            count: 0,
            step: 0,
            played: 0,
            noise: 0x1234_5678,
            last_noise: 0,
            kick_phase: 0,
            kick_amp: 0,
            kick_pitch: 0,
            snare_phase: 0,
            snare_tone: 0,
            snare_noise: 0,
            hat: 0,
            open_hat: 0,
            bass_phase: 0,
            bass_inc: C2_INC,
            bass_gate: 0,
            bass_amp: 0,
            bass_env: 0,
            bass_y: 0,
        }
    }

    /// Fill `out` with compare levels (channel A's; B's stays 0), playing
    /// the groove as it goes.
    fn render(&mut self, out: &mut [u32]) {
        let playing = PLAYING.load(Ordering::Relaxed).then(groove);
        if playing.is_none() {
            (self.count, self.step) = (0, 0);
        }
        for (i, o) in out.iter_mut().enumerate() {
            if let Some(g) = &playing {
                // A sixteenth note's samples.
                let step_len = RATE * 15 / g.bpm as u32;
                if self.count == 0 {
                    self.fire(g, step_len);
                }
                self.count += 1;
                if self.count >= step_len {
                    self.count = 0;
                    self.step = (self.step + 1) % STEPS as u8;
                }
            }
            let s = self.sample().clamp(-32767, 32767);
            if i % 4 == 0 {
                let at = SCOPE_AT.load(Ordering::Relaxed);
                SCOPE[at].store(s as i16, Ordering::Relaxed);
                SCOPE_AT.store((at + 1) % SCOPE_N, Ordering::Relaxed);
            }
            self.rest = (self.rest + REST_STEP).min(32767);
            *o = (((self.rest + s).clamp(0, 65534) * MID as i32) >> 15) as u32;
        }
        let levels = [
            self.kick_amp,
            self.snare_noise.max(self.snare_tone),
            self.hat,
            self.open_hat,
            self.bass_amp,
        ];
        for (l, v) in LEVELS.iter().zip(levels) {
            l.store(v as u16, Ordering::Relaxed);
        }
    }

    /// Start what step `self.step` of `g` plays.
    fn fire(&mut self, g: &Groove, step_len: u32) {
        let on = |bits: u16| bits >> self.step & 1 != 0;
        if on(g.drums[0]) {
            (self.kick_amp, self.kick_pitch, self.kick_phase) = (32767, 32767, 0);
        }
        if on(g.drums[1]) {
            (self.snare_tone, self.snare_noise, self.snare_phase) = (32767, 32767, 0);
        }
        if on(g.drums[2]) {
            // A closed hat chokes an open one.
            (self.hat, self.open_hat) = (32767, 0);
        }
        if on(g.drums[3]) {
            self.open_hat = 32767;
        }
        if on(g.bass) {
            self.bass_inc = note_inc(g.notes[self.step as usize]);
            self.bass_gate = step_len * 3 / 4;
            self.bass_env = 32767;
        }
        self.played += 1;
        STEP.store(self.played << 4 | self.step as u32, Ordering::Relaxed);
    }

    fn sample(&mut self) -> i32 {
        let mut x = self.noise;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.noise = x;
        let noise = (x >> 16) as i32 - 32768;
        // The first difference: noise with its lows taken out, for hats.
        let hiss = (noise - self.last_noise) >> 1;
        self.last_noise = noise;

        // Kick: a sine falling in pitch from 170 to 48 Hz.
        let k_inc = KICK_LO + (((KICK_HI - KICK_LO) as u64 * self.kick_pitch as u64) >> 15) as u32;
        self.kick_phase = self.kick_phase.wrapping_add(k_inc);
        let kick = (sine(self.kick_phase) * self.kick_amp) >> 15;
        fall(&mut self.kick_amp, decay(280));
        fall(&mut self.kick_pitch, decay(35));

        // Snare: a short tone under a longer burst of noise.
        self.snare_phase = self.snare_phase.wrapping_add(SNARE_HZ);
        let snare =
            ((sine(self.snare_phase) * self.snare_tone) >> 16) + ((noise * self.snare_noise) >> 16);
        fall(&mut self.snare_tone, decay(45));
        fall(&mut self.snare_noise, decay(110));

        let hats = ((hiss * self.hat) >> 15) + ((hiss * self.open_hat) >> 15);
        fall(&mut self.hat, decay(30));
        fall(&mut self.open_hat, decay(260));

        // Bass: a saw through a one-pole low-pass that opens on each note
        // and closes again, gated with a short ramp against clicks.
        self.bass_phase = self.bass_phase.wrapping_add(self.bass_inc);
        let saw = (self.bass_phase >> 16) as i32 - 32768;
        let cut = CUT_LO + ((CUT_SWEEP * self.bass_env) >> 15);
        self.bass_y += ((saw - self.bass_y) * cut) >> 12;
        fall(&mut self.bass_env, decay(140));
        let target = if self.bass_gate > 0 { 32767 } else { 0 };
        self.bass_gate = self.bass_gate.saturating_sub(1);
        self.bass_amp += (target - self.bass_amp) >> 5;
        let bass = (self.bass_y * self.bass_amp) >> 15;

        (kick * 14 + snare * 9 + hats * 6 + bass * 9) >> 5
    }
}

// -------------------------------------------------------------- screen ----

const BG: u16 = rgb(10, 8, 20);
const PANEL: u16 = rgb(22, 18, 38);
const GUIDE: u16 = rgb(34, 28, 56);
const INK: u16 = rgb(240, 232, 255);
const DIM: u16 = rgb(130, 120, 170);
const COLORS: [u16; TRACKS] = [
    rgb(255, 70, 140),  // kick
    rgb(255, 205, 70),  // snare
    rgb(110, 250, 200), // hat
    rgb(90, 170, 255),  // open hat
    rgb(200, 120, 255), // bass
];

/// The rings: their centre, and each track's radius and cell size, the
/// kick outermost.
const CX: i32 = 116;
const CY: i32 = 120;
const RADIUS: [i32; TRACKS] = [106, 89, 72, 55, 38];
const CELL: [i32; TRACKS] = [7, 6, 6, 5, 4];
/// The side panel.
const PX: i32 = 234;

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

fn note_name(n: u8) -> String<4> {
    let mut s = String::new();
    let _ = write!(s, "{}{}", NOTE_NAMES[n as usize % 12], 2 + n / 12);
    s
}

struct Screen {
    /// Each cell's centre, and where the playhead's spoke ends.
    cells: [[(i32, i32); STEPS]; TRACKS],
    spoke: [(i32, i32); STEPS],
    track: usize,
    cursor: usize,
}

impl Screen {
    fn new() -> Self {
        let at = |r: i32, i: usize| {
            // Step 0 at the top, clockwise.
            let a = i as f32 * core::f32::consts::TAU / STEPS as f32;
            (
                CX + (r as f32 * sinf(a)) as i32,
                CY - (r as f32 * cosf(a)) as i32,
            )
        };
        Screen {
            cells: core::array::from_fn(|t| core::array::from_fn(|i| at(RADIUS[t], i))),
            spoke: core::array::from_fn(|i| at(RADIUS[0] + CELL[0] + 4, i)),
            track: 0,
            cursor: 0,
        }
    }

    fn draw(&self, cv: &mut Canvas, g: &Groove, playing: bool, step: usize, hint: &[&str]) {
        // Guides, then the playhead's spoke under the cells.
        for r in RADIUS {
            cv.circle(CX, CY, r, GUIDE);
        }
        if playing {
            let (x, y) = self.spoke[step];
            cv.line(CX, CY, x, y, mix(BG, INK, 70));
        }
        for t in 0..TRACKS {
            let bits = g.bits(t);
            for (i, &(x, y)) in self.cells[t].iter().enumerate() {
                let c = COLORS[t];
                let on = bits >> i & 1 != 0;
                let fill = match (on, playing && i == step) {
                    (true, true) => mix(c, INK, 150),
                    (true, false) => c,
                    (false, true) => mix(BG, c, 90),
                    (false, false) if i % 4 == 0 => mix(BG, c, 55),
                    (false, false) => mix(BG, c, 30),
                };
                cv.disc(x, y, CELL[t], fill);
            }
        }
        let (x, y) = self.cells[self.track][self.cursor];
        cv.circle(x, y, CELL[self.track] + 3, INK);
        cv.circle(x, y, CELL[self.track] + 4, INK);

        // The hub: play / stop and the tempo.
        if cv.rows(CY - 26, 52) {
            cv.disc(CX, CY, 26, PANEL);
            if playing {
                for dy in 0..10 {
                    let w = 5 - (dy - 5i32).abs();
                    cv.fill(CX - 3, CY - 18 + dy, w * 2, 1, COLORS[2]);
                }
            } else {
                cv.fill(CX - 4, CY - 17, 9, 9, COLORS[0]);
            }
            let mut s: String<4> = String::new();
            let _ = write!(s, "{}", g.bpm);
            cv.text_c(CX, CY - 5, &s, &FONT_9X15_BOLD, INK);
            cv.text_c(CX, CY + 10, "bpm", &FONT_6X10, DIM);
        }

        self.panel(cv, g, hint);
    }

    fn panel(&self, cv: &mut Canvas, g: &Groove, hint: &[&str]) {
        cv.fill(PX, 0, W - PX, 240, PANEL);
        cv.text(PX + 6, 6, "GROOVEBOX", &FONT_7X13_BOLD, INK);
        cv.text(PX + 6, 20, "AUDIO", &FONT_6X10, COLORS[0]);
        cv.text(PX + 6, 34, g.name, &FONT_6X10, DIM);
        // The tracks, each with its voice's envelope as a meter.
        for t in 0..TRACKS {
            let y = 52 + t as i32 * 14;
            if !cv.rows(y, 12) {
                continue;
            }
            let selected = t == self.track;
            if selected {
                cv.fill(PX, y, 3, 11, COLORS[t]);
            }
            let ink = if selected { INK } else { DIM };
            cv.text(PX + 6, y + 1, NAMES[t], &FONT_6X10, ink);
            let level = LEVELS[t].load(Ordering::Relaxed) as i32;
            let (bx, bw) = (PX + 44, W - PX - 50);
            cv.fill(bx, y + 3, bw, 5, GUIDE);
            cv.fill(bx, y + 3, level * bw / 32767, 5, COLORS[t]);
        }
        // The step under the cursor.
        if cv.rows(124, 12) {
            let mut s: String<20> = String::new();
            let _ = write!(s, "step {:02}", self.cursor + 1);
            if self.track == BASS && g.bass >> self.cursor & 1 != 0 {
                let _ = write!(s, " {}", note_name(g.notes[self.cursor]).as_str());
            }
            cv.text(PX + 6, 124, &s, &FONT_6X10, INK);
        }
        self.scope(cv, 140, 46);
        for (n, line) in hint.iter().enumerate() {
            cv.text(PX + 6, 190 + n as i32 * 10, line, &FONT_6X10, DIM);
        }
    }

    /// The last 20 ms of the speaker's signal.
    fn scope(&self, cv: &mut Canvas, top: i32, h: i32) {
        if !cv.rows(top, h) {
            return;
        }
        let (x0, w) = (PX + 4, W - PX - 8);
        cv.fill(x0, top, w, h, BG);
        let mid = top + h / 2;
        let at = SCOPE_AT.load(Ordering::Relaxed);
        let y = |c: i32| {
            let i = (at + (c * SCOPE_N as i32 / w) as usize) % SCOPE_N;
            let s = SCOPE[i].load(Ordering::Relaxed) as i32;
            (mid - s * (h / 2 - 2) / 32768).clamp(top + 1, top + h - 2)
        };
        for c in 1..w {
            cv.line(x0 + c - 1, y(c - 1), x0 + c, y(c), COLORS[2]);
        }
    }
}

// ---------------------------------------------------------------- main ----

/// The shield's buttons, in A, B, X, Y order.
const A: usize = 0;
const B: usize = 1;
const X: usize = 2;
const Y: usize = 3;

/// Button edges, (button, down), in the order they happened: a frame can
/// take longer than a quick press.
static EDGES: Channel<CriticalSectionRawMutex, (usize, bool), 16> = Channel::new();

#[embassy_executor::task]
async fn buttons(buttons: [Input<'static>; 4]) {
    let mut was = [false; 4];
    let mut ticker = Ticker::every(Duration::from_millis(5));
    loop {
        for (k, b) in buttons.iter().enumerate() {
            let down = b.is_low();
            if down != was[k] {
                was[k] = down;
                let _ = EDGES.try_send((k, down));
            }
        }
        ticker.next().await;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // Slice 0 on GPIO16, low until the sound rises to its resting level. The
    // DMA channel is reserved for the sound, which drives it through its
    // registers.
    let mut cfg = pwm::Config::default();
    cfg.top = (PERIOD - 1) as u16;
    let _pwm = Pwm::new_output_a(p.PWM_SLICE0, p.PIN_16, cfg);
    let _ch = p.DMA_CH1;
    start_sound();
    info!("GROOVEBOX AUDIO: A step, B on/off, X track, Y play; Y+A/B tempo, Y+X groove");

    let mut scfg = spi::Config::default();
    scfg.frequency = embassy_rp::time::Hertz(62_500_000);
    let spi = Spi::new_txonly(p.SPI0, p.PIN_18, p.PIN_19, p.DMA_CH0, Irqs, scfg).unwrap();
    let mut lcd = Lcd::new(
        spi,
        Output::new(p.PIN_9, Level::High),
        Output::new(p.PIN_13, Level::Low),
    );
    let _bl = Output::new(p.PIN_17, Level::High);
    lcd.init().await;
    spawner.spawn(
        buttons([
            Input::new(p.PIN_12, Pull::Up),
            Input::new(p.PIN_20, Pull::Up),
            Input::new(p.PIN_14, Pull::Up),
            Input::new(p.PIN_15, Pull::Up),
        ])
        .unwrap(),
    );

    let mut screen = Screen::new();
    let mut held = [false; 4];
    // Whether a held button has been part of a chord: then its own action,
    // on release, doesn't happen.
    let mut chorded = [false; 4];
    let mut preset = 0;
    loop {
        while let Ok((k, down)) = EDGES.try_receive() {
            match (held[k], down) {
                (false, true) => {
                    chorded[k] = false;
                    match k {
                        A if held[B] && screen.track == BASS => {
                            let c = screen.cursor;
                            edit(|g| {
                                g.notes[c] = (g.notes[c] + 1) % (NOTE_MAX + 1);
                                g.bass |= 1 << c;
                            });
                            chorded[B] = true;
                            info!(
                                "step {} note {}",
                                c + 1,
                                note_name(groove().notes[c]).as_str()
                            );
                        }
                        A | B if held[Y] => {
                            let d = if k == A { 4 } else { -4 };
                            edit(|g| g.bpm = (g.bpm as i32 + d).clamp(60, 180) as u16);
                            (chorded[Y], chorded[k]) = (true, true);
                            info!("tempo {} bpm", groove().bpm);
                        }
                        X if held[Y] => {
                            preset = (preset + 1) % GROOVES.len();
                            GROOVE.lock(|c| c.set(GROOVES[preset]));
                            (chorded[Y], chorded[X]) = (true, true);
                            info!("groove: {}", GROOVES[preset].name);
                        }
                        A if held[X] => {
                            screen.cursor = (screen.cursor + STEPS - 1) % STEPS;
                            chorded[X] = true;
                        }
                        A => screen.cursor = (screen.cursor + 1) % STEPS,
                        _ => {}
                    }
                }
                (true, false) if !chorded[k] => match k {
                    B => {
                        let (t, c) = (screen.track, screen.cursor);
                        edit(|g| *g.bits_mut(t) ^= 1 << c);
                    }
                    X => screen.track = (screen.track + 1) % TRACKS,
                    Y => {
                        let on = !PLAYING.load(Ordering::Relaxed);
                        PLAYING.store(on, Ordering::Relaxed);
                        info!("{}", if on { "play" } else { "stop" });
                    }
                    _ => {}
                },
                _ => {}
            }
            held[k] = down;
        }

        let hint: [&str; 5] = if held[Y] {
            ["Y held:", "A tempo up", "B tempo down", "X groove", ""]
        } else if held[B] && screen.track == BASS {
            ["B held:", "A note up", "", "", ""]
        } else if held[X] {
            ["X held:", "A step back", "", "", ""]
        } else {
            [
                "A next step",
                "B on/off",
                "X next track",
                "Y play/stop",
                "hold Y: more",
            ]
        };
        let (g, playing) = (groove(), PLAYING.load(Ordering::Relaxed));
        let step = (STEP.load(Ordering::Relaxed) & 15) as usize;
        // Only the strips that changed go to the panel.
        lcd.frame(BG, &mut |cv| screen.draw(cv, &g, playing, step, &hint))
            .await;
        Timer::after_millis(15).await;
    }
}
