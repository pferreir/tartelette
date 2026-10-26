//! The front panel: board parts as interface elements on a grid.
//!
//! The grid comes from the board file's `panel` lines; without any, every
//! part is packed onto a grid automatically. Each device kind has one widget,
//! and a few widget options (from the `panel` line) adjust it:
//!
//! | kind       | widget                          | options                         |
//! |------------|---------------------------------|---------------------------------|
//! | `st7789`   | the screen                      | `rotate=0\|90\|180\|270`        |
//! | `led`      | a lamp                          | `color=red\|green\|...\|rrggbb` |
//! | `ws2812`   | a row/grid of RGB lamps         |                                 |
//! | `button`   | push button (held while down)   | `key=Space` (egui key name)     |
//! | `encoder`  | knob: drag, scroll or keys      | `keys=Left,Right`               |
//! | `mcp23008` | eight pin switches with levels  | `mode=hold\|toggle` `active=low\|high` |
//! | `dac8165`  | four voltage meters             |                                 |
//! | `sdcard`   | card status, load / save image  |                                 |
//! | `cyw43439` | Wi-Fi: LED, link, address, errors |                               |
use crate::session::Session;
use egui::{Align2, Color32, FontId, Key, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2, pos2, vec2};
use pcb::{Board, BoardEvent};
use pcb_devices::{button::Button, cyw43439::{Cyw43439, Link}, dac8165::Dac8165, led::Led, mcp23008::Mcp23008, sdcard::SdCard, st7789::St7789, ws2812::Ws2812};
use std::collections::HashMap;

/// Detents per knob revolution, for turning by dragging around the knob.
const DETENTS_PER_REV: f32 = 20.0;
/// How long (seconds) the knob shows the direction of its last detent.
const TURN_SHOWN: f64 = 0.5;
/// Smooth (touchpad) scroll travel in points per encoder detent; a mouse
/// wheel notch is one detent.
const SCROLL_PER_DETENT: f32 = 40.0;

pub struct Layout {
    pub items: Vec<Item>,
    pub cols: u16,
    pub rows: u16,
}

pub struct Item {
    pub part: usize,
    pub name: String,
    pub label: String,
    pub cell: (u16, u16),
    pub span: (u16, u16),
    pub widget: Widget,
}

#[derive(Clone, Copy)]
pub enum Widget {
    Screen { rotate: u16 },
    Led { color: Color32 },
    Leds,
    Button { key: Option<Key> },
    Encoder { keys: Option<(Key, Key)> },
    Expander { toggle: bool, active_high: bool },
    Dac,
    SdCard,
    Wifi,
    /// A kind this app has no widget for.
    Other,
}

/// What the panel wants done after drawing (the session is borrowed while
/// it draws).
pub enum Request {
    Input(String, String, i64),
    LoadSd(String),
    SaveSd(String),
}

impl Layout {
    /// The board's panel layout, or an automatic one if it has no `panel`
    /// lines. Returns messages for widget options that were ignored.
    pub fn new(board: &Board) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut items = Vec::new();
        if board.panel.is_empty() {
            let mut placed: Vec<(u16, u16, u16, u16)> = Vec::new();
            let width = board.parts.iter().map(|p| auto_span(board, p).0).max().unwrap_or(1).max(6);
            for (i, p) in board.parts.iter().enumerate() {
                let span = auto_span(board, p);
                let span = (span.0.min(width), span.1);
                let cell = first_fit(&placed, span, width);
                placed.push((cell.0, cell.1, span.0, span.1));
                let widget = widget_for(&p.kind, &mut std::iter::empty()).unwrap_or(Widget::Other);
                items.push(Item { part: i, name: p.name.clone(), label: p.name.clone(), cell, span, widget });
            }
        } else {
            for it in &board.panel {
                let p = &board.parts[it.part];
                let mut opts = it.options.iter().map(|(k, v)| (k.as_str(), v.as_str()));
                let widget = widget_for(&p.kind, &mut opts).unwrap_or_else(|e| {
                    warnings.push(format!("line {}: {e} (ignored)", it.line));
                    widget_for(&p.kind, &mut std::iter::empty()).unwrap_or(Widget::Other)
                });
                let label = it.label.clone().unwrap_or_else(|| p.name.clone());
                items.push(Item { part: it.part, name: p.name.clone(), label, cell: (it.col, it.row), span: (it.cols, it.rows), widget });
            }
        }
        let cols = items.iter().map(|i| i.cell.0 + i.span.0).max().unwrap_or(1);
        let rows = items.iter().map(|i| i.cell.1 + i.span.1).max().unwrap_or(1);
        (Self { items, cols, rows }, warnings)
    }
}

