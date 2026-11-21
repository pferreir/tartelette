//! A real MIDI input port, for the firmware's MIDI in (a UART receiver).
//!
//! Native it listens to an ALSA / CoreMIDI / WinMM port, in the browser to a
//! WebMIDI one (the same `midir` calls; the browser asks for permission the
//! first time, so there it only starts on [`MidiIn::start`]). What arrives
//! waits here until a front end [takes](MidiIn::take) it for the UART.
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use midir::{Ignore, MidiInput, MidiInputConnection};

/// Bytes kept while the run is paused or busy; past this the oldest go.
const KEEP: usize = 4096;
const CLIENT: &str = "tartelette";

/// What the port's callback fills and [`MidiIn::take`] empties.
#[derive(Default)]
struct Wire {
    bytes: Mutex<VecDeque<u8>>,
    total: AtomicU64,
}

impl Wire {
    fn push(&self, data: &[u8]) {
        push_capped(
            &mut self.bytes.lock().unwrap_or_else(|e| e.into_inner()),
            data,
        );
        self.total.fetch_add(data.len() as u64, Ordering::Relaxed);
    }
}

fn push_capped(q: &mut VecDeque<u8>, data: &[u8]) {
    q.extend(data);
    let extra = q.len().saturating_sub(KEEP);
    q.drain(..extra);
}

pub struct MidiIn {
    /// Whether ports are being listed (the browser waits for a click).
    started: bool,
    ports: Vec<String>,
    selected: Option<String>,
    conn: Option<MidiInputConnection<()>>,
    wire: Arc<Wire>,
    pub error: Option<String>,
}

impl Default for MidiIn {
    fn default() -> Self {
        Self::new()
    }
}

impl MidiIn {
    pub fn new() -> Self {
        Self {
            started: !cfg!(target_arch = "wasm32"),
            ports: Vec::new(),
            selected: None,
            conn: None,
            wire: Arc::default(),
            error: None,
        }
    }

    /// Have the ports been listed? In the browser this asks for permission,
    /// so it waits for [`MidiIn::start`] (a click).
    pub fn started(&self) -> bool {
        self.started
    }

    /// List the ports for the first time.
    pub fn start(&mut self) {
        self.started = true;
        self.refresh();
    }

    /// The input ports, as of the last [`MidiIn::refresh`].
    pub fn ports(&self) -> &[String] {
        &self.ports
    }

    /// The port being listened to.
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Bytes received so far, for an activity light.
    pub fn total(&self) -> u64 {
        self.wire.total.load(Ordering::Relaxed)
    }

    /// The bytes received since the last call.
    pub fn take(&mut self) -> Vec<u8> {
        self.wire
            .bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    /// List the ports again.
    pub fn refresh(&mut self) {
        match MidiInput::new(CLIENT) {
            Ok(input) => {
                self.ports = input
                    .ports()
                    .iter()
                    .filter_map(|p| input.port_name(p).ok())
                    .collect()
            }
            Err(e) => self.error = Some(format!("MIDI unavailable: {e}")),
        }
    }

    /// Listen to the port called `name` (or to none), dropping the old one.
    /// `wake` runs on MIDI's thread after each message, to wake the front end.
    pub fn select(&mut self, name: Option<String>, wake: impl Fn() + Send + 'static) {
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
        let Some(port) = input
            .ports()
            .into_iter()
            .find(|p| input.port_name(p).is_ok_and(|n| n == name))
        else {
            return self.error = Some(format!("{name}: port is gone"));
        };
        let wire = self.wire.clone();
        let on_message = move |_: u64, data: &[u8], _: &mut ()| {
            wire.push(data);
            wake();
        };
        match input.connect(&port, "midi in", on_message, ()) {
            Ok(conn) => {
                self.conn = Some(conn);
                self.selected = Some(name);
            }
            Err(e) => self.error = Some(format!("{name}: {e}")),
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
        let mut m = MidiIn::new();
        m.wire.push(&[0x90, 60, 100]);
        assert_eq!(m.take(), [0x90, 60, 100]);
        assert!(m.take().is_empty());
        assert_eq!(m.total(), 3);
    }
}
