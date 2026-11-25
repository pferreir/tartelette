//! Sitronix ST7789 TFT controller on a 4-wire SPI link (D/C on its own pin).
//!
//! Modelled: the command/data protocol, the address window (`CASET`/`RASET`)
//! with `MADCTL` orientation (MX/MY/MV mirror and exchange), 16 bpp and
//! 12 bpp, 16 bpp and 18 bpp pixel streams into a frame RAM, display on/off, sleep, inversion,
//! the RGB/BGR order, and hardware reset through the RST pin. Commands it
//! does not know (gamma, power control, ...) are accepted and their
//! parameters ignored. Reads support the ID registers only, returned without
//! the dummy cycle real chips insert. Not modelled: partial and scroll
//! modes, the tearing-effect output, 3-wire (9-bit) SPI.
//!
//! The frame RAM is kept in the panel's native orientation (`width` x
//! `height`, row-major, RGB565 as sent); [`St7789::rgba`] turns it into what
//! the glass shows. A `update` event (value = pixels written) is emitted
//! when chip select is released after pixel data.
use alloc::{boxed::Box, vec, vec::Vec};

use pcb::{BusType, Device, Io, Kind, Param, SpiDevice};

// Commands
const SWRESET: u8 = 0x01;
const RDDID: u8 = 0x04;
const SLPIN: u8 = 0x10;
const SLPOUT: u8 = 0x11;
const INVOFF: u8 = 0x20;
const INVON: u8 = 0x21;
const DISPOFF: u8 = 0x28;
const DISPON: u8 = 0x29;
const CASET: u8 = 0x2A;
const RASET: u8 = 0x2B;
const RAMWR: u8 = 0x2C;
const MADCTL: u8 = 0x36;
const COLMOD: u8 = 0x3A;
const RAMWRC: u8 = 0x3C;
const RDID1: u8 = 0xDA;
const RDID2: u8 = 0xDB;
const RDID3: u8 = 0xDC;

// MADCTL bits
const MY: u8 = 1 << 7;
const MX: u8 = 1 << 6;
const MV: u8 = 1 << 5;
const BGR: u8 = 1 << 3;

pub struct St7789 {
    width: usize,
    height: usize,
    dc: usize,
    rst: Option<usize>,
    bl: Option<usize>,
    /// Frame RAM, `width * height` RGB565 words in panel orientation.
    ram: Vec<u16>,
    /// Chip select is asserted (frames are only delivered while it is).
    selected: bool,
    cmd: u8,
    args: [u8; 4],
    nargs: usize,
    /// Partial pixel being assembled and how many of its bytes arrived.
    pixel: [u8; 3],
    pixel_bytes: usize,
    /// Address window: column/row counters and their limits.
    cols: (u16, u16),
    rows: (u16, u16),
    col: u16,
    row: u16,
    madctl: u8,
    colmod: u8,
    sleeping: bool,
    display_on: bool,
    inverted: bool,
    /// Bytes still to be returned by a read command.
    reply: Vec<u8>,
    /// Pixels written since chip select was last asserted.
    burst_pixels: u32,
    /// Pixels written since power-up.
    pub total_pixels: u64,
}

pub const KIND: Kind = Kind {
    name: "st7789",
    summary: "SPI TFT display controller with frame RAM",
    bus: Some(BusType::Spi),
    params: &[
        Param::pin("dc", "data/command select"),
        Param::pin("rst", "reset, active low").optional(),
        Param::pin("bl", "backlight enable").optional(),
        Param::int(
            "width",
            1,
            480,
            "panel width in pixels, in its native orientation",
        )
        .default_int(240),
        Param::int("height", 1, 480, "panel height in pixels").default_int(320),
    ],
    build: |p| {
        let (w, h) = (p.int("width") as usize, p.int("height") as usize);
        Ok(Box::new(St7789::new(
            w,
            h,
            p.pin("dc"),
            p.opt_pin("rst"),
            p.opt_pin("bl"),
        )))
    },
};

