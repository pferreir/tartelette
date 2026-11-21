//! `mcp23008`: a cell per pin, following the wiring or the item's mode.
use super::*;

/// How a panel cell treats its expander pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CellRole {
    /// A bare pin the cell drives (through `gpN`) or only shows.
    Bare(PinMode),
    /// A wired switch the cell actuates (through `swN`).
    Switch {
        latching: bool,
    },
    Led,
}

/// What a cell does for `pin`: the item's `mode` if it has one, else what
/// the board wires to the pin.
fn cell_role(x: &Expander, dev: &Mcp23008, pin: u8) -> CellRole {
    if let Some(mode) = x.mode {
        return CellRole::Bare(mode);
    }
    match dev.wiring(pin).wire {
        Wire::Bare => CellRole::Bare(PinMode::Hold),
        Wire::Switch(s) => CellRole::Switch {
            latching: s.latching,
        },
        Wire::Led { .. } => CellRole::Led,
    }
}

pub(in crate::panel) fn expander(
    c: Ctx,
    st: &mut PanelState,
    x: &Expander,
    out: &mut Vec<Request>,
) {
    let Some(dev) = c.device::<Mcp23008>() else {
        return;
    };
    let (levels, actuated, lit) = (dev.pin_levels(), dev.actuated(), dev.lit());
    // Pins (bare) and switches (wired) the panel holds down.
    let driven = st.driven.entry(c.it.part).or_default();
    let n = x.pins.len();
    let (cols, _, d) = circle_grid(n, c.body.size());
    let rows = n.div_ceil(cols);
    let start = c.body.center() - vec2(cols as f32, rows as f32) * d / 2.0;
    for (i, &pin) in x.pins.iter().enumerate() {
        let cell = Rect::from_min_size(
            start + vec2((i % cols) as f32, (i / cols) as f32) * d,
            Vec2::splat(d),
        )
        .shrink(d * 0.08);
        let role = cell_role(x, dev, pin);
        let wiring = dev.wiring(pin);
        let bit = 1u8 << pin;
        let interactive = !matches!(role, CellRole::Led | CellRole::Bare(PinMode::View));
        let key = x
            .keys
            .get(i)
            .copied()
            .filter(|_| c.keys_free && interactive);
        let sense = if interactive {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        };
        let mut hover = match wiring.name {
            Some(name) => format!("GP{pin} {name}"),
            None => format!("GP{pin}"),
        };
        match role {
            CellRole::Switch { latching } => {
                hover += if latching { ": toggle" } else { ": switch" };
                if wiring.bounce > 0 {
                    hover += " (bouncy)";
                }
            }
            CellRole::Led => hover += ": LED",
            CellRole::Bare(_) => {}
        }
        hover += if levels & bit != 0 { ", high" } else { ", low" };
        if let Some(k) = x.keys.get(i).filter(|_| interactive) {
            hover += &format!(" [{}]", k.name());
        }
        let resp =
            c.ui.interact(cell, c.id(("pin", c.it.cell, pin)), sense)
                .on_hover_text(hover);
        let clicked = resp.clicked() || key.is_some_and(|k| c.ui.input(|i| i.key_pressed(k)));
        let held =
            resp.is_pointer_button_down_on() || key.is_some_and(|k| c.ui.input(|i| i.key_down(k)));
        let is_driven = *driven & bit != 0;
        match role {
            CellRole::Led | CellRole::Bare(PinMode::View) => {}
            CellRole::Switch { latching: true } => {
                if clicked {
                    c.input(out, &format!("sw{pin}"), (actuated & bit == 0) as i64);
                }
            }
            CellRole::Switch { latching: false } => {
                if held != is_driven {
                    *driven ^= bit;
                    c.input(out, &format!("sw{pin}"), held as i64);
                }
            }
            CellRole::Bare(mode) => {
                let want = match mode {
                    PinMode::Toggle => is_driven ^ clicked,
                    _ => held,
                };
                if want != is_driven {
                    *driven ^= bit;
                    c.input(
                        out,
                        &format!("gp{pin}"),
                        if want { x.active_high as i64 } else { -1 },
                    );
                }
            }
        }
        let on = match role {
            CellRole::Led => lit & bit != 0,
            CellRole::Switch { .. } => actuated & bit != 0,
            CellRole::Bare(_) => levels & bit != 0,
        };
        let v = c.ui.visuals();
        let p = c.ui.painter();
        let fill = match role {
            CellRole::Led if on => Color32::from_rgb(255, 48, 32),
            _ if on => Color32::from_rgb(230, 170, 40),
            _ => Color32::from_gray(45),
        };
        if role == CellRole::Led {
            p.circle_filled(cell.center(), cell.width().min(cell.height()) / 2.0, fill);
        } else {
            p.rect_filled(cell, 4.0, fill);
        }
        if *driven & bit != 0 && matches!(role, CellRole::Bare(_)) {
            p.rect_stroke(
                cell,
                4.0,
                Stroke::new(2.0, v.selection.stroke.color),
                StrokeKind::Inside,
            );
        }
        let ink = if on {
            Color32::BLACK
        } else {
            Color32::from_gray(170)
        };
        let text = x
            .labels
            .get(i)
            .cloned()
            .or_else(|| wiring.name.map(String::from))
            .unwrap_or_else(|| pin.to_string());
        // Shrink long labels to fit the cell (monospace glyphs are ~0.6 em wide).
        let chars = text.chars().count().max(1) as f32;
        let size = (d * 0.35).min(14.0).min(cell.width() * 0.9 / (chars * 0.6));
        p.text(
            cell.center(),
            Align2::CENTER_CENTER,
            text,
            FontId::monospace(size),
            ink,
        );
        if let Some(&k) = x.keys.get(i).filter(|_| interactive) {
            let size = (d * 0.2).clamp(8.0, 11.0);
            Keycaps::new(c.ui, &[k], size).draw(c.ui, cell.right_top() + vec2(-2.0, size * 0.75));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expander_cells_follow_the_wiring_unless_given_a_mode() {
        let b = test_board(
            "buses.i2c0 = { sda = 0, scl = 1 }\n\
             parts.x = { kind = \"mcp23008\", bus = \"i2c0\", pins = [\
               { pin = 0, as = \"led\" }, { pin = 1, as = \"switch\" }, { pin = 2, as = \"toggle\" }] }",
        );
        let dev = b.device::<Mcp23008>("x").unwrap();
        let wired = Expander::default();
        let roles: Vec<_> = (0..4).map(|p| cell_role(&wired, dev, p)).collect();
        assert_eq!(
            roles,
            [
                CellRole::Led,
                CellRole::Switch { latching: false },
                CellRole::Switch { latching: true },
                CellRole::Bare(PinMode::Hold),
            ]
        );
        let viewed = Expander {
            mode: Some(PinMode::View),
            ..Expander::default()
        };
        assert!((0..4).all(|p| cell_role(&viewed, dev, p) == CellRole::Bare(PinMode::View)));
    }
}
