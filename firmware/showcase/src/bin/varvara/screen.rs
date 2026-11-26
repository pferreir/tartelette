//! The Varvara screen device (ports 0x20-0x2f) and the System colours: two
//! layers of 2-bit pixels, the foreground over the background wherever it
//! isn't colour 0, and a palette of four 12-bit colours. See
//! <https://wiki.xxiivv.com/site/varvara.html#screen>; the drawing follows
//! uxn11.
//!
//! A pixel is a byte, `bg | fg << 2`, in rows of the screen's width. Rows
//! that change are marked dirty for the display to pick up.

/// The largest screen a program can ask for; asking for more gets this.
pub const MAX_W: usize = 320;
pub const MAX_H: usize = 256;

/// What a sprite pixel of colour 0..3 becomes, by blending mode 0..15
/// (uxn11's table, for the background layer).
const BLEND: [[u8; 4]; 16] = [
    [0, 0, 1, 2],
    [0, 1, 2, 3],
    [0, 2, 3, 1],
    [0, 3, 1, 2],
    [1, 0, 1, 2],
    [1, 1, 2, 3],
    [1, 2, 3, 1],
    [1, 3, 1, 2],
    [2, 0, 1, 2],
    [2, 1, 2, 3],
    [2, 2, 3, 1],
    [2, 3, 1, 2],
    [3, 0, 1, 2],
    [3, 1, 2, 3],
    [3, 2, 3, 1],
    [3, 3, 1, 2],
];

/// Each byte with its bits in reverse order (ARMv6-M has no RBIT).
const REVERSE: [u8; 256] = {
    let mut t = [0; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = (i as u8).reverse_bits();
        i += 1;
    }
    t
};

pub struct Screen {
    pub w: usize,
    pub h: usize,
    px: [u8; MAX_W * MAX_H],
    /// The four colours, as RGB565.
    pub colors: [u16; 4],
    dirty: [u32; MAX_H / 32],
    /// The size or palette changed: everything is to be redrawn.
    pub all_dirty: bool,
    // The registers, kept apart from the ports as in uxn11: x and y can go
    // negative while sprites step off the edge.
    x: i32,
    y: i32,
    addr: u16,
    /// Auto mode: x, y and address steps, and the repeat count.
    auto_x: bool,
    auto_y: bool,
    auto_addr: u16,
    repeat: u8,
}

fn peek2(d: &[u8], i: usize) -> u16 {
    (d[i] as u16) << 8 | d[i + 1] as u16
}

impl Screen {
    /// All zeros, to sit in .bss: `boot` gives it its size.
    pub const fn new() -> Self {
        Self {
            w: 0,
            h: 0,
            px: [0; MAX_W * MAX_H],
            colors: [0; 4],
            dirty: [0; MAX_H / 32],
            all_dirty: false,
            x: 0,
            y: 0,
            addr: 0,
            auto_x: false,
            auto_y: false,
            auto_addr: 0,
            repeat: 0,
        }
    }

    /// The size a program starts with (the panel's), before it runs.
    pub fn boot(&mut self, dev: &mut [u8; 256], w: usize, h: usize) {
        self.resize(dev, w, h);
    }

    /// Back to as `new` left it, field by field (a new one is too big to
    /// build on the stack), for another program: `boot` then sizes it.
    pub fn reset(&mut self) {
        self.px.fill(0);
        (self.w, self.h) = (0, 0);
        self.colors = [0; 4];
        self.dirty = [0; MAX_H / 32];
        self.all_dirty = true;
        (self.x, self.y, self.addr) = (0, 0, 0);
        (self.auto_x, self.auto_y, self.auto_addr, self.repeat) = (false, false, 0, 0);
    }

    /// Sets background pixel (x, y) to colour `c`, for the firmware's own
    /// drawing (the menu).
    pub fn put(&mut self, x: usize, y: usize, c: u8) {
        if x < self.w && y < self.h {
            self.px[y * self.w + x] = c & 3;
            self.mark(y, y + 1);
        }
    }

    /// Sets the size ports to the screen's size.
    fn ports(&self, dev: &mut [u8; 256]) {
        dev[0x22..0x24].copy_from_slice(&(self.w as u16).to_be_bytes());
        dev[0x24..0x26].copy_from_slice(&(self.h as u16).to_be_bytes());
    }

