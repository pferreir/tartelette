//! `sdcard`: the card's status, with load, save and browse buttons.
use super::*;

pub(in crate::panel) fn sdcard(c: Ctx, out: &mut Vec<Request>) {
    let Some(card) = c.device::<SdCard>() else {
        return;
    };
    let inserted = card.inserted();
    let status = if inserted {
        let mib = card.blocks() as f64 * 512.0 / (1024.0 * 1024.0);
        let state = if card.dirty() { "written" } else { "unchanged" };
        format!("{mib:.0} MiB \u{2022} {state}")
    } else {
        "No card".to_string()
    };
    let name = c.it.name.clone();
    // Two rows: the card glyph and its status, then the buttons across the
    // whole width, so they fit in one line of a 2x1 cell.
    c.ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(c.body)
            .layout(egui::Layout::top_down(egui::Align::Min).with_main_align(egui::Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 6.0);
            ui.horizontal(|ui| {
                let h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
                let (glyph, _) = ui.allocate_exact_size(vec2(h * 0.75, h), Sense::hover());
                sd_glyph(ui, glyph, inserted);
                ui.add_space(2.0);
                let text = egui::RichText::new(status);
                ui.add(egui::Label::new(if inserted { text } else { text.weak() }).truncate());
            });
            ui.horizontal(|ui| {
                let load = if inserted { "Load" } else { "Insert" };
                if ui
                    .small_button(load)
                    .on_hover_text("load a disk image")
                    .clicked()
                {
                    out.push(Request::LoadSd(name.clone()));
                }
                if !inserted {
                    return;
                }
                if ui
                    .small_button("Save")
                    .on_hover_text("save the card's image to a file")
                    .clicked()
                {
                    out.push(Request::SaveSd(name.clone()));
                }
                if ui
                    .small_button("Browse")
                    .on_hover_text("look at the files on the card")
                    .clicked()
                {
                    out.push(Request::BrowseSd(name));
                }
            });
        },
    );
}

/// An SD card outline (corner clipped top right), filled when one is in.
fn sd_glyph(ui: &Ui, r: Rect, inserted: bool) {
    let v = ui.visuals();
    let cut = r.width() * 0.3;
    let pts = vec![
        r.left_top(),
        pos2(r.right() - cut, r.top()),
        pos2(r.right(), r.top() + cut),
        r.right_bottom(),
        r.left_bottom(),
    ];
    let stroke = egui::Stroke::new(1.5, v.widgets.inactive.fg_stroke.color);
    let fill = if inserted {
        v.selection.bg_fill
    } else {
        Color32::TRANSPARENT
    };
    let p = ui.painter();
    if inserted {
        p.add(egui::Shape::convex_polygon(pts, fill, stroke));
        // Contacts along the top edge.
        let n = 4;
        let w = (r.width() - cut) / (n as f32 * 2.0);
        for i in 0..n {
            let x = r.left() + w * (0.5 + 2.0 * i as f32);
            p.rect_filled(
                Rect::from_min_size(pos2(x, r.top() + 2.0), vec2(w, r.height() * 0.22)),
                1.0,
                v.extreme_bg_color,
            );
        }
    } else {
        p.add(egui::Shape::dashed_line(
            &[pts, vec![r.left_top()]].concat(),
            stroke,
            3.0,
            2.0,
        ));
    }
}
