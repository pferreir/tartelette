//! Solomon Systech SSD1306, the controller of the common monochrome OLEDs
//! (128x64 and 128x32), on I2C.
//!
//! Each write starts with a control byte: bit 6 says whether what follows
//! is data (for the display RAM) or commands, and bit 7 (Co) that only one
//! byte follows before the next control byte. Data goes to the 128x64-bit
//! GDDRAM (8 pages of 128 columns, a byte a column, LSB at the top) at the
//! address pointer, which moves on as in the addressing mode: horizontal,
//! vertical or page. The commands that change what the glass shows are
//! modelled: display on/off, contrast, inversion, entire display on,
//! segment remap, COM scan direction, start line, display offset and the
//! multiplex ratio. The glass is mounted as on the common modules: upright
//! with the segments remapped (A1) and the COM scan reversed (C8). Timing, charge pump and scroll commands are taken with
//! their arguments and otherwise ignored. Reads return 0.
//!
//! Emits `update` with the bytes of display data a write carried, at its
//! STOP.
//!
//! A command's arguments may come in later writes (each with its control
//! byte), as drivers that send a byte a write do.
use alloc::{boxed::Box, vec, vec::Vec};

use pcb::{BusType, Device, I2cDevice, Io, Kind, Param};

use crate::Glass;

const COLS: usize = 128;
const PAGES: usize = 8;

pub struct Ssd1306 {
    addr: u8,
    height: usize,
    color: [u8; 3],
    ram: Vec<u8>,
    /// Address window and pointer (column, page), and the addressing mode
    /// (0 horizontal, 1 vertical, 2 page).
    cols: (usize, usize),
    pages: (usize, usize),
    col: usize,
    page: usize,
    mode: u8,
    on: bool,
    invert: bool,
    entire_on: bool,
    remap: bool,
    scan_reversed: bool,
    start_line: usize,
    offset: usize,
    mux: usize,
    contrast: u8,
    /// Where the bytes of a write go: expecting a control byte, or a stream
    /// of commands or data (`single` when Co was set: one byte, then a
    /// control byte again).
    expect_control: bool,
    data: bool,
    single: bool,
    /// The command being collected, with its arguments.
    cmd: Vec<u8>,
    /// Data bytes in the write under way, and in all.
    burst: u64,
    pub total: u64,
}

pub const KIND: Kind = Kind {
    name: "ssd1306",
    summary: "SSD1306 monochrome OLED controller on I2C",
    bus: Some(BusType::I2c),
    params: &[
        Param::hex("addr", 0x3C, 0x3D, "I2C address (SA0 low: 0x3C)").default_int(0x3C),
        Param::choice("height", &["32", "64"], "rows of the panel (128 wide)").default_str("64"),
        Param::choice(
            "color",
            &["white", "blue", "yellow", "green"],
            "the colour the pixels glow",
        )
        .default_str("white"),
    ],
    build: |p| {
        let color = match p.str("color") {
            "blue" => [80, 180, 255],
            "yellow" => [255, 220, 60],
            "green" => [80, 255, 120],
            _ => [235, 240, 255],
        };
        Ok(Box::new(Ssd1306::new(
            p.int("addr") as u8,
            if p.str("height") == "32" { 32 } else { 64 },
            color,
        )))
    },
};

/// Argument bytes that follow command `c`.
fn args(c: u8) -> usize {
    match c {
        0x81 | 0x20 | 0xA8 | 0xD3 | 0xD5 | 0xD9 | 0xDA | 0xDB | 0x8D => 1,
        0x21 | 0x22 | 0xA3 => 2,
        0x29 | 0x2A => 5,
        0x26 | 0x27 => 6,
        _ => 0,
    }
}

impl Ssd1306 {
    fn new(addr: u8, height: usize, color: [u8; 3]) -> Self {
        Ssd1306 {
            addr,
            height,
            color,
            ram: vec![0; COLS * PAGES],
            cols: (0, COLS - 1),
            pages: (0, PAGES - 1),
            col: 0,
            page: 0,
            mode: 2,
            on: false,
            invert: false,
            entire_on: false,
            remap: false,
            scan_reversed: false,
            start_line: 0,
            offset: 0,
            mux: 64,
            contrast: 0x7F,
            expect_control: true,
            data: false,
            single: false,
            cmd: Vec::new(),
            burst: 0,
            total: 0,
        }
    }

