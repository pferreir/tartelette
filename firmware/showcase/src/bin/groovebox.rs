//! GROOVEBOX: a 16-step drum machine and bass sequencer (`boards/groovebox.toml`).
//!
//! Five tracks: kick, snare, closed and open hat, and a bass line. The drums
//! are trigger pulses on the expander's gate outputs; the bass is a gate
//! plus a 1 V/octave pitch on DAC channel A. It starts playing a groove.
//!
//! - knob: move along the steps; **push** toggles the step under the cursor
//! - push held + knob: the note of a bass step
//! - **Track** (T): next track; **Play** (P): stop / start
//! - **Shift** (S) + knob: tempo; Shift + Track: next preset groove;
//!   Shift + Play: clear the track
//!
//! Two tasks: the sequencer keeps time and owns the I2C bus (gates out, keys
//! in) and the DAC; the UI task reads the knob and draws the screen.
#![no_std]
#![no_main]

use core::{cell::RefCell, fmt::Write as _};

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_rp::{
    bind_interrupts,
    gpio::{Input, Level, Output, Pull},
    i2c::{self, I2c},
    mode::Blocking,
    peripherals::{DMA_CH0, PIO0},
    pio::{InterruptHandler, Pio},
    pio_programs::rotary_encoder::{Direction, PioEncoder, PioEncoderProgram},
    spi::{self, Spi},
    uart::{self, UartTx},
};
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Duration, Instant, Ticker, Timer};
use embedded_graphics::mono_font::ascii::{FONT_6X10, FONT_7X13_BOLD, FONT_9X15_BOLD, FONT_10X20};
use heapless::String;
use panic_probe as _;
use portable_atomic::{AtomicI32, AtomicU8, Ordering};
use showcase::{Canvas, Lcd, mix, rgb};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
});

// --------------------------------------------------------------- state ----

const STEPS: usize = 16;
const TRACKS: usize = 5;
const BASS: usize = 4;
const NAMES: [&str; TRACKS] = ["KICK", "SNARE", "HAT", "OPEN", "BASS"];

/// Semitones above C1 for the bass; 0 is C1 (-3 V at the jack).
const NOTE_MAX: u8 = 36;

#[derive(Clone, Copy)]
struct Pattern {
    name: &'static str,
    bpm: u16,
    /// One bit per step, step 0 the lowest.
    drums: [u16; 4],
    bass: u16,
    notes: [u8; STEPS],
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

const PRESETS: [Pattern; 3] = [
    Pattern {
        name: "Night Drive",
        bpm: 118,
        drums: [
            steps(b"x...x...x...x..x"),
            steps(b"....x.......x..."),
            steps(b"x.x.x.x.x.x.x.x."),
            steps(b"..x...x...x...x."),
        ],
        bass: steps(b"x..x..x.x..x..x."),
        notes: [12, 0, 0, 12, 0, 0, 15, 0, 10, 0, 0, 10, 0, 0, 17, 0],
    },
    Pattern {
        name: "Broken Clock",
        bpm: 96,
        drums: [
            steps(b"x.....x...x....."),
            steps(b"....x.......x.x."),
            steps(b"x.xxx.x.x.xxx.x."),
            steps(b".......x.......x"),
        ],
        bass: steps(b"x.....x.x.x....x"),
        notes: [7, 0, 0, 0, 0, 0, 7, 0, 10, 0, 12, 0, 0, 0, 0, 5],
    },
    Pattern {
        name: "Arcade Run",
        bpm: 138,
        drums: [
            steps(b"x...x...x...x..."),
            steps(b"....x.......x..x"),
            steps(b"xxxxxxxxxxxxxxxx"),
            steps(b"................"),
        ],
        bass: steps(b"xxxxxxxxxxxxxxxx"),
        notes: [
            12, 12, 24, 12, 15, 15, 27, 15, 10, 10, 22, 10, 17, 17, 29, 19,
        ],
    },
];

struct State {
    pat: Pattern,
    preset: usize,
    playing: bool,
    /// The step playing now.
    step: usize,
    /// When each track last fired, for the screen's flashes.
    fired: [Instant; TRACKS],
    version: u32,
}

static STATE: Mutex<CriticalSectionRawMutex, RefCell<State>> = Mutex::new(RefCell::new(State {
    pat: PRESETS[0],
    preset: 0,
    playing: true,
    step: 0,
    fired: [Instant::from_ticks(0); TRACKS],
    version: 0,
}));

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.lock(|s| f(&mut s.borrow_mut()))
}

