//! The emulated PC's side of the USB cable, outside the emulator: input for
//! the serial port, and the bridges that hand the device's functions to the
//! host OS (see [`UsbBridges`]).
//!
//! Without a bridge every function still works inside the emulator: what the
//! serial port sends and the traffic log go to the run's [`Output`], and HID
//! reports and MIDI messages show in the log (and are otherwise dropped).
use std::collections::VecDeque;

use usbhost::{
    Host,
    function::Function,
    log::{Event, Kind},
};

use crate::Output;

pub mod midi;

use midi::MidiBridge;

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
    /// Typed into the console, for the device.
    pub input: VecDeque<u8>,
    /// What is attached, in words, once enumerated.
    pub summary: Option<String>,
    /// News from the bridges, until they go to the run's output.
    notes: Vec<String>,
    generation: u32,
    #[cfg(target_os = "linux")]
    pty: Option<bridge_usb::Pty>,
    #[cfg(target_os = "linux")]
    hid: bool,
    #[cfg(target_os = "linux")]
    uhid: Vec<(usize, bridge_usb::Uhid)>,
    pub midi: MidiBridge,
}

impl UsbSide {
    pub fn new(bridges: &UsbBridges) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        let pty = match bridges.serial {
            true => Some(bridge_usb::Pty::open(bridges.serial_link.as_deref())?),
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
            input: VecDeque::new(),
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
    pub fn bridged(&self) -> bool {
        #[cfg(target_os = "linux")]
        if self.pty.is_some() || self.hid {
            return true;
        }
        self.midi.connected()
    }

    /// Move everything between the host's functions and the outside world,
    /// and what the device sent to `out`. `seconds` converts host time
    /// (cycles) for the log.
    pub(crate) fn exchange(
        &mut self,
        host: &mut Host,
        seconds: impl Fn(u64) -> f64,
        out: &mut Output,
    ) {
        out.notes(self.notes.drain(..));
        for event in host.take_events() {
            if let Kind::Note(n) = &event.kind
                && n.starts_with("configured ")
            {
                self.summary = Some(n["configured ".len()..].to_string());
            }
            if matches!(event.kind, Kind::Detached) {
                self.summary = None;
            }
            out.usb_log(LogEntry {
                at: seconds(event.at),
                event,
            });
        }
        if host.generation() != self.generation {
            self.generation = host.generation();
            self.rebind(host);
        }
        self.serial(host, out);
        #[cfg(target_os = "linux")]
        self.hid(host);
        out.notes(self.notes.drain(..));
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

    fn serial(&mut self, host: &mut Host, out: &mut Output) {
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
        out.usb_serial(&rx);
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
                let dev = bridge_usb::UhidDevice {
                    name: &format!("{name} (tartelette, if {})", h.interface),
                    vendor,
                    product,
                    country: h.country,
                    report_descriptor: &h.report_descriptor,
                };
                match bridge_usb::Uhid::create(&dev) {
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
