//! A view of what is on an SD card: its directories and files, from a copy
//! of the card's image taken when the tab opens or on Refresh. Read
//! only, so it can never fight with the firmware over the file system.
use egui::{Color32, RichText};
use pcb_devices::fat::read::{self, Entry, FatType};

use crate::files;

/// How much of a file the preview shows.
const PREVIEW: usize = 64 * 1024;
const HEX_ROW: usize = 16;

pub struct SdBrowser {
    pub part: String,
    image: Vec<u8>,
    /// Emulated time of the copy, and the card's write count then.
    taken_at: f64,
    pub written_then: u32,
    /// What the copy holds (label and type), or why it can't be read.
    volume: Result<String, String>,
    /// Directories from the root down to the one shown.
    path: Vec<Entry>,
    listing: Result<Vec<Entry>, String>,
    preview: Option<Preview>,
}

struct Preview {
    file: Entry,
    /// The first `PREVIEW` bytes as lines of text or hex, or why not.
    lines: Result<Vec<String>, String>,
    hex: bool,
}

/// What the browser wants from the run.
pub enum Want {
    Nothing,
    /// A fresh copy of the card.
    Refresh,
}

impl SdBrowser {
    pub fn new(part: String, image: Vec<u8>, taken_at: f64, written: u32) -> Self {
        let mut b = SdBrowser {
            part,
            image: Vec::new(),
            taken_at: 0.0,
            written_then: 0,
            volume: Err(String::new()),
            path: Vec::new(),
            listing: Ok(Vec::new()),
            preview: None,
        };
        b.refresh(image, taken_at, written);
        b
    }

    /// Show a new copy of the card, staying in the same directory and on the
    /// same file where they still exist.
    pub fn refresh(&mut self, image: Vec<u8>, taken_at: f64, written: u32) {
        self.image = image;
        self.taken_at = taken_at;
        self.written_then = written;
        let names: Vec<String> = self.path.drain(..).map(|e| e.name).collect();
        let selected = self.preview.take().map(|p| p.file.name);
        self.volume = read::open(&self.image).map(|v| {
            let kind = match v.fat_type {
                FatType::Fat12 => "FAT12",
                FatType::Fat16 => "FAT16",
                FatType::Fat32 => "FAT32",
            };
            match v.label.as_str() {
                "" => kind.to_string(),
                label => format!("\"{label}\", {kind}"),
            }
        });
        self.listing = self.list();
        for name in names {
            let Some(dir) = self.find(&name, true) else {
                break;
            };
            self.enter(dir);
        }
        if let Some(file) = selected.and_then(|n| self.find(&n, false)) {
            self.select(file);
        }
    }

    fn find(&self, name: &str, dir: bool) -> Option<Entry> {
        let entries = self.listing.as_ref().ok()?;
        entries
            .iter()
            .find(|e| e.name == name && e.dir == dir)
            .cloned()
    }