fn snapshot() -> (Pattern, usize, bool, usize, [Instant; TRACKS], u32) {
    with(|s| (s.pat, s.preset, s.playing, s.step, s.fired, s.version))
}

/// The expander's keys (GP5..7), pressed = bit set, read by the sequencer.
static KEYS: AtomicU8 = AtomicU8::new(0);
static KNOB: AtomicI32 = AtomicI32::new(0);

// ------------------------------------------------------------ hardware ----

const MCP: u8 = 0x20;
const IODIR: u8 = 0x00;
const GPPU: u8 = 0x06;
const GPIO: u8 = 0x09;
const OLAT: u8 = 0x0A;

/// The DAC8165, bit-banged: 24-bit frames, data taken on SCLK's falling edge.
struct Dac {
    sdi: Output<'static>,
    cs: Output<'static>,
    clk: Output<'static>,
}

impl Dac {
    fn write(&mut self, ch: u8, code: u16) {
        // Load mode 01: write the channel and update its output.
        let frame: u32 = (0b01 << 20) | ((ch as u32 & 3) << 17) | ((code as u32 & 0x3fff) << 2);
        self.cs.set_low();
        for bit in (0..24).rev() {
            self.sdi.set_level(Level::from(frame >> bit & 1 == 1));
            self.clk.set_low();
            self.clk.set_high();
        }
        self.cs.set_high();
    }

    /// The jack at `volts` (-5..+5), through the board's output stage:
    /// 1.25 V out of the DAC is 0 V, plus a quarter volt per volt.
    fn volts(&mut self, ch: u8, millivolts: i32) {
        let dac_uv = 1_250_000 + millivolts * 250;
        let code = (dac_uv as i64 * 16384 / 2_500_000).clamp(0, 16383) as u16;
        self.write(ch, code);
    }
}

fn note_mv(note: u8) -> i32 {
    // C1 is -3 V; 1000/12 mV a semitone.
    -3000 + note as i32 * 1000 / 12
}

#[embassy_executor::task]
async fn sequencer(mut i2c: I2c<'static, Blocking>, mut dac: Dac) {
    let _ = i2c.blocking_write(MCP, &[IODIR, 0b1110_0000]);
    let _ = i2c.blocking_write(MCP, &[GPPU, 0b1110_0000]);
    let _ = i2c.blocking_write(MCP, &[OLAT, 0]);
    dac.volts(0, 0);

    let mut ticker = Ticker::every(Duration::from_millis(1));
    let mut next = Instant::now();
    let mut gates = 0u8;
    let mut shown = 0u8;
    let mut drum_off = Instant::now();
    let mut bass_off = Instant::now();
    let mut ms = 0u32;
    loop {
        let now = Instant::now();
        let (playing, bpm) = with(|s| (s.playing, s.pat.bpm));
        let step_len = Duration::from_micros(15_000_000 / bpm as u64);
        if !playing {
            gates = 0;
            next = now;
        } else if now >= next {
            let (step, pat) = with(|s| {
                let step = s.step;
                for t in 0..TRACKS {
                    let on = if t == BASS {
                        s.pat.bass
                    } else {
                        s.pat.drums[t]
                    } >> step
                        & 1
                        == 1;
                    if on {
                        s.fired[t] = now;
                    }
                }
                s.step = (s.step + 1) % STEPS;
                (step, s.pat)
            });
            let mut g = 0u8;
            for (t, drum) in pat.drums.iter().enumerate() {
                if drum >> step & 1 == 1 {
                    g |= 1 << t;
                }
            }
            if pat.bass >> step & 1 == 1 {
                dac.volts(0, note_mv(pat.notes[step]));
                g |= 1 << BASS;
                bass_off = now + step_len * 3 / 4;
            }
            gates = g;
            drum_off = now + Duration::from_millis(10);
            next += step_len;
            if next < now {
                next = now + step_len;
            }
        }
        if now >= drum_off {
            gates &= 1 << BASS;
        }
        if now >= bass_off {
            gates &= !(1 << BASS);
        }
        if gates != shown {
            let _ = i2c.blocking_write(MCP, &[OLAT, gates]);
            shown = gates;
        }
        ms += 1;
        if ms.is_multiple_of(5) {
            let mut v = [0u8];
            if i2c.blocking_write_read(MCP, &[GPIO], &mut v).is_ok() {
                KEYS.store((!v[0] >> 5) & 0b111, Ordering::Relaxed);
            }
        }
        ticker.next().await;
    }
}

