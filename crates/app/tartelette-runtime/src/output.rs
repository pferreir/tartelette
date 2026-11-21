//! What a run produces, between two takes: see [`Session::take_output`].
//!
//! A front end takes it after every step and keeps or prints what it wants.
//! Until it is taken it is capped, oldest first, so a front end that looks
//! away for a while (a hidden window) doesn't make it grow without bound.
//!
//! [`Session::take_output`]: crate::Session::take_output
use std::collections::VecDeque;

use pcb::BoardEvent;

use crate::{LogEntry, usb};

/// At most this many bytes per serial port.
const BYTES_KEEP: usize = 256 * 1024;
/// At most this many board events, log messages, USB log entries, warnings.
const ITEMS_KEEP: usize = 10_000;

#[derive(Default)]
pub struct Output {
    /// What each UART sent.
    pub uart: [Vec<u8>; 2],
    /// What the USB serial port sent (also when it is bridged).
    pub usb_serial: Vec<u8>,
    /// USB transfers and packets.
    pub usb_log: VecDeque<usb::LogEntry>,
    /// Board events, in the order they happened.
    pub events: VecDeque<BoardEvent>,
    /// The firmware's defmt log.
    pub log: VecDeque<LogEntry>,
    /// Wiring problems and faults the emulator spotted.
    pub warnings: VecDeque<String>,
    /// News from the bridges to the host (a pseudo-terminal opened, ...).
    pub notes: VecDeque<String>,
}

impl Output {
    pub fn is_empty(&self) -> bool {
        self.uart.iter().all(Vec::is_empty)
            && self.usb_serial.is_empty()
            && self.usb_log.is_empty()
            && self.events.is_empty()
            && self.log.is_empty()
            && self.warnings.is_empty()
            && self.notes.is_empty()
    }

    pub(crate) fn uart(&mut self, n: usize, bytes: &[u8]) {
        push_bytes(&mut self.uart[n], bytes);
    }

    pub(crate) fn usb_serial(&mut self, bytes: &[u8]) {
        push_bytes(&mut self.usb_serial, bytes);
    }

    pub(crate) fn usb_log(&mut self, e: usb::LogEntry) {
        push_item(&mut self.usb_log, e);
    }

    pub(crate) fn events(&mut self, events: impl IntoIterator<Item = BoardEvent>) {
        for e in events {
            push_item(&mut self.events, e);
        }
    }

    pub(crate) fn log(&mut self, e: LogEntry) {
        push_item(&mut self.log, e);
    }

    pub(crate) fn warnings(&mut self, w: impl IntoIterator<Item = String>) {
        for w in w {
            push_item(&mut self.warnings, w);
        }
    }

    pub(crate) fn notes(&mut self, n: impl IntoIterator<Item = String>) {
        for n in n {
            push_item(&mut self.notes, n);
        }
    }
}

fn push_bytes(v: &mut Vec<u8>, bytes: &[u8]) {
    v.extend_from_slice(bytes);
    let extra = v.len().saturating_sub(BYTES_KEEP);
    v.drain(..extra);
}

fn push_item<T>(q: &mut VecDeque<T>, item: T) {
    if q.len() >= ITEMS_KEEP {
        q.pop_front();
    }
    q.push_back(item);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untaken_output_keeps_only_the_newest() {
        let mut o = Output::default();
        assert!(o.is_empty());
        o.uart(1, &vec![1; BYTES_KEEP]);
        o.uart(1, &[2, 3]);
        assert_eq!(o.uart[1].len(), BYTES_KEEP);
        assert_eq!(o.uart[1][BYTES_KEEP - 2..], [2, 3]);
        assert!(o.uart[0].is_empty());
        o.warnings((0..ITEMS_KEEP + 5).map(|i| i.to_string()));
        assert_eq!(o.warnings.len(), ITEMS_KEEP);
        assert_eq!(o.warnings.front().map(String::as_str), Some("5"));
        assert!(!o.is_empty());
    }
}
