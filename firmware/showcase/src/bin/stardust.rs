//! STARDUST: a little arcade game for a Pico with a Pimoroni Pico Display
//! Pack 2.0 (`--board pico --shield pico-display-2`: 320x240 ST7789,
//! buttons A/B/X/Y).
//!
//! A ship falls around a star. Scoop up the drifting motes of stardust:
//! each one is a point and a sip of fuel. Comets cross the sky; touching
//! one, or the star, costs a ship. A pulse breaks comets apart, for five
//! points, but drains fuel. Every ten motes, more comets.
//!
//! - **A** / **B**: turn left / right
//! - **X**: thrust
//! - **Y**: pulse (and start)
//!
//! The screen is redrawn whole at 25 frames a second, a strip at a
//! time (see `showcase::Lcd`), with soft-float physics on the M0+.
#![no_std]
#![no_main]

use core::{f32::consts::PI, fmt::Write as _};

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_rp::{
    bind_interrupts,
    gpio::{Input, Level, Output, Pull},
    peripherals::DMA_CH0,
    spi::{self, Spi},
};
use embassy_time::{Duration, Instant, Ticker};
use embedded_graphics::mono_font::ascii::{FONT_6X10, FONT_9X15_BOLD, FONT_10X20};
use heapless::String;
use libm::{cosf, sinf, sqrtf};
use panic_probe as _;
use showcase::{Canvas, H, Lcd, Rng, W, mix, rgb};

bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
});

const SPACE: u16 = rgb(4, 4, 16);
const SUN: u16 = rgb(255, 214, 120);
const SUN_CORE: u16 = rgb(255, 250, 230);
const SHIP: u16 = rgb(220, 240, 255);
const FLAME: u16 = rgb(255, 140, 40);
const DUST: [u16; 3] = [rgb(120, 255, 220), rgb(255, 120, 230), rgb(255, 240, 120)];
const COMET: u16 = rgb(170, 200, 255);
const HUD: u16 = rgb(200, 210, 255);
const DIM: u16 = rgb(90, 100, 150);
const BAD: u16 = rgb(255, 80, 80);

const CX: f32 = W as f32 / 2.0;
const CY: f32 = H as f32 / 2.0 + 6.0;
const SUN_R: f32 = 14.0;
/// Orbital speed is sqrt(GM / r): about 48 px/s at r = 85.
const GM: f32 = 200_000.0;
const DT: f32 = 1.0 / 25.0;

#[derive(Clone, Copy, Default)]
struct Body {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    alive: bool,
    /// Kind-specific: a mote's colour, a comet's size.
    k: u8,
    /// Frames lived (particles fade by it).
    age: u16,
}

impl Body {
    fn fall(&mut self, g: f32) {
        let (dx, dy) = (CX - self.x, CY - self.y);
        let r2 = (dx * dx + dy * dy).max(80.0);
        let a = g * GM / (r2 * sqrtf(r2));
        self.vx += dx * a * DT;
        self.vy += dy * a * DT;
        // A close pass round the star could fling it out at any speed.
        let v2 = self.vx * self.vx + self.vy * self.vy;
        if v2 > MAX_V * MAX_V {
            let k = MAX_V / sqrtf(v2);
            self.vx *= k;
            self.vy *= k;
        }
        self.x += self.vx * DT;
        self.y += self.vy * DT;
        // Wrap round the edges of the screen.
        self.x = wrap(self.x, W as f32);
        self.y = wrap(self.y, H as f32);
    }

    fn sun_dist2(&self) -> f32 {
        (self.x - CX) * (self.x - CX) + (self.y - CY) * (self.y - CY)
    }
}

const MAX_V: f32 = 240.0;

fn wrap(v: f32, n: f32) -> f32 {
    if !v.is_finite() {
        return n / 2.0;
    }
    let w = v - n * libm::floorf(v / n);
    if w >= n { 0.0 } else { w }
}

fn dist2(a: &Body, b: &Body) -> f32 {
    // Across the wrap, too.
    let mut dx = (a.x - b.x).abs();
    let mut dy = (a.y - b.y).abs();
    if dx > W as f32 / 2.0 {
        dx = W as f32 - dx;
    }
    if dy > H as f32 / 2.0 {
        dy = H as f32 - dy;
    }
    dx * dx + dy * dy
}

