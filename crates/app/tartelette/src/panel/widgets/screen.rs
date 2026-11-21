//! `st7789`: the screen, a texture redrawn when the frame RAM changes.
use super::*;

/// An image of `px` texels fitted inside `outer`, centred: a whole number of
/// physical pixels per texel when it fits at all, on the pixel grid.
pub(in crate::panel) fn fit_pixels(outer: Rect, px: Vec2, pixels_per_point: f32) -> Rect {
    let k = (outer.width() / px.x).min(outer.height() / px.y) * pixels_per_point;
    let k = if k >= 1.0 { k.floor() } else { k };
    let size = px * k / pixels_per_point;
    let min = ((outer.center() - size / 2.0) * pixels_per_point).round() / pixels_per_point;
    Rect::from_min_size(min, size)
}

pub(in crate::panel) fn screen(c: Ctx, st: &mut PanelState, rotate: u16) {
    let Some(lcd) = c.device::<St7789>() else {
        return;
    };
    let (w, h) = lcd.size();
    let stamp = (lcd.total_pixels, lcd.is_showing());
    let (tw, th) = if rotate.is_multiple_of(180) {
        (w, h)
    } else {
        (h, w)
    };
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
        egui::ColorImage {
            size: [tw, th],
            source_size: vec2(tw as f32, th as f32),
            pixels: dst,
        }
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
            let tex = c.ui.ctx().load_texture(
                format!("screen-{}", c.it.name),
                make(),
                TextureOptions::NEAREST,
            );
            let id = tex.id();
            st.screens.insert(c.it.part, (tex, stamp));
            id
        }
    };
    let r = fit_pixels(
        c.body,
        vec2(tw as f32, th as f32),
        c.ui.ctx().pixels_per_point(),
    );
    let p = c.ui.painter_at(c.body);
    p.image(
        tex,
        r,
        Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    p.rect_stroke(
        r,
        0.0,
        Stroke::new(1.0, Color32::from_gray(60)),
        StrokeKind::Outside,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
