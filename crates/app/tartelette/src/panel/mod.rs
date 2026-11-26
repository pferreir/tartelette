//! The front panel: board parts as interface elements on a grid.
//!
//! The grid comes from the board file's `panel` lines; without any, every
//! part is packed onto a grid automatically. Each device kind has one widget,
//! and a few widget options (from the `panel` line) adjust it:
//!
//! | kind       | widget                          | options                         |
//! |------------|---------------------------------|---------------------------------|
//! | `st7789`   | the screen                      | `rotate=0\|90\|180\|270`        |
//! | `ssd1306`  | the OLED screen                 | `rotate=0\|90\|180\|270`        |
//! | `led`      | a lamp                          | `color=red\|green\|...\|rrggbb` |
//! | `ws2812`   | a row/grid of RGB lamps         |                                 |
//! | `button`   | push button (held while down)   | `key=Space` (egui key name)     |
//! | `encoder`  | knob: drag, scroll or keys      | `keys=Left,Right`               |
//! | `mcp23008` | pin switches with levels        | see below                       |
//! | `dac8165`  | four voltage meters (at the jacks) |                              |
//! | `sdcard`   | card status, load / save / browse image |                         |
//! | `cyw43439` | Wi-Fi: LED, link, address, errors |                               |
//! | `pot`      | knob: drag or scroll, shows volts |                               |
//! | `wav`      | player: load, rewind / play / stop, position |                    |
//! | `speaker`  | icon: click to mute, scroll for volume |                         |
//! | `midi`     | where it goes, a light for bytes on its pin |                    |
//! | `pcm5102`  | sample rate, a level per channel  |                               |
//! | `ps2mouse` | pad: click to capture the pointer, Escape to let go |              |
//!
//! An `mcp23008` item shows a cell per pin. By default each cell follows what
//! the board file wires to its pin: an LED is shown lit or dark, a switch is
//! pressed while the cell is held (a latching one flips on each click) and
//! shown lit while actuated, and a bare pin is pulled low while held and
//! shown lit while high. Options:
//!
//! - `pins = [4, 5, 6, 7]`: which pins, in that order (default all eight);
//! - `mode = "hold"|"toggle"|"view"`: treat every pin as a bare one that a
//!   cell drives while held, flips on each click, or only shows, whatever is
//!   wired to it;
//! - `active = "low"|"high"`: the level a cell drives a bare pin to (default
//!   low: a switch to ground); a released pin floats;
//! - `keys = ["1", "2", ...]`: a keyboard key per pin, working like a click;
//! - `labels = ["A", "B", ...]`: text per cell instead of the pin's name or
//!   number.
//!
//! A part can have several items, so inputs and outputs of one expander can
//! sit apart with their own settings.
//!
//! Keyboard keys show as keycaps: a button's or encoder's in its title row
//! (or under the widget when the title leaves no room), an expander pin's in
//! its cell's corner.
//!
//! Each widget lives in [`widgets`]; [`layout`] places the items on the grid.
use std::collections::HashMap;

use egui::{
    Align2, Color32, FontId, Key, Pos2, Rect, Sense, StrokeKind, TextureHandle, Ui, Vec2, pos2,
    vec2,
};
use pcb::BoardEvent;
use tartelette_runtime::Session;

mod grab;
pub mod layout;
mod widgets;

pub use layout::{Expander, Item, Layout, PinMode, Widget};
use widgets::*;

/// What the panel wants done after drawing (the session is borrowed while
/// it draws).
pub enum Request {
    Input(String, String, i64),
    LoadSd(String),
    SaveSd(String),
    LoadWav(String),
    BrowseSd(String),
}

/// UI-side state of the panel for one run.
#[derive(Default)]
pub struct PanelState {
    /// Screen textures and the (pixels written, showing) they were made from.
    screens: HashMap<usize, (TextureHandle, (u64, bool))>,
    /// Direction of each encoder's last detent (+1 clockwise) and when it
    /// came (UI time), and the pointer travel not yet turned into a detent
    /// (or, for a mouse pad, the scroll not yet a wheel notch).
    turned: HashMap<usize, (i64, f64)>,
    travel: HashMap<usize, f32>,
    /// Mouse motion captured by each pad not yet sent as counts.
    motion: HashMap<usize, Vec2>,
    /// Expander pins the panel is driving.
    driven: HashMap<usize, u8>,
    /// What each Wi-Fi chip last reported.
    wifi: HashMap<usize, WifiNotes>,
}