impl crate::Glass for St7789 {
    fn size(&self) -> (usize, usize) {
        St7789::size(self)
    }

    fn stamp(&self) -> (u64, bool) {
        (self.total_pixels, self.is_showing())
    }

    fn for_each_pixel(&self, f: &mut dyn FnMut(usize, [u8; 4])) {
        St7789::for_each_pixel(self, f)
    }
}

impl St7789 {
    pub fn new(
        width: usize,
        height: usize,
        dc: usize,
        rst: Option<usize>,
        bl: Option<usize>,
    ) -> Self {
        let mut d = Self {
            width,
            height,
            dc,
            rst,
            bl,
            ram: vec![0; width * height],
            selected: false,
            cmd: 0,
            args: [0; 4],
            nargs: 0,
            pixel: [0; 3],
            pixel_bytes: 0,
            cols: (0, 0),
            rows: (0, 0),
            col: 0,
            row: 0,
            madctl: 0,
            colmod: 0x66,
            sleeping: true,
            display_on: false,
            inverted: false,
            reply: Vec::new(),
            burst_pixels: 0,
            total_pixels: 0,
        };
        d.reset_registers();
        d
    }

    /// Power-on / software reset state (frame RAM is left alone).
    fn reset_registers(&mut self) {
        self.cols = (0, (self.width - 1) as u16);
        self.rows = (0, (self.height - 1) as u16);
        self.col = 0;
        self.row = 0;
        self.madctl = 0;
        self.colmod = 0x66;
        self.sleeping = true;
        self.display_on = false;
        self.inverted = false;
        self.cmd = 0;
        self.nargs = 0;
        self.pixel_bytes = 0;
        self.reply.clear();
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Frame RAM word at physical position (x, y).
    pub fn pixel(&self, x: usize, y: usize) -> u16 {
        self.ram[y * self.width + x]
    }

    /// The whole frame RAM, row-major in panel orientation.
    pub fn framebuffer(&self) -> &[u16] {
        &self.ram
    }

    /// Is the panel lit (display on and out of sleep)?
    pub fn is_showing(&self) -> bool {
        self.display_on && !self.sleeping
    }

    pub fn backlight_pin(&self) -> Option<usize> {
        self.bl
    }

    /// What the glass shows, as RGBA bytes (`width * height * 4`): black when
    /// the display is off or asleep, with inversion and R/B order applied.
    pub fn rgba(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.ram.len() * 4);
        self.for_each_pixel(|_, p| out.extend_from_slice(&p));
        out
    }

    /// Calls `f(index, [r, g, b, 255])` for every pixel in row-major order,
    /// as [`rgba`](Self::rgba) would report it.
    pub fn for_each_pixel(&self, mut f: impl FnMut(usize, [u8; 4])) {
        if !self.is_showing() {
            for i in 0..self.ram.len() {
                f(i, [0, 0, 0, 255]);
            }
            return;
        }
        let flip = if self.inverted { 0xFFFF } else { 0 };
        let bgr = self.madctl & BGR != 0;
        for (i, &w) in self.ram.iter().enumerate() {
            let w = w ^ flip;
            let (mut r, g, mut b) = (
                (w >> 11) as u8 & 0x1F,
                (w >> 5) as u8 & 0x3F,
                w as u8 & 0x1F,
            );
            if bgr {
                core::mem::swap(&mut r, &mut b);
            }
            // Expand to 8 bits by replicating the top bits.
            f(i, [r << 3 | r >> 2, g << 2 | g >> 4, b << 3 | b >> 2, 255]);
        }
    }

