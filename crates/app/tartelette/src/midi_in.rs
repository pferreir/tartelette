//! The toolbar's MIDI input: a chooser for the port that feeds the
//! firmware's MIDI in (see [`MidiIn`]) and an activity light.
use egui::{Color32, RichText, Ui};
use tartelette_runtime::uart::midi::MidiIn;
use web_time::{Duration, Instant};

/// How often the list of ports is read again.
const REFRESH: Duration = Duration::from_secs(1);
/// How long the activity light stays on after a byte.
const LIGHT: Duration = Duration::from_millis(120);

pub struct MidiInPicker {
    port: MidiIn,
    refreshed: Option<Instant>,
    /// Bytes received as of the last frame, and when the count last moved.
    seen: u64,
    lit_at: Option<Instant>,
}

impl MidiInPicker {
    pub fn new() -> Self {
        Self {
            port: MidiIn::new(),
            refreshed: None,
            seen: 0,
            lit_at: None,
        }
    }

    /// The bytes received since the last call.
    pub fn take(&mut self) -> Vec<u8> {
        self.port.take()
    }

    fn refresh(&mut self) {
        self.refreshed = Some(Instant::now());
        self.port.refresh();
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        if !self.port.started() {
            if ui
                .button("MIDI in…")
                .on_hover_text("Choose a MIDI input port (the browser asks for permission)")
                .clicked()
            {
                self.refreshed = Some(Instant::now());
                self.port.start();
            }
            return;
        }
        if self.refreshed.is_none_or(|t| t.elapsed() >= REFRESH) {
            self.refresh();
        }
        let mut choice = None;
        egui::ComboBox::from_id_salt("midi-in")
            .selected_text(
                self.port
                    .selected()
                    .map_or("MIDI in: off".to_string(), |n| format!("MIDI: {n}")),
            )
            .width(150.0)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.port.selected().is_none(), "off")
                    .clicked()
                {
                    choice = Some(None);
                }
                for name in self.port.ports() {
                    if ui
                        .selectable_label(self.port.selected() == Some(name), name)
                        .clicked()
                    {
                        choice = Some(Some(name.clone()));
                    }
                }
                if self.port.ports().is_empty() {
                    ui.weak("no MIDI input ports");
                }
            });
        if let Some(name) = choice {
            let ctx = ui.ctx().clone();
            self.port.select(name, move || ctx.request_repaint());
        }
        let total = self.port.total();
        if total != self.seen {
            self.seen = total;
            self.lit_at = Some(Instant::now());
        }
        let lit = self.lit_at.is_some_and(|t| t.elapsed() < LIGHT);
        if lit {
            ui.ctx().request_repaint_after(LIGHT);
        }
        let color = if lit {
            Color32::from_rgb(40, 230, 70)
        } else {
            ui.visuals().weak_text_color()
        };
        ui.label(RichText::new("●").color(color))
            .on_hover_text("MIDI activity");
        if let Some(e) = &self.port.error {
            ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
        }
    }
}
