//! The device's USB-MIDI interface, connected to the host's MIDI system.
//!
//! On Linux (ALSA) and macOS (CoreMIDI) `--usb-bridge midi` makes virtual
//! ports named after the emulator, so the emulated device shows up to other
//! programs as a MIDI device does. Windows and browsers can't make virtual
//! ports; there (and anywhere, when the virtual ports are off) the console's
//! USB tab picks existing ports to send the device's messages to and take
//! messages for it from (a loopback port, or a real synth).
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use usbhost::function::Midi;

const CLIENT: &str = "tartelette";
/// The device's MIDI out (other programs read it) and MIDI in (they write
/// to it). midir makes each virtual port in a client of its own.
const PORT_OUT: &str = "USB MIDI out";
const PORT_IN: &str = "USB MIDI in";
/// Messages kept for the device while it isn't reading.
const KEEP: usize = 1024;

type Inbox = Arc<Mutex<VecDeque<Vec<u8>>>>;

pub struct MidiBridge {
    /// Where the device's messages go.
    out: Option<MidiOutputConnection>,
    out_name: Option<String>,
    /// What feeds messages to the device (keeps the callback alive).
    _input: Option<MidiInputConnection<()>>,
    in_name: Option<String>,
    inbox: Inbox,
    /// The ports are virtual (made by us), not chosen.
    virtual_ports: bool,
    /// Port lists, read when the user opens the chooser.
    outputs: Vec<String>,
    inputs: Vec<String>,
    /// The browser asks for permission: only list ports after a click.
    started: bool,
    pub error: Option<String>,
    /// Messages each way, for the activity counters.
    pub sent: u64,
    pub received: u64,
}

