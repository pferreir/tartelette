//! SPECTRA: an audio scope and spectrum analyser for a Pico with a Pimoroni
//! Pico Display Pack 2.0 (`boards/spectra.toml` with the `pico-display-2`
//! shield), listening to a line input on GPIO26 (ADC0) with a gain knob on
//! GPIO27 (ADC1). In the emulator the input is a `wav` player looping a
//! song; on a real Pico, any audio biased to 1.65 V.
//!
//! - **A**: next view (scope and spectrum, spectrum, waterfall)
//! - **B**: freeze
//! - **X**: peak markers on or off
//! - **Y**: next colour theme
//! - the **Gain** knob scales the scope and the spectrum, 0.25x to 4x
//!
//! The ADC samples both inputs in round robin at 24 kHz each, 512 at a
//! time, and DMA moves them while the CPU draws the previous block: a
//! triggered scope trace, a 512-point fixed-point FFT (Hann window) shown
//! as 40 log-spaced bands with falling peaks, and a scrolling waterfall.
#![no_std]
#![no_main]

use core::fmt::Write as _;

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_rp::{
    adc::{self, Adc, Channel},
    bind_interrupts, dma,
    gpio::{Input, Level, Output, Pull},
    peripherals::{DMA_CH0, DMA_CH1},
    spi::{self, Spi},
};
use embedded_graphics::mono_font::ascii::{FONT_6X10, FONT_9X15_BOLD};
use heapless::String;
use libm::{cosf, log10f, powf, sinf};
use panic_probe as _;
use showcase::{Canvas, Lcd, W, mix, rgb};

bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
    ADC_IRQ_FIFO => adc::InterruptHandler;
});

/// Samples per block, per input.
const N: usize = 512;
const LOG_N: u32 = 9;
/// Per input, with two inputs in round robin: 48 MHz / (DIV + 1) / 2.
const RATE: u32 = 24_000;
const DIV: u16 = (48_000_000 / (2 * RATE) - 1) as u16;
/// Bands shown, log-spaced from about 50 Hz to 12 kHz.
const BANDS: usize = 40;
const BAND_W: i32 = W / BANDS as i32;
/// Decibels from the bottom of the display to full scale.
const RANGE_DB: f32 = 60.0;
/// Waterfall history, a row per block.
const HISTORY: usize = 48;

const BG: u16 = rgb(6, 6, 14);
const GRID: u16 = rgb(26, 28, 48);
const TEXT: u16 = rgb(200, 205, 230);
const DIM: u16 = rgb(100, 105, 140);

struct Theme {
    name: &'static str,
    low: u16,
    high: u16,
    trace: u16,
    peak: u16,
}

const THEMES: [Theme; 3] = [
    Theme {
        name: "neon",
        low: rgb(20, 200, 255),
        high: rgb(255, 60, 220),
        trace: rgb(120, 255, 230),
        peak: rgb(255, 255, 255),
    },
    Theme {
        name: "ember",
        low: rgb(200, 20, 10),
        high: rgb(255, 230, 60),
        trace: rgb(255, 170, 60),
        peak: rgb(255, 250, 220),
    },
    Theme {
        name: "phosphor",
        low: rgb(10, 120, 30),
        high: rgb(160, 255, 120),
        trace: rgb(90, 255, 110),
        peak: rgb(220, 255, 210),
    },
];

#[derive(Clone, Copy, PartialEq)]
enum View {
    Split,
    Bars,
    Waterfall,
}

impl View {
    fn next(self) -> Self {
        match self {
            View::Split => View::Bars,
            View::Bars => View::Waterfall,
            View::Waterfall => View::Split,
        }
    }

    fn name(self) -> &'static str {
        match self {
            View::Split => "scope+fft",
            View::Bars => "fft",
            View::Waterfall => "waterfall",
        }
    }
}

/// A 512-point radix-2 FFT on Q15 data, halving at each stage so nothing
/// overflows: the result is the DFT divided by N.
struct Fft {
    /// e^(-2πik/N) for k < N/2, as (cos, -sin) in Q15.
    twiddle: [(i32, i32); N / 2],
    hann: [i32; N],
    re: [i32; N],
    im: [i32; N],
}