/// Grid size a part gets in an automatic layout.
fn auto_span(board: &Board, part: &pcb::Part) -> (u16, u16) {
    match part.kind.as_str() {
        "st7789" => {
            let (w, h) = board.device::<St7789>(&part.name).map_or((240, 320), St7789::size);
            if w > h { (4, 3) } else { (3, 4) }
        }
        "ws2812" | "mcp23008" => (4, 1),
        "dac8165" => (2, 2),
        "sdcard" => (2, 1),
        "cyw43439" => (3, 2),
        _ => (1, 1),
    }
}

/// Top-most, then left-most free spot for `span` in a grid `width` wide.
fn first_fit(placed: &[(u16, u16, u16, u16)], span: (u16, u16), width: u16) -> (u16, u16) {
    let free = |c: u16, r: u16| placed.iter().all(|&(pc, pr, pw, ph)| c + span.0 <= pc || pc + pw <= c || r + span.1 <= pr || pr + ph <= r);
    for r in 0.. {
        for c in 0..=width - span.0 {
            if free(c, r) {
                return (c, r);
            }
        }
    }
    unreachable!()
}

/// The widget for a part `kind` with `options` from its panel line.
fn widget_for<'a>(kind: &str, options: &mut dyn Iterator<Item = (&'a str, &'a str)>) -> Result<Widget, String> {
    let mut w = match kind {
        "st7789" => Widget::Screen { rotate: 0 },
        "led" => Widget::Led { color: Color32::from_rgb(255, 48, 32) },
        "ws2812" => Widget::Leds,
        "button" => Widget::Button { key: None },
        "encoder" => Widget::Encoder { keys: None },
        "mcp23008" => Widget::Expander { toggle: false, active_high: false },
        "dac8165" => Widget::Dac,
        "sdcard" => Widget::SdCard,
        "cyw43439" => Widget::Wifi,
        _ => Widget::Other,
    };
    let key = |v: &str| Key::from_name(v).ok_or_else(|| format!("unknown key {v:?}"));
    for (k, v) in options {
        match (&mut w, k) {
            (Widget::Screen { rotate }, "rotate") => {
                *rotate = match v {
                    "0" | "90" | "180" | "270" => v.parse().unwrap(),
                    _ => return Err("rotate must be 0, 90, 180 or 270".into()),
                }
            }
            (Widget::Led { color }, "color") => *color = parse_color(v).ok_or_else(|| format!("unknown color {v:?}"))?,
            (Widget::Button { key: k }, "key") => *k = Some(key(v)?),
            (Widget::Encoder { keys }, "keys") => {
                let (a, b) = v.split_once(',').ok_or("keys must be CCW,CW")?;
                *keys = Some((key(a)?, key(b)?));
            }
            (Widget::Expander { toggle, .. }, "mode") => {
                *toggle = match v {
                    "hold" => false,
                    "toggle" => true,
                    _ => return Err("mode must be hold or toggle".into()),
                }
            }
            (Widget::Expander { active_high, .. }, "active") => {
                *active_high = match v {
                    "low" => false,
                    "high" => true,
                    _ => return Err("active must be low or high".into()),
                }
            }
            _ => return Err(format!("{kind} has no panel option {k:?}")),
        }
    }
    Ok(w)
}

fn parse_color(v: &str) -> Option<Color32> {
    Some(match v {
        "red" => Color32::from_rgb(255, 48, 32),
        "green" => Color32::from_rgb(40, 230, 70),
        "blue" => Color32::from_rgb(40, 110, 255),
        "yellow" => Color32::from_rgb(255, 220, 40),
        "orange" => Color32::from_rgb(255, 140, 20),
        "white" => Color32::from_rgb(240, 240, 255),
        _ => {
            // Hex without a '#', which would start a board-file comment.
            let h = Some(v).filter(|h| h.len() == 6)?;
            let n = u32::from_str_radix(h, 16).ok()?;
            Color32::from_rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)
        }
    })
}

