//! `ps2mouse`: a pad that captures the pointer when clicked. Captured, the
//! real mouse drives the emulated one, its motion, buttons and wheel, over
//! the whole window and past its edges; Escape lets go.
use egui::{Key, Modifiers, PointerButton};
use pcb_devices::ps2mouse::{Button, Model, Ps2Mouse};

use super::*;
use crate::panel::grab;

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
    let ctx = c.ui.ctx().clone();
    let part = c.it.part;
    let mut captured = grab::holder(&ctx) == Some(part);
    let resp =
        c.ui.interact(pad, c.id("pad"), Sense::click())
            .on_hover_text(if captured {
                "the mouse is captured: Escape lets go"
            } else {
                "click to capture the mouse; Escape lets go"
            });
    let held = mouse.buttons();
    let buttons = match mouse.model() {
        Model::Explorer => &BUTTONS[..],
        _ => &BUTTONS[..3],
    };

    if captured {
        let (esc, focused) =
            c.ui.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::Escape), i.focused));
        if esc || !focused {
            grab::release(&ctx);
            captured = false;
            // Let go of anything held when the capture ended.
            for &(_, button) in buttons {
                if held & 1 << button as u8 != 0 {
                    c.input(out, "release", button as i64);
                }
            }
        }
    } else if resp.clicked() {
        // The click that captures is not one for the emulated mouse: it
        // counts once let go, so no button is down when the capture starts.
        grab::take(&ctx, part);
        st.motion.insert(part, Vec2::ZERO);
        st.travel.insert(part, 0.0);
        grab::motion(&ctx);
    }

    if captured {
        grab::keep(&ctx, pad.center());
        let travel = st.motion.entry(part).or_default();
        *travel += grab::motion(&ctx);
        let (x, y) = (travel.x.trunc(), travel.y.trunc());
        *travel -= vec2(x, y);
        if x != 0.0 {
            c.input(out, "move_x", x as i64);
        }
        if y != 0.0 {
            c.input(out, "move_y", y as i64);
        }
        let (down, notches) = c.ui.input_mut(|i| {
            let down = buttons
                .iter()
                .filter(|(pointer, _)| i.pointer.button_down(*pointer))
                .fold(0u8, |m, &(_, b)| m | 1 << b as u8);
            // Raw wheel events, so one notch is one notch; and swallow the
            // scroll so that nothing on the panel moves meanwhile.
            let mut n = 0.0;
            for e in &i.events {
                if let egui::Event::MouseWheel { unit, delta, .. } = e {
                    n += match unit {
                        egui::MouseWheelUnit::Point => delta.y / SCROLL_PER_NOTCH,
                        _ => delta.y.signum(),
                    };
                }
            }
            i.smooth_scroll_delta = Vec2::ZERO;
            (down, n)
        });
        for &(_, button) in buttons {
            let bit = 1 << button as u8;
            if held & bit != down & bit {
                let action = if down & bit != 0 { "press" } else { "release" };
                c.input(out, action, button as i64);
            }
        }
        let travel = st.travel.entry(part).or_default();
        *travel += notches;
        let whole = travel.trunc();
        *travel -= whole;
        if whole != 0.0 {
            c.input(out, "scroll", whole as i64);
        }
        // Keep the motion coming while captured.
        ctx.request_repaint();
    }

    let v = c.ui.visuals();
    let p = c.ui.painter();
    let fill = if captured {
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
    let hint = if captured {
        "Esc to let go"
    } else {
        "click to capture"
    };
    p.text(
        pad.center() + vec2(0.0, h / 2.0),
        Align2::CENTER_CENTER,
        hint,
        FontId::proportional(12.0),
        v.text_color(),
    );
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
