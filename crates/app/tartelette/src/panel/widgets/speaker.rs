//! `speaker`: a speaker icon that mutes on a click, with the volume, turned
//! by scrolling.
use super::*;

/// Percent per mouse wheel notch.
const NOTCH: i64 = 5;
/// Percent per point scrolled.
const SCROLL: f32 = 0.25;
const HELP: &str = "click to mute, scroll for the volume\n\
                    heard while Sound is on in the toolbar";

pub(in crate::panel) fn speaker(c: Ctx, out: &mut Vec<Request>) {
    let Some(spk) = c.device::<Speaker>() else {
        return;
    };
    let text_h = 14.0;
    let side = c.body.width().min(c.body.height() - text_h) * 0.8;
    let rect = Rect::from_center_size(c.body.center() - vec2(0.0, text_h / 2.0), Vec2::splat(side));
    let resp =
        c.ui.interact(rect, c.id("speaker"), Sense::click())
            .on_hover_text(HELP);
    if resp.clicked() {
        c.input(out, if spk.muted { "unmute" } else { "mute" }, 0);
    }
    if resp.hovered() {
        let mut volume = spk.volume;
        c.ui.input_mut(|i| {
            for e in &i.events {
                if let egui::Event::MouseWheel { unit, delta, .. } = e {
                    volume += match unit {
                        egui::MouseWheelUnit::Point => (delta.y * SCROLL).round() as i64,
                        _ => delta.y.signum() as i64 * NOTCH,
                    };
                }
            }
            i.smooth_scroll_delta = Vec2::ZERO;
        });
        let volume = volume.clamp(0, 100);
        if volume != spk.volume {
            c.input(out, "volume", volume);
        }
    }

    let v = c.ui.visuals();
    let color = if resp.hovered() {
        v.strong_text_color()
    } else {
        v.text_color()
    };
    let p = c.ui.painter();
    // The magnet and the cone, facing right.
    let s = side / 2.0;
    let o = rect.center() - vec2(s * 0.35, 0.0);
    let at = |x: f32, y: f32| o + vec2(x, y) * s;
    p.add(egui::Shape::convex_polygon(
        vec![
            at(-0.45, -0.2),
            at(-0.15, -0.2),
            at(0.25, -0.55),
            at(0.25, 0.55),
            at(-0.15, 0.2),
            at(-0.45, 0.2),
        ],
        color,
        Stroke::NONE,
    ));
    let stroke = Stroke::new(s * 0.08, color);
    if spk.muted {
        p.line_segment([at(0.5, -0.25), at(1.0, 0.25)], stroke);
        p.line_segment([at(0.5, 0.25), at(1.0, -0.25)], stroke);
    } else {
        // A wave per third of the volume.
        let waves = (spk.volume as f32 / 100.0 * 3.0).ceil() as usize;
        for k in 0..waves {
            let r = 0.45 + 0.25 * k as f32;
            let arc: Vec<Pos2> = (0..=12)
                .map(|i| {
                    let a = (i as f32 / 12.0 - 0.5) * 1.6;
                    at(0.1 + r * a.cos(), r * a.sin())
                })
                .collect();
            p.add(egui::Shape::line(arc, stroke));
        }
    }
    let text = match spk.muted {
        true => "muted".to_string(),
        false => format!("{}%", spk.volume),
    };
    p.text(
        pos2(c.body.center().x, c.body.bottom()),
        Align2::CENTER_BOTTOM,
        text,
        FontId::monospace(12.0),
        v.text_color(),
    );
}