    fn list(&self) -> Result<Vec<Entry>, String> {
        let v = read::open(&self.image)?;
        let mut entries = match self.path.last() {
            Some(dir) => v.dir(dir)?,
            None => v.root()?,
        };
        // Directories first, then by name, as file managers do.
        entries.sort_by(|a, b| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    fn enter(&mut self, dir: Entry) {
        self.path.push(dir);
        self.listing = self.list();
        self.preview = None;
    }

    fn up_to(&mut self, depth: usize) {
        self.path.truncate(depth);
        self.listing = self.list();
        self.preview = None;
    }

    fn read(&self, file: &Entry, limit: usize) -> Result<Vec<u8>, String> {
        read::open(&self.image)?.read(file, limit)
    }

    fn select(&mut self, file: Entry) {
        let bytes = self.read(&file, PREVIEW);
        let hex = bytes.as_deref().is_ok_and(|b| !is_text(b));
        let lines = bytes.map(|b| if hex { hex_lines(&b) } else { text_lines(&b) });
        self.preview = Some(Preview { file, lines, hex });
    }

    /// Draw the browser. `written_now` is the card's write count in the run
    /// (none: the card is gone).
    pub fn ui(&mut self, ui: &mut egui::Ui, written_now: Option<u32>) -> Want {
        let mut want = Want::Nothing;
        let id = egui::IdSalt::new(("sd-browser", self.part.clone()));
        ui.push_id(id, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(written_now.is_some(), egui::Button::new("Refresh"))
                    .clicked()
                {
                    want = Want::Refresh;
                }
                match &self.volume {
                    Ok(v) => ui.label(v),
                    Err(e) => ui.colored_label(ui.visuals().error_fg_color, e),
                };
                ui.label(RichText::new(format!("copy taken at {:.3} s", self.taken_at)).weak());
                match written_now {
                    None => {
                        ui.colored_label(ui.visuals().warn_fg_color, "the card is gone");
                    }
                    Some(n) if n != self.written_then => {
                        ui.colored_label(ui.visuals().warn_fg_color, "written since");
                    }
                    _ => {}
                }
            });
            if self.volume.is_err() {
                return;
            }
            self.breadcrumbs(ui);
            ui.separator();
            ui.columns(2, |cols| {
                self.listing_ui(&mut cols[0]);
                self.preview_ui(&mut cols[1]);
            });
        });
        want
    }

    fn breadcrumbs(&mut self, ui: &mut egui::Ui) {
        let mut go = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if ui.link("/").clicked() {
                go = Some(0);
            }
            for (i, dir) in self.path.iter().enumerate() {
                if i > 0 {
                    ui.label("/");
                }
                if ui.link(&dir.name).clicked() {
                    go = Some(i + 1);
                }
            }
        });
        if let Some(depth) = go {
            self.up_to(depth);
        }
    }

    fn listing_ui(&mut self, ui: &mut egui::Ui) {
        let entries = match &self.listing {
            Ok(e) => e,
            Err(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
        };
        if entries.is_empty() {
            ui.label(RichText::new("empty").weak());
        }
        let mut clicked = None;
        egui::ScrollArea::vertical()
            .id_salt("entries")
            .auto_shrink(false)
            .show(ui, |ui| {
                egui::Grid::new("entries-grid")
                    .num_columns(3)
                    .striped(true)
                    .show(ui, |ui| {
                        for e in entries {
                            let selected = self.preview.as_ref().is_some_and(|p| p.file == *e);
                            let name = if e.dir {
                                format!("{}/", e.name)
                            } else {
                                e.name.clone()
                            };
                            if ui.selectable_label(selected, name).clicked() {
                                clicked = Some(e.clone());
                            }
                            ui.label(if e.dir {
                                String::new()
                            } else {
                                size_text(e.size)
                            });
                            ui.label(RichText::new(date_text(e.modified)).weak());
                            ui.end_row();
                        }
                    });
            });
        match clicked {
            Some(e) if e.dir => self.enter(e),
            Some(e) => self.select(e),
            None => {}
        }
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        let Some(p) = &self.preview else {
            ui.label(RichText::new("pick a file to see what it holds").weak());
            return;
        };
        ui.horizontal(|ui| {
            ui.strong(&p.file.name);
            ui.label(size_text(p.file.size));
            if ui.button("Save file…").clicked() {
                match self.read(&p.file, usize::MAX) {
                    Ok(bytes) => files::save(&p.file.name, bytes),
                    Err(e) => files::log(&format!("{}: {e}", p.file.name)),
                }
            }
        });
        if p.file.size as usize > PREVIEW {
            ui.label(
                RichText::new(format!("the first {} shown", size_text(PREVIEW as u32))).weak(),
            );
        }
        let lines = match &p.lines {
            Ok(l) => l,
            Err(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
        };
        let font = egui::TextStyle::Monospace;
        let row = ui.text_style_height(&font);
        egui::ScrollArea::both()
            .id_salt("preview")
            .auto_shrink(false)
            .show_rows(ui, row, lines.len(), |ui, rows| {
                for line in &lines[rows] {
                    let text = RichText::new(line).text_style(font.clone());
                    let text = if p.hex {
                        text.color(Color32::GRAY)
                    } else {
                        text
                    };
                    ui.add(egui::Label::new(text).extend());
                }
            });
    }
}

/// Text if it is UTF-8 without control characters other than whitespace.
fn is_text(b: &[u8]) -> bool {
    // A preview may cut a character in half at the end.
    let s = match std::str::from_utf8(b) {
        Ok(s) => s,
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&b[..e.valid_up_to()]).unwrap(),
        Err(_) => return false,
    };
    s.chars()
        .all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
}