/// Things the Wi-Fi panel remembers from the chip's events.
#[derive(Default, Clone, Copy)]
struct WifiNotes {
    /// Networks the last scan found.
    scanned: Option<i64>,
    /// The last join attempt failed.
    join_failed: bool,
    /// A scan or join was tried with no network behind the chip.
    unavailable: bool,
    /// The firmware scanned, connected or advertised with no Bluetooth
    /// radio behind the chip.
    bt_unavailable: bool,
}

impl PanelState {
    /// Follow what happened on the board, at UI time `now` (seconds).
    pub fn absorb(&mut self, events: &[BoardEvent], now: f64) {
        for e in events {
            match e.label {
                "detent" => {
                    self.turned.insert(e.part, (e.value.signum(), now));
                }
                "scan" => self.wifi.entry(e.part).or_default().scanned = Some(e.value),
                "join" => self.wifi.entry(e.part).or_default().join_failed = e.value == 0,
                "unavailable" => self.wifi.entry(e.part).or_default().unavailable = true,
                "bt_unavailable" => self.wifi.entry(e.part).or_default().bt_unavailable = true,
                _ => {}
            }
        }
    }
}

/// The smallest a grid cell gets: below this the widgets' text and buttons
/// stop fitting, so the panel scrolls instead of shrinking further.
pub const MIN_CELL: f32 = 84.0;
/// Margins around the grid inside the central panel.
const PAD: f32 = 16.0;

impl Layout {
    /// The space the board needs to show every cell at [`MIN_CELL`].
    pub fn min_size(&self) -> Vec2 {
        vec2(self.cols as f32, self.rows as f32) * MIN_CELL + Vec2::splat(PAD)
    }
}
const MAX_CELL: f32 = 220.0;

/// Draw the panel filling `ui`'s available space; returns what the user did.
pub fn show(ui: &mut Ui, layout: &Layout, st: &mut PanelState, s: &Session) -> Vec<Request> {
    let mut out = Vec::new();
    let gap = 8.0;
    let avail = ui.available_size();
    let cell = ((avail.x + gap) / layout.cols as f32)
        .min((avail.y + gap) / layout.rows as f32)
        .clamp(MIN_CELL, MAX_CELL);
    let size = vec2(
        cell * layout.cols as f32 - gap,
        cell * layout.rows as f32 - gap,
    );
    ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        let (area, _) = ui.allocate_exact_size(size.max(vec2(avail.x, size.y)), Sense::hover());
        let origin = pos2(area.center().x - size.x / 2.0, area.top());
        let keys_free = !ui.ctx().egui_wants_keyboard_input();
        for it in &layout.items {
            let min = origin + vec2(it.cell.0 as f32, it.cell.1 as f32) * cell;
            let rect = Rect::from_min_size(
                min,
                vec2(it.span.0 as f32, it.span.1 as f32) * cell - Vec2::splat(gap),
            );
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 8.0, ui.visuals().faint_bg_color);
            painter.rect_stroke(
                rect,
                8.0,
                ui.visuals().widgets.noninteractive.bg_stroke,
                StrokeKind::Inside,
            );
            painter.text(
                rect.left_top() + vec2(8.0, 5.0),
                Align2::LEFT_TOP,
                &it.label,
                FontId::proportional(12.0),
                ui.visuals().weak_text_color(),
            );
            let body = Rect::from_min_max(rect.min + vec2(6.0, 22.0), rect.max - vec2(6.0, 6.0));
            let ctx = Ctx {
                ui,
                body,
                it,
                s,
                keys_free,
            };
            match &it.widget {
                &Widget::Screen { rotate } => screen(ctx, st, rotate),
                &Widget::Led { color } => led(ctx, color),
                Widget::Leds => leds(ctx),
                &Widget::Button { key } => button(ctx, key, &mut out),
                &Widget::Encoder { keys } => encoder(ctx, st, keys, &mut out),
                Widget::Expander(x) => expander(ctx, st, x, &mut out),
                Widget::Dac => dac(ctx),
                Widget::SdCard => sdcard(ctx, &mut out),
                Widget::Wifi => wifi(ctx, st),
                Widget::Pot => pot(ctx, &mut out),
                Widget::Wav => wav(ctx, &mut out),
                Widget::Speaker => speaker(ctx, &mut out),
                &Widget::Midi { keyboard, channel } => midi(ctx, keyboard, channel, &mut out),
                Widget::Pcm5102 => pcm5102(ctx),
                Widget::Mouse => mouse(ctx, st, &mut out),
                Widget::Other => {
                    let kind = &s.sys.board.parts[it.part].kind;
                    ctx.ui.painter_at(body).text(
                        body.center(),
                        Align2::CENTER_CENTER,
                        kind,
                        FontId::monospace(12.0),
                        ctx.ui.visuals().weak_text_color(),
                    );
                }
            }
        }
    });
    out
}

