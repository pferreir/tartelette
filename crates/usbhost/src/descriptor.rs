//! Standard requests and the descriptors the host reads during enumeration.
use alloc::{string::String, vec::Vec};

pub const GET_STATUS: u8 = 0x00;
pub const CLEAR_FEATURE: u8 = 0x01;
pub const SET_FEATURE: u8 = 0x03;
pub const SET_ADDRESS: u8 = 0x05;
pub const GET_DESCRIPTOR: u8 = 0x06;
pub const SET_DESCRIPTOR: u8 = 0x07;
pub const GET_CONFIGURATION: u8 = 0x08;
pub const SET_CONFIGURATION: u8 = 0x09;
pub const GET_INTERFACE: u8 = 0x0A;
pub const SET_INTERFACE: u8 = 0x0B;

pub const DT_DEVICE: u8 = 1;
pub const DT_CONFIG: u8 = 2;
pub const DT_STRING: u8 = 3;
pub const DT_INTERFACE: u8 = 4;
pub const DT_ENDPOINT: u8 = 5;
pub const DT_IAD: u8 = 11;
pub const DT_HID: u8 = 0x21;
pub const DT_HID_REPORT: u8 = 0x22;
pub const DT_CS_INTERFACE: u8 = 0x24;
pub const DT_CS_ENDPOINT: u8 = 0x25;

pub const CLASS_AUDIO: u8 = 0x01;
pub const CLASS_CDC: u8 = 0x02;
pub const CLASS_HID: u8 = 0x03;
pub const CLASS_CDC_DATA: u8 = 0x0A;
pub const SUBCLASS_ACM: u8 = 0x02;
pub const SUBCLASS_MIDI: u8 = 0x03;
/// CDC union functional descriptor subtype.
const CDC_UNION: u8 = 0x06;
/// USB-MIDI class-specific endpoint descriptor subtype.
const MS_GENERAL: u8 = 0x01;

/// The 8 bytes of a SETUP packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    pub fn new(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Self {
        Self {
            request_type,
            request,
            value,
            index,
            length,
        }
    }

    pub fn get_descriptor(kind: u8, index: u8, lang: u16, length: u16) -> Self {
        Self::new(
            0x80,
            GET_DESCRIPTOR,
            (kind as u16) << 8 | index as u16,
            lang,
            length,
        )
    }

    pub fn bytes(&self) -> [u8; 8] {
        let [v0, v1] = self.value.to_le_bytes();
        let [i0, i1] = self.index.to_le_bytes();
        let [l0, l1] = self.length.to_le_bytes();
        [self.request_type, self.request, v0, v1, i0, i1, l0, l1]
    }

    pub fn from_bytes(b: [u8; 8]) -> Self {
        Self::new(
            b[0],
            b[1],
            u16::from_le_bytes([b[2], b[3]]),
            u16::from_le_bytes([b[4], b[5]]),
            u16::from_le_bytes([b[6], b[7]]),
        )
    }

    /// Device to host data stage.
    pub fn is_in(&self) -> bool {
        self.request_type & 0x80 != 0
    }
}

/// The parts of the device descriptor the host uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub usb: u16,
    pub class: u8,
    pub max_packet0: u8,
    pub vendor: u16,
    pub product: u16,
    pub release: u16,
    pub manufacturer: u8,
    pub product_string: u8,
    pub serial: u8,
    pub configurations: u8,
}

impl DeviceDescriptor {
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < 18 || d[1] != DT_DEVICE {
            return None;
        }
        let w = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]);
        Some(Self {
            usb: w(2),
            class: d[4],
            max_packet0: d[7],
            vendor: w(8),
            product: w(10),
            release: w(12),
            manufacturer: d[14],
            product_string: d[15],
            serial: d[16],
            configurations: d[17],
        })
    }
}

