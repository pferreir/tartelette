//! `ps2mouse`: a pad that moves the mouse as the pointer crosses it, with
//! its buttons and wheel.
use egui::PointerButton;
use pcb_devices::ps2mouse::{Button, Model, Ps2Mouse};

use super::*;

/// Smooth (touchpad) scroll travel in points per wheel notch; a mouse wheel
/// notch is one notch.
const SCROLL_PER_NOTCH: f32 = 40.0;

const BUTTONS: [(PointerButton, Button); 5] = [
    (PointerButton::Primary, Button::Left),
    (PointerButton::Middle, Button::Middle),
    (PointerButton::Secondary, Button::Right),
    (PointerButton::Extra1, Button::Back),
    (PointerButton::Extra2, Button::Forward),
];

pub(in crate::panel) fn mouse(c: Ctx, st: &mut PanelState, out: &mut Vec<Request>) {
    let Some(mouse) = c.device::<Ps2Mouse>() else {
        return;
    };
    let text_h = 14.0;
    let pad = Rect::from_min_max(c.body.min, c.body.max - vec2(0.0, text_h));
    let resp =
        c.ui.interact(pad, c.id("pad"), Sense::click_and_drag())
            .on_hover_text("move across to move the mouse; click and scroll here");
    let held = mouse.buttons();
    let buttons = match mouse.model() {
        Model::Explorer => &BUTTONS[..],
        _ => &BUTTONS[..3],
    };
    // A button pressed on the pad stays held, and keeps moving the mouse,
    // until it is let go anywhere.
    let (delta, moved) = c.ui.input(|i| {
        for &(pointer, button) in buttons {
            let bit = 1 << button as u8;
            if held & bit == 0 && resp.hovered() && i.pointer.button_pressed(pointer) {
                c.input(out, "press", button as i64);
            } else if held & bit != 0 && !i.pointer.button_down(pointer) {
                c.input(out, "release", button as i64);
            }
        }
        (i.pointer.delta(), resp.hovered() || held != 0)
    });
    if moved {
        let travel = st.motion.entry(c.it.part).or_default();
        *travel += delta;
        let (x, y) = (travel.x.trunc(), travel.y.trunc());
        *travel -= vec2(x, y);
        if x != 0.0 {
            c.input(out, "move_x", x as i64);
        }
        if y != 0.0 {
            c.input(out, "move_y", y as i64);
        }
    }
    if resp.hovered() {
        // Raw wheel events, so one notch is one notch; and swallow the
        // scroll so the panel's scroll area doesn't move under the pad.
        let travel = st.travel.entry(c.it.part).or_default();
        c.ui.input_mut(|i| {
            for e in &i.events {
                if let egui::Event::MouseWheel { unit, delta, .. } = e {
                    *travel += match unit {
                        egui::MouseWheelUnit::Point => delta.y / SCROLL_PER_NOTCH,
                        _ => delta.y.signum(),
                    };
                }
            }
            i.smooth_scroll_delta = Vec2::ZERO;
        });
        let notches = travel.trunc();
        *travel -= notches;
        if notches != 0.0 {
            c.input(out, "scroll", notches as i64);
        }
    }

    let v = c.ui.visuals();
    let p = c.ui.painter();
    let fill = if held != 0 {
        v.widgets.active.bg_fill
    } else if resp.hovered() {
        v.widgets.hovered.bg_fill
    } else {
        v.widgets.inactive.bg_fill
    };
    p.rect_filled(pad, 6.0, fill);
    p.rect_stroke(pad, 6.0, v.widgets.inactive.fg_stroke, StrokeKind::Inside);
    // The buttons along the top of the pad, lit while held.
    let n = buttons.len() as f32;
    let gap = 3.0;
    let w = (pad.width() - gap * (n + 1.0)) / n;
    let h = (pad.height() * 0.3).min(24.0);
    for (k, &(_, button)) in buttons.iter().enumerate() {
        let min = pad.min + vec2(gap + k as f32 * (w + gap), gap);
        let r = Rect::from_min_size(min, vec2(w, h));
        let lit = held & 1 << button as u8 != 0;
        let color = if lit {
            v.selection.bg_fill
        } else {
            v.widgets.noninteractive.bg_fill
        };
        p.rect_filled(r, 3.0, color);
    }
    // Whether the firmware has turned reporting on, and what it made of the
    // mouse.
    let state = match (mouse.reporting(), mouse.id()) {
        (false, _) => "not reporting".to_owned(),
        (true, 0) => "reporting".to_owned(),
        (true, id) => format!("reporting, ID {id}"),
    };
    p.text(
        pos2(c.body.center().x, c.body.bottom()),
        Align2::CENTER_BOTTOM,
        state,
        FontId::proportional(11.0),
        v.weak_text_color(),
    );
}
