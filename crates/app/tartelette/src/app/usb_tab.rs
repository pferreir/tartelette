//! The dock's USB tab: what is on the port, the ports the device's MIDI
//! interface is connected to (see [`MidiBridge`]), and the traffic.
use egui::{RichText, Ui};
use tartelette_runtime::{
    Session,
    usb::{UsbSide, midi::MidiBridge},
};

use super::history::History;

/// What is on the USB port, the MIDI bridge's ports, then the traffic: one
/// line per transfer or packet.
pub fn show(ui: &mut Ui, s: &mut Session, history: &History) {
    ui.horizontal(|ui| {
        match &s.usb.summary {
            Some(d) => ui.label(d),
            None => ui.label(RichText::new("no USB device configured").weak()),
        };
        if UsbSide::has_midi(s.sys.usb_host()) {
            ui.separator();
            midi_ports(&mut s.usb.midi, ui);
            if let Some(e) = &s.usb.midi.error {
                ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
            }
        }
    });
    let row = ui.text_style_height(&egui::TextStyle::Monospace);
    let entries = &history.usb_log;
    if entries.is_empty() {
        ui.label(RichText::new("no USB traffic yet").weak());
        return;
    }
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .auto_shrink(false)
        .show_rows(ui, row, entries.len(), |ui, rows| {
            for e in entries.range(rows) {
                let text = format!("{:9.6} {}", e.at, e.event.describe());
                ui.add(egui::Label::new(RichText::new(text).monospace()).truncate());
            }
        });
}

/// Port choosers (when the ports are not virtual).
fn midi_ports(bridge: &mut MidiBridge, ui: &mut Ui) {
    if bridge.virtual_ports() {
        ui.label(format!("MIDI: {}", bridge.describe()));
        return;
    }
    if !bridge.started() {
        if ui
            .button("Connect MIDI…")
            .on_hover_text("Choose MIDI ports for the device (the browser asks for permission)")
            .clicked()
        {
            bridge.start();
        }
        return;
    }
    let mut out_choice = None;
    let mut in_choice = None;
    let mut opened = false;
    ui.label("MIDI out →");
    egui::ComboBox::from_id_salt("usb-midi-out")
        .selected_text(bridge.out_name().unwrap_or("off"))
        .width(140.0)
        .show_ui(ui, |ui| {
            opened = true;
            out_choice = choose(ui, bridge.out_name(), bridge.outputs());
        });
    ui.label("in ←");
    egui::ComboBox::from_id_salt("usb-midi-in")
        .selected_text(bridge.in_name().unwrap_or("off"))
        .width(140.0)
        .show_ui(ui, |ui| {
            opened = true;
            in_choice = choose(ui, bridge.in_name(), bridge.inputs());
        });
    if opened && bridge.outputs().is_empty() && bridge.inputs().is_empty() {
        bridge.refresh();
    }
    if let Some(c) = out_choice {
        bridge.connect_out(c);
    }
    if let Some(c) = in_choice {
        bridge.connect_in(c);
    }
    if ui
        .small_button("⟳")
        .on_hover_text("List the ports again")
        .clicked()
    {
        bridge.refresh();
    }
}

/// "off" and the ports, `current` selected. Some(choice) once one is clicked.
fn choose(ui: &mut Ui, current: Option<&str>, ports: &[String]) -> Option<Option<String>> {
    let mut choice = None;
    if ui.selectable_label(current.is_none(), "off").clicked() {
        choice = Some(None);
    }
    for n in ports {
        if ui.selectable_label(current == Some(n), n).clicked() {
            choice = Some(Some(n.clone()));
        }
    }
    choice
}