    fn command(&mut self) {
        let c = &self.cmd;
        match c[0] {
            0x81 => self.contrast = c[1],
            0x20 => self.mode = c[1] & 3,
            0x21 => {
                self.cols = ((c[1] & 0x7F) as usize, (c[2] & 0x7F) as usize);
                self.col = self.cols.0;
            }
            0x22 => {
                self.pages = ((c[1] & 7) as usize, (c[2] & 7) as usize);
                self.page = self.pages.0;
            }
            0xA8 => self.mux = ((c[1] & 0x3F) as usize + 1).max(16),
            0xD3 => self.offset = (c[1] & 0x3F) as usize,
            0x40..=0x7F => self.start_line = (c[0] & 0x3F) as usize,
            0xA0 | 0xA1 => self.remap = c[0] & 1 != 0,
            0xC0 => self.scan_reversed = false,
            0xC8 => self.scan_reversed = true,
            0xA4 | 0xA5 => self.entire_on = c[0] & 1 != 0,
            0xA6 | 0xA7 => self.invert = c[0] & 1 != 0,
            0xAE | 0xAF => self.on = c[0] & 1 != 0,
            0xB0..=0xB7 => self.page = (c[0] & 7) as usize,
            0x00..=0x0F => self.col = self.col & 0xF0 | (c[0] & 0xF) as usize,
            0x10..=0x1F => self.col = self.col & 0x0F | ((c[0] & 0x7) as usize) << 4,
            _ => {}
        }
    }

    fn data_byte(&mut self, b: u8) {
        self.ram[self.page * COLS + self.col.min(COLS - 1)] = b;
        self.burst += 1;
        let (cols, pages) = (self.cols, self.pages);
        match self.mode {
            0 => {
                self.col += 1;
                if self.col > cols.1 {
                    self.col = cols.0;
                    self.page = if self.page >= pages.1 {
                        pages.0
                    } else {
                        self.page + 1
                    };
                }
            }
            1 => {
                self.page += 1;
                if self.page > pages.1 {
                    self.page = pages.0;
                    self.col = if self.col >= cols.1 {
                        cols.0
                    } else {
                        self.col + 1
                    };
                }
            }
            _ => self.col = (self.col + 1) % COLS,
        }
    }

    /// Whether pixel (x, y) of the glass glows.
    pub fn lit(&self, x: usize, y: usize) -> bool {
        if !self.on {
            return false;
        }
        if self.entire_on {
            return true;
        }
        // The glass sits as on the common modules: upright with the segments
        // remapped (A1) and the COM scan reversed (C8), as drivers set them.
        let com = if self.scan_reversed {
            y
        } else {
            self.mux - 1 - y.min(self.mux - 1)
        };
        let row = (com + self.start_line + self.offset) % 64;
        let seg = if self.remap { x } else { COLS - 1 - x };
        let bit = self.ram[row / 8 * COLS + seg] >> (row % 8) & 1 != 0;
        bit != self.invert
    }
}

impl Glass for Ssd1306 {
    fn size(&self) -> (usize, usize) {
        (COLS, self.height)
    }

    fn stamp(&self) -> (u64, bool) {
        // Settings change the picture too: fold them in.
        let settings = (self.on as u64)
            | (self.invert as u64) << 1
            | (self.entire_on as u64) << 2
            | (self.remap as u64) << 3
            | (self.scan_reversed as u64) << 4
            | (self.start_line as u64) << 5
            | (self.offset as u64) << 11
            | (self.contrast as u64) << 17;
        (self.total ^ settings << 40, self.on)
    }

    fn for_each_pixel(&self, f: &mut dyn FnMut(usize, [u8; 4])) {
        // Contrast dims the glow, from a third up to full.
        let k = 85 + self.contrast as u32 * 170 / 255;
        let glow = self.color.map(|c| (c as u32 * k / 255) as u8);
        for y in 0..self.height {
            for x in 0..COLS {
                let [r, g, b] = if self.lit(x, y) { glow } else { [0, 0, 0] };
                f(y * COLS + x, [r, g, b, 255]);
            }
        }
    }
}