    fn command(&mut self, cmd: u8) {
        self.cmd = cmd;
        self.nargs = 0;
        self.reply.clear();
        match cmd {
            SWRESET => self.reset_registers(),
            SLPIN => self.sleeping = true,
            SLPOUT => self.sleeping = false,
            INVOFF => self.inverted = false,
            INVON => self.inverted = true,
            DISPOFF => self.display_on = false,
            DISPON => self.display_on = true,
            RAMWR => {
                self.col = self.cols.0;
                self.row = self.rows.0;
                self.pixel_bytes = 0;
            }
            RAMWRC => {} // continues where the last write stopped
            RDDID => self.reply = vec![0x85, 0x85, 0x52],
            RDID1 => self.reply = vec![0x85],
            RDID2 => self.reply = vec![0x85],
            RDID3 => self.reply = vec![0x52],
            _ => {}
        }
    }

    fn data(&mut self, byte: u8) {
        match self.cmd {
            CASET | RASET => {
                self.args[self.nargs.min(3)] = byte;
                self.nargs += 1;
                if self.nargs == 4 {
                    let range = (
                        u16::from_be_bytes([self.args[0], self.args[1]]),
                        u16::from_be_bytes([self.args[2], self.args[3]]),
                    );
                    if self.cmd == CASET {
                        self.cols = range;
                        self.col = range.0;
                    } else {
                        self.rows = range;
                        self.row = range.0;
                    }
                    self.nargs = 0;
                }
            }
            MADCTL => self.madctl = byte,
            COLMOD => self.colmod = byte & 0x07 | 0x50, // only the pixel-format bits matter
            RAMWR | RAMWRC => self.pixel_byte(byte),
            _ => {}
        }
    }

    fn pixel_byte(&mut self, byte: u8) {
        let format = self.colmod & 7;
        self.pixel[self.pixel_bytes] = byte;
        self.pixel_bytes += 1;
        if self.pixel_bytes < if format == 5 { 2 } else { 3 } {
            return;
        }
        self.pixel_bytes = 0;
        let p = self.pixel;
        match format {
            // Two pixels in three bytes, 4 bits per channel: R1G1 B1R2 G2B2.
            3 => {
                self.store(rgb444(p[0] >> 4, p[0] & 15, p[1] >> 4));
                self.store(rgb444(p[1] & 15, p[2] >> 4, p[2] & 15));
            }
            // Three bytes of 6 significant bits (top-aligned): keep 5/6/5.
            6 => self.store((p[0] as u16 >> 3) << 11 | (p[1] as u16 >> 2) << 5 | p[2] as u16 >> 3),
            _ => self.store(u16::from_be_bytes([p[0], p[1]])),
        }
    }

    /// Put a pixel at the current window position and step the counters.
    fn store(&mut self, word: u16) {
        // The counters walk the window; MADCTL decides where that lands in RAM.
        let (mut x, mut y) = if self.madctl & MV != 0 {
            (self.row as usize, self.col as usize)
        } else {
            (self.col as usize, self.row as usize)
        };
        if self.madctl & MX != 0 {
            x = (self.width - 1).wrapping_sub(x);
        }
        if self.madctl & MY != 0 {
            y = (self.height - 1).wrapping_sub(y);
        }
        if x < self.width && y < self.height {
            self.ram[y * self.width + x] = word;
        }
        self.burst_pixels += 1;
        self.total_pixels += 1;
        // The counters also wrap at the end of RAM, so an out-of-range window
        // end (firmware that sends `width` instead of `width - 1`) is clamped.
        let (cmax, rmax) = if self.madctl & MV != 0 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        if self.col >= self.cols.1.min(cmax as u16 - 1) {
            self.col = self.cols.0;
            self.row = if self.row >= self.rows.1.min(rmax as u16 - 1) {
                self.rows.0
            } else {
                self.row + 1
            };
        } else {
            self.col += 1;
        }
    }
}

/// A 12 bpp pixel as RGB565, low bits filled by repeating the high ones.
fn rgb444(r: u8, g: u8, b: u8) -> u16 {
    let (r, g, b) = (r as u16, g as u16, b as u16);
    (r << 1 | r >> 3) << 11 | (g << 2 | g >> 2) << 5 | (b << 1 | b >> 3)
}