#[embassy_executor::task]
async fn knob_task(mut enc: PioEncoder<'static, PIO0, 0>) {
    loop {
        let d = match enc.read().await {
            Direction::Clockwise => 1,
            Direction::CounterClockwise => -1,
        };
        KNOB.fetch_add(d, Ordering::Relaxed);
    }
}

// -------------------------------------------------------------- screen ----

const BG: u16 = rgb(16, 14, 24);
const PANEL: u16 = rgb(30, 27, 44);
const OFF: u16 = rgb(48, 44, 66);
const OFF_BEAT: u16 = rgb(64, 58, 88);
const INK: u16 = rgb(236, 230, 250);
const DIM: u16 = rgb(140, 132, 170);
const CURSOR: u16 = rgb(255, 255, 255);
const COLORS: [u16; TRACKS] = [
    rgb(255, 92, 92),   // kick
    rgb(255, 176, 60),  // snare
    rgb(90, 220, 140),  // hat
    rgb(80, 200, 255),  // open hat
    rgb(200, 120, 255), // bass
];

const GRID_X: i32 = 58;
const GRID_Y: i32 = 34;
const CELL_W: i32 = 15;
const ROW_H: i32 = 24;

fn step_x(i: usize) -> i32 {
    GRID_X + i as i32 * CELL_W + (i as i32 / 4) * 4
}

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

fn note_name(n: u8) -> String<4> {
    let mut s = String::new();
    let _ = write!(s, "{}{}", NOTE_NAMES[n as usize % 12], 1 + n / 12);
    s
}

struct View {
    track: usize,
    cursor: usize,
    shift: bool,
    editing: bool,
}