/// UI-side state of the panel for one run.
#[derive(Default)]
pub struct PanelState {
    /// Screen textures and the (pixels written, showing) they were made from.
    screens: HashMap<usize, (TextureHandle, (u64, bool))>,
    /// Direction of each encoder's last detent (+1 clockwise) and when it
    /// came (UI time), and the pointer travel not yet turned into a detent.
    turned: HashMap<usize, (i64, f64)>,
    travel: HashMap<usize, f32>,
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

/// Draw the panel filling `ui`'s available space; returns what the user did.
pub fn show(ui: &mut Ui, layout: &Layout, st: &mut PanelState, s: &Session) -> Vec<Request> {
    let mut out = Vec::new();
    let gap = 8.0;
    let avail = ui.available_size();
    let cell = ((avail.x + gap) / layout.cols as f32).min((avail.y + gap) / layout.rows as f32).clamp(40.0, 220.0);
    let size = vec2(cell * layout.cols as f32 - gap, cell * layout.rows as f32 - gap);
    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        let (area, _) = ui.allocate_exact_size(size.max(vec2(avail.x, size.y)), Sense::hover());
        let origin = pos2(area.center().x - size.x / 2.0, area.top());
        let keys_free = !ui.ctx().egui_wants_keyboard_input();
        for it in &layout.items {
            let min = origin + vec2(it.cell.0 as f32, it.cell.1 as f32) * cell;
            let rect = Rect::from_min_size(min, vec2(it.span.0 as f32, it.span.1 as f32) * cell - Vec2::splat(gap));
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 8.0, ui.visuals().faint_bg_color);
            painter.rect_stroke(rect, 8.0, ui.visuals().widgets.noninteractive.bg_stroke, StrokeKind::Inside);
            painter.text(rect.left_top() + vec2(8.0, 5.0), Align2::LEFT_TOP, &it.label, FontId::proportional(12.0), ui.visuals().weak_text_color());
            let body = Rect::from_min_max(rect.min + vec2(6.0, 22.0), rect.max - vec2(6.0, 6.0));
            let ctx = Ctx { ui, body, it, s, keys_free };
            match it.widget {
                Widget::Screen { rotate } => screen(ctx, st, rotate),
                Widget::Led { color } => led(ctx, color),
                Widget::Leds => leds(ctx),
                Widget::Button { key } => button(ctx, key, &mut out),
                Widget::Encoder { keys } => encoder(ctx, st, keys, &mut out),
                Widget::Expander { toggle, active_high } => expander(ctx, st, toggle, active_high, &mut out),
                Widget::Dac => dac(ctx),
                Widget::SdCard => sdcard(ctx, &mut out),
                Widget::Wifi => wifi(ctx, st),
                Widget::Other => {
                    let kind = &s.sys.board.parts[it.part].kind;
                    ctx.ui.painter_at(body).text(body.center(), Align2::CENTER_CENTER, kind, FontId::monospace(12.0), ctx.ui.visuals().weak_text_color());
                }
            }
        }
    });
    out
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

/// An image of `px` texels fitted inside `outer`, centred: a whole number of
/// physical pixels per texel when it fits at all, on the pixel grid.
fn fit_pixels(outer: Rect, px: Vec2, pixels_per_point: f32) -> Rect {
    let k = (outer.width() / px.x).min(outer.height() / px.y) * pixels_per_point;
    let k = if k >= 1.0 { k.floor() } else { k };
    let size = px * k / pixels_per_point;
    let min = ((outer.center() - size / 2.0) * pixels_per_point).round() / pixels_per_point;
    Rect::from_min_size(min, size)
}

