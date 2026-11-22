//! What the showcase demos share: a strip renderer for a 320x240 ST7789
//! (the Pimoroni Pico Display Pack 2.0, or a board wired the same way), and
//! a few small helpers.
//!
//! The screen is drawn a strip of rows at a time, so no frame buffer is
//! needed: a scene is a function that draws everything, clipped to the
//! strip in hand. `Lcd::frame` sends only the strips that changed since the
//! last frame (by a hash of each), and two strip buffers take turns so the
//! CPU draws one while DMA sends the other. Sending less matters twice: on
//! the real panel for the frame rate, and in the emulator, where a busy SPI
//! bus is what costs the most.
#![no_std]

use core::convert::Infallible;

use embassy_futures::join::join;
use embassy_rp::{gpio::Output, mode::Async, spi::Spi};
use embassy_time::Timer;
use embedded_graphics::{
    mono_font::{MonoFont, MonoTextStyle},
    pixelcolor::{Rgb565, raw::RawU16},
    prelude::*,
    text::{Baseline, Text},
};

pub const W: i32 = 320;
pub const H: i32 = 240;
/// Rows per strip: 20 strips per frame, 9600 bytes each.
pub const STRIP: i32 = 12;

/// RGB565 from 8-bit channels.
pub const fn rgb(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 & 0xF8) << 8) | ((g as u16 & 0xFC) << 3) | (b as u16 >> 3)
}

/// Blend two RGB565 colours, `t` from 0 (all `a`) to 256 (all `b`).
pub fn mix(a: u16, b: u16, t: u32) -> u16 {
    let t = t.min(256);
    let ch = |c: u16, shift: u16, mask: u16| ((c >> shift) & mask) as u32;
    let lerp = |x: u32, y: u32| (x * (256 - t) + y * t) >> 8;
    let r = lerp(ch(a, 11, 0x1f), ch(b, 11, 0x1f));
    let g = lerp(ch(a, 5, 0x3f), ch(b, 5, 0x3f));
    let bl = lerp(ch(a, 0, 0x1f), ch(b, 0, 0x1f));
    ((r << 11) | (g << 5) | bl) as u16
}

/// A tiny xorshift generator: plenty for sparkles and stars.
pub struct Rng(pub u32);

impl Rng {
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// In `0..n`.
    pub fn below(&mut self, n: u32) -> u32 {
        self.next_u32() % n.max(1)
    }

    /// In `0.0..1.0`.
    pub fn unit(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32
    }
}

/// One horizontal slice of the screen. Pixels are RGB565 stored big-endian,
/// as the panel takes them, in words so that clearing and hashing go a word
/// (two pixels) at a time.
pub struct Canvas {
    buf: [u32; (W * STRIP / 2) as usize],
    pub y0: i32,
    pub h: i32,
}

impl Canvas {
    pub const fn new() -> Self {
        Self {
            buf: [0; (W * STRIP / 2) as usize],
            y0: 0,
            h: 0,
        }
    }

    fn pixels(&mut self) -> &mut [u16] {
        // SAFETY: u32s are u16-aligned, and twice the size.
        unsafe {
            core::slice::from_raw_parts_mut(self.buf.as_mut_ptr() as *mut u16, self.buf.len() * 2)
        }
    }

    fn begin(&mut self, y: i32, h: i32, bg: u16) {
        self.y0 = y;
        self.h = h;
        let be = bg.to_be() as u32;
        self.buf[..(W * h / 2) as usize].fill(be | be << 16);
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: plain integers, viewed as the bytes they are in memory.
        unsafe {
            core::slice::from_raw_parts(self.buf.as_ptr() as *const u8, (W * self.h * 2) as usize)
        }
    }

    /// FNV-1a over the pixels, a word at a time; never 0.
    fn hash(&self) -> u32 {
        let mut h = 0x811c_9dc5u32;
        for &w in &self.buf[..(W * self.h / 2) as usize] {
            h = (h ^ w).wrapping_mul(0x0100_0193);
        }
        h | 1
    }

    /// Whether rows `y..y + h` cross this strip.
    #[inline]
    pub fn rows(&self, y: i32, h: i32) -> bool {
        y < self.y0 + self.h && y + h > self.y0
    }