fn draw(
    cv: &mut Canvas,
    pat: &Pattern,
    playing: bool,
    step: usize,
    fired: &[Instant; TRACKS],
    v: &View,
    now: Instant,
) {
    // Header.
    if cv.rows(0, 30) {
        cv.fill(0, 0, 320, 28, PANEL);
        cv.text(8, 5, "GROOVEBOX", &FONT_10X20, INK);
        let mut s: String<24> = String::new();
        let _ = write!(s, "{} BPM", pat.bpm);
        let tempo_ink = if v.shift { COLORS[1] } else { INK };
        cv.text_r(312, 8, &s, &FONT_9X15_BOLD, tempo_ink);
        // Play / stop symbol.
        let (px, py) = (112, 7);
        if playing {
            for dy in 0..14i32 {
                let w = 7 - (dy - 7).abs();
                cv.fill(px, py + dy, w * 2, 1, COLORS[2]);
            }
        } else {
            cv.fill(px, py, 13, 13, COLORS[0]);
        }
        cv.text(134, 9, pat.name, &FONT_7X13_BOLD, DIM);
    }

    // The step grid.
    for t in 0..TRACKS {
        let y = GRID_Y + t as i32 * ROW_H;
        if !cv.rows(y, ROW_H) {
            continue;
        }
        let bits = if t == BASS { pat.bass } else { pat.drums[t] };
        let selected = t == v.track;
        let age = now.saturating_duration_since(fired[t]).as_millis() as u32;
        let flash = 256u32.saturating_sub(age * 2);
        let label_bg = mix(PANEL, COLORS[t], flash * 3 / 4);
        cv.fill(2, y + 2, 52, ROW_H - 4, label_bg);
        if selected {
            cv.fill(2, y + 2, 3, ROW_H - 4, COLORS[t]);
        }
        cv.text(
            9,
            y + 7,
            NAMES[t],
            &FONT_6X10,
            if selected { INK } else { DIM },
        );
        for i in 0..STEPS {
            let x = step_x(i);
            let on = bits >> i & 1 == 1;
            let mut c = if on {
                COLORS[t]
            } else if i % 4 == 0 {
                OFF_BEAT
            } else {
                OFF
            };
            if playing && i == (step + STEPS - 1) % STEPS {
                c = if on {
                    mix(c, CURSOR, 140)
                } else {
                    mix(c, CURSOR, 60)
                };
            }
            cv.fill(x + 1, y + 3, CELL_W - 3, ROW_H - 6, c);
            if selected && i == v.cursor {
                cv.frame(x - 1, y + 1, CELL_W + 1, ROW_H - 2, 2, CURSOR);
            }
        }
    }

    // The bass line, as a piano roll.
    let roll_y = GRID_Y + TRACKS as i32 * ROW_H + 6;
    let roll_h = 52;
    if cv.rows(roll_y, roll_h) {
        cv.fill(
            step_x(0) - 2,
            roll_y,
            step_x(15) + CELL_W - step_x(0) + 2,
            roll_h,
            PANEL,
        );
        cv.text(9, roll_y + 4, "NOTE", &FONT_6X10, DIM);
        let n = pat.notes[v.cursor];
        let name = note_name(n);
        let ink = if v.editing { COLORS[BASS] } else { INK };
        cv.text(9, roll_y + 20, &name, &FONT_9X15_BOLD, ink);
        for i in 0..STEPS {
            if pat.bass >> i & 1 == 0 {
                continue;
            }
            let x = step_x(i);
            let ny = roll_y + roll_h - 6 - (pat.notes[i] as i32 * (roll_h - 12)) / NOTE_MAX as i32;
            let c = if i == v.cursor && v.track == BASS {
                CURSOR
            } else {
                COLORS[BASS]
            };
            cv.fill(x + 1, ny - 2, CELL_W - 3, 5, c);
        }
    }

    // Help.
    if cv.rows(222, 18) {
        let help = if v.shift {
            "SHIFT  knob: tempo  T: next groove  P: clear"
        } else if v.track == BASS {
            "knob: step  push: on/off  push+knob: note"
        } else {
            "knob: step  push: on/off  T: track  P: play"
        };
        cv.text_c(160, 226, help, &FONT_6X10, DIM);
    }
}

