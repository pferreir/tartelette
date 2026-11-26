//! `pcm5102`: the DAC's sample rate as it receives frames, and a peak
//! meter per channel in dBFS.
use super::*;

pub(in crate::panel) fn pcm5102(c: Ctx) {
    let Some(d) = c.device::<Pcm5102>() else {
        return;
    };
    let v = c.ui.visuals();
    let p = c.ui.painter();
    let rate = match d.rate() {
        Some(hz) => format!("I2S {:.1} kHz", hz / 1000.0),
        None => "I2S: no frames".into(),
    };
    let top = c.body.top() + 4.0;
    p.text(
        pos2(c.body.left() + 6.0, top),
        Align2::LEFT_TOP,
        rate,
        FontId::monospace(12.0),
        v.text_color(),
    );
    // A peak meter per channel, from -60 to 0 dBFS.
    let w = c.body.width() - 30.0;
    let now = c.s.sys.cycles();
    for (i, name) in ["L", "R"].into_iter().enumerate() {
        let y = top + 20.0 + i as f32 * 12.0;
        p.text(
            pos2(c.body.left() + 6.0, y),
            Align2::LEFT_TOP,
            name,
            FontId::monospace(10.0),
            v.weak_text_color(),
        );
        let bar = Rect::from_min_size(pos2(c.body.left() + 20.0, y + 2.0), vec2(w, 6.0));
        p.rect_filled(bar, 2.0, v.extreme_bg_color);
        let db = 20.0 * (d.level(i, now) as f32 / 32768.0).log10();
        let f = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        p.rect_filled(
            Rect::from_min_size(bar.min, vec2(w * f, 6.0)),
            2.0,
            v.selection.bg_fill,
        );
    }
    if d.frames > 0 {
        crate::app::next_frame(c.ui.ctx());
    }
}