    pub fn row(&self, y: usize) -> &[u8] {
        &self.px[y * self.w..(y + 1) * self.w]
    }

    /// Whether row `y` changed since the last call, and forget it.
    pub fn take_dirty(&mut self, y: usize) -> bool {
        let (w, b) = (y / 32, 1 << (y % 32));
        let d = self.dirty[w] & b != 0;
        self.dirty[w] &= !b;
        d
    }

    fn mark(&mut self, y0: usize, y1: usize) {
        for y in y0..y1.min(self.h) {
            self.dirty[y / 32] |= 1 << (y % 32);
        }
    }

    fn resize(&mut self, dev: &mut [u8; 256], w: usize, h: usize) {
        let (w, h) = (w.clamp(8, MAX_W), h.clamp(8, MAX_H));
        if (w, h) != (self.w, self.h) {
            (self.w, self.h) = (w, h);
            self.px[..w * h].fill(0);
            self.all_dirty = true;
        }
        self.ports(dev);
    }

    /// System/r, g, b changed: each holds a nibble per colour.
    pub fn colorize(&mut self, dev: &[u8; 256]) {
        for (i, c) in self.colors.iter_mut().enumerate() {
            let shift = 12 - 4 * i;
            let ch = |p: usize| (peek2(dev, p) >> shift) & 0xf;
            let (r, g, b) = (ch(0x08), ch(0x0a), ch(0x0c));
            // 4-bit channels widened to 5, 6 and 5 bits.
            *c = (r << 12 | (r >> 3) << 11) | (g << 7 | (g >> 2) << 5) | (b << 1 | b >> 3);
        }
        self.all_dirty = true;
    }

    pub fn dei(&mut self, dev: &mut [u8; 256], port: u8) -> u8 {
        let (v, lo) = match port {
            0x28 => (self.x as u16, 0x29),
            0x2a => (self.y as u16, 0x2b),
            0x2c => (self.addr, 0x2d),
            _ => return dev[port as usize],
        };
        dev[lo] = v as u8;
        (v >> 8) as u8
    }

    pub fn deo(&mut self, dev: &mut [u8; 256], ram: &[u8; 0x10000], port: u8) {
        match port {
            0x23 => self.resize(dev, (peek2(dev, 0x22) & 0xfff) as usize, self.h),
            0x25 => self.resize(dev, self.w, (peek2(dev, 0x24) & 0xfff) as usize),
            0x26 => {
                let a = dev[0x26];
                self.auto_x = a & 1 != 0;
                self.auto_y = a & 2 != 0;
                self.auto_addr = if a & 4 != 0 { 8 } else { 0 };
                self.repeat = a >> 4;
            }
            0x29 => self.x = peek2(dev, 0x28) as i16 as i32,
            0x2b => self.y = peek2(dev, 0x2a) as i16 as i32,
            0x2d => self.addr = peek2(dev, 0x2c),
            0x2e => self.pixel(dev[0x2e]),
            0x2f => self.sprite(ram, dev[0x2f]),
            _ => {}
        }
    }

    fn pixel(&mut self, ctrl: u8) {
        let fg = ctrl & 0x40 != 0;
        let (keep, color) = if fg {
            (0x3, (ctrl & 3) << 2)
        } else {
            (0xc, ctrl & 3)
        };
        let (x, y) = (self.x.max(0) as usize, self.y.max(0) as usize);
        let (w, h) = (self.w, self.h);
        if x >= w || y >= h {
            return;
        }
        if ctrl & 0x80 != 0 {
            // Fill from the point to the right and bottom edges, or (flipped)
            // from the left and top ones up to it.
            let (x0, x1) = if ctrl & 0x10 != 0 { (0, x) } else { (x, w) };
            let (y0, y1) = if ctrl & 0x20 != 0 { (0, y) } else { (y, h) };
            let (keep4, color4) = (keep as u32 * 0x0101_0101, color as u32 * 0x0101_0101);
            for row in y0..y1 {
                let span = &mut self.px[row * w + x0..row * w + x1];
                // SAFETY: any bytes make valid u32s.
                let (head, words, tail) = unsafe { span.align_to_mut::<u32>() };
                for p in words {
                    *p = (*p & keep4) | color4;
                }
                for p in head.iter_mut().chain(tail) {
                    *p = (*p & keep) | color;
                }
            }
            self.mark(y0, y1);
        } else {
            let p = &mut self.px[y * w + x];
            *p = (*p & keep) | color;
            self.mark(y, y + 1);
            if self.auto_x {
                self.x += if ctrl & 0x10 != 0 { -1 } else { 1 };
            }
            if self.auto_y {
                self.y += if ctrl & 0x20 != 0 { -1 } else { 1 };
            }
        }
    }

