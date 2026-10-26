//! The receipt as the printer wants it: 384 dots a row, 1 bit a dot, the
//! leftmost dot in bit 0, 1 = black. Drawn with embedded-graphics (`On` is
//! ink) plus a few receipt-shaped helpers that move a cursor down the roll.
use core::convert::Infallible;

use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::text::{Baseline, Text};

pub const W: i32 = 384;
pub const ROW_BYTES: usize = 48;
/// 200 mm of paper at 8 dots per mm.
pub const MAX_ROWS: usize = 1600;
/// Rows the printer insists on per job.
pub const MIN_ROWS: usize = 90;

pub struct Paper {
    buf: [u8; ROW_BYTES * MAX_ROWS],
    /// Rows in use.
    pub rows: usize,
    /// Where the next block goes.
    pub y: i32,
}

impl Paper {
    pub const fn new() -> Self {
        Self { buf: [0; ROW_BYTES * MAX_ROWS], rows: 0, y: 0 }
    }

    pub fn clear(&mut self) {
        self.buf.fill(0);
        self.rows = 0;
        self.y = 0;
    }

    pub fn row(&self, y: usize) -> &[u8] {
        &self.buf[y * ROW_BYTES..(y + 1) * ROW_BYTES]
    }

    pub fn dot(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && x < W && (y as usize) < MAX_ROWS && self.buf[y as usize * ROW_BYTES + x as usize / 8] >> (x % 8) & 1 != 0
    }

