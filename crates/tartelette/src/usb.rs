//! The emulated PC's side of the USB cable, outside the emulator: the serial
//! console text, the traffic log, and the bridges that hand the device's
//! functions to the host OS (see [`UsbBridges`]).
//!
//! Without a bridge every function still works inside the emulator: the
//! serial port's text shows in the console, and HID reports and MIDI
//! messages show in the log (and are otherwise dropped).
use std::collections::VecDeque;

use usbhost::{
    Host,
    function::Function,
    log::{Event, Kind},
};

use crate::midi_bridge::MidiBridge;

/// Never keep more than this much serial text.
const CONSOLE_KEEP: usize = 256 * 1024;
/// Never keep more than this many log entries.
const LOG_KEEP: usize = 5000;

/// Which functions go to the host OS (from the command line).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsbBridges {
    /// The serial port as a pseudo-terminal (Linux).
    pub serial: bool,
    /// A symlink to the pseudo-terminal, for scripts.
    pub serial_link: Option<std::path::PathBuf>,
    /// HID interfaces as UHID devices: real input devices (Linux).
    pub hid: bool,
    /// MIDI interfaces as virtual MIDI ports (Linux, macOS).
    pub midi: bool,
}

/// A log entry with its emulated time in seconds.
pub struct LogEntry {
    pub at: f64,
    pub event: Event,
}

pub struct UsbSide {
    /// Everything the serial port sent, as text.
    pub console: String,
    /// What it sent since the last [`UsbSide::take_fresh`] (headless runs).
    fresh: Option<Vec<u8>>,
    /// Typed into the console, for the device.
    pub input: VecDeque<u8>,
    pub log: VecDeque<LogEntry>,
    /// What is attached, in words, once enumerated.
    pub summary: Option<String>,
    /// Problems and news from the bridges, for the UI to show once.
    pub notes: Vec<String>,
    generation: u32,
    #[cfg(target_os = "linux")]
    pty: Option<usbbridge::Pty>,
    #[cfg(target_os = "linux")]
    hid: bool,
    #[cfg(target_os = "linux")]
    uhid: Vec<(usize, usbbridge::Uhid)>,
    pub midi: MidiBridge,
}