/// A body on a roughly circular orbit at radius `r`, angle `a`.
fn orbit(r: f32, a: f32, dir: f32) -> Body {
    let v = sqrtf(GM / r) * dir;
    Body {
        x: CX + r * cosf(a),
        y: CY + r * sinf(a),
        vx: -v * sinf(a),
        vy: v * cosf(a),
        alive: true,
        k: 0,
        age: 0,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Title,
    Play,
    Respawn(u16),
    Over(u16),
}

struct Game {
    rng: Rng,
    mode: Mode,
    ship: Body,
    angle: f32,
    thrusting: bool,
    fuel: f32,
    lives: u8,
    score: u32,
    best: u32,
    level: u32,
    motes_got: u32,
    motes: [Body; 10],
    comets: [Body; 6],
    sparks: [Body; 40],
    pulse: Option<(f32, f32, u16)>,
    stars: [(i16, i16, u8); 70],
    frame: u32,
    flash: u16,
}

impl Game {
    fn new() -> Self {
        let mut rng = Rng(0x5eed_d057);
        let mut stars = [(0i16, 0i16, 0u8); 70];
        for s in stars.iter_mut() {
            *s = (
                rng.below(W as u32) as i16,
                rng.below(H as u32) as i16,
                rng.below(255) as u8,
            );
        }
        let mut g = Game {
            rng,
            mode: Mode::Title,
            ship: Body::default(),
            angle: 0.0,
            thrusting: false,
            fuel: 1.0,
            lives: 3,
            score: 0,
            best: 0,
            level: 1,
            motes_got: 0,
            motes: [Body::default(); 10],
            comets: [Body::default(); 6],
            sparks: [Body::default(); 40],
            pulse: None,
            stars,
            frame: 0,
            flash: 0,
        };
        // Something to look at on the title screen.
        for i in 0..6 {
            g.spawn_mote(i);
        }
        g
    }

    fn start(&mut self) {
        self.lives = 3;
        self.score = 0;
        self.level = 1;
        self.motes_got = 0;
        self.comets = [Body::default(); 6];
        self.place_ship();
        self.mode = Mode::Play;
        for i in 0..self.motes.len() {
            self.spawn_mote(i);
        }
    }

    fn place_ship(&mut self) {
        self.ship = orbit(85.0, PI, 1.0);
        self.angle = -PI / 2.0;
        self.fuel = 1.0;
        self.pulse = None;
    }

    fn spawn_mote(&mut self, i: usize) {
        let r = 40.0 + self.rng.unit() * 70.0;
        let a = self.rng.unit() * 2.0 * PI;
        let dir = if self.rng.below(4) == 0 { -1.0 } else { 1.0 };
        self.motes[i] = orbit(r, a, dir);
        self.motes[i].k = self.rng.below(3) as u8;
    }

    fn spawn_comet(&mut self) {
        let Some(slot) = self.comets.iter().position(|c| !c.alive) else {
            return;
        };
        // In from an edge, roughly towards the star but not quite.
        let side = self.rng.below(4);
        let (x, y) = match side {
            0 => (0.0, self.rng.unit() * H as f32),
            1 => (W as f32 - 1.0, self.rng.unit() * H as f32),
            2 => (self.rng.unit() * W as f32, 0.0),
            _ => (self.rng.unit() * W as f32, H as f32 - 1.0),
        };
        let aim = libm::atan2f(CY - y, CX - x) + (self.rng.unit() - 0.5) * 1.2;
        let speed = 45.0 + self.rng.unit() * 25.0 + self.level as f32 * 4.0;
        self.comets[slot] = Body {
            x,
            y,
            vx: speed * cosf(aim),
            vy: speed * sinf(aim),
            alive: true,
            k: 3 + self.rng.below(3) as u8,
            age: 0,
        };
    }

    fn burst(&mut self, x: f32, y: f32, n: usize, k: u8) {
        let mut made = 0;
        for s in self.sparks.iter_mut() {
            if made == n {
                break;
            }
            if s.alive {
                continue;
            }
            let a = self.rng.unit() * 2.0 * PI;
            let v = 20.0 + self.rng.unit() * 60.0;
            *s = Body {
                x,
                y,
                vx: v * cosf(a),
                vy: v * sinf(a),
                alive: true,
                k,
                age: 0,
            };
            made += 1;
        }
    }

    fn lose_ship(&mut self) {
        self.burst(self.ship.x, self.ship.y, 24, 1);
        self.flash = 8;
        self.lives = self.lives.saturating_sub(1);
        self.mode = if self.lives == 0 {
            Mode::Over(0)
        } else {
            Mode::Respawn(0)
        };
        info!("ship lost, {} left, score {}", self.lives, self.score);
    }

    /// One frame. `keys` is A, B, X, Y; `fresh` the ones newly pressed.
    fn step(&mut self, keys: [bool; 4], fresh: [bool; 4]) -> Option<&'static str> {
        self.frame = self.frame.wrapping_add(1);
        self.flash = self.flash.saturating_sub(1);
        let mut said = None;
        match self.mode {
            Mode::Title => {
                if fresh.iter().any(|&k| k) {
                    self.start();
                    said = Some("go!");
                }
            }
            Mode::Over(t) => {
                self.mode = Mode::Over(t + 1);
                if t > 45 && fresh[3] {
                    self.start();
                    said = Some("go!");
                }
            }
            Mode::Respawn(t) => {
                self.mode = Mode::Respawn(t + 1);
                if t > 40 {
                    self.place_ship();
                    self.mode = Mode::Play;
                }
            }
            Mode::Play => {
                if keys[0] {
                    self.angle -= 4.0 * DT;
                }
                if keys[1] {
                    self.angle += 4.0 * DT;
                }
                self.thrusting = keys[2] && self.fuel > 0.0;
                if self.thrusting {
                    self.ship.vx += 90.0 * cosf(self.angle) * DT;
                    self.ship.vy += 90.0 * sinf(self.angle) * DT;
                    self.fuel = (self.fuel - 0.12 * DT).max(0.0);
                    if self.frame.is_multiple_of(2) {
                        let back = self.angle + PI + (self.rng.unit() - 0.5) * 0.6;
                        if let Some(s) = self.sparks.iter_mut().find(|s| !s.alive) {
                            *s = Body {
                                x: self.ship.x - 6.0 * cosf(self.angle),
                                y: self.ship.y - 6.0 * sinf(self.angle),
                                vx: self.ship.vx + 50.0 * cosf(back),
                                vy: self.ship.vy + 50.0 * sinf(back),
                                alive: true,
                                k: 2,
                                age: 8,
                            };
                        }
                    }
                }
                if fresh[3] && self.pulse.is_none() && self.fuel > 0.08 {
                    self.fuel -= 0.08;
                    self.pulse = Some((self.ship.x, self.ship.y, 0));
                }
                self.ship.fall(1.0);
                if self.ship.sun_dist2() < (SUN_R + 3.0) * (SUN_R + 3.0) {
                    self.lose_ship();
                    said = Some("into the star");
                }
            }
        }

        // The rest of the sky moves whatever the ship does.
        let playing = self.mode == Mode::Play;
        for i in 0..self.motes.len() {
            self.motes[i].fall(1.0);
            if playing && dist2(&self.motes[i], &self.ship) < 10.0 * 10.0 {
                self.score += 1;
                self.motes_got += 1;
                self.fuel = (self.fuel + 0.15).min(1.0);
                let (x, y, k) = (self.motes[i].x, self.motes[i].y, self.motes[i].k);
                self.burst(x, y, 6, 10 + k);
                self.spawn_mote(i);
                if self.motes_got.is_multiple_of(10) {
                    self.level += 1;
                    said = Some("level up");
                }
            }
        }
        let comet_every = 220u32.saturating_sub(self.level * 25).max(50);
        if playing && self.frame.is_multiple_of(comet_every) {
            self.spawn_comet();
        }
        for i in 0..self.comets.len() {
            if !self.comets[i].alive {
                continue;
            }
            let c = &mut self.comets[i];
            c.fall(0.4);
            c.age += 1;
            let r = c.k as f32;
            if c.age > 600 || c.sun_dist2() < (SUN_R + r) * (SUN_R + r) {
                c.alive = false;
                continue;
            }
            if let Some((px, py, pt)) = self.pulse {
                let pr = pt as f32 * 4.0;
                let p = Body {
                    x: px,
                    y: py,
                    ..Body::default()
                };
                let d = sqrtf(dist2(c, &p));
                if (d - pr).abs() < r + 3.0 {
                    let (x, y) = (c.x, c.y);
                    c.alive = false;
                    self.score += 5;
                    self.burst(x, y, 14, 3);
                    continue;
                }
            }
            let hit = playing && dist2(&self.comets[i], &self.ship) < (r + 5.0) * (r + 5.0);
            if hit {
                let (x, y) = (self.comets[i].x, self.comets[i].y);
                self.comets[i].alive = false;
                self.burst(x, y, 10, 3);
                self.lose_ship();
                said = Some("hit by a comet");
            }
        }
        if let Some((x, y, t)) = self.pulse {
            self.pulse = if t < 14 { Some((x, y, t + 1)) } else { None };
        }
        for s in self.sparks.iter_mut().filter(|s| s.alive) {
            s.x += s.vx * DT;
            s.y += s.vy * DT;
            s.vx *= 0.96;
            s.vy *= 0.96;
            s.age += 1;
            if s.age > 30 {
                s.alive = false;
            }
        }
        self.best = self.best.max(self.score);
        said
    }

    fn draw(&self, cv: &mut Canvas) {
        let f = self.frame;
        // Stars, twinkling.
        for &(x, y, ph) in &self.stars {
            if cv.rows(y as i32, 1) {
                // In steps: a strip that doesn't change isn't sent again.
                let tw = (((f / 8).wrapping_mul(24) + ph as u32 * 7) % 256) as i32;
                let v = 60 + (tw - 128).unsigned_abs();
                cv.px(
                    x as i32,
                    y as i32,
                    rgb(v as u8, v as u8, (v + 40).min(255) as u8),
                );
            }
        }

        // The star: a glow, then the disc.
        let (cx, cy) = (CX as i32, CY as i32);
        if cv.rows(cy - 40, 80) {
            let pulse = (((f / 6) % 10) as i32 - 5).unsigned_abs() * 6;
            for (i, r) in [36, 30, 25, 21, 18].iter().enumerate() {
                cv.disc(cx, cy, *r, mix(SPACE, SUN, 30 + i as u32 * 26 + pulse));
            }
            cv.disc(cx, cy, SUN_R as i32, SUN);
            cv.disc(cx - 3, cy - 3, 7, SUN_CORE);
        }

        // Pulse ring.
        if let Some((x, y, t)) = self.pulse {
            let c = mix(SPACE, rgb(140, 220, 255), 256 - t as u32 * 16);
            cv.circle(x as i32, y as i32, t as i32 * 4, c);
            cv.circle(x as i32, y as i32, t as i32 * 4 + 1, c);
        }

        // Motes.
        for m in &self.motes {
            let (x, y) = (m.x as i32, m.y as i32);
            if !cv.rows(y - 3, 7) {
                continue;
            }
            let c = DUST[m.k as usize % 3];
            let glow = mix(SPACE, c, 90);
            let tw = (f / 4 + m.k as u32).is_multiple_of(2);
            cv.fill(x - 1, y - 1, 3, 3, glow);
            cv.px(x, y - 2, c);
            cv.px(x, y + 2, c);
            cv.px(x - 2, y, c);
            cv.px(x + 2, y, c);
            cv.px(x, y, if tw { 0xffff } else { c });
        }

        // Comets: a head and a fading tail behind it.
        for c in self.comets.iter().filter(|c| c.alive) {
            let r = c.k as i32;
            let sp = sqrtf(c.vx * c.vx + c.vy * c.vy).max(1.0);
            let (ux, uy) = (c.vx / sp, c.vy / sp);
            for i in (1..14).rev() {
                let t = i as f32 * 2.2;
                let tx = (c.x - ux * t) as i32;
                let ty = (c.y - uy * t) as i32;
                let tr = (r - i / 3).max(1);
                cv.disc(tx, ty, tr, mix(SPACE, COMET, 200 - i as u32 * 14));
            }
            cv.disc(c.x as i32, c.y as i32, r, COMET);
            cv.disc(c.x as i32 - 1, c.y as i32 - 1, (r / 2).max(1), 0xffff);
        }

        // Sparks.
        for s in self.sparks.iter().filter(|s| s.alive) {
            let fade = 256 - (s.age as u32 * 8).min(256);
            let c = match s.k {
                1 => SHIP,
                2 => FLAME,
                3 => COMET,
                k => DUST[(k as usize - 10) % 3],
            };
            cv.px(s.x as i32, s.y as i32, mix(SPACE, c, fade));
            if s.k != 2 {
                cv.px(s.x as i32 + 1, s.y as i32, mix(SPACE, c, fade / 2));
            }
        }

        // The ship.
        let show_ship = matches!(self.mode, Mode::Play);
        if show_ship {
            let (x, y, a) = (self.ship.x, self.ship.y, self.angle);
            let pt =
                |r: f32, da: f32| ((x + r * cosf(a + da)) as i32, (y + r * sinf(a + da)) as i32);
            let nose = pt(8.0, 0.0);
            let l = pt(6.0, 2.4);
            let r = pt(6.0, -2.4);
            let tail = pt(2.5, PI);
            cv.line(nose.0, nose.1, l.0, l.1, SHIP);
            cv.line(nose.0, nose.1, r.0, r.1, SHIP);
            cv.line(l.0, l.1, tail.0, tail.1, SHIP);
            cv.line(r.0, r.1, tail.0, tail.1, SHIP);
            if self.thrusting && f.is_multiple_of(2) {
                let fl = pt(7.0 + (f % 3) as f32, PI);
                cv.line(tail.0, tail.1, fl.0, fl.1, FLAME);
            }
        }

        if self.flash > 0 {
            cv.frame(0, 0, W, H, self.flash as i32, BAD);
        }

        // HUD.
        if cv.rows(0, 16) {
            let mut s: String<24> = String::new();
            let _ = write!(s, "{:05}", self.score);
            cv.text(6, 3, &s, &FONT_9X15_BOLD, HUD);
            s.clear();
            let _ = write!(s, "BEST {:05}", self.best);
            cv.text_r(314, 5, &s, &FONT_6X10, DIM);
            // Fuel.
            let fx = 120;
            cv.text(fx - 32, 5, "FUEL", &FONT_6X10, DIM);
            cv.frame(fx, 4, 62, 10, 1, DIM);
            let fw = (self.fuel * 58.0) as i32;
            let fc = if self.fuel < 0.2 { BAD } else { DUST[0] };
            cv.fill(fx + 2, 6, fw, 6, fc);
            // Ships left.
            for i in 0..self.lives as i32 {
                let sx = 200 + i * 12;
                cv.line(sx, 13, sx + 4, 4, SHIP);
                cv.line(sx + 4, 4, sx + 8, 13, SHIP);
                cv.line(sx, 13, sx + 8, 13, SHIP);
            }
        }

        match self.mode {
            Mode::Title => {
                cv.text_c(160, 60, "S T A R D U S T", &FONT_10X20, SUN);
                cv.text_c(160, 170, "A/B turn  X thrust  Y pulse", &FONT_6X10, HUD);
                if (f / 15).is_multiple_of(2) {
                    cv.text_c(160, 190, "press any button", &FONT_6X10, DUST[0]);
                }
            }
            Mode::Over(t) => {
                cv.text_c(160, 70, "GAME OVER", &FONT_10X20, BAD);
                let mut s: String<24> = String::new();
                let _ = write!(s, "score {}  level {}", self.score, self.level);
                cv.text_c(160, 160, &s, &FONT_6X10, HUD);
                if t > 45 && (f / 15).is_multiple_of(2) {
                    cv.text_c(160, 180, "Y to fly again", &FONT_6X10, DUST[0]);
                }
            }
            _ => {}
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
    info!("STARDUST: A/B turn, X thrust, Y pulse");

    let mut game = Game::new();
    let mut prev = [false; 4];
    let mut ticker = Ticker::every(Duration::from_hz(25));
    let mut slow = 0u32;
    loop {
        let t0 = Instant::now();
        let keys = [
            buttons[0].is_low(),
            buttons[1].is_low(),
            buttons[2].is_low(),
            buttons[3].is_low(),
        ];
        let fresh = [
            keys[0] && !prev[0],
            keys[1] && !prev[1],
            keys[2] && !prev[2],
            keys[3] && !prev[3],
        ];
        prev = keys;
        if let Some(what) = game.step(keys, fresh) {
            info!("{} (score {})", what, game.score);
        }
        lcd.frame(SPACE, &mut |cv| game.draw(cv)).await;
        if t0.elapsed() > Duration::from_millis(34) {
            slow += 1;
            if slow % 100 == 1 {
                info!("frame took {} ms", t0.elapsed().as_millis());
            }
        }
        ticker.next().await;
    }
}
