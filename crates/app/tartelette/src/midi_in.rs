//! A MIDI input in the toolbar: a chooser for the port (see [`MidiIn`]),
//! one for where on the board it goes (see [`Dest`]), and an activity
//! light. There are two, numbered from 0.
use egui::{Color32, RichText, Ui};
use pcb::Board;
use tartelette_runtime::uart::midi::{self, Dest, MidiIn};
use web_time::{Duration, Instant};

/// How often the list of ports is read again.
const REFRESH: Duration = Duration::from_secs(1);
/// How long the activity light stays on after a byte.
const LIGHT: Duration = Duration::from_millis(120);

pub struct MidiInPicker {
    n: usize,
    port: MidiIn,
    /// Where the user sent it, over the board's default.
    dest: Option<Dest>,
    refreshed: Option<Instant>,
    /// Bytes received as of the last frame, and when the count last moved.
    seen: u64,
    lit_at: Option<Instant>,
}

impl MidiInPicker {
    pub fn new(n: usize) -> Self {
        Self {
            n,
            port: MidiIn::new(),
            dest: None,
            refreshed: None,
            seen: 0,
            lit_at: None,
        }
    }

    /// The bytes received since the last call.
    pub fn take(&mut self) -> Vec<u8> {
        self.port.take()
    }

    /// Whether a port is chosen.
    pub fn in_use(&self) -> bool {
        self.port.selected().is_some()
    }

    pub fn started(&self) -> bool {
        self.port.started()
    }

    /// List the ports (in the browser, this asks for permission).
    pub fn start(&mut self) {
        self.refreshed = Some(Instant::now());
        self.port.start();
    }

    /// Where the bytes go on `board`: as chosen, if `board` still has it,
    /// else the board's default for this input.
    pub fn dest(&self, board: &Board) -> Dest {
        self.dest
            .clone()
            .filter(|d| midi::destinations(board).contains(d))
            .unwrap_or_else(|| midi::default_dest(board, self.n))
    }

    fn refresh(&mut self) {
        self.refreshed = Some(Instant::now());
        self.port.refresh();
    }

    /// The choosers and the light; the destinations are `board`'s, when
    /// there is one.
    pub fn ui(&mut self, ui: &mut Ui, board: Option<&Board>) {
        if !self.port.started() {
            if ui
                .button("MIDI in…")
                .on_hover_text("Choose a MIDI input port (the browser asks for permission)")
                .clicked()
            {
                self.start();
            }
            return;
        }
        if self.refreshed.is_none_or(|t| t.elapsed() >= REFRESH) {
            self.refresh();
        }
        let mut choice = None;
        let n = self.n + 1;
        egui::ComboBox::from_id_salt(("midi-in", self.n))
            .selected_text(
                self.port
                    .selected()
                    .map_or(format!("MIDI {n}: off"), |p| format!("MIDI {n}: {p}")),
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
        if let Some(board) = board {
            let now = self.dest(board);
            egui::ComboBox::from_id_salt(("midi-dest", self.n))
                .selected_text(format!("→ {}", now.label(board)))
                .show_ui(ui, |ui| {
                    for d in midi::destinations(board) {
                        if ui.selectable_label(d == now, d.label(board)).clicked() {
                            self.dest = Some(d);
                        }
                    }
                })
                .response
                .on_hover_text("Where on the board this input goes");
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