/// Text of a string descriptor (UTF-16LE).
pub fn parse_string(d: &[u8]) -> Option<String> {
    if d.len() < 2 || d[1] != DT_STRING {
        return None;
    }
    let len = (d[0] as usize).min(d.len());
    let units: Vec<u16> = d[2..len]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    Some(
        char::decode_utf16(units)
            .map(|c| c.unwrap_or('\u{FFFD}'))
            .collect(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferType {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// With the direction bit (0x80 = IN).
    pub address: u8,
    pub kind: TransferType,
    pub max_packet: u16,
    /// Polling interval in frames (interrupt endpoints).
    pub interval: u8,
    /// USB-MIDI: embedded jacks behind this endpoint (cables).
    pub midi_jacks: u8,
}

impl Endpoint {
    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }
}

/// One interface (alternate setting 0) and what was declared inside it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Interface {
    pub number: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<Endpoint>,
    /// HID: length of the report descriptor and the country code.
    pub hid_report_len: Option<u16>,
    pub hid_country: u8,
    /// CDC: the data interfaces a union descriptor ties to this one.
    pub cdc_slaves: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Configuration {
    pub value: u8,
    pub attributes: u8,
    pub max_power_ma: u16,
    pub interfaces: Vec<Interface>,
}

impl Configuration {
    /// Parse a whole configuration descriptor (with everything after it).
    /// Alternate settings other than 0 are skipped.
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < 9 || d[1] != DT_CONFIG {
            return None;
        }
        let total = (u16::from_le_bytes([d[2], d[3]]) as usize).min(d.len());
        let mut cfg = Self {
            value: d[5],
            attributes: d[7],
            max_power_ma: d[8] as u16 * 2,
            interfaces: Vec::new(),
        };
        // Whether the descriptors being read belong to alternate setting 0.
        let mut current = false;
        let mut i = d[0] as usize;
        while i + 2 <= total {
            let len = d[i] as usize;
            if len < 2 || i + len > total {
                break;
            }
            let b = &d[i..i + len];
            match b[1] {
                DT_INTERFACE if len >= 9 => {
                    current = b[3] == 0;
                    if current {
                        cfg.interfaces.push(Interface {
                            number: b[2],
                            class: b[5],
                            subclass: b[6],
                            protocol: b[7],
                            ..Default::default()
                        });
                    }
                }
                DT_ENDPOINT if len >= 7 && current => {
                    let kind = match b[3] & 3 {
                        0 => TransferType::Control,
                        1 => TransferType::Isochronous,
                        2 => TransferType::Bulk,
                        _ => TransferType::Interrupt,
                    };
                    if let Some(itf) = cfg.interfaces.last_mut() {
                        itf.endpoints.push(Endpoint {
                            address: b[2],
                            kind,
                            max_packet: u16::from_le_bytes([b[4], b[5]]) & 0x7FF,
                            interval: b[6],
                            midi_jacks: 0,
                        });
                    }
                }
                DT_HID if len >= 9 && current => {
                    if let Some(itf) = cfg.interfaces.last_mut() {
                        itf.hid_country = b[4];
                        // (type, length) pairs follow; take the report descriptor.
                        for k in 0..b[5] as usize {
                            let o = 6 + 3 * k;
                            if o + 3 <= len && b[o] == DT_HID_REPORT {
                                itf.hid_report_len = Some(u16::from_le_bytes([b[o + 1], b[o + 2]]));
                            }
                        }
                    }
                }
                DT_CS_INTERFACE if len >= 5 && current && b[2] == CDC_UNION => {
                    if let Some(itf) = cfg.interfaces.last_mut() {
                        itf.cdc_slaves.extend_from_slice(&b[4..]);
                    }
                }
                DT_CS_ENDPOINT if len >= 4 && current && b[2] == MS_GENERAL => {
                    if let Some(ep) = cfg
                        .interfaces
                        .last_mut()
                        .and_then(|i| i.endpoints.last_mut())
                    {
                        ep.midi_jacks = b[3];
                    }
                }
                _ => {}
            }
            i += len;
        }
        Some(cfg)
    }

    pub fn interface(&self, number: u8) -> Option<&Interface> {
        self.interfaces.iter().find(|i| i.number == number)
    }
}

/// A name for a standard request, for the log.
pub fn request_name(s: &Setup) -> Option<&'static str> {
    let kind = s.request_type >> 5 & 3;
    Some(match (kind, s.request) {
        (0, GET_STATUS) => "GET_STATUS",
        (0, CLEAR_FEATURE) => "CLEAR_FEATURE",
        (0, SET_FEATURE) => "SET_FEATURE",
        (0, SET_ADDRESS) => "SET_ADDRESS",
        (0, GET_DESCRIPTOR) => "GET_DESCRIPTOR",
        (0, SET_DESCRIPTOR) => "SET_DESCRIPTOR",
        (0, GET_CONFIGURATION) => "GET_CONFIGURATION",
        (0, SET_CONFIGURATION) => "SET_CONFIGURATION",
        (0, GET_INTERFACE) => "GET_INTERFACE",
        (0, SET_INTERFACE) => "SET_INTERFACE",
        // Class requests: CDC (interface) and HID share the numbers, so the
        // direction tells them apart where they collide.
        (1, 0x01) if s.is_in() => "HID GET_REPORT",
        (1, 0x02) => "HID GET_IDLE",
        (1, 0x03) => "HID GET_PROTOCOL",
        (1, 0x09) => "HID SET_REPORT",
        (1, 0x0A) => "HID SET_IDLE",
        (1, 0x0B) => "HID SET_PROTOCOL",
        (1, 0x20) => "CDC SET_LINE_CODING",
        (1, 0x21) => "CDC GET_LINE_CODING",
        (1, 0x22) => "CDC SET_CONTROL_LINE_STATE",
        (1, 0x23) => "CDC SEND_BREAK",
        _ => return None,
    })
}