/// After the panel is drawn, or wasn't: give back a captured pointer that no
/// mouse pad holds any more.
pub fn end_frame(ctx: &egui::Context) {
    grab::end_frame(ctx);
}

/// What every widget gets: where to draw, which part, the run.
struct Ctx<'a> {
    ui: &'a mut Ui,
    body: Rect,
    it: &'a Item,
    s: &'a Session,
    keys_free: bool,
}

impl Ctx<'_> {
    fn device<T: pcb::Device>(&self) -> Option<&T> {
        self.s.sys.board.device::<T>(&self.it.name)
    }
    fn id(&self, salt: impl std::hash::Hash + std::fmt::Debug) -> egui::Id {
        egui::Id::unique(("panel", self.it.part, salt))
    }
    fn input(&self, out: &mut Vec<Request>, action: &str, value: i64) {
        out.push(Request::Input(self.it.name.clone(), action.into(), value));
    }
}

/// The text on a key's cap: arrows as arrows, everything else by name.
fn cap_text(k: Key) -> &'static str {
    match k {
        Key::ArrowLeft => "←",
        Key::ArrowRight => "→",
        Key::ArrowUp => "↑",
        Key::ArrowDown => "↓",
        _ => k.symbol_or_name(),
    }
}

/// Keycaps for `keys` in a row at `size`, laid out but not yet drawn.
struct Keycaps {
    caps: Vec<(std::sync::Arc<egui::Galley>, Vec2)>,
    gap: f32,
}

impl Keycaps {
    fn new(ui: &Ui, keys: &[Key], size: f32) -> Self {
        let pad = vec2(size * 0.35, size * 0.15);
        let caps = keys
            .iter()
            .map(|&k| {
                let g = ui.painter().layout_no_wrap(
                    cap_text(k).into(),
                    FontId::monospace(size),
                    ui.visuals().text_color(),
                );
                let s = vec2(g.size().x.max(g.size().y * 0.6), g.size().y) + pad * 2.0;
                (g, s)
            })
            .collect();
        Keycaps {
            caps,
            gap: size * 0.3,
        }
    }

    fn size(&self) -> Vec2 {
        let w: f32 = self.caps.iter().map(|(_, s)| s.x).sum();
        let h = self.caps.iter().map(|(_, s)| s.y).fold(0.0, f32::max);
        vec2(w + self.gap * self.caps.len().saturating_sub(1) as f32, h)
    }

    /// Draw the row with its right end at `right`, centred vertically on it.
    fn draw(self, ui: &Ui, right: Pos2) {
        let v = ui.visuals();
        let p = ui.painter();
        let mut x = right.x - self.size().x;
        for (g, s) in self.caps {
            let cap = Rect::from_min_size(pos2(x, right.y - s.y / 2.0), s);
            p.rect_filled(cap, 3.0, v.extreme_bg_color);
            p.rect_stroke(
                cap,
                3.0,
                v.widgets.noninteractive.bg_stroke,
                StrokeKind::Inside,
            );
            p.galley(cap.center() - g.size() / 2.0, g, v.text_color());
            x += s.x + self.gap;
        }
    }
}

impl Ctx<'_> {
    /// Show `keys` in the item's title row, at its right end; or, when the
    /// title leaves no room, centred along the bottom of the body. Returns
    /// the body left for the widget.
    fn show_keys(&self, keys: &[Key]) -> Rect {
        if keys.is_empty() {
            return self.body;
        }
        let caps = Keycaps::new(self.ui, keys, 11.0);
        let title = self
            .ui
            .painter()
            .layout_no_wrap(
                self.it.label.clone(),
                FontId::proportional(12.0),
                Color32::WHITE,
            )
            .size()
            .x;
        // The title starts 2 points left of the body; keep 6 points between.
        let free = self.body.width() - 2.0 - 2.0 - title - 6.0;
        if caps.size().x <= free {
            caps.draw(
                self.ui,
                pos2(self.body.right() - 2.0, self.body.top() - 11.0),
            );
            self.body
        } else {
            let size = caps.size();
            let bottom = self.body.center_bottom();
            caps.draw(self.ui, bottom + vec2(size.x / 2.0, -size.y / 2.0));
            let mut rest = self.body;
            rest.max.y -= size.y + 4.0;
            rest
        }
    }
}

#[cfg(test)]
fn test_board(text: &str) -> pcb::Board {
    pcb::Board::parse_with(text, &pcb_devices::registry()).unwrap()
}
