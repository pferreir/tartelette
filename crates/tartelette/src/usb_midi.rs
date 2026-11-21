//! Port choosers for the USB device's MIDI interface (see
//! [`MidiBridge`]), in the console's USB tab.
use egui::Ui;
use tartelette_runtime::midi_bridge::MidiBridge;

/// Port choosers (when the ports are not virtual).
pub fn ui(bridge: &mut MidiBridge, ui: &mut Ui) {
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