fn screen(c: Ctx, st: &mut PanelState, rotate: u16) {
    let Some(lcd) = c.device::<St7789>() else { return };
    let (w, h) = lcd.size();
    let stamp = (lcd.total_pixels, lcd.is_showing());
    let (tw, th) = if rotate.is_multiple_of(180) { (w, h) } else { (h, w) };
    let make = || {
        let mut dst = vec![Color32::BLACK; w * h];
        let (mut x, mut y) = (0, 0);
        lcd.for_each_pixel(|_, p| {
            let (dx, dy) = match rotate {
                90 => (h - 1 - y, x),
                180 => (w - 1 - x, h - 1 - y),
                270 => (y, w - 1 - x),
                _ => (x, y),
            };
            dst[dy * tw + dx] = Color32::from_rgb(p[0], p[1], p[2]);
            x += 1;
            if x == w {
                x = 0;
                y += 1;
            }
        });
        egui::ColorImage { size: [tw, th], source_size: vec2(tw as f32, th as f32), pixels: dst }
    };
    let tex = match st.screens.get_mut(&c.it.part) {
        Some((tex, seen)) => {
            if *seen != stamp {
                tex.set(make(), TextureOptions::NEAREST);
                *seen = stamp;
            }
            tex.id()
        }
        None => {
            let tex = c.ui.ctx().load_texture(format!("screen-{}", c.it.name), make(), TextureOptions::NEAREST);
            let id = tex.id();
            st.screens.insert(c.it.part, (tex, stamp));
            id
        }
    };
    let r = fit_pixels(c.body, vec2(tw as f32, th as f32), c.ui.ctx().pixels_per_point());
    let p = c.ui.painter_at(c.body);
    p.image(tex, r, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
    p.rect_stroke(r, 0.0, Stroke::new(1.0, Color32::from_gray(60)), StrokeKind::Outside);
}

/// A round lamp of `color` at `center`, lit or dark.
fn lamp(ui: &Ui, center: Pos2, radius: f32, color: Color32, lit: bool) {
    let p = ui.painter();
    if lit {
        p.circle_filled(center, radius * 1.5, color.gamma_multiply(0.18));
        p.circle_filled(center, radius, color);
        p.circle_filled(center - Vec2::splat(radius * 0.3), radius * 0.3, Color32::from_white_alpha(90));
    } else {
        let dim = |v: u8| (v as f32 * 0.2) as u8;
        p.circle_filled(center, radius, Color32::from_rgb(dim(color.r()), dim(color.g()), dim(color.b())));
    }
    p.circle_stroke(center, radius, Stroke::new(1.0, Color32::from_black_alpha(120)));
}

fn led(c: Ctx, color: Color32) {
    let Some(l) = c.device::<Led>() else { return };
    let r = (c.body.width().min(c.body.height()) * 0.32).max(4.0);
    lamp(c.ui, c.body.center(), r, color, l.lit);
}

/// Best grid (columns, rows) for `n` equal circles in `size`.
fn circle_grid(n: usize, size: Vec2) -> (usize, usize, f32) {
    (1..=n.max(1))
        .map(|cols| {
            let rows = n.div_ceil(cols).max(1);
            (cols, rows, (size.x / cols as f32).min(size.y / rows as f32))
        })
        .max_by(|a, b| a.2.total_cmp(&b.2))
        .unwrap()
}

fn leds(c: Ctx) {
    let Some(chain) = c.device::<Ws2812>() else { return };
    let colours = chain.leds();
    let (cols, rows, d) = circle_grid(colours.len(), c.body.size());
    let start = c.body.center() - vec2(cols as f32, rows as f32) * d / 2.0;
    for (i, rgb) in colours.iter().enumerate() {
        let center = start + vec2((i % cols) as f32 + 0.5, (i / cols) as f32 + 0.5) * d;
        // Real LEDs are bright at low codes: lift the levels so dim colours show.
        let lift = |v: u8| ((v as f32 / 255.0).powf(1.0 / 2.2) * 255.0) as u8;
        let on = rgb.iter().any(|&v| v > 0);
        let color = if on { Color32::from_rgb(lift(rgb[0]), lift(rgb[1]), lift(rgb[2])) } else { Color32::from_gray(70) };
        lamp(c.ui, center, d * 0.36, color, on);
    }
}

fn button(c: Ctx, key: Option<Key>, out: &mut Vec<Request>) {
    let Some(b) = c.device::<Button>() else { return };
    let side = c.body.width().min(c.body.height()) * 0.8;
    let rect = Rect::from_center_size(c.body.center(), Vec2::splat(side));
    let mut resp = c.ui.interact(rect, c.id("button"), Sense::click_and_drag());
    if let Some(k) = key {
        resp = resp.on_hover_text(format!("hold, or {}", k.name()));
    }
    let down = resp.is_pointer_button_down_on() || (c.keys_free && key.is_some_and(|k| c.ui.input(|i| i.key_down(k))));
    if down != b.pressed {
        c.input(out, if down { "press" } else { "release" }, 0);
    }
    let v = c.ui.visuals();
    let fill = if b.pressed { v.selection.bg_fill } else if resp.hovered() { v.widgets.hovered.bg_fill } else { v.widgets.inactive.bg_fill };
    let p = c.ui.painter();
    p.circle_filled(rect.center(), side / 2.0, fill);
    p.circle_stroke(rect.center(), side / 2.0, v.widgets.inactive.fg_stroke);
}

fn encoder(c: Ctx, st: &mut PanelState, keys: Option<(Key, Key)>, out: &mut Vec<Request>) {
    let side = c.body.width().min(c.body.height()) * 0.9;
    let rect = Rect::from_center_size(c.body.center(), Vec2::splat(side));
    let resp = c.ui.interact(rect, c.id("knob"), Sense::drag()).on_hover_text("drag around or scroll to turn");
    let center = rect.center();
    let travel = st.travel.entry(c.it.part).or_default();
    if resp.drag_started() {
        *travel = 0.0;
    }
    // Dragging turns the knob by the pointer's angle around its centre, like
    // a finger on the real one (ignored right at the centre, where it jumps).
    if resp.dragged()
        && let Some(now) = resp.interact_pointer_pos()
    {
        // The first drag frame counts from the press, not the last move.
        let was = if resp.drag_started() { c.ui.input(|i| i.pointer.press_origin()) } else { None };
        let (a, b) = (was.unwrap_or(now - resp.drag_delta()) - center, now - center);
        if a.length() > side * 0.1 && b.length() > side * 0.1 {
            let turned = (a.x * b.y - a.y * b.x).atan2(a.dot(b));
            *travel += turned / std::f32::consts::TAU * DETENTS_PER_REV;
        }
    }
    if resp.hovered() {
        // Raw wheel events, so one notch is one detent; and swallow the
        // scroll so the panel's scroll area doesn't move under the knob.
        c.ui.input_mut(|i| {
            for e in &i.events {
                if let egui::Event::MouseWheel { unit, delta, .. } = e {
                    *travel += match unit {
                        egui::MouseWheelUnit::Point => delta.y / SCROLL_PER_DETENT,
                        _ => delta.y.signum(),
                    };
                }
            }
            i.smooth_scroll_delta = Vec2::ZERO;
        });
    }
    // Detents sit at whole travel values and click over half way between,
    // so turning back undoes a detent as readily as it made it.
    let mut turn = travel.round() as i64;
    *travel -= turn as f32;
    if let Some((ccw, cw)) = keys.filter(|_| c.keys_free) {
        turn += c.ui.input(|i| i.key_pressed(cw) as i64 - i.key_pressed(ccw) as i64);
    }
    if turn != 0 {
        c.input(out, "turn", turn);
    }
    let v = c.ui.visuals();
    let p = c.ui.painter();
    let fill = if resp.dragged() { v.widgets.active.bg_fill } else if resp.hovered() { v.widgets.hovered.bg_fill } else { v.widgets.inactive.bg_fill };
    p.circle_filled(center, side / 2.0, fill);
    p.circle_stroke(center, side / 2.0, v.widgets.inactive.fg_stroke);
    // An encoder has no position, only steps: show which way the last one
    // went, fading out.
    if let Some(&(dir, at)) = st.turned.get(&c.it.part) {
        let fade = 1.0 - ((c.ui.input(|i| i.time) - at) / TURN_SHOWN) as f32;
        if fade > 0.0 {
            turn_arrow(p, center, side * 0.3, dir as f32, v.strong_text_color().gamma_multiply(fade));
        }
    }
}

/// A curved arrow over the top of a knob, pointing clockwise when `dir` > 0.
fn turn_arrow(p: &egui::Painter, center: Pos2, radius: f32, dir: f32, color: Color32) {
    let at = |a: f32, r: f32| center + vec2(a.sin(), -a.cos()) * r;
    let (from, to) = (-dir, dir); // one radian either side of the top
    let arc: Vec<Pos2> = (0..=16).map(|i| at(from + (to - from) * i as f32 / 16.0, radius)).collect();
    let width = radius * 0.16;
    p.add(egui::Shape::line(arc, Stroke::new(width, color)));
    let head = width * 2.2;
    let tip = at(to + dir * head / radius, radius);
    let back = to - dir * 0.15;
    p.add(egui::Shape::convex_polygon(vec![tip, at(back, radius + head * 0.7), at(back, radius - head * 0.7)], color, Stroke::NONE));
}

fn expander(c: Ctx, st: &mut PanelState, toggle: bool, active_high: bool, out: &mut Vec<Request>) {
    let Some(x) = c.device::<Mcp23008>() else { return };
    let levels = x.pin_levels();
    let driven = st.driven.entry(c.it.part).or_default();
    let (cols, _, d) = circle_grid(8, c.body.size());
    let rows = 8usize.div_ceil(cols);
    let start = c.body.center() - vec2(cols as f32, rows as f32) * d / 2.0;
    for pin in 0..8 {
        let cell = Rect::from_min_size(start + vec2((pin % cols) as f32, (pin / cols) as f32) * d, Vec2::splat(d)).shrink(d * 0.08);
        let resp = c.ui.interact(cell, c.id(("pin", pin)), Sense::click_and_drag()).on_hover_text(format!("GP{pin}"));
        let bit = 1u8 << pin;
        let want = if toggle {
            (*driven & bit != 0) ^ resp.clicked()
        } else {
            resp.is_pointer_button_down_on()
        };
        if want != (*driven & bit != 0) {
            *driven ^= bit;
            c.input(out, &format!("gp{pin}"), if want { active_high as i64 } else { -1 });
        }
        let v = c.ui.visuals();
        let p = c.ui.painter();
        let high = levels & bit != 0;
        p.rect_filled(cell, 4.0, if high { Color32::from_rgb(230, 170, 40) } else { Color32::from_gray(45) });
        if *driven & bit != 0 {
            p.rect_stroke(cell, 4.0, Stroke::new(2.0, v.selection.stroke.color), StrokeKind::Inside);
        }
        let ink = if high { Color32::BLACK } else { Color32::from_gray(170) };
        p.text(cell.center(), Align2::CENTER_CENTER, pin, FontId::monospace((d * 0.35).min(14.0)), ink);
    }
}

fn dac(c: Ctx) {
    let Some(dac) = c.device::<Dac8165>() else { return };
    let row = c.body.height() / 4.0;
    let font = FontId::monospace((row * 0.5).clamp(9.0, 14.0));
    let v = c.ui.visuals();
    let p = c.ui.painter_at(c.body);
    for ch in 0..4 {
        let r = Rect::from_min_size(c.body.min + vec2(0.0, row * ch as f32), vec2(c.body.width(), row)).shrink2(vec2(0.0, row * 0.12));
        let text = format!("{} {:>5.3} V", (b'A' + ch as u8) as char, dac.millivolts(ch) as f32 / 1000.0);
        p.text(r.left_center(), Align2::LEFT_CENTER, text, font.clone(), v.text_color());
        let bar = Rect::from_min_max(pos2(r.left() + r.width() * 0.55, r.top()), r.max);
        p.rect_filled(bar, 2.0, v.extreme_bg_color);
        let frac = dac.code(ch) as f32 / 16384.0;
        p.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * frac, bar.height())), 2.0, v.selection.bg_fill);
    }
}

