//! `pot`: a knob with a 270° travel, turned by dragging or scrolling, and
//! the wiper's voltage.
use super::*;

/// Per mille of travel per point dragged.
const DRAG: f32 = 4.0;
/// Per mille of travel per mouse wheel notch.
const NOTCH: i64 = 20;
/// The knob's travel each side of straight up, in radians.
const SWEEP: f32 = 0.75 * std::f32::consts::PI;

pub(in crate::panel) fn pot(c: Ctx, out: &mut Vec<Request>) {
    let Some(pot) = c.device::<Pot>() else {
        return;
    };
    let text_h = 14.0;
    let side = c.body.width().min(c.body.height() - text_h) * 0.9;
    let rect = Rect::from_center_size(c.body.center() - vec2(0.0, text_h / 2.0), Vec2::splat(side));
    let resp =
        c.ui.interact(rect, c.id("pot"), Sense::drag())
            .on_hover_text("drag up or right, or scroll, to turn");
    let mut value = pot.value;
    if resp.dragged() {
        let d = resp.drag_delta();
        value += ((d.x - d.y) * DRAG).round() as i64;
    }
    if resp.hovered() {
        c.ui.input_mut(|i| {
            for e in &i.events {
                if let egui::Event::MouseWheel { unit, delta, .. } = e {
                    value += match unit {
                        egui::MouseWheelUnit::Point => (delta.y * DRAG).round() as i64,
                        _ => delta.y.signum() as i64 * NOTCH,
                    };
                }
            }
            i.smooth_scroll_delta = Vec2::ZERO;
        });
    }
    let value = value.clamp(0, 1000);
    if value != pot.value {
        c.input(out, "set", value);
    }

    let v = c.ui.visuals();
    let p = c.ui.painter();
    let center = rect.center();
    let r = side / 2.0;
    let fill = if resp.dragged() {
        v.widgets.active.bg_fill
    } else if resp.hovered() {
        v.widgets.hovered.bg_fill
    } else {
        v.widgets.inactive.bg_fill
    };
    // The track, and how far along it the wiper is.
    let at = |a: f32, r: f32| center + vec2(a.sin(), -a.cos()) * r;
    let arc = |to: f32| -> Vec<Pos2> {
        (0..=32)
            .map(|i| at(-SWEEP + (to + SWEEP) * i as f32 / 32.0, r * 0.92))
            .collect()
    };
    let angle = -SWEEP + 2.0 * SWEEP * value as f32 / 1000.0;
    p.add(egui::Shape::line(
        arc(SWEEP),
        Stroke::new(r * 0.1, v.extreme_bg_color),
    ));
    p.add(egui::Shape::line(
        arc(angle),
        Stroke::new(r * 0.1, v.selection.bg_fill),
    ));
    p.circle_filled(center, r * 0.75, fill);
    p.circle_stroke(center, r * 0.75, v.widgets.inactive.fg_stroke);
    p.line_segment(
        [at(angle, r * 0.25), at(angle, r * 0.68)],
        Stroke::new(r * 0.08, v.strong_text_color()),
    );
    p.text(
        pos2(c.body.center().x, c.body.bottom()),
        Align2::CENTER_BOTTOM,
        format!("{:.2} V", pot.volts()),
        FontId::monospace(12.0),
        v.text_color(),
    );
}
