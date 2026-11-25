//! A real MIDI input port, for the firmware's MIDI in, and where on the
//! board its bytes go ([`Dest`]): a UART's receiver, or a `midi` part (a
//! MIDI in jack, on a UART or sending on a pin of its own).
//!
//! Native it listens to an ALSA / CoreMIDI / WinMM port, in the browser to a
//! WebMIDI one (the same `midir` calls; the browser asks for permission the
//! first time, so there it only starts on [`MidiIn::start`]). What arrives
//! waits here until a front end [takes](MidiIn::take) it to
//! [`Session::midi`](crate::Session::midi).
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use midir::{Ignore, MidiInput, MidiInputConnection};
use pcb::Board;
use pcb_devices::midi::{Midi, Route};

/// Where a MIDI input's bytes go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dest {
    Uart(usize),
    /// A `midi` part, by name.
    Part(String),
}

impl Dest {
    /// `UART1`, or a part with where it is: `din (GPIO5)`.
    pub fn label(&self, board: &Board) -> String {
        match self {
            Dest::Uart(n) => format!("UART{n}"),
            Dest::Part(name) => match board.device::<Midi>(name).map(|m| m.route) {
                Some(Route::Uart(n)) => format!("{name} (UART{n})"),
                Some(Route::Pin(p)) => format!("{name} (GPIO{p})"),
                None => format!("{name} (gone)"),
            },
        }
    }
}

/// Where MIDI can go on `board`: its `midi` parts, then the UARTs.
pub fn destinations(board: &Board) -> Vec<Dest> {
    let parts = board.parts.iter().filter(|p| p.kind == "midi");
    parts
        .map(|p| Dest::Part(p.name.clone()))
        .chain((0..2).map(Dest::Uart))
        .collect()
}

/// Where input `n` goes unless told otherwise: the board's `n`th `midi`
/// part, else UART`n`.
pub fn default_dest(board: &Board, n: usize) -> Dest {
    board
        .parts
        .iter()
        .filter(|p| p.kind == "midi")
        .nth(n)
        .map_or(Dest::Uart(n.min(1)), |p| Dest::Part(p.name.clone()))
}

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
    fn destinations_come_from_the_board() {
        let b = Board::parse_with(
            "parts.din = { kind = \"midi\", gpio = 5 }\nparts.trs = { kind = \"midi\", uart = 1 }",
            &pcb_devices::registry(),
        )
        .unwrap();
        let labels: Vec<_> = destinations(&b).iter().map(|d| d.label(&b)).collect();
        assert_eq!(labels, ["din (GPIO5)", "trs (UART1)", "UART0", "UART1"]);
        assert_eq!(default_dest(&b, 1), Dest::Part("trs".into()));
        let plain = Board::parse_with("", &pcb_devices::registry()).unwrap();
        assert_eq!(default_dest(&plain, 0), Dest::Uart(0));
        assert_eq!(default_dest(&plain, 1), Dest::Uart(1));
    }

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
