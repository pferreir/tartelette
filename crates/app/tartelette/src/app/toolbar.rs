//! The top bar's controls beside the logo: opening files, running, speed,
//! MIDI in, and the status line under them.
use egui::RichText;
use tartelette_runtime::run::Run;

use super::{App, Tab};
use crate::files::{self, Slot};

const SPEEDS: &[(f64, &str)] = &[
    (0.01, "0.01×"),
    (0.1, "0.1×"),
    (0.25, "0.25×"),
    (0.5, "0.5×"),
    (1.0, "1×"),
    (2.0, "2×"),
    (4.0, "4×"),
    (f64::INFINITY, "max"),
];

impl App {
    /// The controls and the status line beside the logo. Returns whether
    /// Reset was pressed.
    pub(super) fn toolbar(&mut self, ui: &mut egui::Ui, mut run: Option<&mut Run>) -> bool {
        let mut reset = false;
        ui.horizontal_wrapped(|ui| {
            if ui.button("Open firmware…").clicked() {
                files::pick(Slot::Firmware, &self.inbox, ui.ctx());
            }
            if ui.button("Open board…").clicked() {
                files::pick(Slot::Board, &self.inbox, ui.ctx());
            }
            ui.separator();
            let running = run.as_deref().is_some_and(|r| r.clock.running);
            let label = if running { "⏸ Pause" } else { "▶ Run" };
            if ui
                .add_enabled(run.is_some(), egui::Button::new(label))
                .on_hover_text("Pause or resume the emulation")
                .clicked()
                && let Some(r) = run.as_deref_mut()
            {
                r.clock.running = !r.clock.running;
            }
            if ui
                .add_enabled(self.firmware.is_some(), egui::Button::new("↻ Reset"))
                .on_hover_text("Power-cycle the board (SD cards keep their contents)")
                .clicked()
            {
                reset = true;
            }
            if let Some(r) = run.as_deref_mut() {
                let current = SPEEDS
                    .iter()
                    .find(|s| s.0 == r.clock.speed)
                    .map_or("?", |s| s.1);
                egui::ComboBox::from_id_salt("speed")
                    .selected_text(current)
                    .width(60.0)
                    .show_ui(ui, |ui| {
                        for &(v, name) in SPEEDS {
                            ui.selectable_value(&mut r.clock.speed, v, name);
                        }
                    })
                    .response
                    .on_hover_text("Emulation speed");
            }
            ui.separator();
            self.midi.ui(ui);
        });
        ui.horizontal_wrapped(|ui| {
            let names: Vec<&str> = self
                .firmware
                .iter()
                .chain(&self.board)
                .chain(&self.shields)
                .map(|s| s.name.as_str())
                .collect();
            if names.is_empty() {
                ui.label(RichText::new("no firmware loaded").weak());
            } else {
                ui.label(RichText::new(names.join(" + ")).weak());
            }
            if let Some(Run { session: s, clock }) = run.as_deref() {
                ui.separator();
                ui.monospace(format!("{:.6} s", s.seconds()));
                if let Some(r) = &s.halted {
                    ui.label(
                        RichText::new(format!("halted: {r}")).color(ui.visuals().warn_fg_color),
                    );
                } else if clock.running {
                    ui.monospace(format!("{:5.2}× real time", clock.measured()));
                } else {
                    ui.label("paused");
                }
            }
            let fresh = self.errors.len() - self.errors_seen;
            if fresh > 0 {
                ui.separator();
                let badge = RichText::new(format!("⚠ {fresh} new problem{}", plural(fresh)))
                    .color(ui.visuals().warn_fg_color);
                if ui
                    .add(egui::Button::new(badge).frame(false))
                    .on_hover_text("Show them")
                    .clicked()
                {
                    self.tab = Tab::Problems;
                }
            }
        });
        reset
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
