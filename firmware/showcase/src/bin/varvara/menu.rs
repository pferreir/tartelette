//! The menu of ROMs on the SD card: the built-in ROM and the card's
//! directories and `.rom` files, drawn on the Varvara screen with the
//! firmware's own font.
//!
//! Up and Down move (repeating while held), A, Start or Right open a
//! directory or run a ROM, B or Left go up a directory.

use embedded_graphics::{
    mono_font::{MonoTextStyle, ascii::FONT_7X13},
    pixelcolor::Gray2,
    prelude::*,
    primitives::Rectangle,
    text::{Baseline, Text},
};

use crate::screen::Screen;

/// The longest name the menu keeps, and the most entries in a directory.
pub const NAME: usize = 48;
pub const ENTRIES: usize = 96;

/// What a menu line opens.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Builtin,
    Dir,
    Rom,
    /// The directory above.
    Up,
}

pub struct Entry {
    pub kind: Kind,
    pub name: heapless::String<NAME>,
}

/// The directory the menu shows and what is in it.
pub struct Menu {
    /// The directory's path, `/`-separated, empty for the root.
    pub dir: heapless::String<256>,
    pub entries: heapless::Vec<Entry, ENTRIES>,
    pub sel: usize,
    top: usize,
}

/// The controller's bits.
pub const A: u8 = 0x01;
pub const B: u8 = 0x02;
pub const START: u8 = 0x08;
pub const UP: u8 = 0x10;
pub const DOWN: u8 = 0x20;
pub const LEFT: u8 = 0x40;
pub const RIGHT: u8 = 0x80;

/// What the buttons asked for.
pub enum Action {
    None,
    Open(usize),
    Back,
}

const LINE: i32 = 15;
const TOP: i32 = 20;
/// Lines of entries on the screen.
const ROWS: usize = 13;

/// Draws on the background layer, the colours being Varvara's 0..3.
struct Canvas<'a>(&'a mut Screen);

impl OriginDimensions for Canvas<'_> {
    fn size(&self) -> Size {
        Size::new(self.0.w as u32, self.0.h as u32)
    }
}

impl DrawTarget for Canvas<'_> {
    type Color = Gray2;
    type Error = core::convert::Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<Gray2>>>(
        &mut self,
        pixels: I,
    ) -> Result<(), Self::Error> {
        for Pixel(p, c) in pixels {
            if p.x >= 0 && p.y >= 0 {
                self.0.put(p.x as usize, p.y as usize, c.luma());
            }
        }
        Ok(())
    }
}

impl Menu {
    pub const fn new() -> Self {
        Self {
            dir: heapless::String::new(),
            entries: heapless::Vec::new(),
            sel: 0,
            top: 0,
        }
    }

    /// Starts the list of a directory afresh: `..` below the root, the
    /// built-in ROM in it (if there is one).
    pub fn clear(&mut self, builtin: Option<&str>) {
        self.entries.clear();
        (self.sel, self.top) = (0, 0);
        if !self.dir.is_empty() {
            self.add(Kind::Up, "..");
        } else if let Some(name) = builtin {
            self.add(Kind::Builtin, name);
        }
    }

    pub fn add(&mut self, kind: Kind, name: &str) {
        let mut n = heapless::String::new();
        for c in name.chars() {
            if n.push(c).is_err() {
                break;
            }
        }
        let _ = self.entries.push(Entry { kind, name: n });
    }

    /// Directories first, then ROMs, each in name order (after `..` or the
    /// built-in ROM).
    pub fn sort(&mut self) {
        let fixed = self
            .entries
            .iter()
            .take_while(|e| matches!(e.kind, Kind::Up | Kind::Builtin))
            .count();
        self.entries[fixed..].sort_unstable_by(|a, b| {
            (a.kind != Kind::Dir, a.name.as_bytes()).cmp(&(b.kind != Kind::Dir, b.name.as_bytes()))
        });
    }

    /// Selects the entry called `name`, if there is one (the directory just
    /// left, when going up).
    pub fn select(&mut self, name: &str) {
        if let Some(i) = self.entries.iter().position(|e| e.name == name) {
            self.sel = i;
        }
    }

    /// Moves on the buttons just pressed (`pressed`), or held for a while
    /// (`repeat`).
    pub fn input(&mut self, pressed: u8, repeat: u8) -> Action {
        let n = self.entries.len();
        if n == 0 {
            return Action::None;
        }
        if (pressed | repeat) & UP != 0 {
            self.sel = (self.sel + n - 1) % n;
        }
        if (pressed | repeat) & DOWN != 0 {
            self.sel = (self.sel + 1) % n;
        }
        if pressed & (A | START | RIGHT) != 0 {
            return Action::Open(self.sel);
        }
        if pressed & (B | LEFT) != 0 && !self.dir.is_empty() {
            return Action::Back;
        }
        Action::None
    }

    /// Draws the whole menu: the path, the entries (the selected one
    /// highlighted) and a line of help, with a note (an error) if any.
    pub fn draw(&mut self, s: &mut Screen, note: &str) {
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + ROWS {
            self.top = self.sel + 1 - ROWS;
        }
        let (w, h) = (s.w as i32, s.h as i32);
        let mut c = Canvas(s);
        let text = |c: &mut Canvas, x: i32, y: i32, color: u8, t: &str| {
            let style = MonoTextStyle::new(&FONT_7X13, Gray2::new(color));
            let _ = Text::with_baseline(t, Point::new(x, y), style, Baseline::Top).draw(c);
        };
        let _ = c.clear(Gray2::new(0));
        let _ = c.fill_solid(
            &Rectangle::new(Point::zero(), Size::new(w as u32, TOP as u32 - 4)),
            Gray2::new(2),
        );
        text(&mut c, 4, 2, 0, "VARVARA");
        let mut path: heapless::String<64> = heapless::String::new();
        let _ = path.push('/');
        // The end of a long path, which says where one is.
        let dir = self.dir.as_str();
        let tail = dir.len().saturating_sub(30);
        let _ = path.push_str(dir.get(tail..).unwrap_or(dir));
        text(&mut c, 64, 2, 0, &path);

        for (row, (i, e)) in self
            .entries
            .iter()
            .enumerate()
            .skip(self.top)
            .take(ROWS)
            .enumerate()
        {
            let y = TOP + row as i32 * LINE;
            let sel = i == self.sel;
            if sel {
                let _ = c.fill_solid(
                    &Rectangle::new(Point::new(0, y - 1), Size::new(w as u32, LINE as u32)),
                    Gray2::new(3),
                );
            }
            let (ink, dim) = if sel { (0, 0) } else { (1, 3) };
            let (mark, suffix) = match e.kind {
                Kind::Dir => ("+", "/"),
                Kind::Up => ("<", ""),
                Kind::Builtin => ("*", ""),
                Kind::Rom => (" ", ""),
            };
            text(&mut c, 4, y, dim, mark);
            text(&mut c, 18, y, ink, &e.name);
            let x = 18 + 7 * e.name.chars().count() as i32;
            text(&mut c, x, y, ink, suffix);
            if e.kind == Kind::Builtin {
                text(&mut c, x + 7, y, dim, "(built in)");
            }
        }
        if self.entries.is_empty() {
            text(&mut c, 18, TOP, 3, "No ROMs here");
        }
        let help = if note.is_empty() {
            "A: run   B: back   Select+Start: menu"
        } else {
            note
        };
        text(&mut c, 4, h - LINE, 3, help);
    }
}