    /// One 8x8 sprite at (x, y), at least partly on the screen.
    #[inline(never)]
    fn blit(
        &mut self,
        x: i32,
        y: i32,
        planes: &[[u8; 2]; 8],
        table: [u8; 4],
        keep: u8,
        opaque: bool,
    ) {
        let (w, h) = (self.w as i32, self.h as i32);
        let (c0, c1) = ((-x).max(0) as u32, (w - x).min(8) as u32);
        let (r0, r1) = ((-y).max(0), (h - y).min(8));
        // Few live values, for the M0+'s eight low registers: the blend
        // table packed in a word, both planes in another, shifted along.
        let table = u32::from_le_bytes(table);
        for r in r0..r1 {
            let [lo, hi] = planes[r as usize];
            let mut bits = (lo as u32 | (hi as u32) << 8) >> c0;
            let start = (y + r) as usize * self.w + (x + c0 as i32) as usize;
            for p in &mut self.px[start..start + (c1 - c0) as usize] {
                let ch = bits & 1 | (bits >> 7 & 2);
                bits >>= 1;
                if ch != 0 || opaque {
                    *p = (*p & keep) | (table >> (ch * 8)) as u8;
                }
            }
        }
        self.mark((y + r0) as usize, (y + r1) as usize);
    }

    #[inline(never)]
    fn sprite(&mut self, ram: &[u8; 0x10000], ctrl: u8) {
        let flip_x = ctrl & 0x10 != 0;
        let flip_y = ctrl & 0x20 != 0;
        let fg = ctrl & 0x40 != 0;
        let two_bpp = ctrl & 0x80 != 0;
        let blend = (ctrl & 0xf) as usize;
        // Modes 0, 5, 10 and 15 leave colour 0 transparent.
        let opaque = !blend.is_multiple_of(5);
        let (keep, shift) = if fg { (0x3u8, 2) } else { (0xcu8, 0) };
        let table = BLEND[blend].map(|c| c << shift);
        // With a repeat count, auto x lays the sprites out down a column and
        // auto y along a row; the registers then step once.
        let dx8 = if self.auto_x { 8 } else { 0 };
        let dy8 = if self.auto_y { 8 } else { 0 };
        let step_x = if flip_x { -dy8 } else { dy8 };
        let step_y = if flip_y { -dx8 } else { dx8 };
        let addr_step = self.auto_addr << two_bpp as u16;
        let (w, h) = (self.w as i32, self.h as i32);
        let (mut x, mut y) = (self.x, self.y);
        for _ in 0..=self.repeat {
            if x > -8 && y > -8 && x < w && y < h {
                // The sprite's rows, top to bottom on the screen, lined up so
                // that bit c is column c.
                let mut planes = [[0u8; 2]; 8];
                for (r, p) in planes.iter_mut().enumerate() {
                    let a = self
                        .addr
                        .wrapping_add(if flip_y { 7 - r } else { r } as u16)
                        as usize;
                    let lo = ram[a];
                    let hi = if two_bpp { ram[(a + 8) & 0xffff] } else { 0 };
                    *p = if flip_x {
                        [lo, hi]
                    } else {
                        [REVERSE[lo as usize], REVERSE[hi as usize]]
                    };
                }
                self.blit(x, y, &planes, table, keep, opaque);
            }
            x += step_x;
            y += step_y;
            self.addr = self.addr.wrapping_add(addr_step);
        }
        if self.auto_x {
            self.x += if flip_x { -dx8 } else { dx8 };
        }
        if self.auto_y {
            self.y += if flip_y { -dy8 } else { dy8 };
        }
    }
}
