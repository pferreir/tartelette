//! `wav`: a player with the file's name, its position, and load, rewind,
//! play and stop buttons.
use super::*;

pub(in crate::panel) fn wav(c: Ctx, out: &mut Vec<Request>) {
    // Through copies of the references, so `c.ui` stays free to borrow.
    let (s, it) = (c.s, c.it);
    let Some(wav) = s.sys.board.device::<Wav>(&it.name) else {
        return;
    };
    let loaded = wav.name.is_some();
    let playing = wav.playing();
    let (pos, len) = wav.position(s.sys.cycles());
    let name = it.name.clone();
    c.ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(c.body)
            .layout(egui::Layout::top_down(egui::Align::Min).with_main_align(egui::Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 5.0);
            let title = match &wav.name {
                Some(n) => egui::RichText::new(n),
                None => egui::RichText::new("No file").weak(),
            };
            ui.add(egui::Label::new(title).truncate());
            // The position along the file, and the times.
            let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
            let v = ui.visuals();
            ui.painter().rect_filled(bar, 2.0, v.extreme_bg_color);
            if len > 0.0 {
                let done = (pos / len) as f32;
                ui.painter().rect_filled(
                    Rect::from_min_size(bar.min, vec2(bar.width() * done, bar.height())),
                    2.0,
                    v.selection.bg_fill,
                );
            }
            ui.horizontal(|ui| {
                let enabled = loaded;
                if transport(ui, enabled, Glyph::Rewind, "back to the start") {
                    c_input(out, &name, "rewind");
                }
                let (glyph, action, tip) = match playing {
                    true => (Glyph::Stop, "stop", "stop where it is"),
                    false => (Glyph::Play, "play", "play"),
                };
                if transport(ui, enabled, glyph, tip) {
                    c_input(out, &name, action);
                }
                if ui
                    .small_button("Load")
                    .on_hover_text("load a WAV file")
                    .clicked()
                {
                    out.push(Request::LoadWav(name.clone()));
                }
                if loaded {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("{} / {}", clock(pos), clock(len)))
                                .monospace()
                                .small(),
                        )
                        .truncate(),
                    );
                }
            });
        },
    );
}

fn c_input(out: &mut Vec<Request>, part: &str, action: &str) {
    out.push(Request::Input(part.into(), action.into(), 0));
}

/// `m:ss`.
fn clock(secs: f64) -> String {
    let s = secs as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

#[derive(Clone, Copy)]
enum Glyph {
    Rewind,
    Play,
    Stop,
}

/// A small button with a drawn transport symbol (fonts may lack them).
fn transport(ui: &mut Ui, enabled: bool, glyph: Glyph, tip: &str) -> bool {
    let h = ui.spacing().interact_size.y.min(18.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(h * 1.4, h), Sense::click());
    let resp = resp.on_hover_text(tip);
    let v = ui.visuals();
    let w = if !enabled {
        &v.widgets.noninteractive
    } else if resp.is_pointer_button_down_on() {
        &v.widgets.active
    } else if resp.hovered() {
        &v.widgets.hovered
    } else {
        &v.widgets.inactive
    };
    let p = ui.painter();
    p.rect(rect, 3.0, w.weak_bg_fill, w.bg_stroke, StrokeKind::Inside);
    let color = if enabled {
        w.fg_stroke.color
    } else {
        v.weak_text_color()
    };
    let c = rect.center();
    let s = h * 0.28;
    let triangle = |tip: Pos2, dir: f32| {
        egui::Shape::convex_polygon(
            vec![
                tip,
                tip - vec2(dir * s * 1.6, s),
                tip - vec2(dir * s * 1.6, -s),
            ],
            color,
            Stroke::NONE,
        )
    };
    match glyph {
        Glyph::Play => {
            p.add(triangle(c + vec2(s * 0.8, 0.0), 1.0));
        }
        Glyph::Stop => {
            p.rect_filled(Rect::from_center_size(c, Vec2::splat(s * 1.8)), 1.0, color);
        }
        Glyph::Rewind => {
            p.rect_filled(
                Rect::from_center_size(c - vec2(s * 0.9, 0.0), vec2(s * 0.4, s * 2.0)),
                0.0,
                color,
            );
            p.add(triangle(c - vec2(s * 0.6, 0.0), -1.0));
        }
    }
    enabled && resp.clicked()
}
