//! `dac8165`: four voltage meters, at the jacks.
use super::*;

pub(in crate::panel) fn dac(c: Ctx) {
    let Some(dac) = c.device::<Dac8165>() else {
        return;
    };
    let row = c.body.height() / 4.0;
    let font = FontId::monospace((row * 0.5).clamp(9.0, 14.0));
    let v = c.ui.visuals();
    let p = c.ui.painter_at(c.body);
    for ch in 0..4 {
        let r = Rect::from_min_size(
            c.body.min + vec2(0.0, row * ch as f32),
            vec2(c.body.width(), row),
        )
        .shrink2(vec2(0.0, row * 0.12));
        // At the jack when the board says how it gets there.
        let volts = if dac.has_output_stage() {
            dac.cv_volts(ch)
        } else {
            dac.millivolts(ch) as f32 / 1000.0
        };
        let text = format!("{} {volts:>+6.3} V", (b'A' + ch as u8) as char);
        p.text(
            r.left_center(),
            Align2::LEFT_CENTER,
            text,
            font.clone(),
            v.text_color(),
        );
        let bar = Rect::from_min_max(pos2(r.left() + r.width() * 0.55, r.top()), r.max);
        p.rect_filled(bar, 2.0, v.extreme_bg_color);
        let frac = dac.code(ch) as f32 / 16384.0;
        p.rect_filled(
            Rect::from_min_size(bar.min, vec2(bar.width() * frac, bar.height())),
            2.0,
            v.selection.bg_fill,
        );
    }
}
