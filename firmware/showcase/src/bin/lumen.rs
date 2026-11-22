//! LUMEN: generative light for an 8x8 WS2812 matrix
//! (`boards/lumen.toml`: the chain on GPIO2, a knob on GPIO4/5, buttons on
//! GPIO6 and 7).
//!
//! Six scenes: plasma, fire, rain, life, ripples and fireflies. **Next**
//! (GPIO6) moves to the next scene, the knob tweaks it (hue, heat, rain,
//! speed...) and **Bright** (GPIO7) steps the brightness. UART0 says what
//! is going on.
//!
//! The LEDs are driven by Embassy's PIO WS2812 program with DMA, and the
//! knob is read by its PIO quadrature decoder, so the CPU only draws.
#![no_std]
#![no_main]

use core::fmt::Write as _;

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_rp::{
    bind_interrupts,
    gpio::{Input, Level, Output, Pull},
    mode::Blocking,
    peripherals::{DMA_CH0, PIO0},
    pio::{InterruptHandler, Pio},
    pio_programs::{
        rotary_encoder::{Direction, PioEncoder, PioEncoderProgram},
        ws2812::{PioWs2812, PioWs2812Program},
    },
    uart::{self, UartTx},
};
use embassy_time::{Duration, Instant, Ticker};
use heapless::String;
use panic_probe as _;
use portable_atomic::{AtomicI32, Ordering};
use showcase::Rng;
use smart_leds::RGB8;

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
});

const N: usize = 64;
const FPS: u64 = 50;

static KNOB: AtomicI32 = AtomicI32::new(0);

#[embassy_executor::task]
async fn knob_task(mut enc: PioEncoder<'static, PIO0, 1>) {
    loop {
        let d = match enc.read().await {
            Direction::Clockwise => 1,
            Direction::CounterClockwise => -1,
        };
        KNOB.fetch_add(d, Ordering::Relaxed);
    }
}

// ------------------------------------------------------------- colours ----

/// sin over a 256-step turn, -127..127.
fn sin8(a: u32) -> i32 {
    const Q: [i8; 65] = [
        0, 3, 6, 9, 12, 16, 19, 22, 25, 28, 31, 34, 37, 40, 43, 46, 49, 51, 54, 57, 60, 63, 65, 68,
        71, 73, 76, 78, 81, 83, 85, 88, 90, 92, 94, 96, 98, 100, 102, 104, 106, 107, 109, 111, 112,
        113, 115, 116, 117, 118, 120, 121, 122, 122, 123, 124, 125, 125, 126, 126, 126, 127, 127,
        127, 127,
    ];
    let a = a & 255;
    let (quad, i) = (a / 64, (a % 64) as usize);
    match quad {
        0 => Q[i] as i32,
        1 => Q[64 - i] as i32,
        2 => -(Q[i] as i32),
        _ => -(Q[64 - i] as i32),
    }
}

/// A colour wheel, `h` over 256 steps, full saturation.
fn hue(h: u32, v: u8) -> RGB8 {
    let h = (h & 255) as u16 * 6;
    let (sector, f) = (h / 256, h % 256);
    let v16 = v as u16;
    let up = (v16 * f / 256) as u8;
    let down = (v16 * (255 - f) / 256) as u8;
    let (r, g, b) = match sector {
        0 => (v, up, 0),
        1 => (down, v, 0),
        2 => (0, v, up),
        3 => (0, down, v),
        4 => (up, 0, v),
        _ => (v, 0, down),
    };
    RGB8 { r, g, b }
}

fn heat(t: u8) -> RGB8 {
    // Black, red, orange, yellow, white.
    let t = t as u16;
    let r = (t * 3).min(255) as u8;
    let g = (t.saturating_sub(85) * 3).min(255) as u8;
    let b = (t.saturating_sub(170) * 3).min(255) as u8;
    RGB8 { r, g, b }
}

fn scale(c: RGB8, s: u16) -> RGB8 {
    // A rough gamma, then brightness: dim LEDs look better that way.
    let g = |v: u8| ((v as u16 * v as u16 / 255) * s / 256) as u8;
    RGB8 {
        r: g(c.r),
        g: g(c.g),
        b: g(c.b),
    }
}

