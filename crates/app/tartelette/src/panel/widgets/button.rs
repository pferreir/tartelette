//! `button`: a push button, held while the pointer or its key is down.
use super::*;

pub(in crate::panel) fn button(c: Ctx, key: Option<Key>, out: &mut Vec<Request>) {
    let Some(b) = c.device::<Button>() else {
        return;
    };
    let body = c.show_keys(key.as_slice());
    let side = body.width().min(body.height()) * 0.8;
    let rect = Rect::from_center_size(body.center(), Vec2::splat(side));
    let mut resp = c.ui.interact(rect, c.id("button"), Sense::click_and_drag());
    if let Some(k) = key {
        resp = resp.on_hover_text(format!("hold, or {}", k.name()));
    }
    let down = resp.is_pointer_button_down_on()
        || (c.keys_free && key.is_some_and(|k| c.ui.input(|i| i.key_down(k))));
    if down != b.pressed {
        c.input(out, if down { "press" } else { "release" }, 0);
    }
    let v = c.ui.visuals();
    let fill = if b.pressed {
        v.selection.bg_fill
    } else if resp.hovered() {
        v.widgets.hovered.bg_fill
    } else {
        v.widgets.inactive.bg_fill
    };
    let p = c.ui.painter();
    p.circle_filled(rect.center(), side / 2.0, fill);
    p.circle_stroke(rect.center(), side / 2.0, v.widgets.inactive.fg_stroke);
}