impl I2cDevice for Ssd1306 {
    fn address(&self) -> u8 {
        self.addr
    }

    fn start(&mut self, _io: &mut Io, _read: bool) -> bool {
        // A command waiting for its arguments keeps waiting: drivers send
        // each byte of a command in a write of its own.
        self.expect_control = true;
        true
    }

    fn write(&mut self, _io: &mut Io, byte: u8) -> bool {
        if self.expect_control {
            self.data = byte & 0x40 != 0;
            self.single = byte & 0x80 != 0;
            self.expect_control = false;
            return true;
        }
        if self.data {
            self.data_byte(byte);
        } else {
            self.cmd.push(byte);
            if self.cmd.len() > args(self.cmd[0]) {
                self.command();
                self.cmd.clear();
            }
        }
        if self.single {
            self.expect_control = true;
        }
        true
    }

    fn read(&mut self, _io: &mut Io, _last: bool) -> u8 {
        0
    }

    fn stop(&mut self, io: &mut Io) {
        if self.burst > 0 {
            self.total += self.burst;
            io.emit("update", self.burst as i64);
            self.burst = 0;
        }
    }
}

impl Device for Ssd1306 {
    fn i2c(&mut self) -> Option<&mut dyn I2cDevice> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakePins;

    fn send(d: &mut Ssd1306, bytes: &[u8]) {
        let (mut pins, mut events) = (FakePins::default(), Vec::new());
        let mut io = Io::new(&mut pins, 0, &mut events, 0, false);
        d.start(&mut io, false);
        for &b in bytes {
            assert!(d.write(&mut io, b));
        }
        d.stop(&mut io);
    }

    fn display() -> Ssd1306 {
        Ssd1306::new(0x3C, 32, [255, 255, 255])
    }

    #[test]
    fn horizontal_mode_fills_the_window() {
        let mut d = display();
        // As a typical init: mux 32, horizontal mode, window 0..127 x 0..3.
        send(
            &mut d,
            &[
                0x00, 0xA8, 31, 0xA1, 0xC8, 0x20, 0x00, 0x21, 0, 127, 0x22, 0, 3, 0xAF,
            ],
        );
        let mut data = vec![0x40];
        data.extend((0..128 * 4).map(|i| if i == 129 { 0x81 } else { 0 }));
        send(&mut d, &data);
        // Byte 129: page 1, column 1, rows 8 and 15.
        assert!(d.lit(1, 8) && d.lit(1, 15));
        assert!(!d.lit(1, 9) && !d.lit(0, 8));
        assert_eq!(d.total, 512);
        assert_eq!((d.col, d.page), (0, 0), "wrapped round the window");
    }

    #[test]
    fn page_mode_remap_and_settings() {
        let mut d = display();
        // Co set: a command byte, then another control byte each time.
        send(
            &mut d,
            &[
                0x80, 0xA8, 0x80, 31, 0x80, 0xB2, 0x80, 0x05, 0x80, 0x11, 0x80, 0xAF,
            ],
        );
        assert_eq!((d.page, d.col), (2, 0x15));
        send(&mut d, &[0x40, 0x01]);
        // Without remap and COM scan reversed, as the chip resets, the
        // module shows it upside down.
        assert!(d.lit(127 - 0x15, 31 - 16));
        send(&mut d, &[0x00, 0xA1, 0xC8]);
        assert!(d.lit(0x15, 16));
        send(&mut d, &[0x00, 0xA7]);
        assert!(!d.lit(0x15, 16) && d.lit(0, 0), "inverted");
        send(&mut d, &[0x00, 0xAE]);
        assert!(!d.lit(0, 0), "off");
        // A command's argument in a write of its own, as pico-examples'
        // driver sends them.
        send(&mut d, &[0x00, 0x81]);
        send(&mut d, &[0x00, 0x20]);
        assert_eq!(d.contrast, 0x20);
        // Scroll setup takes its arguments without eating what follows.
        send(&mut d, &[0x00, 0x26, 0, 0, 0, 7, 0, 0xFF, 0xAF, 0xA6]);
        assert!(d.on && !d.invert);
    }
}