fn add(a: RGB8, b: RGB8) -> RGB8 {
    RGB8 {
        r: a.r.saturating_add(b.r),
        g: a.g.saturating_add(b.g),
        b: a.b.saturating_add(b.b),
    }
}

// -------------------------------------------------------------- scenes ----

#[derive(Clone, Copy, PartialEq)]
enum Scene {
    Plasma,
    Fire,
    Rain,
    Life,
    Ripples,
    Fireflies,
}

const SCENES: [Scene; 6] = [
    Scene::Plasma,
    Scene::Fire,
    Scene::Rain,
    Scene::Life,
    Scene::Ripples,
    Scene::Fireflies,
];

impl Scene {
    fn name(self) -> &'static str {
        match self {
            Scene::Plasma => "plasma",
            Scene::Fire => "fire",
            Scene::Rain => "rain",
            Scene::Life => "life",
            Scene::Ripples => "ripples",
            Scene::Fireflies => "fireflies",
        }
    }

    fn knob(self) -> &'static str {
        match self {
            Scene::Plasma => "hue",
            Scene::Fire => "heat",
            Scene::Rain => "downpour",
            Scene::Life => "speed",
            Scene::Ripples => "hue",
            Scene::Fireflies => "swarm",
        }
    }
}

struct World {
    rng: Rng,
    t: u32,
    heat: [u8; N],
    drops: [u8; N],
    life: [bool; N],
    life_age: [u8; N],
    still: u8,
    rings: [(i32, i32, i32, u32); 4],
    flies: [(i32, i32, i32, i32, u32); 12],
}

fn at(x: i32, y: i32) -> usize {
    (y.rem_euclid(8) * 8 + x.rem_euclid(8)) as usize
}

impl World {
    fn seed_life(&mut self) {
        for (c, age) in self.life.iter_mut().zip(&mut self.life_age) {
            *c = self.rng.below(100) < 35;
            *age = 0;
        }
        self.still = 0;
    }

