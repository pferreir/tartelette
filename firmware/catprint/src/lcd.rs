//! The Pico Display Pack's 320x240 ST7789, drawn in 16-row strips so no
//! frame buffer is needed (the receipt takes most of the RAM).
use core::convert::Infallible;

use embassy_rp::gpio::Output;
use embassy_rp::spi::{Blocking, Spi};
use embassy_time::Timer;
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::raw::RawU16;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::text::{Baseline, Text};

pub const W: i32 = 320;
pub const H: i32 = 240;
const STRIP: i32 = 16;

pub const fn rgb(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 & 0xF8) << 8) | ((g as u16 & 0xFC) << 3) | (b as u16 >> 3)
}

/// `a` to `b` by `f` (0..=1), per channel.
pub fn mix(a: (u8, u8, u8), b: (u8, u8, u8), f: f32) -> u16 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f) as u8;
    rgb(l(a.0, b.0), l(a.1, b.1), l(a.2, b.2))
}

/// A horizontal slice of the screen, RGB565 big-endian as the panel wants it.
pub struct Canvas {
    buf: [u8; (W * STRIP * 2) as usize],
    pub x0: i32,
    pub y0: i32,
    pub w: i32,
    pub h: i32,
}

impl Canvas {
    pub const fn new() -> Self {
        Self { buf: [0; (W * STRIP * 2) as usize], x0: 0, y0: 0, w: 0, h: 0 }
    }

    fn begin(&mut self, x: i32, y: i32, w: i32, h: i32) {
        (self.x0, self.y0, self.w, self.h) = (x, y, w, h);
        self.buf[..(w * h * 2) as usize].fill(0);
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..(self.w * self.h * 2) as usize]
    }

    pub fn px(&mut self, x: i32, y: i32, c: u16) {
        let (lx, ly) = (x - self.x0, y - self.y0);
        if lx >= 0 && ly >= 0 && lx < self.w && ly < self.h {
            let i = ((ly * self.w + lx) * 2) as usize;
            self.buf[i..i + 2].copy_from_slice(&c.to_be_bytes());
        }
    }

    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: u16) {
        let xa = x.max(self.x0);
        let xb = (x + w).min(self.x0 + self.w);
        let ya = y.max(self.y0);
        let yb = (y + h).min(self.y0 + self.h);
        if xa >= xb {
            return;
        }
        let b = c.to_be_bytes();
        for yy in ya..yb {
            let i = (((yy - self.y0) * self.w + (xa - self.x0)) * 2) as usize;
            for k in 0..(xb - xa) as usize {
                self.buf[i + 2 * k] = b[0];
                self.buf[i + 2 * k + 1] = b[1];
            }
        }
    }

    pub fn text(&mut self, x: i32, y: i32, s: &str, font: &MonoFont, c: u16) {
        let fh = font.character_size.height as i32;
        if y + fh <= self.y0 || y >= self.y0 + self.h || x >= self.x0 + self.w {
            return;
        }
        let style = MonoTextStyle::new(font, Rgb565::from(RawU16::new(c)));
        let _ = Text::with_baseline(s, Point::new(x, y), style, Baseline::Top).draw(self);
    }

    pub fn text_r(&mut self, right: i32, y: i32, s: &str, font: &MonoFont, c: u16) {
        let w = s.len() as i32 * (font.character_size.width + font.character_spacing) as i32;
        self.text(right - w, y, s, font, c);
    }
}

impl OriginDimensions for Canvas {
    fn size(&self) -> Size {
        Size::new(W as u32, H as u32)
    }
}

impl DrawTarget for Canvas {
    type Color = Rgb565;
    type Error = Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<Rgb565>>>(&mut self, pixels: I) -> Result<(), Infallible> {
        for Pixel(p, c) in pixels {
            self.px(p.x, p.y, c.into_storage());
        }
        Ok(())
    }
}

pub struct Lcd {
    pub spi: Spi<'static, Blocking>,
    pub cs: Output<'static>,
    pub dc: Output<'static>,
}

impl Lcd {
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

    /// Render `scene` into the rectangle, one strip at a time.
    pub fn flush(&mut self, cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, scene: &mut dyn FnMut(&mut Canvas)) {
        let mut yy = y;
        while yy < y + h {
            let rows = STRIP.min(y + h - yy);
            cv.begin(x, yy, w, rows);
            scene(cv);
            let (xe, ye) = (x + w - 1, yy + rows - 1);
            self.cmd(0x2A, &[(x >> 8) as u8, x as u8, (xe >> 8) as u8, xe as u8]);
            self.cmd(0x2B, &[(yy >> 8) as u8, yy as u8, (ye >> 8) as u8, ye as u8]);
            self.cs.set_low();
            self.dc.set_low();
            let _ = self.spi.blocking_write(&[0x2C]);
            self.dc.set_high();
            let _ = self.spi.blocking_write(cv.bytes());
            self.cs.set_high();
            yy += rows;
        }
    }
}