impl UsbSide {
    pub fn new(bridges: &UsbBridges) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        let pty = match bridges.serial {
            true => Some(usbbridge::Pty::open(bridges.serial_link.as_deref())?),
            false => None,
        };
        #[cfg(not(target_os = "linux"))]
        if bridges.serial || bridges.hid {
            return Err("--usb-bridge serial and hid need Linux (pty and UHID)".into());
        }
        #[cfg(target_os = "linux")]
        let notes = pty
            .iter()
            .map(|p| format!("USB serial bridged to {}", p.name()))
            .collect();
        #[cfg(not(target_os = "linux"))]
        let notes = Vec::new();
        Ok(Self {
            console: String::new(),
            fresh: None,
            input: VecDeque::new(),
            log: VecDeque::new(),
            summary: None,
            notes,
            generation: 0,
            #[cfg(target_os = "linux")]
            pty,
            #[cfg(target_os = "linux")]
            hid: bridges.hid,
            #[cfg(target_os = "linux")]
            uhid: Vec::new(),
            midi: MidiBridge::new(bridges.midi)?,
        })
    }

    /// Keep the serial output for [`UsbSide::take_fresh`] too.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // headless only
    pub fn keep_fresh(&mut self) {
        self.fresh.get_or_insert_with(Vec::new);
    }

    /// Serial output since the last call (with [`UsbSide::keep_fresh`]).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // headless only
    pub fn take_fresh(&mut self) -> Vec<u8> {
        self.fresh.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// Is the serial port bridged to a pseudo-terminal?
    pub fn serial_bridged(&self) -> bool {
        self.serial_bridge().is_some()
    }

    /// The pseudo-terminal the serial port is bridged to.
    pub fn serial_bridge(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        return self.pty.as_ref().map(|p| p.name());
        #[cfg(not(target_os = "linux"))]
        None
    }

    /// Does the device have a MIDI interface?
    pub fn has_midi(host: &Host) -> bool {
        host.functions
            .iter()
            .any(|f| matches!(f, Function::Midi(_)))
    }

    /// Is anything bridged to the host OS (so that input can come from there)?
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // headless only
    pub fn bridged(&self) -> bool {
        #[cfg(target_os = "linux")]
        if self.pty.is_some() || self.hid {
            return true;
        }
        self.midi.connected()
    }

    /// Move everything between the host's functions and the outside world.
    /// `seconds` converts host time (cycles) for the log.
    pub fn exchange(&mut self, host: &mut Host, seconds: impl Fn(u64) -> f64) {
        for event in host.take_events() {
            if self.log.len() >= LOG_KEEP {
                self.log.pop_front();
            }
            if let Kind::Note(n) = &event.kind
                && n.starts_with("configured ")
            {
                self.summary = Some(n["configured ".len()..].to_string());
            }
            if matches!(event.kind, Kind::Detached) {
                self.summary = None;
            }
            self.log.push_back(LogEntry {
                at: seconds(event.at),
                event,
            });
        }
        if host.generation() != self.generation {
            self.generation = host.generation();
            self.rebind(host);
        }
        self.serial(host);
        #[cfg(target_os = "linux")]
        self.hid(host);
        for f in &mut host.functions {
            match f {
                Function::Midi(m) => self.midi.exchange(m),
                // Unbridged HID: the reports are in the log.
                #[cfg(not(target_os = "linux"))]
                Function::Hid(h) => h.input.clear(),
                _ => {}
            }
        }
    }

    /// The set of functions changed: drop the bridges of the old ones.
    fn rebind(&mut self, _host: &Host) {
        #[cfg(target_os = "linux")]
        self.uhid.clear();
    }

    fn serial(&mut self, host: &mut Host) {
        let Some(s) = host.serial() else {
            return;
        };
        let rx: Vec<u8> = s.rx.drain(..).collect();
        s.tx.append(&mut self.input);
        #[cfg(target_os = "linux")]
        if let Some(pty) = &mut self.pty {
            // DTR follows the terminal being open, as with a real port.
            s.dtr = pty.is_open();
            pty.write(&rx);
            s.tx.extend(pty.read());
        }
        if rx.is_empty() {
            return;
        }
        if let Some(fresh) = &mut self.fresh {
            fresh.extend_from_slice(&rx);
        }
        self.console.push_str(&String::from_utf8_lossy(&rx));
        if self.console.len() > CONSOLE_KEEP {
            let mut cut = self.console.len() - CONSOLE_KEEP / 2;
            while !self.console.is_char_boundary(cut) {
                cut += 1;
            }
            self.console.drain(..cut);
        }
    }

    #[cfg(target_os = "linux")]
    fn hid(&mut self, host: &mut Host) {
        let name = host
            .device()
            .map(|d| d.product.clone().unwrap_or_else(|| d.name()))
            .unwrap_or_default();
        let (vendor, product) = host
            .device()
            .map_or((0, 0), |d| (d.descriptor.vendor, d.descriptor.product));
        for (i, f) in host.functions.iter_mut().enumerate() {
            let Function::Hid(h) = f else { continue };
            if !self.hid {
                h.input.clear();
                continue;
            }
            if !h.ready {
                continue;
            }
            if !self.uhid.iter().any(|(k, _)| *k == i) {
                let dev = usbbridge::UhidDevice {
                    name: &format!("{name} (tartelette, if {})", h.interface),
                    vendor,
                    product,
                    country: h.country,
                    report_descriptor: &h.report_descriptor,
                };
                match usbbridge::Uhid::create(&dev) {
                    Ok(u) => {
                        self.notes
                            .push(format!("USB HID interface {} bridged to UHID", h.interface));
                        self.uhid.push((i, u));
                    }
                    Err(e) => {
                        self.notes.push(e);
                        self.hid = false;
                        continue;
                    }
                }
            }
            if let Some((_, u)) = self.uhid.iter_mut().find(|(k, _)| *k == i) {
                u.exchange(h);
            }
        }
    }
}