// ---------------------------------------------------------------- main ----

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let mut uart = UartTx::new_blocking(p.UART0, p.PIN_0, uart::Config::default());
    let mut say = move |s: &str| {
        let _ = uart.blocking_write(s.as_bytes());
        let _ = uart.blocking_write(b"\r\n");
    };
    say("GROOVEBOX: T track, P play/stop, S shift; knob and push to edit.");

    let mut cfg = i2c::Config::default();
    cfg.frequency = embassy_rp::time::Hertz(400_000);
    let i2c = I2c::new_blocking(p.I2C1, p.PIN_3, p.PIN_2, cfg);
    let _rst = Output::new(p.PIN_7, Level::High);
    let dac = Dac {
        sdi: Output::new(p.PIN_4, Level::Low),
        cs: Output::new(p.PIN_5, Level::High),
        clk: Output::new(p.PIN_6, Level::High),
    };
    spawner.spawn(sequencer(i2c, dac).unwrap());

    let Pio {
        mut common, sm0, ..
    } = Pio::new(p.PIO0, Irqs);
    let prg = PioEncoderProgram::new(&mut common);
    spawner.spawn(knob_task(PioEncoder::new(&mut common, sm0, p.PIN_21, p.PIN_22, &prg)).unwrap());
    let push = Input::new(p.PIN_20, Pull::Up);

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

    let mut v = View {
        track: 0,
        cursor: 0,
        shift: false,
        editing: false,
    };
    let mut prev_keys = 0u8;
    let mut prev_push = false;
    let mut turned_while_pushed = false;
    let mut knob_seen = KNOB.load(Ordering::Relaxed);
    let mut drawn = (
        u32::MAX,
        usize::MAX,
        usize::MAX,
        usize::MAX,
        false,
        false,
        false,
    );
    let mut msg: String<64> = String::new();
    loop {
        // Input.
        let keys = KEYS.load(Ordering::Relaxed);
        let down = keys & !prev_keys;
        prev_keys = keys;
        v.shift = keys & 0b100 != 0;
        let pushed = push.is_low();
        let k = KNOB.load(Ordering::Relaxed);
        let turn = k - knob_seen;
        knob_seen = k;
        msg.clear();

        if turn != 0 {
            if v.shift {
                with(|s| {
                    s.pat.bpm = (s.pat.bpm as i32 + turn * 2).clamp(60, 200) as u16;
                    s.version += 1;
                    let _ = write!(msg, "tempo {} BPM", s.pat.bpm);
                });
            } else if pushed && v.track == BASS {
                turned_while_pushed = true;
                with(|s| {
                    let n = &mut s.pat.notes[v.cursor];
                    *n = (*n as i32 + turn).clamp(0, NOTE_MAX as i32) as u8;
                    s.pat.bass |= 1 << v.cursor;
                    s.version += 1;
                    let _ = write!(msg, "step {} note {}", v.cursor + 1, note_name(*n).as_str());
                });
            } else {
                v.cursor = (v.cursor as i32 + turn).rem_euclid(STEPS as i32) as usize;
            }
        }
        v.editing = pushed && v.track == BASS;
        if pushed && !prev_push {
            turned_while_pushed = false;
        }
        if !pushed && prev_push && !turned_while_pushed {
            with(|s| {
                let bits = if v.track == BASS {
                    &mut s.pat.bass
                } else {
                    &mut s.pat.drums[v.track]
                };
                *bits ^= 1 << v.cursor;
                s.version += 1;
            });
        }
        prev_push = pushed;
        if down & 0b001 != 0 {
            if v.shift {
                with(|s| {
                    s.preset = (s.preset + 1) % PRESETS.len();
                    s.pat = PRESETS[s.preset];
                    s.step = 0;
                    s.version += 1;
                    let _ = write!(msg, "groove: {} ({} BPM)", s.pat.name, s.pat.bpm);
                });
            } else {
                v.track = (v.track + 1) % TRACKS;
            }
        }
        if down & 0b010 != 0 {
            with(|s| {
                if v.shift {
                    if v.track == BASS {
                        s.pat.bass = 0;
                    } else {
                        s.pat.drums[v.track] = 0;
                    }
                    let _ = write!(msg, "cleared {}", NAMES[v.track]);
                } else {
                    s.playing = !s.playing;
                    s.step = 0;
                    let _ = write!(msg, "{}", if s.playing { "play" } else { "stop" });
                }
                s.version += 1;
            });
        }
        if !msg.is_empty() {
            info!("{}", msg.as_str());
            say(&msg);
        }

        // Screen: when something changed, and while tracks are flashing.
        let (pat, _, playing, step, fired, version) = snapshot();
        let now = Instant::now();
        let flashing = fired
            .iter()
            .any(|&f| now.saturating_duration_since(f) < Duration::from_millis(140));
        let state = (
            version, step, v.track, v.cursor, v.shift, v.editing, playing,
        );
        if state != drawn || flashing {
            // Only the strips that changed go to the panel.
            lcd.frame(BG, &mut |cv| draw(cv, &pat, playing, step, &fired, &v, now))
                .await;
            drawn = state;
            Timer::after_millis(20).await;
        } else {
            Timer::after_millis(10).await;
        }
    }
}