    #[inline]
    pub fn px(&mut self, x: i32, y: i32, c: u16) {
        let ly = y - self.y0;
        if (0..W).contains(&x) && ly >= 0 && ly < self.h {
            self.pixels()[(ly * W + x) as usize] = c.to_be();
        }
    }

    pub fn get(&self, x: i32, y: i32) -> u16 {
        let ly = y - self.y0;
        if (0..W).contains(&x) && ly >= 0 && ly < self.h {
            let i = (ly * W + x) as usize;
            let w = self.buf[i / 2];
            let half = if cfg!(target_endian = "little") == i.is_multiple_of(2) {
                w as u16
            } else {
                (w >> 16) as u16
            };
            u16::from_be(half)
        } else {
            0
        }
    }

    /// Blend `c` over what is there, `t` out of 256.
    pub fn blend(&mut self, x: i32, y: i32, c: u16, t: u32) {
        if y >= self.y0 && y < self.y0 + self.h && (0..W).contains(&x) {
            let old = self.get(x, y);
            self.px(x, y, mix(old, c, t));
        }
    }

    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: u16) {
        let xa = x.max(0);
        let xb = (x + w).min(W);
        let ya = y.max(self.y0);
        let yb = (y + h).min(self.y0 + self.h);
        if xa >= xb || ya >= yb {
            return;
        }
        let (y0, be) = (self.y0, c.to_be());
        let px = self.pixels();
        for yy in ya..yb {
            let i = ((yy - y0) * W + xa) as usize;
            px[i..i + (xb - xa) as usize].fill(be);
        }
    }

    /// A rectangle's outline, `t` pixels thick.
    pub fn frame(&mut self, x: i32, y: i32, w: i32, h: i32, t: i32, c: u16) {
        self.fill(x, y, w, t, c);
        self.fill(x, y + h - t, w, t, c);
        self.fill(x, y, t, h, c);
        self.fill(x + w - t, y, t, h, c);
    }

    pub fn line(&mut self, mut x0: i32, mut y0: i32, x1: i32, y1: i32, c: u16) {
        if y0.max(y1) < self.y0 || y0.min(y1) >= self.y0 + self.h {
            return;
        }
        let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let mut err = dx + dy;
        loop {
            self.px(x0, y0, c);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    pub fn circle(&mut self, cx: i32, cy: i32, r: i32, c: u16) {
        if !self.rows(cy - r, 2 * r + 1) {
            return;
        }
        let (mut x, mut y, mut d) = (r, 0, 1 - r);
        while x >= y {
            for (px, py) in [
                (x, y),
                (y, x),
                (-y, x),
                (-x, y),
                (-x, -y),
                (-y, -x),
                (y, -x),
                (x, -y),
            ] {
                self.px(cx + px, cy + py, c);
            }
            y += 1;
            if d < 0 {
                d += 2 * y + 1;
            } else {
                x -= 1;
                d += 2 * (y - x) + 1;
            }
        }
    }

    pub fn disc(&mut self, cx: i32, cy: i32, r: i32, c: u16) {
        let ya = (cy - r).max(self.y0);
        let yb = (cy + r).min(self.y0 + self.h - 1);
        for y in ya..=yb {
            let dy = y - cy;
            let dx = isqrt(r * r - dy * dy);
            self.fill(cx - dx, y, 2 * dx + 1, 1, c);
        }
    }

    pub fn text(&mut self, x: i32, y: i32, s: &str, font: &MonoFont, c: u16) {
        let fh = font.character_size.height as i32;
        if !self.rows(y, fh) {
            return;
        }
        let style = MonoTextStyle::new(font, Rgb565::from(RawU16::new(c)));
        let _ = Text::with_baseline(s, Point::new(x, y), style, Baseline::Top).draw(self);
    }

    /// Text centred on `cx`.
    pub fn text_c(&mut self, cx: i32, y: i32, s: &str, font: &MonoFont, c: u16) {
        let w = text_width(s, font);
        self.text(cx - w / 2, y, s, font, c);
    }

    pub fn text_r(&mut self, right: i32, y: i32, s: &str, font: &MonoFont, c: u16) {
        let w = text_width(s, font);
        self.text(right - w, y, s, font, c);
    }
}

impl Default for Canvas {
    fn default() -> Self {
        Self::new()
    }
}

pub fn text_width(s: &str, font: &MonoFont) -> i32 {
    s.len() as i32 * (font.character_size.width + font.character_spacing) as i32
}

pub fn isqrt(n: i32) -> i32 {
    if n <= 0 {
        return 0;
    }
    let mut x = libm::sqrtf(n as f32) as i32;
    while x * x > n {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= n {
        x += 1;
    }
    x
}

impl OriginDimensions for Canvas {
    fn size(&self) -> Size {
        Size::new(W as u32, H as u32)
    }
}

impl DrawTarget for Canvas {
    type Color = Rgb565;
    type Error = Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<Self::Color>>>(
        &mut self,
        pixels: I,
    ) -> Result<(), Infallible> {
        for Pixel(p, c) in pixels {
            self.px(p.x, p.y, c.into_storage());
        }
        Ok(())
    }
}

/// A 320x240 ST7789 on SPI0, landscape, as on the Pico Display Pack 2.0:
/// SCK 18, MOSI 19, CS 9, D/C 13 (backlight on 17 is the caller's).
pub struct Lcd {
    pub spi: Spi<'static, Async>,
    pub cs: Output<'static>,
    pub dc: Output<'static>,
    strips: [Canvas; 2],
    /// A hash of each strip as last sent by `frame`, 0 for unknown.
    sent: [u32; (H / STRIP) as usize],
}

impl Lcd {
    pub fn new(spi: Spi<'static, Async>, cs: Output<'static>, dc: Output<'static>) -> Self {
        Self {
            spi,
            cs,
            dc,
            strips: [Canvas::new(), Canvas::new()],
            sent: [0; (H / STRIP) as usize],
        }
    }

    fn cmd(&mut self, c: u8, args: &[u8]) {
        self.cs.set_low();
        self.dc.set_low();
        let _ = self.spi.blocking_write(&[c]);
        self.dc.set_high();
        if !args.is_empty() {
            let _ = self.spi.blocking_write(args);
        }
        self.cs.set_high();
    }

    pub async fn init(&mut self) {
        self.cmd(0x01, &[]); // SWRESET
        Timer::after_millis(150).await;
        self.cmd(0x11, &[]); // SLPOUT
        Timer::after_millis(120).await;
        self.cmd(0x3A, &[0x05]); // 16 bpp
        self.cmd(0x36, &[0x60]); // MADCTL: landscape (MX | MV)
        self.cmd(0x13, &[]); // NORON
        self.cmd(0x29, &[]); // DISPON
        Timer::after_millis(20).await;
    }

    /// Draw the whole screen, sending only the strips that changed. While
    /// DMA sends one strip, the CPU draws the next.
    pub async fn frame(&mut self, bg: u16, scene: &mut dyn FnMut(&mut Canvas)) {
        const N: usize = (H / STRIP) as usize;
        let Self {
            spi,
            cs,
            dc,
            strips,
            sent,
        } = self;
        let [a, b] = strips;
        let (mut ready, mut spare) = (a, b);
        // The strip in `ready`, if it is to be sent.
        let mut pending: Option<usize> = None;
        #[allow(clippy::needless_range_loop)] // `pending` and `s` lag one strip apart
        for s in 0..=N {
            let send = async {
                if let Some(p) = pending {
                    let y = p as i32 * STRIP;
                    let (xe, ye) = (W - 1, y + STRIP - 1);
                    let mut cmd = |c: u8, args: &[u8]| {
                        cs.set_low();
                        dc.set_low();
                        let _ = spi.blocking_write(&[c]);
                        dc.set_high();
                        let _ = spi.blocking_write(args);
                        if c != 0x2C {
                            cs.set_high();
                        }
                    };
                    cmd(0x2A, &[0, 0, (xe >> 8) as u8, xe as u8]);
                    cmd(0x2B, &[(y >> 8) as u8, y as u8, (ye >> 8) as u8, ye as u8]);
                    cmd(0x2C, &[]);
                    let _ = spi.write(ready.bytes()).await;
                    cs.set_high();
                }
            };
            let draw = async {
                if s < N {
                    spare.begin(s as i32 * STRIP, STRIP, bg);
                    scene(spare);
                    let hash = spare.hash();
                    (hash != sent[s]).then(|| {
                        sent[s] = hash;
                        s
                    })
                } else {
                    None
                }
            };
            let ((), next) = join(send, draw).await;
            pending = next;
            core::mem::swap(&mut ready, &mut spare);
        }
    }
}