impl Fft {
    fn new() -> Self {
        let tau = 2.0 * core::f32::consts::PI;
        let mut f = Self {
            twiddle: [(0, 0); N / 2],
            hann: [0; N],
            re: [0; N],
            im: [0; N],
        };
        for (k, t) in f.twiddle.iter_mut().enumerate() {
            let a = tau * k as f32 / N as f32;
            *t = ((cosf(a) * 32767.0) as i32, (-sinf(a) * 32767.0) as i32);
        }
        for (i, h) in f.hann.iter_mut().enumerate() {
            *h = ((0.5 - 0.5 * cosf(tau * i as f32 / N as f32)) * 32767.0) as i32;
        }
        f
    }

    /// Transform `x` (centred samples, about ±2048), windowed.
    fn run(&mut self, x: &[i32; N]) {
        for i in 0..N {
            let j = i.reverse_bits() >> (usize::BITS - LOG_N);
            // 12-bit samples, windowed, up to Q14.
            self.re[j] = (x[i] * self.hann[i]) >> 12;
            self.im[j] = 0;
        }
        let mut half = 1;
        while half < N {
            let step = N / (2 * half);
            for j in 0..half {
                let (wr, wi) = self.twiddle[j * step];
                let mut a = j;
                while a < N {
                    let b = a + half;
                    let tr = (self.re[b] * wr - self.im[b] * wi) >> 15;
                    let ti = (self.re[b] * wi + self.im[b] * wr) >> 15;
                    let (ar, ai) = (self.re[a], self.im[a]);
                    self.re[a] = (ar + tr) >> 1;
                    self.im[a] = (ai + ti) >> 1;
                    self.re[b] = (ar - tr) >> 1;
                    self.im[b] = (ai - ti) >> 1;
                    a += 2 * half;
                }
            }
            half *= 2;
        }
    }

    /// |X[k]|², for bins below N/2.
    fn power(&self, k: usize) -> u32 {
        let (r, i) = (self.re[k], self.im[k]);
        (r * r + i * i) as u32
    }
}

struct Viz {
    fft: Fft,
    /// The first FFT bin of each band, and one past the last band's.
    edges: [usize; BANDS + 1],
    /// Band heights (0..1): shown, and the falling peaks with their hold.
    level: [f32; BANDS],
    peak: [f32; BANDS],
    hold: [u8; BANDS],
    /// The scope trace: one sample per column, centred, with gain.
    trace: [i16; W as usize],
    /// Waterfall rows (newest at `head`), band intensities 0..255.
    history: [[u8; BANDS]; HISTORY],
    head: usize,
    /// Input level in dBFS, and the gain from the knob.
    rms_db: f32,
    gain: f32,
    view: View,
    theme: usize,
    frozen: bool,
    peaks: bool,
}

impl Viz {
    fn new() -> Self {
        // 50 Hz to 12 kHz, each band at least one bin wide.
        let bin_hz = RATE as f32 / N as f32;
        let mut edges = [0; BANDS + 1];
        for (b, e) in edges.iter_mut().enumerate() {
            let hz = 50.0 * powf(240.0, b as f32 / BANDS as f32);
            *e = ((hz / bin_hz) as usize).clamp(1, N / 2);
        }
        for b in 1..=BANDS {
            if edges[b] <= edges[b - 1] {
                edges[b] = edges[b - 1] + 1;
            }
        }
        Self {
            fft: Fft::new(),
            edges,
            level: [0.0; BANDS],
            peak: [0.0; BANDS],
            hold: [0; BANDS],
            trace: [0; W as usize],
            history: [[0; BANDS]; HISTORY],
            head: 0,
            rms_db: -99.0,
            gain: 1.0,
            view: View::Split,
            theme: 0,
            frozen: false,
            peaks: true,
        }
    }

