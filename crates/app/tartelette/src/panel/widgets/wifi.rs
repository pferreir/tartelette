//! `cyw43439`: the Wi-Fi LED, link and address, and the Bluetooth side.
use super::*;

/// The one-line story of a Wi-Fi chip, and whether it is an error.
pub(in crate::panel) fn wifi_status(chip: &Cyw43439, notes: &WifiNotes) -> (String, bool) {
    let ip = |chip: &Cyw43439| {
        chip.lease().map_or_else(
            || "no address yet".to_string(),
            |a| format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]),
        )
    };
    if notes.unavailable && !chip.network_available() {
        let why = if cfg!(target_arch = "wasm32") {
            "Wi-Fi unavailable in the browser"
        } else {
            "Wi-Fi unavailable: no network"
        };
        return (why.into(), true);
    }
    if !chip.firmware_running() {
        return ("radio off".into(), false);
    }
    match chip.link() {
        Link::Up(_) => (
            format!("{} \u{2022} {}", chip.ssid().unwrap_or("?"), ip(chip)),
            false,
        ),
        Link::Joining => (
            format!("joining {}\u{2026}", chip.ssid().unwrap_or("?")),
            false,
        ),
        Link::Down if notes.join_failed => ("join failed".into(), true),
        Link::Down => (
            notes.scanned.map_or("idle".into(), |n| {
                format!("idle \u{2022} last scan found {n}")
            }),
            false,
        ),
    }
}

/// The one-line story of the chip's Bluetooth side, if the firmware uses
/// it, and whether it is an error.
pub(in crate::panel) fn bt_status(chip: &Cyw43439, notes: &WifiNotes) -> Option<(String, bool)> {
    if !chip.bt_up() {
        return None;
    }
    if notes.bt_unavailable && !chip.bt_backend().available() {
        let why = if cfg!(target_arch = "wasm32") {
            "Bluetooth unavailable in the browser"
        } else {
            "Bluetooth unavailable: no radio (see --bt)"
        };
        return Some((why.into(), true));
    }
    let (scanning, advertising, links) = chip.bt_activity();
    let mut parts = Vec::new();
    if links > 0 {
        parts.push(format!("{links} connected"));
    }
    if advertising {
        parts.push("advertising".into());
    }
    if scanning {
        parts.push("scanning".into());
    }
    let what = if parts.is_empty() {
        "idle".into()
    } else {
        parts.join(" \u{2022} ")
    };
    Some((format!("Bluetooth {what}"), false))
}

pub(in crate::panel) fn wifi(c: Ctx, st: &PanelState) {
    let session: &Session = c.s;
    let Some(chip) = session.sys.board.device::<Cyw43439>(&c.it.name) else {
        return;
    };
    let notes = st.wifi.get(&c.it.part).copied().unwrap_or_default();
    let (status, is_error) = wifi_status(chip, &notes);
    let r = (c.body.height().min(c.body.width() / 3.0) * 0.28).max(4.0);
    lamp(
        c.ui,
        c.body.left_center() + vec2(r + 4.0, 0.0),
        r,
        Color32::from_rgb(40, 230, 70),
        chip.led(),
    );
    let text = Rect::from_min_max(
        pos2(c.body.left() + 2.0 * r + 14.0, c.body.top()),
        c.body.right_bottom(),
    );
    c.ui.scope_builder(egui::UiBuilder::new().max_rect(text), |ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(&status).color(if is_error {
                ui.visuals().error_fg_color
            } else {
                ui.visuals().text_color()
            }));
            if let Some((bt, is_error)) = bt_status(chip, &notes) {
                ui.label(egui::RichText::new(bt).color(if is_error {
                    ui.visuals().error_fg_color
                } else {
                    ui.visuals().text_color()
                }));
            }
            ui.label(
                egui::RichText::new(format!(
                    "{} \u{2022} {} network{}",
                    mac_text(chip.mac()),
                    chip.aps().len(),
                    if chip.aps().len() == 1 { "" } else { "s" }
                ))
                .small()
                .weak(),
            );
        });
    });
}

fn mac_text(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pico_w_board_lays_out_and_its_wifi_widget_reports_state() {
        let b = test_board(include_str!("../../../../../../boards/pico-w.toml"));
        let (l, warnings) = Layout::new(&b);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(matches!(l.items[0].widget, Widget::Wifi));
        let chip = b.device::<Cyw43439>("wifi").unwrap();
        // Fresh board: radio off. After an attempt with no network, say why.
        assert_eq!(
            wifi_status(chip, &WifiNotes::default()),
            ("radio off".to_string(), false)
        );
        let (text, error) = wifi_status(
            chip,
            &WifiNotes {
                unavailable: true,
                ..Default::default()
            },
        );
        assert!(error && text.starts_with("Wi-Fi unavailable"), "{text}");
        let mut st = PanelState::default();
        st.absorb(
            &[
                BoardEvent {
                    cycles: 0,
                    part: 0,
                    label: "join",
                    value: 0,
                },
                BoardEvent {
                    cycles: 0,
                    part: 0,
                    label: "scan",
                    value: 2,
                },
            ],
            0.0,
        );
        assert!(st.wifi[&0].join_failed && st.wifi[&0].scanned == Some(2));
        // Bluetooth says nothing until the firmware brings it up.
        assert_eq!(
            bt_status(
                chip,
                &WifiNotes {
                    bt_unavailable: true,
                    ..Default::default()
                }
            ),
            None
        );
        st.absorb(
            &[BoardEvent {
                cycles: 0,
                part: 0,
                label: "bt_unavailable",
                value: 1,
            }],
            0.0,
        );
        assert!(st.wifi[&0].bt_unavailable);
    }
}
