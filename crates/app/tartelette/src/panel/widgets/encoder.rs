//! `encoder`: a knob turned by dragging, scrolling or keys.
use super::*;

/// Detents per knob revolution, for turning by dragging around the knob.
const DETENTS_PER_REV: f32 = 20.0;
/// How long (seconds) the knob shows the direction of its last detent.
const TURN_SHOWN: f64 = 0.5;
/// Smooth (touchpad) scroll travel in points per encoder detent; a mouse
/// wheel notch is one detent.
const SCROLL_PER_DETENT: f32 = 40.0;

pub(in crate::panel) fn encoder(
    c: Ctx,
    st: &mut PanelState,
    keys: Option<(Key, Key)>,
    out: &mut Vec<Request>,
) {
    let body = c.show_keys(keys.map(|(a, b)| vec![a, b]).as_deref().unwrap_or_default());
    let side = body.width().min(body.height()) * 0.9;
    let rect = Rect::from_center_size(body.center(), Vec2::splat(side));
    let resp =
        c.ui.interact(rect, c.id("knob"), Sense::drag())
            .on_hover_text(match keys {
                Some((ccw, cw)) => format!(
                    "drag around or scroll to turn, or {} / {}",
                    ccw.name(),
                    cw.name()
                ),
                None => "drag around or scroll to turn".into(),
            });
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
        let was = if resp.drag_started() {
            c.ui.input(|i| i.pointer.press_origin())
        } else {
            None
        };
        let (a, b) = (
            was.unwrap_or(now - resp.drag_delta()) - center,
            now - center,
        );
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
        turn +=
            c.ui.input(|i| i.key_pressed(cw) as i64 - i.key_pressed(ccw) as i64);
    }
    if turn != 0 {
        c.input(out, "turn", turn);
    }
    let v = c.ui.visuals();
    let p = c.ui.painter();
    let fill = if resp.dragged() {
        v.widgets.active.bg_fill
    } else if resp.hovered() {
        v.widgets.hovered.bg_fill
    } else {
        v.widgets.inactive.bg_fill
    };
    p.circle_filled(center, side / 2.0, fill);
    p.circle_stroke(center, side / 2.0, v.widgets.inactive.fg_stroke);
    // An encoder has no position, only steps: show which way the last one
    // went, fading out.
    if let Some(&(dir, at)) = st.turned.get(&c.it.part) {
        let fade = 1.0 - ((c.ui.input(|i| i.time) - at) / TURN_SHOWN) as f32;
        if fade > 0.0 {
            turn_arrow(
                p,
                center,
                side * 0.3,
                dir as f32,
                v.strong_text_color().gamma_multiply(fade),
            );
        }
    }
}

/// A curved arrow over the top of a knob, pointing clockwise when `dir` > 0.
fn turn_arrow(p: &egui::Painter, center: Pos2, radius: f32, dir: f32, color: Color32) {
    let at = |a: f32, r: f32| center + vec2(a.sin(), -a.cos()) * r;
    let (from, to) = (-dir, dir); // one radian either side of the top
    let arc: Vec<Pos2> = (0..=16)
        .map(|i| at(from + (to - from) * i as f32 / 16.0, radius))
        .collect();
    let width = radius * 0.16;
    p.add(egui::Shape::line(arc, Stroke::new(width, color)));
    let head = width * 2.2;
    let tip = at(to + dir * head / radius, radius);
    let back = to - dir * 0.15;
    p.add(egui::Shape::convex_polygon(
        vec![
            tip,
            at(back, radius + head * 0.7),
            at(back, radius - head * 0.7),
        ],
        color,
        Stroke::NONE,
    ));
}
