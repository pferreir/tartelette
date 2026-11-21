//! What the dock shows of a run: the serial consoles' text, the firmware's
//! log and the USB traffic, kept from each [`Output`] and capped, oldest
//! first.
use std::collections::VecDeque;

use tartelette_runtime::{LogEntry, Output, usb};

/// Never keep more than this much text per console.
const TEXT_KEEP: usize = 256 * 1024;
/// Never keep more than this many log messages or USB log entries.
const LOG_KEEP: usize = 5000;

#[derive(Default)]
pub struct History {
    /// Everything each UART sent, as text.
    pub uart: [String; 2],
    /// Everything the USB serial port sent, as text.
    pub usb_serial: String,
    pub log: VecDeque<LogEntry>,
    pub usb_log: VecDeque<usb::LogEntry>,
}

impl History {
    /// Keep the text and logs in `o`; the events and warnings stay there.
    pub fn absorb(&mut self, o: &mut Output) {
        for (text, bytes) in self.uart.iter_mut().zip(&o.uart) {
            push_text(text, bytes);
        }
        push_text(&mut self.usb_serial, &o.usb_serial);
        push_capped(&mut self.log, o.log.drain(..));
        push_capped(&mut self.usb_log, o.usb_log.drain(..));
    }

    /// The console of UART `n`, or of the USB serial port for `None`.
    pub fn console(&mut self, uart: Option<usize>) -> &mut String {
        match uart {
            Some(n) => &mut self.uart[n],
            None => &mut self.usb_serial,
        }
    }
}

fn push_text(text: &mut String, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    text.push_str(&String::from_utf8_lossy(bytes));
    if text.len() > TEXT_KEEP {
        let mut cut = text.len() - TEXT_KEEP / 2;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text.drain(..cut);
    }
}

fn push_capped<T>(q: &mut VecDeque<T>, items: impl IntoIterator<Item = T>) {
    q.extend(items);
    let extra = q.len().saturating_sub(LOG_KEEP);
    q.drain(..extra);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consoles_keep_their_newest_text_on_a_char_boundary() {
        let mut h = History::default();
        let mut o = Output::default();
        o.uart[1] = "é".repeat(TEXT_KEEP).into_bytes();
        h.absorb(&mut o);
        assert!(h.uart[1].len() <= TEXT_KEEP);
        assert!(h.uart[1].chars().all(|c| c == 'é'));
        assert!(h.uart[0].is_empty() && h.usb_serial.is_empty());
        o.uart[0] = b"hi".to_vec();
        o.usb_serial = b"usb".to_vec();
        h.absorb(&mut o);
        assert_eq!(h.uart[0], "hi");
        assert_eq!(h.console(None), "usb");
    }
}