impl Device for St7789 {
    fn watched_pins(&self) -> u32 {
        self.rst.map_or(0, |p| 1 << p)
    }

    fn pin_changed(&mut self, io: &mut Io, _changed: u32) {
        // Hardware reset: a low level on RST holds the controller in reset.
        if self.rst.is_some_and(|p| !io.level(p)) {
            self.reset_registers();
        }
    }

    fn spi(&mut self) -> Option<&mut dyn SpiDevice> {
        Some(self)
    }
}

impl SpiDevice for St7789 {
    fn select(&mut self, io: &mut Io, active: bool) {
        if active && !self.selected {
            self.burst_pixels = 0;
        } else if !active && self.selected && self.burst_pixels > 0 {
            io.emit("update", self.burst_pixels as i64);
            self.burst_pixels = 0;
        }
        self.selected = active;
    }

    fn transfer(&mut self, io: &mut Io, mosi: u16, _bits: u8) -> u16 {
        let byte = mosi as u8;
        if io.level(self.dc) {
            let reply = if self.reply.is_empty() {
                0
            } else {
                self.reply.remove(0)
            };
            self.data(byte);
            reply as u16
        } else {
            self.command(byte);
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use pcb::BoardEvent;

    use super::*;
    use crate::testutil::FakePins;

    const DC: usize = 13;
    const RST: usize = 17;

    struct Rig {
        pins: FakePins,
        events: Vec<BoardEvent>,
        lcd: St7789,
    }

    impl Rig {
        fn new() -> Self {
            // A small panel keeps the expected values readable.
            let mut r = Rig {
                pins: FakePins::default(),
                events: Vec::new(),
                lcd: St7789::new(8, 6, DC, Some(RST), None),
            };
            r.pins.set_chip(RST, true);
            r.lcd
                .select(&mut Io::new(&mut r.pins, 0, &mut r.events, 0, false), true);
            r
        }
        fn io(&mut self) -> (&mut St7789, Io<'_>) {
            (
                &mut self.lcd,
                Io::new(&mut self.pins, 5, &mut self.events, 0, false),
            )
        }
        fn cmd(&mut self, c: u8, data: &[u8]) {
            self.pins.set_chip(DC, false);
            let (l, mut io) = self.io();
            l.transfer(&mut io, c as u16, 8);
            self.pins.set_chip(DC, true);
            let (l, mut io) = self.io();
            for &b in data {
                l.transfer(&mut io, b as u16, 8);
            }
        }
        fn window(&mut self, x0: u8, x1: u8, y0: u8, y1: u8) {
            self.cmd(CASET, &[0, x0, 0, x1]);
            self.cmd(RASET, &[0, y0, 0, y1]);
        }
        /// Write `n` RGB565 pixels of increasing value starting at `first`.
        fn pixels(&mut self, first: u16, n: u16) {
            let data: Vec<u8> = (first..first + n).flat_map(|v| v.to_be_bytes()).collect();
            self.cmd(RAMWR, &data);
        }
    }

    fn at(r: &Rig, x: usize, y: usize) -> u16 {
        r.lcd.pixel(x, y)
    }

    #[test]
    fn window_fills_in_raster_order_and_wraps() {
        let mut r = Rig::new();
        r.cmd(COLMOD, &[0x55]);
        r.window(2, 4, 1, 2); // 3 x 2 window
        r.pixels(1, 6);
        assert_eq!([at(&r, 2, 1), at(&r, 3, 1), at(&r, 4, 1)], [1, 2, 3]);
        assert_eq!([at(&r, 2, 2), at(&r, 3, 2), at(&r, 4, 2)], [4, 5, 6]);
        assert_eq!(
            (at(&r, 1, 1), at(&r, 5, 1), at(&r, 2, 0), at(&r, 2, 3)),
            (0, 0, 0, 0)
        );
        // More data wraps back to the window's start.
        r.cmd(RAMWRC, &[0x00, 0x63]);
        assert_eq!(at(&r, 2, 1), 99);
        // RAMWR restarts at the window origin.
        r.cmd(RAMWR, &[0x00, 0x07]);
        assert_eq!(at(&r, 2, 1), 7);
    }

    #[test]
    fn pixels_split_across_transfers_and_18bpp() {
        let mut r = Rig::new();
        r.cmd(COLMOD, &[0x55]);
        r.window(0, 7, 0, 5);
        r.cmd(RAMWR, &[0x12]);
        r.cmd(RAMWRC, &[0x34]); // continuation keeps the half pixel
        assert_eq!(at(&r, 0, 0), 0x1234);
        r.cmd(COLMOD, &[0x66]);
        r.window(0, 7, 0, 5);
        // R=0b111111 G=0b000000 B=0b100000 in the top six bits of each byte.
        r.cmd(RAMWR, &[0xFC, 0x00, 0x80, 0x00, 0xFC, 0x00]);
        assert_eq!(at(&r, 0, 0), 0xF810);
        assert_eq!(at(&r, 1, 0), 0x07E0);
    }

    #[test]
    fn twelve_bpp_packs_two_pixels_in_three_bytes() {
        let mut r = Rig::new();
        r.cmd(COLMOD, &[0x53]);
        r.window(0, 7, 0, 5);
        // (R,G,B) = (F,0,8) then (0,F,1); the half of a third pair waits.
        r.cmd(RAMWR, &[0xF0, 0x80, 0xF1, 0x12]);
        assert_eq!(at(&r, 0, 0), 0xF800 | 0b10001);
        assert_eq!(at(&r, 1, 0), 0x07E0 | 0b00010);
        r.cmd(RAMWRC, &[0x34, 0x56]);
        assert_eq!(at(&r, 2, 0), rgb444(1, 2, 3));
        assert_eq!(at(&r, 3, 0), rgb444(4, 5, 6));
    }

    #[test]
    fn madctl_mirrors_and_exchanges() {
        let mut r = Rig::new();
        r.cmd(COLMOD, &[0x55]);
        // MX: columns mirrored.
        r.cmd(MADCTL, &[MX]);
        r.window(0, 1, 0, 0);
        r.pixels(1, 2);
        assert_eq!((at(&r, 7, 0), at(&r, 6, 0)), (1, 2));
        // MY: rows mirrored.
        r.cmd(MADCTL, &[MY]);
        r.window(0, 0, 0, 1);
        r.pixels(3, 2);
        assert_eq!((at(&r, 0, 5), at(&r, 0, 4)), (3, 4));
        // MV: the "column" counter now walks down the panel.
        r.cmd(MADCTL, &[MV]);
        r.window(0, 2, 1, 1);
        r.pixels(5, 3);
        assert_eq!([at(&r, 1, 0), at(&r, 1, 1), at(&r, 1, 2)], [5, 6, 7]);
        // MX|MV (a 90 degree rotation): counters (c, r) land at (W-1-r, c).
        r.cmd(MADCTL, &[MX | MV]);
        r.window(0, 1, 0, 0);
        r.pixels(8, 2);
        assert_eq!((at(&r, 7, 0), at(&r, 7, 1)), (8, 9));
    }

    #[test]
    fn window_end_past_ram_wraps_at_ram_end() {
        let mut r = Rig::new();
        r.cmd(COLMOD, &[0x55]);
        r.window(0, 8, 0, 0); // end column 8 is one past the 8-pixel width
        r.pixels(1, 9);
        assert_eq!([at(&r, 0, 0), at(&r, 7, 0), at(&r, 0, 1)], [9, 8, 0]);
    }

    #[test]
    fn out_of_range_writes_are_dropped() {
        let mut r = Rig::new();
        r.cmd(COLMOD, &[0x55]);
        r.window(9, 20, 0, 0); // starts past the 8-pixel width
        r.pixels(1, 4);
        assert!(r.lcd.framebuffer().iter().all(|&w| w == 0));
        assert_eq!(r.lcd.total_pixels, 4);
    }

    #[test]
    fn display_state_and_rgba() {
        let mut r = Rig::new();
        r.cmd(COLMOD, &[0x55]);
        r.window(0, 0, 0, 0);
        r.pixels(0xF800, 1); // pure red as sent
        assert!(!r.lcd.is_showing(), "asleep and off after reset");
        assert_eq!(&r.lcd.rgba()[..4], [0, 0, 0, 255]);
        r.cmd(SLPOUT, &[]);
        r.cmd(DISPON, &[]);
        assert!(r.lcd.is_showing());
        assert_eq!(&r.lcd.rgba()[..4], [255, 0, 0, 255]);
        r.cmd(MADCTL, &[BGR]);
        assert_eq!(
            &r.lcd.rgba()[..4],
            [0, 0, 255, 255],
            "BGR order swaps red and blue"
        );
        r.cmd(INVON, &[]);
        assert_eq!(&r.lcd.rgba()[..4], [255, 255, 0, 255]);
        r.cmd(INVOFF, &[]);
        r.cmd(DISPOFF, &[]);
        assert_eq!(&r.lcd.rgba()[..4], [0, 0, 0, 255]);
        assert_eq!(r.lcd.rgba().len(), 8 * 6 * 4);
    }

    #[test]
    fn software_and_hardware_reset_keep_ram_but_reset_state() {
        let mut r = Rig::new();
        r.cmd(COLMOD, &[0x55]);
        r.cmd(MADCTL, &[MX]);
        r.cmd(SLPOUT, &[]);
        r.cmd(DISPON, &[]);
        r.window(0, 0, 0, 0);
        r.pixels(7, 1);
        r.cmd(SWRESET, &[]);
        assert!(!r.lcd.is_showing());
        assert_eq!((r.lcd.madctl, r.lcd.colmod, r.lcd.cols), (0, 0x66, (0, 7)));
        assert_eq!(at(&r, 7, 0), 7, "frame RAM survives");
        // Hardware reset through the pin, seen by the board as a pin change.
        r.cmd(SLPOUT, &[]);
        r.cmd(DISPON, &[]);
        r.pins.set_chip(RST, false);
        let (l, mut io) = r.io();
        l.pin_changed(&mut io, 1 << RST);
        assert!(!r.lcd.is_showing());
        assert_eq!(r.lcd.watched_pins(), 1 << RST);
    }

    #[test]
    fn id_reads_and_chip_select_events() {
        let mut r = Rig::new();
        r.cmd(RDDID, &[]);
        let (l, mut io) = r.io();
        let id: Vec<u16> = (0..4).map(|_| l.transfer(&mut io, 0, 8)).collect();
        assert_eq!(id, [0x85, 0x85, 0x52, 0]);
        // `update` is reported when CS is released after pixels, once.
        r.cmd(COLMOD, &[0x55]);
        r.window(0, 7, 0, 5);
        r.pixels(1, 10);
        assert!(r.events.is_empty());
        let (l, mut io) = r.io();
        l.select(&mut io, false);
        l.select(&mut io, true);
        l.select(&mut io, false); // nothing written in between: silent
        let ev: Vec<_> = r.events.iter().map(|e| (e.label, e.value)).collect();
        assert_eq!(ev, [("update", 10)]);
    }

    #[test]
    fn factory_parses_parameters() {
        let parse = |t: &str| pcb::Board::parse_with(t, &crate::registry());
        let b =
            parse("parts.s = { kind = \"st7789\", dc = 13, rst = 17, width = 240, height = 240 }")
                .unwrap();
        let lcd = b.device::<St7789>("s").unwrap();
        assert_eq!(
            (lcd.size(), lcd.framebuffer().len()),
            ((240, 240), 240 * 240)
        );
        assert_eq!(
            parse("parts.s = { kind = \"st7789\" }").unwrap_err().msg,
            "missing dc (data/command select)"
        );
    }
}