fn sdcard(c: Ctx, out: &mut Vec<Request>) {
    let Some(card) = c.device::<SdCard>() else { return };
    let mib = card.blocks() as f64 * 512.0 / (1024.0 * 1024.0);
    let state = if card.dirty() { "written" } else { "unchanged" };
    let name = c.it.name.clone();
    c.ui.scope_builder(egui::UiBuilder::new().max_rect(c.body), |ui| {
        ui.label(format!("{mib:.0} MiB, {state}"));
        ui.horizontal(|ui| {
            if ui.button("Load…").clicked() {
                out.push(Request::LoadSd(name.clone()));
            }
            if ui.button("Save…").clicked() {
                out.push(Request::SaveSd(name));
            }
        });
    });
}

/// The one-line story of a Wi-Fi chip, and whether it is an error.
fn wifi_status(chip: &Cyw43439, notes: &WifiNotes) -> (String, bool) {
    let ip = |chip: &Cyw43439| chip.lease().map_or_else(|| "no address yet".to_string(), |a| format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]));
    if notes.unavailable && !chip.network_available() {
        let why = if cfg!(target_arch = "wasm32") { "Wi-Fi unavailable in the browser" } else { "Wi-Fi unavailable: no network" };
        return (why.into(), true);
    }
    if !chip.firmware_running() {
        return ("radio off".into(), false);
    }
    match chip.link() {
        Link::Up(_) => (format!("{} \u{2022} {}", chip.ssid().unwrap_or("?"), ip(chip)), false),
        Link::Joining => (format!("joining {}\u{2026}", chip.ssid().unwrap_or("?")), false),
        Link::Down if notes.join_failed => ("join failed".into(), true),
        Link::Down => (notes.scanned.map_or("idle".into(), |n| format!("idle \u{2022} last scan found {n}")), false),
    }
}