    /// Take a block: the input on even samples, the knob on odd ones.
    fn analyse(&mut self, block: &[u16; 2 * N]) {
        let knob = block
            .iter()
            .skip(1)
            .step_by(2)
            .map(|&v| v as u32)
            .sum::<u32>()
            / N as u32;
        // 0.25x at one end, 1x in the middle, 4x at the other.
        self.gain = powf(4.0, 2.0 * knob as f32 / 4095.0 - 1.0);
        if self.frozen {
            return;
        }
        let mean = block.iter().step_by(2).map(|&v| v as i32).sum::<i32>() / N as i32;
        let mut x = [0i32; N];
        let mut energy = 0u64;
        for (i, v) in block.iter().step_by(2).enumerate() {
            x[i] = *v as i32 - mean;
            energy += (x[i] * x[i]) as u64;
        }
        let rms = libm::sqrtf(energy as f32 / N as f32) / 2048.0;
        self.rms_db = 20.0 * log10f(rms.max(1e-5));

        // Trigger the scope on a rising crossing through the middle, with
        // a little hysteresis, so a steady tone stands still.
        let span = W as usize;
        let start = (1..N - span)
            .find(|&i| x[i - 1] < -8 && x[i] >= 0)
            .unwrap_or(0);
        for (c, t) in self.trace.iter_mut().enumerate() {
            *t = (x[start + c] as f32 * self.gain) as i16;
        }

        self.fft.run(&x);
        // A full-scale sine comes out at about 2^12 in its bin: 0 dB.
        let boost = 20.0 * log10f(self.gain);
        let row = &mut self.history[self.head];
        for b in 0..BANDS {
            let p = (self.edges[b]..self.edges[b + 1])
                .map(|k| self.fft.power(k))
                .max()
                .unwrap_or(0);
            let db = 10.0 * log10f(p.max(1) as f32 / 16_777_216.0) + boost;
            let h = ((db + RANGE_DB) / RANGE_DB).clamp(0.0, 1.0);
            // Bars jump up and fall back; peaks hold, then fall slower.
            self.level[b] = h.max(self.level[b] - 0.04);
            if h >= self.peak[b] {
                (self.peak[b], self.hold[b]) = (h, 18);
            } else if self.hold[b] > 0 {
                self.hold[b] -= 1;
            } else {
                self.peak[b] = (self.peak[b] - 0.012).max(self.level[b]);
            }
            // Squared, so the loud parts stand out of the floor.
            row[b] = (h * h * 255.0) as u8;
        }
        self.head = (self.head + 1) % HISTORY;
    }

    fn draw(&self, cv: &mut Canvas) {
        let th = &THEMES[self.theme];
        self.header(cv, th);
        match self.view {
            View::Split => {
                self.scope(cv, th, 20, 100);
                self.bars(cv, th, 124, 116);
            }
            View::Bars => self.bars(cv, th, 20, 220),
            View::Waterfall => {
                self.bars(cv, th, 20, 60);
                self.waterfall(cv, th, 84, 156);
            }
        }
    }

    fn header(&self, cv: &mut Canvas, th: &Theme) {
        if !cv.rows(0, 18) {
            return;
        }
        cv.text(4, 2, "SPECTRA", &FONT_9X15_BOLD, th.trace);
        let mut s: String<40> = String::new();
        let _ = write!(s, "{} {}", self.view.name(), th.name);
        cv.text(74, 5, &s, &FONT_6X10, DIM);
        s.clear();
        let _ = write!(s, "x{:.2}", self.gain);
        cv.text_r(W - 70, 5, &s, &FONT_6X10, TEXT);
        if self.frozen {
            cv.text_r(W - 116, 5, "FROZEN", &FONT_6X10, th.peak);
        }
        // A level meter, -48 to 0 dBFS.
        let (mx, my, mw) = (W - 64, 6, 60);
        cv.fill(mx, my, mw, 7, GRID);
        let lit = (((self.rms_db + 48.0) / 48.0).clamp(0.0, 1.0) * mw as f32) as i32;
        for x in 0..lit {
            let c = match x * 100 / mw {
                0..=69 => rgb(60, 220, 90),
                70..=89 => rgb(250, 210, 40),
                _ => rgb(255, 60, 50),
            };
            cv.fill(mx + x, my, 1, 7, c);
        }
    }

    fn scope(&self, cv: &mut Canvas, th: &Theme, top: i32, h: i32) {
        if !cv.rows(top, h) {
            return;
        }
        let mid = top + h / 2;
        cv.frame(0, top, W, h, 1, GRID);
        for x in (0..W).step_by(4) {
            cv.px(x, mid, GRID);
        }
        for y in (top..top + h).step_by(4) {
            cv.px(W / 2, y, GRID);
        }
        // Half scale fills the box at 1x: music rarely gets nearer.
        let y = |c: usize| {
            (mid - self.trace[c] as i32 * (h / 2 - 2) / 1024).clamp(top + 1, top + h - 2)
        };
        let glow = mix(BG, th.trace, 90);
        for c in 1..W as usize {
            let (x0, y0, y1) = (c as i32 - 1, y(c - 1), y(c));
            cv.line(x0, y0 + 1, x0 + 1, y1 + 1, glow);
            cv.line(x0, y0, x0 + 1, y1, th.trace);
        }
    }