    /// One frame of `scene` into `px`, with `k` the knob (any integer).
    fn draw(&mut self, scene: Scene, k: i32, px: &mut [RGB8; N]) -> bool {
        self.t = self.t.wrapping_add(1);
        let t = self.t;
        let mut beat = false;
        match scene {
            Scene::Plasma => {
                let shift = (k * 8) as u32;
                for y in 0..8i32 {
                    for x in 0..8i32 {
                        let v = sin8(x as u32 * 24 + t * 3)
                            + sin8(y as u32 * 20 + t * 2)
                            + sin8((x + y) as u32 * 16 + t * 5 / 2)
                            + sin8(((x - 4) * (x - 4) + (y - 4) * (y - 4)) as u32 * 6 + t * 4);
                        px[at(x, y)] = hue((v / 4 + 128) as u32 + shift + t / 4, 255);
                    }
                }
                beat = t.is_multiple_of(50);
            }
            Scene::Fire => {
                let fuel = (180 + k * 6).clamp(60, 255) as u32;
                // The bottom row is the fuel, flickering.
                for x in 0..8 {
                    self.heat[at(x, 7)] = (fuel - self.rng.below(fuel / 2 + 1)) as u8;
                }
                // Heat rises, drifts a little and cools.
                for y in 0..7 {
                    for x in 0..8 {
                        let below = self.heat[at(x, y + 1)] as u32;
                        let side = self.heat[at(x + self.rng.below(3) as i32 - 1, y + 1)] as u32;
                        let h = (below * 2 + side) / 3;
                        self.heat[at(x, y)] = h.saturating_sub(self.rng.below(40) + 10) as u8;
                    }
                }
                for i in 0..N {
                    px[i] = heat(self.heat[i]);
                }
                beat = self.rng.below(12) == 0;
            }
            Scene::Rain => {
                let chance = (8 + k * 3).clamp(2, 90) as u32;
                // Each drop is a bright head; trails fade below... above it.
                for v in self.drops.iter_mut() {
                    *v = v.saturating_sub(40);
                }
                if t.is_multiple_of(3) {
                    for y in (1..8).rev() {
                        for x in 0..8 {
                            if self.drops[at(x, y - 1)] >= 200 {
                                self.drops[at(x, y)] = 255;
                            }
                        }
                    }
                    for x in 0..8 {
                        if self.rng.below(100) < chance / 4 + 1 {
                            self.drops[at(x, 0)] = 255;
                            beat = true;
                        }
                    }
                }
                for i in 0..N {
                    let v = self.drops[i];
                    px[i] = RGB8 {
                        r: v / 6,
                        g: v / 2,
                        b: v,
                    };
                }
            }
            Scene::Life => {
                let every = (12 - k).clamp(1, 40) as u32;
                if t.is_multiple_of(every) {
                    let mut next = [false; N];
                    for y in 0..8 {
                        for x in 0..8 {
                            let mut n = 0;
                            for (dx, dy) in [
                                (-1, -1),
                                (0, -1),
                                (1, -1),
                                (-1, 0),
                                (1, 0),
                                (-1, 1),
                                (0, 1),
                                (1, 1),
                            ] {
                                n += self.life[at(x + dx, y + dy)] as u8;
                            }
                            let alive = self.life[at(x, y)];
                            next[at(x, y)] = n == 3 || (alive && n == 2);
                        }
                    }
                    if next == self.life {
                        self.still += 1;
                    }
                    let count = next.iter().filter(|&&c| c).count();
                    for i in 0..N {
                        self.life_age[i] = if next[i] {
                            self.life_age[i].saturating_add(1)
                        } else {
                            0
                        };
                    }
                    self.life = next;
                    if self.still > 3 || count < 3 || t.is_multiple_of(every * 300) {
                        self.seed_life();
                    }
                    beat = true;
                }
                for i in 0..N {
                    px[i] = if self.life[i] {
                        hue(96 + self.life_age[i] as u32 * 9, 255)
                    } else {
                        RGB8 { r: 0, g: 0, b: 6 }
                    };
                }
            }
            Scene::Ripples => {
                if t.is_multiple_of(40) || self.rings.iter().all(|r| r.2 > 400) {
                    let slot = (t / 40) as usize % self.rings.len();
                    self.rings[slot] = (
                        self.rng.below(8) as i32,
                        self.rng.below(8) as i32,
                        0,
                        self.rng.below(256),
                    );
                    beat = true;
                }
                px.fill(RGB8::default());
                for r in self.rings.iter_mut() {
                    r.2 += 3;
                    for y in 0..8 {
                        for x in 0..8 {
                            let d = showcase::isqrt(
                                ((x - r.0) * (x - r.0) + (y - r.1) * (y - r.1)) * 256,
                            );
                            let off = (d - r.2 / 2 * 3).abs();
                            if off < 24 {
                                let v = ((24 - off) * 10).clamp(0, 255) as u8;
                                let fade = (255 - (r.2 / 2).min(255)) as u16;
                                let c = scale(hue(r.3 + (k * 10) as u32, v), fade + 1);
                                px[at(x, y)] = add(px[at(x, y)], c);
                            }
                        }
                    }
                }
            }
            Scene::Fireflies => {
                let count = (6 + k).clamp(1, self.flies.len() as i32) as usize;
                px.fill(RGB8 { r: 0, g: 2, b: 4 });
                for (i, f) in self.flies.iter_mut().enumerate() {
                    // Wander, in 1/256 of a pixel.
                    f.2 = (f.2 + self.rng.below(17) as i32 - 8).clamp(-24, 24);
                    f.3 = (f.3 + self.rng.below(17) as i32 - 8).clamp(-24, 24);
                    f.0 = (f.0 + f.2).rem_euclid(8 * 256);
                    f.1 = (f.1 + f.3).rem_euclid(8 * 256);
                    if i >= count {
                        continue;
                    }
                    let glow = sin8(t * 4 + f.4).max(0) as u32 * 2;
                    if glow > 200 && t.is_multiple_of(4) && i == 0 {
                        beat = true;
                    }
                    let (cx, cy) = (f.0 / 256, f.1 / 256);
                    let c = RGB8 {
                        r: (glow * 3 / 4) as u8,
                        g: glow as u8,
                        b: 0,
                    };
                    px[at(cx, cy)] = add(px[at(cx, cy)], c);
                }
            }
        }
        beat
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let Pio {
        mut common,
        sm0,
        sm1,
        ..
    } = Pio::new(p.PIO0, Irqs);
    let prg = PioWs2812Program::new(&mut common);
    let mut leds: PioWs2812<'_, PIO0, 0, _> =
        PioWs2812::new(&mut common, sm0, p.PIN_2, p.DMA_CH0, Irqs, &prg);
    let enc_prg = PioEncoderProgram::new(&mut common);
    let enc = PioEncoder::new(&mut common, sm1, p.PIN_4, p.PIN_5, &enc_prg);
    spawner.spawn(knob_task(enc).unwrap());

    let next = Input::new(p.PIN_6, Pull::Up);
    let bright = Input::new(p.PIN_7, Pull::Up);
    let mut led = Output::new(p.PIN_25, Level::Low);
    let mut uart: UartTx<'_, Blocking> =
        UartTx::new_blocking(p.UART0, p.PIN_0, uart::Config::default());
    let mut say = |s: &str| {
        let _ = uart.blocking_write(s.as_bytes());
        let _ = uart.blocking_write(b"\r\n");
    };
    say("LUMEN: Next changes the scene, the knob tweaks it, Bright dims.");

    let mut world = World {
        rng: Rng(0x1234_5678),
        t: 0,
        heat: [0; N],
        drops: [0; N],
        life: [false; N],
        life_age: [0; N],
        still: 0,
        rings: [(0, 0, 1000, 0); 4],
        flies: [(0, 0, 0, 0, 0); 12],
    };
    world.seed_life();
    for (i, f) in world.flies.iter_mut().enumerate() {
        *f = (
            world.rng.below(8 * 256) as i32,
            world.rng.below(8 * 256) as i32,
            0,
            0,
            i as u32 * 37,
        );
    }

    const LEVELS: [u16; 4] = [256, 128, 48, 16];
    let (mut scene, mut level) = (0usize, 0usize);
    let mut saved = [0i32; SCENES.len()];
    let mut shown_k = 0;
    let mut prev = (false, false);
    let mut px = [RGB8::default(); N];
    let mut out = [RGB8::default(); N];
    let mut ticker = Ticker::every(Duration::from_hz(FPS));
    let mut s: String<80> = String::new();
    let _ = write!(
        s,
        "scene 1/{}: {} (knob: {})",
        SCENES.len(),
        SCENES[0].name(),
        SCENES[0].knob()
    );
    say(&s);
    let mut beat_until = Instant::now();
    loop {
        let now = (next.is_low(), bright.is_low());
        let knob = KNOB.load(Ordering::Relaxed);
        if now.0 && !prev.0 {
            // Each scene keeps its own knob setting.
            saved[scene] = knob;
            scene = (scene + 1) % SCENES.len();
            KNOB.store(saved[scene], Ordering::Relaxed);
            shown_k = saved[scene];
            s.clear();
            let sc = SCENES[scene];
            let _ = write!(
                s,
                "scene {}/{}: {} (knob: {})",
                scene + 1,
                SCENES.len(),
                sc.name(),
                sc.knob()
            );
            say(&s);
            info!("scene {}", sc.name());
        }
        if now.1 && !prev.1 {
            level = (level + 1) % LEVELS.len();
            s.clear();
            let _ = write!(s, "brightness {}%", LEVELS[level] as u32 * 100 / 256);
            say(&s);
        }
        prev = now;
        let k = KNOB.load(Ordering::Relaxed);
        if k != shown_k {
            shown_k = k;
            s.clear();
            let _ = write!(s, "{} {:+}", SCENES[scene].knob(), k);
            say(&s);
        }

        if world.draw(SCENES[scene], k, &mut px) {
            beat_until = Instant::now() + Duration::from_millis(60);
        }
        led.set_level(Level::from(Instant::now() < beat_until));
        for (o, &c) in out.iter_mut().zip(&px) {
            *o = scale(c, LEVELS[level]);
        }
        leds.write_slice(&out).await;
        ticker.next().await;
    }
}