/// The one-line story of the chip's Bluetooth side, if the firmware uses
/// it, and whether it is an error.
fn bt_status(chip: &Cyw43439, notes: &WifiNotes) -> Option<(String, bool)> {
    if !chip.bt_up() {
        return None;
    }
    if notes.bt_unavailable && !chip.bt_backend().available() {
        let why = if cfg!(target_arch = "wasm32") { "Bluetooth unavailable in the browser" } else { "Bluetooth unavailable: no radio (see --bt)" };
        return Some((why.into(), true));
    }
    let (scanning, advertising, links) = chip.bt_activity();
    let mut parts = Vec::new();
    if links > 0 {
        parts.push(format!("{links} connected"));
    }
    if advertising {
        parts.push("advertising".into());
    }
    if scanning {
        parts.push("scanning".into());
    }
    let what = if parts.is_empty() { "idle".into() } else { parts.join(" \u{2022} ") };
    Some((format!("Bluetooth {what}"), false))
}

fn wifi(c: Ctx, st: &PanelState) {
    let session: &Session = c.s;
    let Some(chip) = session.sys.board.device::<Cyw43439>(&c.it.name) else { return };
    let notes = st.wifi.get(&c.it.part).copied().unwrap_or_default();
    let (status, is_error) = wifi_status(chip, &notes);
    let r = (c.body.height().min(c.body.width() / 3.0) * 0.28).max(4.0);
    lamp(c.ui, c.body.left_center() + vec2(r + 4.0, 0.0), r, Color32::from_rgb(40, 230, 70), chip.led());
    let text = Rect::from_min_max(pos2(c.body.left() + 2.0 * r + 14.0, c.body.top()), c.body.right_bottom());
    c.ui.scope_builder(egui::UiBuilder::new().max_rect(text), |ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(&status).color(if is_error { ui.visuals().error_fg_color } else { ui.visuals().text_color() }));
            if let Some((bt, is_error)) = bt_status(chip, &notes) {
                ui.label(egui::RichText::new(bt).color(if is_error { ui.visuals().error_fg_color } else { ui.visuals().text_color() }));
            }
            ui.label(egui::RichText::new(format!("{} \u{2022} {} network{}", mac_text(chip.mac()), chip.aps().len(), if chip.aps().len() == 1 { "" } else { "s" })).small().weak());
        });
    });
}