fn text_lines(b: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(b)
        .lines()
        .map(|l| l.replace('\t', "    "))
        .collect()
}

/// `offset  hex bytes  |ascii|`, sixteen bytes a line.
fn hex_lines(b: &[u8]) -> Vec<String> {
    b.chunks(HEX_ROW)
        .enumerate()
        .map(|(i, row)| {
            let hex: String = (0..HEX_ROW)
                .map(|j| match row.get(j) {
                    Some(v) => format!("{v:02x}{}", if j == 7 { "  " } else { " " }),
                    None => if j == 7 { "    " } else { "   " }.to_string(),
                })
                .collect();
            let ascii: String = row
                .iter()
                .map(|&c| {
                    if c.is_ascii_graphic() || c == b' ' {
                        c as char
                    } else {
                        '.'
                    }
                })
                .collect();
            format!("{:08x}  {hex} |{ascii}|", i * HEX_ROW)
        })
        .collect()
}

fn size_text(n: u32) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1048576 => format!("{:.1} KiB", n as f64 / 1024.0),
        _ => format!("{:.1} MiB", n as f64 / 1048576.0),
    }
}

fn date_text(d: Option<[u16; 6]>) -> String {
    d.map_or_else(String::new, |[y, mo, d, h, mi, _]| {
        format!("{y}-{mo:02}-{d:02} {h:02}:{mi:02}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_or_hex() {
        assert!(is_text(b"hello\r\n\tworld"));
        assert!(is_text("caf\u{e9}".as_bytes()));
        assert!(is_text(&"caf\u{e9}".as_bytes()[..4]), "cut mid-character");
        assert!(!is_text(&[0x00, 0x01]));
        assert!(!is_text(&[0xFF, b'a', b'b']));
        assert_eq!(text_lines(b"a\tb\nc"), ["a    b", "c"]);
        let lines = hex_lines(&(0..20).collect::<Vec<u8>>());
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "00000000  00 01 02 03 04 05 06 07  08 09 0a 0b 0c 0d 0e 0f  |................|"
        );
        assert!(lines[1].starts_with("00000010  10 11 12 13    "));
        assert!(lines[1].ends_with("|....|"));
        assert_eq!(lines[0].find('|'), lines[1].find('|'), "columns line up");
    }

    #[test]
    fn browses_a_card_and_keeps_its_place_on_refresh() {
        let image =
            pcb_devices::fat::blank_fat16_image(8, "CARD", &["SAMPLES", "PATTERNS"]).unwrap();
        let mut b = SdBrowser::new("sd".into(), image.clone(), 1.5, 0);
        assert_eq!(b.volume, Ok("\"CARD\", FAT16".to_string()));
        let names = |b: &SdBrowser| -> Vec<String> {
            b.listing
                .as_ref()
                .unwrap()
                .iter()
                .map(|e| e.name.clone())
                .collect()
        };
        assert_eq!(names(&b), ["PATTERNS", "SAMPLES"], "sorted");
        let samples = b.find("SAMPLES", true).unwrap();
        b.enter(samples);
        assert!(names(&b).is_empty());
        b.refresh(image, 2.0, 3);
        assert_eq!((b.path.len(), b.written_then), (1, 3));
        b.up_to(0);
        assert_eq!(names(&b).len(), 2);
        // A card with no file system says so.
        let b = SdBrowser::new("sd".into(), vec![0; 1 << 20], 0.0, 0);
        assert!(b.volume.is_err());
    }

    #[test]
    fn sizes_and_dates_read_well() {
        assert_eq!(size_text(512), "512 B");
        assert_eq!(size_text(20_000), "19.5 KiB");
        assert_eq!(size_text(3 << 20), "3.0 MiB");
        assert_eq!(date_text(Some([2026, 10, 29, 9, 5, 0])), "2026-10-29 09:05");
        assert_eq!(date_text(None), "");
    }
}