/// A name for a descriptor type, for the log.
pub fn descriptor_name(kind: u8) -> &'static str {
    match kind {
        DT_DEVICE => "device",
        DT_CONFIG => "configuration",
        DT_STRING => "string",
        DT_INTERFACE => "interface",
        DT_ENDPOINT => "endpoint",
        6 => "device qualifier",
        7 => "other speed",
        0x0F => "BOS",
        DT_HID => "HID",
        DT_HID_REPORT => "HID report",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_round_trips() {
        let s = Setup::get_descriptor(DT_STRING, 2, 0x0409, 255);
        assert_eq!(s.bytes(), [0x80, 6, 2, 3, 0x09, 0x04, 255, 0]);
        assert_eq!(Setup::from_bytes(s.bytes()), s);
        assert!(s.is_in());
    }

    #[test]
    fn strings_are_utf16() {
        assert_eq!(
            parse_string(&[8, 3, b'P', 0, b'i', 0, 0xAC, 0x20]).as_deref(),
            Some("Pi€")
        );
        assert_eq!(parse_string(&[2, 2]), None);
    }

    #[test]
    fn a_cdc_and_hid_configuration_parses() {
        #[rustfmt::skip]
        let d = [
            9, 2, 0, 0, 3, 1, 0, 0x80, 50,
            8, 11, 0, 2, 2, 2, 0, 0,                  // IAD
            9, 4, 0, 0, 1, 2, 2, 0, 0,                // CDC comm
            5, 0x24, 6, 0, 1,                         // union: 0 -> 1
            7, 5, 0x81, 3, 8, 0, 255,
            9, 4, 1, 0, 2, 10, 0, 0, 0,               // CDC data
            7, 5, 0x02, 2, 64, 0, 0,
            7, 5, 0x82, 2, 64, 0, 0,
            9, 4, 1, 1, 0, 10, 0, 0, 0,               // alt 1: skipped
            7, 5, 0x03, 2, 64, 0, 0,
            9, 4, 2, 0, 1, 3, 1, 1, 0,                // HID keyboard
            9, 0x21, 0x11, 1, 0, 1, 0x22, 63, 0,
            7, 5, 0x83, 3, 8, 0, 10,
        ];
        let mut d = d.to_vec();
        let total = d.len() as u16;
        d[2..4].copy_from_slice(&total.to_le_bytes());
        let c = Configuration::parse(&d).unwrap();
        assert_eq!((c.value, c.max_power_ma), (1, 100));
        assert_eq!(c.interfaces.len(), 3);
        assert_eq!(c.interfaces[0].cdc_slaves, [1]);
        let data = c.interface(1).unwrap();
        assert_eq!(data.endpoints.len(), 2);
        assert_eq!(data.endpoints[1].address, 0x82);
        let hid = c.interface(2).unwrap();
        assert_eq!(hid.hid_report_len, Some(63));
        assert_eq!(hid.endpoints[0].kind, TransferType::Interrupt);
        assert_eq!(hid.endpoints[0].interval, 10);
    }
}
