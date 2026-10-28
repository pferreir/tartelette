//! What the host found on the device, as the frontends see it: one entry per
//! function, with queues in each direction. The host fills and empties the
//! queues as the device's endpoints allow; frontends do the rest.
use alloc::{collections::VecDeque, vec::Vec};

use crate::midi;

/// Bytes a serial port holds from the device before the host stops reading
/// (the device then waits, as with a real host whose program doesn't read).
pub const SERIAL_KEEP: usize = 16 * 1024;
/// Reports or messages held from the device before the host stops reading.
pub const MESSAGES_KEEP: usize = 256;

// A device has a handful of functions, made once: no point boxing them.
#[allow(clippy::large_enum_variant)]
pub enum Function {
    Serial(Serial),
    Hid(Hid),
    Midi(Midi),
    /// An interface no class driver here handles.
    Other(Other),
}

impl Function {
    pub fn interface(&self) -> u8 {
        match self {
            Function::Serial(f) => f.interface,
            Function::Hid(f) => f.interface,
            Function::Midi(f) => f.interface,
            Function::Other(f) => f.interface,
        }
    }
}

/// A CDC-ACM serial port.
pub struct Serial {
    /// The communication interface (the one class requests go to).
    pub interface: u8,
    pub data_interface: u8,
    /// From the device.
    pub rx: VecDeque<u8>,
    /// To the device.
    pub tx: VecDeque<u8>,
    /// DTR and RTS as the host wants them; they reach the device with
    /// SET_CONTROL_LINE_STATE. Most firmware only talks once DTR is up.
    pub dtr: bool,
    pub rts: bool,
    /// Baud rate, stop bits (0: 1, 1: 1.5, 2: 2), parity (0: none), data bits.
    pub line_coding: (u32, u8, u8, u8),
    /// The device's last SERIAL_STATE notification (DCD, DSR, ...).
    pub serial_state: u16,
    pub(crate) lines_sent: Option<(bool, bool)>,
    pub(crate) coding_sent: Option<(u32, u8, u8, u8)>,
}

impl Serial {
    pub(crate) fn new(interface: u8, data_interface: u8, lines: bool) -> Self {
        Self {
            interface,
            data_interface,
            rx: VecDeque::new(),
            tx: VecDeque::new(),
            dtr: lines,
            rts: lines,
            line_coding: (115_200, 0, 0, 8),
            serial_state: 0,
            lines_sent: None,
            coding_sent: None,
        }
    }

    pub(crate) fn coding_bytes(&self) -> Vec<u8> {
        let (baud, stop, parity, bits) = self.line_coding;
        let mut v = baud.to_le_bytes().to_vec();
        v.extend([stop, parity, bits]);
        v
    }
}

/// HID report types, as in GET_REPORT/SET_REPORT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportType {
    Input = 1,
    Output = 2,
    Feature = 3,
}

/// A GET_REPORT or SET_REPORT to run on the control pipe for a frontend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HidRequest {
    /// Chosen by the frontend, handed back with the reply.
    pub id: u32,
    pub kind: ReportType,
    pub report_id: u8,
    /// None: GET_REPORT. Some: SET_REPORT with these bytes (report ID first
    /// when the device numbers its reports).
    pub set: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HidReply {
    pub id: u32,
    /// The report read (empty for a SET_REPORT), or None if the device refused.
    pub data: Option<Vec<u8>>,
}

/// A HID interface.
pub struct Hid {
    pub interface: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub country: u8,
    /// Empty until the host has read it; frontends wait for `ready`.
    pub report_descriptor: Vec<u8>,
    pub ready: bool,
    /// Input reports from the device, as the interrupt IN endpoint gave them.
    pub input: VecDeque<Vec<u8>>,
    /// Output reports for the device: over its interrupt OUT endpoint, or with
    /// SET_REPORT if it has none.
    pub output: VecDeque<Vec<u8>>,
    pub requests: VecDeque<HidRequest>,
    pub replies: VecDeque<HidReply>,
    pub(crate) report_len: u16,
    pub(crate) has_out: bool,
}

/// A USB-MIDI streaming interface. Messages are whole, with their cable.
pub struct Midi {
    pub interface: u8,
    /// Cables (embedded jacks) in each direction.
    pub cables_in: u8,
    pub cables_out: u8,
    /// From the device.
    pub rx: VecDeque<(u8, Vec<u8>)>,
    /// To the device.
    pub tx: VecDeque<(u8, Vec<u8>)>,
    pub(crate) decoder: midi::Decoder,
}

pub struct Other {
    pub interface: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

/// A name for an interface class, for the log.
pub fn class_name(class: u8, subclass: u8) -> &'static str {
    match (class, subclass) {
        (0x01, 0x01) => "audio control",
        (0x01, 0x02) => "audio streaming",
        (0x01, 0x03) => "MIDI streaming",
        (0x02, _) => "CDC",
        (0x03, _) => "HID",
        (0x07, _) => "printer",
        (0x08, _) => "mass storage",
        (0x0A, _) => "CDC data",
        (0xE0, _) => "wireless",
        (0xEF, _) => "miscellaneous",
        (0xFE, 0x01) => "DFU",
        (0xFF, _) => "vendor",
        _ => "unknown class",
    }
}