    pub fn set(&mut self, x: i32, y: i32, ink: bool) {
        if x < 0 || y < 0 || x >= W || y as usize >= MAX_ROWS {
            return;
        }
        let i = y as usize * ROW_BYTES + x as usize / 8;
        if ink {
            self.buf[i] |= 1 << (x % 8);
        } else {
            self.buf[i] &= !(1 << (x % 8));
        }
        self.rows = self.rows.max(y as usize + 1);
    }

    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, ink: bool) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.set(xx, yy, ink);
            }
        }
    }

    /// Ordered dither: `level` of 16 dots inked.
    pub fn shade(&mut self, x: i32, y: i32, w: i32, h: i32, level: u8) {
        const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
        for yy in y..y + h {
            for xx in x..x + w {
                if BAYER[(yy & 3) as usize][(xx & 3) as usize] < level {
                    self.set(xx, yy, true);
                }
            }
        }
    }

    pub fn hline(&mut self, x0: i32, x1: i32, y: i32, dash: i32) {
        for x in x0..=x1 {
            if dash == 0 || (x - x0) / dash % 2 == 0 {
                self.set(x, y, true);
            }
        }
    }

    pub fn vline(&mut self, x: i32, y0: i32, y1: i32, dash: i32) {
        for y in y0..=y1 {
            if dash == 0 || (y - y0) / dash % 2 == 0 {
                self.set(x, y, true);
            }
        }
    }

    pub fn line(&mut self, mut x0: i32, mut y0: i32, x1: i32, y1: i32) {
        let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let mut err = dx + dy;
        loop {
            self.set(x0, y0, true);
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

    pub fn circle(&mut self, cx: i32, cy: i32, r: i32) {
        let (mut x, mut y, mut d) = (r, 0, 1 - r);
        while x >= y {
            for (px, py) in [(x, y), (y, x), (-y, x), (-x, y), (-x, -y), (-y, -x), (y, -x), (x, -y)] {
                self.set(cx + px, cy + py, true);
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

    pub fn disc(&mut self, cx: i32, cy: i32, r: i32, ink: bool) {
        for dy in -r..=r {
            let dx = libm::sqrtf((r * r - dy * dy) as f32) as i32;
            self.fill(cx - dx, cy + dy, 2 * dx + 1, 1, ink);
        }
    }

    /// Text with its top-left at (x, y), each font pixel `scale` dots
    /// square. Returns the width.
    pub fn text(&mut self, x: i32, y: i32, s: &str, font: &MonoFont, scale: i32, ink: bool) -> i32 {
        let style = MonoTextStyle::new(font, if ink { BinaryColor::On } else { BinaryColor::Off });
        let mut t = Scaled { paper: self, x, y, scale };
        let _ = Text::with_baseline(s, Point::zero(), style, Baseline::Top).draw(&mut t);
        text_width(s, font) * scale
    }

    pub fn text_center(&mut self, y: i32, s: &str, font: &MonoFont, scale: i32, ink: bool) {
        let w = text_width(s, font) * scale;
        self.text((W - w) / 2, y, s, font, scale, ink);
    }

    pub fn text_right(&mut self, right: i32, y: i32, s: &str, font: &MonoFont, scale: i32, ink: bool) {
        let w = text_width(s, font) * scale;
        self.text(right - w, y, s, font, scale, ink);
    }

    // ------------------------------------------------- cursor helpers ----

    pub fn gap(&mut self, h: i32) {
        self.y += h;
    }

    /// One line of body text at the cursor.
    pub fn line_text(&mut self, s: &str, font: &MonoFont) {
        self.text(0, self.y, s, font, 1, true);
        self.y += font.character_size.height as i32 + 2;
    }

    /// `key ........ value`, the receipt way.
    pub fn kv(&mut self, key: &str, value: &str, font: &MonoFont) {
        let cw = (font.character_size.width + font.character_spacing) as i32;
        let kw = self.text(0, self.y, key, font, 1, true);
        let vw = text_width(value, font);
        self.text(W - vw, self.y, value, font, 1, true);
        // Dot leaders on the baseline between them.
        let base = self.y + font.baseline as i32;
        let mut x = kw + cw / 2;
        while x < W - vw - cw / 2 {
            self.set(x, base, true);
            x += 4;
        }
        self.y += font.character_size.height as i32 + 2;
    }

    /// Words wrapped to the width, each line prefixed with `indent` dots.
    pub fn para(&mut self, s: &str, font: &MonoFont, indent: i32) {
        let cw = (font.character_size.width + font.character_spacing) as i32;
        let cols = ((W - indent) / cw) as usize;
        let mut line: heapless::String<64> = heapless::String::new();
        for word in s.split(' ').filter(|w| !w.is_empty()) {
            if !line.is_empty() && line.len() + 1 + word.len() > cols {
                self.text(indent, self.y, &line, font, 1, true);
                self.y += font.character_size.height as i32 + 2;
                line.clear();
            }
            if !line.is_empty() {
                let _ = line.push(' ');
            }
            let _ = line.push_str(&word[..word.len().min(cols)]);
        }
        if !line.is_empty() {
            self.text(indent, self.y, &line, font, 1, true);
            self.y += font.character_size.height as i32 + 2;
        }
    }

    /// A section header: a framed title with a note on the right. Framed,
    /// not white on black: the head has a fixed energy budget per row, so
    /// rows that are mostly black print grey and blotchy.
    pub fn section(&mut self, title: &str, note: &str, font: &MonoFont) {
        let h = font.character_size.height as i32 + 6;
        let y = self.y;
        self.hline(0, W - 1, y, 0);
        self.hline(0, W - 1, y + h - 1, 0);
        self.fill(0, y, 3, h, true);
        self.fill(W - 3, y, 3, h, true);
        self.fill(7, y + 4, 4, h - 8, true);
        self.text(15, y + 3, title, font, 1, true);
        self.text_right(W - 8, y + 3, note, font, 1, true);
        self.y += h + 6;
    }

    /// The height the job needs: what was drawn, and at least the minimum.
    pub fn job_rows(&self) -> usize {
        self.rows.max(self.y.max(0) as usize).clamp(MIN_ROWS, MAX_ROWS)
    }
}

pub fn text_width(s: &str, font: &MonoFont) -> i32 {
    s.len() as i32 * (font.character_size.width + font.character_spacing) as i32
}

/// Draws through to the paper with every pixel blown up.
struct Scaled<'a> {
    paper: &'a mut Paper,
    x: i32,
    y: i32,
    scale: i32,
}

impl OriginDimensions for Scaled<'_> {
    fn size(&self) -> Size {
        Size::new(W as u32, MAX_ROWS as u32)
    }
}

impl DrawTarget for Scaled<'_> {
    type Color = BinaryColor;
    type Error = Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<BinaryColor>>>(&mut self, pixels: I) -> Result<(), Infallible> {
        // The style has no background, so only glyph pixels come through.
        let s = self.scale;
        for Pixel(p, c) in pixels {
            self.paper.fill(self.x + p.x * s, self.y + p.y * s, s, s, c.is_on());
        }
        Ok(())
    }
}