fn mac_text(mac: [u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(text: &str) -> Board {
        Board::parse_with(text, &pcb_devices::registry()).unwrap()
    }

    #[test]
    fn explicit_layout_uses_panel_lines_and_reports_bad_options() {
        let b = board("led a gpio=1\nbutton b gpio=2\nst7789 s bus=spi0 cs=9 dc=13\npanel s at=0,0 span=3,4 rotate=90\npanel a at=3,0 color=00ff00\npanel b at=3,1 key=Nope\n");
        let (l, warnings) = Layout::new(&b);
        assert_eq!((l.cols, l.rows), (4, 4));
        assert_eq!(l.items.len(), 3);
        assert!(matches!(l.items[0].widget, Widget::Screen { rotate: 90 }));
        assert!(matches!(l.items[1].widget, Widget::Led { color } if color == Color32::from_rgb(0, 255, 0)));
        assert!(matches!(l.items[2].widget, Widget::Button { key: None }));
        assert_eq!(warnings, ["line 6: unknown key \"Nope\" (ignored)"]);
    }

    #[test]
    fn automatic_layout_packs_every_part_without_overlap() {
        let b = board(include_str!("../../../boards/btl-01.board"));
        let (l, warnings) = Layout::new(&b);
        assert!(warnings.is_empty());
        assert_eq!(l.items.len(), b.parts.len());
        for (i, a) in l.items.iter().enumerate() {
            assert!(a.cell.0 + a.span.0 <= l.cols);
            for o in &l.items[i + 1..] {
                let apart = a.cell.0 + a.span.0 <= o.cell.0 || o.cell.0 + o.span.0 <= a.cell.0 || a.cell.1 + a.span.1 <= o.cell.1 || o.cell.1 + o.span.1 <= a.cell.1;
                assert!(apart, "{} overlaps {}", a.name, o.name);
            }
        }
    }

    #[test]
    fn the_pico_w_board_lays_out_and_its_wifi_widget_reports_state() {
        let b = board(include_str!("../../../boards/pico-w.board"));
        let (l, warnings) = Layout::new(&b);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(matches!(l.items[0].widget, Widget::Wifi));
        let chip = b.device::<Cyw43439>("wifi").unwrap();
        // Fresh board: radio off. After an attempt with no network, say why.
        assert_eq!(wifi_status(chip, &WifiNotes::default()), ("radio off".to_string(), false));
        let (text, error) = wifi_status(chip, &WifiNotes { unavailable: true, ..Default::default() });
        assert!(error && text.starts_with("Wi-Fi unavailable"), "{text}");
        let mut st = PanelState::default();
        st.absorb(&[BoardEvent { cycles: 0, part: 0, label: "join", value: 0 }, BoardEvent { cycles: 0, part: 0, label: "scan", value: 2 }], 0.0);
        assert!(st.wifi[&0].join_failed && st.wifi[&0].scanned == Some(2));
        // Bluetooth says nothing until the firmware brings it up.
        assert_eq!(bt_status(chip, &WifiNotes { bt_unavailable: true, ..Default::default() }), None);
        st.absorb(&[BoardEvent { cycles: 0, part: 0, label: "bt_unavailable", value: 1 }], 0.0);
        assert!(st.wifi[&0].bt_unavailable);
    }

    #[test]
    fn widget_options() {
        let w = |k, o: &[(&'static str, &'static str)]| widget_for(k, &mut o.iter().copied());
        assert!(matches!(w("encoder", &[("keys", "A,D")]), Ok(Widget::Encoder { keys: Some((Key::A, Key::D)) })));
        assert!(matches!(w("mcp23008", &[("mode", "toggle"), ("active", "high")]), Ok(Widget::Expander { toggle: true, active_high: true })));
        assert_eq!(w("led", &[("rotate", "90")]).err().unwrap(), "led has no panel option \"rotate\"");
        assert!(w("st7789", &[("rotate", "45")]).is_err());
        assert!(w("led", &[("color", "12345")]).is_err());
        assert!(w("led", &[("color", "pink")]).is_err());
        assert!(matches!(w("custom-thing", &[]), Ok(Widget::Other)));
    }

    #[test]
    fn screens_scale_by_whole_physical_pixels_on_the_grid() {
        let outer = Rect::from_min_size(pos2(10.3, 20.0), vec2(500.0, 400.0));
        let r = fit_pixels(outer, vec2(240.0, 135.0), 1.0);
        assert_eq!(r.size(), vec2(480.0, 270.0));
        assert_eq!(r.min, pos2(20.0, 85.0));
        // 1.5 points per pixel: 750 x 600 pixels hold 3x, 720 x 405 pixels.
        let r = fit_pixels(outer, vec2(240.0, 135.0), 1.5);
        assert_eq!(r.size() * 1.5, vec2(720.0, 405.0));
        assert_eq!(r.min * 1.5, (r.min * 1.5).round());
        // Too small for 1:1: shrink to fit instead.
        assert_eq!(fit_pixels(outer, vec2(1000.0, 100.0), 1.0).width(), 500.0);
    }

    #[test]
    fn circle_grid_prefers_the_shape_of_the_space() {
        assert_eq!(circle_grid(8, vec2(400.0, 50.0)).0, 8);
        assert_eq!(circle_grid(8, vec2(50.0, 400.0)).0, 1);
        assert_eq!(circle_grid(8, vec2(100.0, 50.0)).0, 4);
    }
}
