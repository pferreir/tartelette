//! A real MIDI input port feeding the firmware's MIDI in (a UART receiver).
//!
//! Native it listens to an ALSA / CoreMIDI / WinMM port, in the browser to a
//! WebMIDI one (the same `midir` calls; the browser asks for permission the
//! first time, so there it only starts on a click).
use egui::{Color32, Context, RichText, Ui};
use midir::{Ignore, MidiInput, MidiInputConnection};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use web_time::{Duration, Instant};

/// Bytes kept while the run is paused or busy; past this the oldest go.
const KEEP: usize = 4096;
/// How often the list of ports is read again.
const REFRESH: Duration = Duration::from_secs(1);
/// How long the activity light stays on after a byte.
const LIGHT: Duration = Duration::from_millis(120);
const CLIENT: &str = "rp2040-app";

/// What the port's callback fills and the UI empties.
#[derive(Default)]
struct Wire {
    bytes: Mutex<VecDeque<u8>>,
    total: AtomicU64,
}

impl Wire {
    fn push(&self, data: &[u8]) {
        push_capped(&mut self.bytes.lock().unwrap_or_else(|e| e.into_inner()), data);
        self.total.fetch_add(data.len() as u64, Ordering::Relaxed);
    }
}

fn push_capped(q: &mut VecDeque<u8>, data: &[u8]) {
    q.extend(data);
    let extra = q.len().saturating_sub(KEEP);
    q.drain(..extra);
}

pub struct Midi {
    /// Whether ports are being listed (the browser waits for a click).
    started: bool,
    ports: Vec<String>,
    refreshed: Option<Instant>,
    selected: Option<String>,
    conn: Option<MidiInputConnection<()>>,
    wire: Arc<Wire>,
    seen: u64,
    lit_at: Option<Instant>,
    error: Option<String>,
}

impl Midi {
    pub fn new() -> Self {
        Self {
            started: !cfg!(target_arch = "wasm32"),
            ports: Vec::new(),
            refreshed: None,
            selected: None,
            conn: None,
            wire: Arc::default(),
            seen: 0,
            lit_at: None,
            error: None,
        }
    }

    /// The bytes received since the last call.
    pub fn take(&mut self) -> Vec<u8> {
        self.wire.bytes.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect()
    }

    fn refresh(&mut self) {
        self.refreshed = Some(Instant::now());
        match MidiInput::new(CLIENT) {
            Ok(input) => self.ports = input.ports().iter().filter_map(|p| input.port_name(p).ok()).collect(),
            Err(e) => self.error = Some(format!("MIDI unavailable: {e}")),
        }
    }

    /// Listen to the port called `name` (or to none), dropping the old one.
    fn select(&mut self, name: Option<String>, ctx: &Context) {
        self.conn = None;
        self.error = None;
        self.selected = None;
        let Some(name) = name else { return };
        let mut input = match MidiInput::new(CLIENT) {
            Ok(i) => i,
            Err(e) => return self.error = Some(format!("MIDI unavailable: {e}")),
        };
        // Active sensing every 300 ms would only fill the receiver's FIFO.
        input.ignore(Ignore::ActiveSense);
        let Some(port) = input.ports().into_iter().find(|p| input.port_name(p).is_ok_and(|n| n == name)) else {
            return self.error = Some(format!("{name}: port is gone"));
        };
        let (wire, ctx) = (self.wire.clone(), ctx.clone());
        let on_message = move |_: u64, data: &[u8], _: &mut ()| {
            wire.push(data);
            ctx.request_repaint();
        };
        match input.connect(&port, "midi in", on_message, ()) {
            Ok(conn) => {
                self.conn = Some(conn);
                self.selected = Some(name);
            }
            Err(e) => self.error = Some(format!("{name}: {e}")),
        }
    }

    /// The toolbar part: a port chooser and an activity light.
    pub fn ui(&mut self, ui: &mut Ui) {
        if !self.started {
            if ui.button("MIDI in…").on_hover_text("Choose a MIDI input port (the browser asks for permission)").clicked() {
                self.started = true;
                self.refresh();
            }
            return;
        }
        if self.refreshed.is_none_or(|t| t.elapsed() >= REFRESH) {
            self.refresh();
        }
        let mut choice = None;
        egui::ComboBox::from_id_salt("midi-in")
            .selected_text(self.selected.as_deref().map_or("MIDI in: off".to_string(), |n| format!("MIDI: {n}")))
            .width(150.0)
            .show_ui(ui, |ui| {
                if ui.selectable_label(self.selected.is_none(), "off").clicked() {
                    choice = Some(None);
                }
                for name in &self.ports {
                    if ui.selectable_label(self.selected.as_ref() == Some(name), name).clicked() {
                        choice = Some(Some(name.clone()));
                    }
                }
                if self.ports.is_empty() {
                    ui.weak("no MIDI input ports");
                }
            });
        if let Some(name) = choice {
            self.select(name, ui.ctx());
        }
        let total = self.wire.total.load(Ordering::Relaxed);
        if total != self.seen {
            self.seen = total;
            self.lit_at = Some(Instant::now());
        }
        let lit = self.lit_at.is_some_and(|t| t.elapsed() < LIGHT);
        if lit {
            ui.ctx().request_repaint_after(LIGHT);
        }
        let color = if lit { Color32::from_rgb(40, 230, 70) } else { ui.visuals().weak_text_color() };
        ui.label(RichText::new("●").color(color)).on_hover_text("MIDI activity");
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_queue_keeps_only_the_newest_bytes() {
        let mut q = VecDeque::new();
        push_capped(&mut q, &[1, 2, 3]);
        assert_eq!(q, [1, 2, 3]);
        push_capped(&mut q, &vec![9; KEEP]);
        assert_eq!(q.len(), KEEP);
        assert!(q.iter().all(|&b| b == 9));
    }

    #[test]
    fn take_empties_what_the_callback_filled() {
        let mut m = Midi::new();
        m.wire.push(&[0x90, 60, 100]);
        assert_eq!(m.take(), [0x90, 60, 100]);
        assert!(m.take().is_empty());
        assert_eq!(m.wire.total.load(Ordering::Relaxed), 3);
    }
}
