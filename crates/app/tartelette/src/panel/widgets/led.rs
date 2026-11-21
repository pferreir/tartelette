//! `led` and `ws2812`: round lamps, one or a grid of them.
use super::*;

/// A round lamp of `color` at `center`, lit or dark.
pub(in crate::panel) fn lamp(ui: &Ui, center: Pos2, radius: f32, color: Color32, lit: bool) {
    let p = ui.painter();
    if lit {
        p.circle_filled(center, radius * 1.5, color.gamma_multiply(0.18));
        p.circle_filled(center, radius, color);
        p.circle_filled(
            center - Vec2::splat(radius * 0.3),
            radius * 0.3,
            Color32::from_white_alpha(90),
        );
    } else {
        let dim = |v: u8| (v as f32 * 0.2) as u8;
        p.circle_filled(
            center,
            radius,
            Color32::from_rgb(dim(color.r()), dim(color.g()), dim(color.b())),
        );
    }
    p.circle_stroke(
        center,
        radius,
        Stroke::new(1.0, Color32::from_black_alpha(120)),
    );
}

pub(in crate::panel) fn led(c: Ctx, color: Color32) {
    let Some(l) = c.device::<Led>() else { return };
    let r = (c.body.width().min(c.body.height()) * 0.32).max(4.0);
    lamp(c.ui, c.body.center(), r, color, l.lit);
}

/// Best grid (columns, rows) for `n` equal circles in `size`.
pub(in crate::panel) fn circle_grid(n: usize, size: Vec2) -> (usize, usize, f32) {
    (1..=n.max(1))
        .map(|cols| {
            let rows = n.div_ceil(cols).max(1);
            (cols, rows, (size.x / cols as f32).min(size.y / rows as f32))
        })
        .max_by(|a, b| a.2.total_cmp(&b.2))
        .unwrap()
}

pub(in crate::panel) fn leds(c: Ctx) {
    let Some(chain) = c.device::<Ws2812>() else {
        return;
    };
    let colours = chain.leds();
    let (cols, rows, d) = circle_grid(colours.len(), c.body.size());
    let start = c.body.center() - vec2(cols as f32, rows as f32) * d / 2.0;
    for (i, rgb) in colours.iter().enumerate() {
        let center = start + vec2((i % cols) as f32 + 0.5, (i / cols) as f32 + 0.5) * d;
        // Real LEDs are bright at low codes: lift the levels so dim colours show.
        let lift = |v: u8| ((v as f32 / 255.0).powf(1.0 / 2.2) * 255.0) as u8;
        let on = rgb.iter().any(|&v| v > 0);
        let color = if on {
            Color32::from_rgb(lift(rgb[0]), lift(rgb[1]), lift(rgb[2]))
        } else {
            Color32::from_gray(70)
        };
        lamp(c.ui, center, d * 0.36, color, on);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circle_grid_prefers_the_shape_of_the_space() {
        assert_eq!(circle_grid(8, vec2(400.0, 50.0)).0, 8);
        assert_eq!(circle_grid(8, vec2(50.0, 400.0)).0, 1);
        assert_eq!(circle_grid(8, vec2(100.0, 50.0)).0, 4);
    }
}