    fn bars(&self, cv: &mut Canvas, th: &Theme, top: i32, h: i32) {
        if !cv.rows(top, h) {
            return;
        }
        let bottom = top + h;
        let (ya, yb) = (top.max(cv.y0), bottom.min(cv.y0 + cv.h));
        for b in 0..BANDS {
            let x = b as i32 * BAND_W;
            let bar = (self.level[b] * h as f32) as i32;
            // Segments three rows high, a row apart, shaded up the bar.
            for yy in ya.max(bottom - bar)..yb {
                if (bottom - yy) % 4 == 0 {
                    continue;
                }
                let t = ((bottom - yy) * 256 / h) as u32;
                cv.fill(x + 1, yy, BAND_W - 2, 1, mix(th.low, th.high, t));
            }
            if self.peaks && self.peak[b] > 0.02 {
                let py = bottom - (self.peak[b] * h as f32) as i32;
                cv.fill(x + 1, py.max(top), BAND_W - 2, 2, th.peak);
            }
        }
    }

    fn waterfall(&self, cv: &mut Canvas, th: &Theme, top: i32, h: i32) {
        if !cv.rows(top, h) {
            return;
        }
        let row_h = h / HISTORY as i32;
        for r in 0..HISTORY {
            let y = top + r as i32 * row_h;
            if !cv.rows(y, row_h) {
                continue;
            }
            // Newest at the top.
            let row = &self.history[(self.head + HISTORY - 1 - r) % HISTORY];
            for (b, &v) in row.iter().enumerate() {
                let v = v as u32;
                let c = if v < 128 {
                    mix(BG, th.low, v * 2)
                } else {
                    mix(th.low, th.high, (v - 128) * 2)
                };
                cv.fill(b as i32 * BAND_W, y, BAND_W, row_h, c);
            }
        }
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
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
    let buttons = [
        Input::new(p.PIN_12, Pull::Up),
        Input::new(p.PIN_20, Pull::Up),
        Input::new(p.PIN_14, Pull::Up),
        Input::new(p.PIN_15, Pull::Up),
    ];

    let mut adc = Adc::new(p.ADC, Irqs, adc::Config::default());
    let mut inputs = [
        Channel::new_pin(p.PIN_26, Pull::None),
        Channel::new_pin(p.PIN_27, Pull::None),
    ];
    let mut dma = dma::Channel::new(p.DMA_CH1, Irqs);
    info!("SPECTRA: A view, B freeze, X peaks, Y theme; the knob is gain");

    let mut viz = Viz::new();
    let (mut front, mut back) = ([2048u16; 2 * N], [2048u16; 2 * N]);
    adc.read_many_multichannel(&mut inputs, &mut front, DIV, &mut dma)
        .await
        .ok();
    let mut prev = [false; 4];
    loop {
        let keys = buttons.each_ref().map(|b| b.is_low());
        let fresh: [bool; 4] = core::array::from_fn(|i| keys[i] && !prev[i]);
        prev = keys;
        if fresh[0] {
            viz.view = viz.view.next();
            info!("view: {}", viz.view.name());
        }
        if fresh[1] {
            viz.frozen = !viz.frozen;
            info!("frozen: {}", viz.frozen);
        }
        if fresh[2] {
            viz.peaks = !viz.peaks;
        }
        if fresh[3] {
            viz.theme = (viz.theme + 1) % THEMES.len();
            info!("theme: {}", THEMES[viz.theme].name);
        }
        viz.analyse(&front);
        // The next block comes in while this one is on its way out.
        let capture = adc.read_many_multichannel(&mut inputs, &mut back, DIV, &mut dma);
        let (got, ()) = join(capture, lcd.frame(BG, &mut |cv| viz.draw(cv))).await;
        if got.is_err() {
            info!("ADC conversion error");
        }
        core::mem::swap(&mut front, &mut back);
    }
}