impl MidiBridge {
    pub fn new(virtual_ports: bool) -> Result<Self, String> {
        let mut b = Self {
            out: None,
            out_name: None,
            _input: None,
            in_name: None,
            inbox: Arc::default(),
            virtual_ports: false,
            outputs: Vec::new(),
            inputs: Vec::new(),
            started: !cfg!(target_arch = "wasm32"),
            error: None,
            sent: 0,
            received: 0,
        };
        if virtual_ports {
            b.make_virtual()?;
        }
        Ok(b)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn make_virtual(&mut self) -> Result<(), String> {
        use midir::os::unix::{VirtualInput, VirtualOutput};
        let out = MidiOutput::new(CLIENT)
            .map_err(|e| e.to_string())
            .and_then(|o| o.create_virtual(PORT_OUT).map_err(|e| e.to_string()))
            .map_err(|e| format!("--usb-bridge midi: {e}"))?;
        let inbox = self.inbox.clone();
        let input = MidiInput::new(CLIENT)
            .map_err(|e| e.to_string())
            .and_then(|i| {
                i.create_virtual(PORT_IN, move |_, msg, _| push(&inbox, msg), ())
                    .map_err(|e| e.to_string())
            })
            .map_err(|e| format!("--usb-bridge midi: {e}"))?;
        self.out = Some(out);
        self._input = Some(input);
        self.out_name = Some(format!("{CLIENT}: {PORT_OUT}"));
        self.in_name = Some(format!("{CLIENT}: {PORT_IN}"));
        self.virtual_ports = true;
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn make_virtual(&mut self) -> Result<(), String> {
        Err("--usb-bridge midi: virtual MIDI ports need Linux or macOS; pick ports in the console's USB tab instead".into())
    }

    /// Hand the device's messages out and the host's in.
    pub fn exchange(&mut self, m: &mut Midi) {
        for (_cable, msg) in m.rx.drain(..) {
            self.received += 1;
            if let Some(out) = &mut self.out
                && let Err(e) = out.send(&msg)
            {
                self.error = Some(format!("MIDI out: {e}"));
            }
        }
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        for msg in inbox.drain(..) {
            self.sent += 1;
            m.tx.push_back((0, msg));
        }
    }

    /// Are messages going anywhere?
    pub fn connected(&self) -> bool {
        self.out.is_some() || self._input.is_some()
    }

    /// Where the bridge goes, in words.
    pub fn describe(&self) -> String {
        match (&self.out_name, &self.in_name) {
            _ if self.virtual_ports => {
                format!("virtual ports \"{CLIENT}: {PORT_OUT}\" and \"{PORT_IN}\"")
            }
            (None, None) => "not connected".into(),
            (o, i) => format!(
                "out → {}, in ← {}",
                o.as_deref().unwrap_or("nothing"),
                i.as_deref().unwrap_or("nothing")
            ),
        }
    }

    /// Read the port lists again.
    pub fn refresh(&mut self) {
        match (MidiOutput::new(CLIENT), MidiInput::new(CLIENT)) {
            (Ok(o), Ok(i)) => {
                self.outputs = o
                    .ports()
                    .iter()
                    .filter_map(|p| o.port_name(p).ok())
                    .collect();
                self.inputs = i
                    .ports()
                    .iter()
                    .filter_map(|p| i.port_name(p).ok())
                    .collect();
            }
            (Err(e), _) => self.error = Some(format!("MIDI unavailable: {e}")),
            (_, Err(e)) => self.error = Some(format!("MIDI unavailable: {e}")),
        }
    }

    /// Send the device's messages to the port `name` (None: nowhere).
    pub fn connect_out(&mut self, name: Option<String>) {
        self.out = None;
        self.out_name = None;
        self.error = None;
        let Some(name) = name else { return };
        let r = MidiOutput::new(CLIENT)
            .map_err(|e| e.to_string())
            .and_then(|o| {
                let port = o
                    .ports()
                    .into_iter()
                    .find(|p| o.port_name(p).is_ok_and(|n| n == name))
                    .ok_or("port is gone")?;
                o.connect(&port, "usb midi out").map_err(|e| e.to_string())
            });
        match r {
            Ok(c) => {
                self.out = Some(c);
                self.out_name = Some(name);
            }
            Err(e) => self.error = Some(format!("{name}: {e}")),
        }
    }

    /// Feed the device messages from the port `name` (None: from nowhere).
    pub fn connect_in(&mut self, name: Option<String>) {
        self._input = None;
        self.in_name = None;
        self.error = None;
        let Some(name) = name else { return };
        let inbox = self.inbox.clone();
        let r = MidiInput::new(CLIENT)
            .map_err(|e| e.to_string())
            .and_then(|i| {
                let port = i
                    .ports()
                    .into_iter()
                    .find(|p| i.port_name(p).is_ok_and(|n| n == name))
                    .ok_or("port is gone")?;
                i.connect(&port, "usb midi in", move |_, msg, _| push(&inbox, msg), ())
                    .map_err(|e| e.to_string())
            });
        match r {
            Ok(c) => {
                self._input = Some(c);
                self.in_name = Some(name);
            }
            Err(e) => self.error = Some(format!("{name}: {e}")),
        }
    }

    /// Are the ports virtual (made by the emulator) rather than chosen?
    pub fn virtual_ports(&self) -> bool {
        self.virtual_ports
    }

    /// Have the port lists been read? In the browser this asks for
    /// permission, so it waits for [`MidiBridge::start`] (a click).
    pub fn started(&self) -> bool {
        self.started
    }

    /// Read the port lists for the first time.
    pub fn start(&mut self) {
        self.started = true;
        self.refresh();
    }

    /// The ports the device's messages can go to.
    pub fn outputs(&self) -> &[String] {
        &self.outputs
    }

    /// The ports that can feed messages to the device.
    pub fn inputs(&self) -> &[String] {
        &self.inputs
    }

    /// The port the device's messages go to.
    pub fn out_name(&self) -> Option<&str> {
        self.out_name.as_deref()
    }

    /// The port feeding messages to the device.
    pub fn in_name(&self) -> Option<&str> {
        self.in_name.as_deref()
    }
}

fn push(inbox: &Inbox, msg: &[u8]) {
    let mut q = inbox.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() >= KEEP {
        q.pop_front();
    }
    q.push_back(msg.to_vec());
}
