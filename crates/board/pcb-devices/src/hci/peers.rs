//! Mock Bluetooth LE devices for the [virtual controller](super::virt):
//! deterministic stand-ins for real ones, for tests and for trying firmware
//! without a radio. Each speaks ATT over L2CAP channel 4 with a 23-byte MTU
//! (the default every host supports, and what keeps each PDU in one 27-byte
//! LE data packet).
//!
//! * A **MIDI peripheral** advertises the BLE MIDI service and serves a tiny
//!   GATT table (GAP device name, the MIDI I/O characteristic and its CCCD).
//!   MIDI given to [`Peer::midi`] goes out as notifications once the
//!   firmware subscribed; what the firmware writes to the characteristic is
//!   reported as `ble_midi_out`.
//! * A **battery peripheral** serves the Battery Service; once subscribed it
//!   notifies a level that drops by one every second.
//! * A **cat printer** (an MXW01) serves the printer service `AE30`; what
//!   the firmware sends it is [printed](super::catprinter) on its paper.
//! * A **central** connects to the firmware when it advertises, discovers its
//!   services and characteristics, reads the device name and subscribes to
//!   everything that notifies or indicates. Values it receives are reported
//!   as `ble_notify`.
use alloc::{collections::VecDeque, string::String, vec, vec::Vec};

use super::catprinter::CatPrinter;

pub const ATT_CID: u16 = 4;
pub const ATT_MTU: usize = 23;

// ATT opcodes.
const ERROR_RSP: u8 = 0x01;
const MTU_REQ: u8 = 0x02;
const MTU_RSP: u8 = 0x03;
const FIND_INFO_REQ: u8 = 0x04;
const FIND_INFO_RSP: u8 = 0x05;
const FIND_BY_TYPE_REQ: u8 = 0x06;
const FIND_BY_TYPE_RSP: u8 = 0x07;
const READ_BY_TYPE_REQ: u8 = 0x08;
const READ_BY_TYPE_RSP: u8 = 0x09;
const READ_REQ: u8 = 0x0A;
const READ_RSP: u8 = 0x0B;
const READ_BLOB_REQ: u8 = 0x0C;
const READ_BLOB_RSP: u8 = 0x0D;
const READ_BY_GROUP_REQ: u8 = 0x10;
const READ_BY_GROUP_RSP: u8 = 0x11;
const WRITE_REQ: u8 = 0x12;
const WRITE_RSP: u8 = 0x13;
const NOTIFY: u8 = 0x1B;
const INDICATE: u8 = 0x1D;
const CONFIRM: u8 = 0x1E;
const WRITE_CMD: u8 = 0x52;

// ATT error codes.
const E_INVALID_HANDLE: u8 = 0x01;
const E_WRITE_NOT_PERMITTED: u8 = 0x03;
const E_NOT_SUPPORTED: u8 = 0x06;
const E_INVALID_OFFSET: u8 = 0x07;
const E_NOT_FOUND: u8 = 0x0A;

// GATT attribute types.
const PRIMARY_SERVICE: u16 = 0x2800;
const CHARACTERISTIC: u16 = 0x2803;
const CCCD: u16 = 0x2902;
const DEVICE_NAME: u16 = 0x2A00;
const BATTERY_SERVICE: u16 = 0x180F;
const BATTERY_LEVEL: u16 = 0x2A19;
/// The MXW01's service and its command, reply and data characteristics.
pub const PRINTER_SERVICE: u16 = 0xAE30;
const PRINTER_CONTROL: u16 = 0xAE01;
const PRINTER_REPLY: u16 = 0xAE02;
const PRINTER_DATA: u16 = 0xAE03;

// Characteristic properties.
const PROP_READ: u8 = 0x02;
const PROP_WRITE_NO_RSP: u8 = 0x04;
const PROP_WRITE: u8 = 0x08;
const PROP_NOTIFY: u8 = 0x10;
const PROP_INDICATE: u8 = 0x20;

/// BLE MIDI service 03B80E5A-EDE8-4B33-A751-6CE34EC4C700, as sent (little-endian).
pub const MIDI_SERVICE: [u8; 16] = [
    0x00, 0xC7, 0xC4, 0x4E, 0xE3, 0x6C, 0x51, 0xA7, 0x33, 0x4B, 0xE8, 0xED, 0x5A, 0x0E, 0xB8, 0x03,
];
/// BLE MIDI I/O characteristic 7772E5DB-3868-4112-A1A9-F2669D106BF3.
pub const MIDI_IO: [u8; 16] = [
    0xF3, 0x6B, 0x10, 0x9D, 0x66, 0xF2, 0xA9, 0xA1, 0x12, 0x41, 0x68, 0x38, 0xDB, 0xE5, 0x72, 0x77,
];

// AD types.
const AD_FLAGS: u8 = 0x01;
const AD_UUIDS16: u8 = 0x03;
const AD_UUIDS128: u8 = 0x07;
const AD_SHORT_NAME: u8 = 0x08;
const AD_NAME: u8 = 0x09;

fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2)
        .map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

fn att_error(req: u8, handle: u16, code: u8) -> Vec<u8> {
    let h = handle.to_le_bytes();
    vec![ERROR_RSP, req, h[0], h[1], code]
}

/// An attribute: its handle, type (2 or 16 bytes, little-endian) and value.
struct Attr {
    handle: u16,
    typ: Vec<u8>,
    value: Vec<u8>,
    writable: bool,
}

/// A GATT server: a flat attribute table and the ATT requests over it.
struct Server {
    attrs: Vec<Attr>,
}

impl Server {
    fn new() -> Self {
        Self { attrs: Vec::new() }
    }

    fn next_handle(&self) -> u16 {
        self.attrs.len() as u16 + 1
    }

    fn push(&mut self, typ: &[u8], value: Vec<u8>, writable: bool) -> u16 {
        let handle = self.next_handle();
        self.attrs.push(Attr {
            handle,
            typ: typ.into(),
            value,
            writable,
        });
        handle
    }

    fn service(&mut self, uuid: &[u8]) {
        self.push(&PRIMARY_SERVICE.to_le_bytes(), uuid.into(), false);
    }

    /// Add a characteristic (declaration, value and, if it notifies, a
    /// CCCD). Returns the value handle.
    fn characteristic(&mut self, uuid: &[u8], props: u8, value: Vec<u8>) -> u16 {
        let value_handle = self.next_handle() + 1;
        let mut decl = vec![props];
        decl.extend_from_slice(&value_handle.to_le_bytes());
        decl.extend_from_slice(uuid);
        self.push(&CHARACTERISTIC.to_le_bytes(), decl, false);
        self.push(uuid, value, props & (PROP_WRITE | PROP_WRITE_NO_RSP) != 0);
        if props & (PROP_NOTIFY | PROP_INDICATE) != 0 {
            self.push(&CCCD.to_le_bytes(), vec![0, 0], true);
        }
        value_handle
    }

    fn set(&mut self, handle: u16, value: Vec<u8>) {
        if let Some(a) = self.attrs.get_mut((handle as usize).wrapping_sub(1)) {
            a.value = value;
        }
    }

    fn attr(&self, handle: u16) -> Option<&Attr> {
        self.attrs.get((handle as usize).checked_sub(1)?)
    }

    fn is(attr: &Attr, typ: u16) -> bool {
        attr.typ == typ.to_le_bytes()
    }

    /// Last handle of the service starting at `start`.
    fn group_end(&self, start: u16) -> u16 {
        self.attrs
            .iter()
            .find(|a| a.handle > start && Self::is(a, PRIMARY_SERVICE))
            .map_or(self.attrs.len() as u16, |a| a.handle - 1)
    }

    /// Notifications enabled on the characteristic whose value is `handle`?
    fn subscribed(&self, handle: u16) -> bool {
        self.attr(handle + 1)
            .is_some_and(|a| Self::is(a, CCCD) && a.value.first().is_some_and(|v| v & 1 != 0))
    }

    /// Answer one request. `written` gets (handle, value) of each write.
    fn request(&mut self, pdu: &[u8], written: &mut Vec<(u16, Vec<u8>)>) -> Option<Vec<u8>> {
        let op = *pdu.first()?;
        let range = || (le16(pdu, 1), le16(pdu, 3));
        let in_range = |a: &Attr, (s, e): (u16, u16)| a.handle >= s && a.handle <= e;
        Some(match op {
            MTU_REQ => {
                let mut r = vec![MTU_RSP];
                r.extend_from_slice(&(ATT_MTU as u16).to_le_bytes());
                r
            }
            FIND_INFO_REQ => {
                let mut r = vec![FIND_INFO_RSP, 0];
                for a in self.attrs.iter().filter(|a| in_range(a, range())) {
                    let format = if a.typ.len() == 2 { 1 } else { 2 };
                    if r[1] == 0 {
                        r[1] = format;
                    }
                    if r[1] != format || r.len() + 2 + a.typ.len() > ATT_MTU {
                        break;
                    }
                    r.extend_from_slice(&a.handle.to_le_bytes());
                    r.extend_from_slice(&a.typ);
                }
                if r[1] == 0 {
                    return Some(att_error(op, range().0, E_NOT_FOUND));
                }
                r
            }
            FIND_BY_TYPE_REQ => {
                let (typ, value) = (&pdu[5..7.min(pdu.len())], pdu.get(7..).unwrap_or(&[]));
                let mut r = vec![FIND_BY_TYPE_RSP];
                for a in self
                    .attrs
                    .iter()
                    .filter(|a| in_range(a, range()) && a.typ == typ && a.value == value)
                {
                    if r.len() + 4 > ATT_MTU {
                        break;
                    }
                    let end = if Self::is(a, PRIMARY_SERVICE) {
                        self.group_end(a.handle)
                    } else {
                        a.handle
                    };
                    r.extend_from_slice(&a.handle.to_le_bytes());
                    r.extend_from_slice(&end.to_le_bytes());
                }
                if r.len() == 1 {
                    return Some(att_error(op, range().0, E_NOT_FOUND));
                }
                r
            }
            READ_BY_TYPE_REQ | READ_BY_GROUP_REQ => {
                let typ = pdu.get(5..).unwrap_or(&[]);
                let group = op == READ_BY_GROUP_REQ;
                if group && typ != PRIMARY_SERVICE.to_le_bytes() {
                    return Some(att_error(op, range().0, E_NOT_SUPPORTED));
                }
                let mut r = vec![
                    if group {
                        READ_BY_GROUP_RSP
                    } else {
                        READ_BY_TYPE_RSP
                    },
                    0,
                ];
                for a in self
                    .attrs
                    .iter()
                    .filter(|a| in_range(a, range()) && a.typ == typ)
                {
                    let head = if group { 4 } else { 2 };
                    let len = (head + a.value.len()).min(ATT_MTU - 2);
                    if r[1] == 0 {
                        r[1] = len as u8;
                    }
                    if r[1] as usize != len || r.len() + len > ATT_MTU {
                        break;
                    }
                    r.extend_from_slice(&a.handle.to_le_bytes());
                    if group {
                        r.extend_from_slice(&self.group_end(a.handle).to_le_bytes());
                    }
                    r.extend_from_slice(&a.value[..len - head]);
                }
                if r[1] == 0 {
                    return Some(att_error(op, range().0, E_NOT_FOUND));
                }
                r
            }
            READ_REQ | READ_BLOB_REQ => {
                let handle = le16(pdu, 1);
                let offset = if op == READ_BLOB_REQ {
                    le16(pdu, 3) as usize
                } else {
                    0
                };
                let Some(a) = self.attr(handle) else {
                    return Some(att_error(op, handle, E_INVALID_HANDLE));
                };
                if offset > a.value.len() {
                    return Some(att_error(op, handle, E_INVALID_OFFSET));
                }
                let mut r = vec![if op == READ_REQ {
                    READ_RSP
                } else {
                    READ_BLOB_RSP
                }];
                let rest = &a.value[offset..];
                r.extend_from_slice(&rest[..rest.len().min(ATT_MTU - 1)]);
                r
            }
            WRITE_REQ | WRITE_CMD => {
                let handle = le16(pdu, 1);
                let data = pdu.get(3..).unwrap_or(&[]);
                let ok = match self.attrs.get_mut((handle as usize).wrapping_sub(1)) {
                    Some(a) if a.writable => {
                        a.value = data.into();
                        written.push((handle, data.into()));
                        Ok(())
                    }
                    Some(_) => Err(E_WRITE_NOT_PERMITTED),
                    None => Err(E_INVALID_HANDLE),
                };
                match (op, ok) {
                    (WRITE_CMD, _) => return None,
                    (_, Ok(())) => vec![WRITE_RSP],
                    (_, Err(e)) => att_error(op, handle, e),
                }
            }
            // Responses, notifications and confirmations need no answer.
            _ if op & 1 == 1 || op == NOTIFY || op == INDICATE => return None,
            _ if op & 0x40 != 0 => return None,
            _ => att_error(op, 0, E_NOT_SUPPORTED),
        })
    }
}

/// A characteristic the mock central found on the firmware.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub uuid: Vec<u8>,
    pub props: u8,
    pub value_handle: u16,
    /// Last handle that belongs to it (descriptors included).
    pub end: u16,
    /// Last value read or notified.
    pub value: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Mtu,
    /// Primary services from this handle on.
    Services(u16),
    /// Characteristics of service n, from this handle on.
    Chars(usize, u16),
    /// Descriptors of characteristic n, from this handle on.
    Descriptors(usize, u16),
    ReadName,
    Subscribe(usize),
    Done,
}

/// The GATT client script of the mock central: one request at a time.
struct Client {
    step: Step,
    /// A request is out and its response hasn't come.
    waiting: bool,
    /// (first handle, last handle, UUID).
    services: Vec<(u16, u16, Vec<u8>)>,
    chars: Vec<Found>,
    /// (characteristic index, CCCD handle) for each one to subscribe to.
    cccds: Vec<(usize, u16)>,
    name: Option<String>,
}

impl Client {
    fn new() -> Self {
        Self {
            step: Step::Mtu,
            waiting: false,
            services: Vec::new(),
            chars: Vec::new(),
            cccds: Vec::new(),
            name: None,
        }
    }

    /// The next request of the script, if it's time for one.
    fn next_request(&mut self) -> Option<Vec<u8>> {
        if self.waiting {
            return None;
        }
        let req = loop {
            match self.step {
                Step::Mtu => {
                    let mut r = vec![MTU_REQ];
                    r.extend_from_slice(&(ATT_MTU as u16).to_le_bytes());
                    break r;
                }
                Step::Services(start) => {
                    break range_req(
                        READ_BY_GROUP_REQ,
                        start,
                        0xFFFF,
                        &PRIMARY_SERVICE.to_le_bytes(),
                    );
                }
                Step::Chars(s, start) => {
                    break range_req(
                        READ_BY_TYPE_REQ,
                        start,
                        self.services[s].1,
                        &CHARACTERISTIC.to_le_bytes(),
                    );
                }
                Step::Descriptors(c, from) => {
                    if from > self.chars[c].end {
                        self.next_char(c);
                        continue;
                    }
                    let mut r = vec![FIND_INFO_REQ];
                    r.extend_from_slice(&from.to_le_bytes());
                    r.extend_from_slice(&self.chars[c].end.to_le_bytes());
                    break r;
                }
                Step::ReadName => match self
                    .chars
                    .iter()
                    .find(|c| c.uuid == DEVICE_NAME.to_le_bytes() && c.props & PROP_READ != 0)
                {
                    Some(c) => {
                        let mut r = vec![READ_REQ];
                        r.extend_from_slice(&c.value_handle.to_le_bytes());
                        break r;
                    }
                    None => self.step = Step::Subscribe(0),
                },
                Step::Subscribe(i) => {
                    let Some(&(c, cccd)) = self.cccds.get(i) else {
                        self.step = Step::Done;
                        continue;
                    };
                    let bits: u16 = if self.chars[c].props & PROP_NOTIFY != 0 {
                        1
                    } else {
                        2
                    };
                    let mut r = vec![WRITE_REQ];
                    r.extend_from_slice(&cccd.to_le_bytes());
                    r.extend_from_slice(&bits.to_le_bytes());
                    break r;
                }
                Step::Done => return None,
            }
        };
        self.waiting = true;
        Some(req)
    }

    /// Take in the response to the outstanding request.
    fn response(&mut self, pdu: &[u8]) {
        self.waiting = false;
        let op = pdu[0];
        // Entries of a list response, `len` bytes each.
        let entries = |len: usize| pdu.get(2..).unwrap_or(&[]).chunks_exact(len.max(1));
        match self.step {
            Step::Mtu => self.step = Step::Services(1),
            Step::Services(_) => {
                let mut next = None;
                if op == READ_BY_GROUP_RSP && pdu[1] >= 6 {
                    for e in entries(pdu[1] as usize) {
                        let (start, end) = (le16(e, 0), le16(e, 2));
                        self.services.push((start, end, e[4..].into()));
                        next = end.checked_add(1);
                    }
                }
                match next {
                    Some(n) => self.step = Step::Services(n),
                    None => self.next_service(None),
                }
            }
            Step::Chars(s, _) => {
                let mut next = None;
                if op == READ_BY_TYPE_RSP && pdu[1] >= 7 {
                    for e in entries(pdu[1] as usize) {
                        let decl = le16(e, 0);
                        self.close_char(decl - 1);
                        self.chars.push(Found {
                            uuid: e[5..].into(),
                            props: e[2],
                            value_handle: le16(e, 3),
                            end: u16::MAX,
                            value: Vec::new(),
                        });
                        next = Some(decl + 1);
                    }
                }
                match next.filter(|&n| n <= self.services[s].1) {
                    Some(n) => self.step = Step::Chars(s, n),
                    None => self.next_service(Some(s)),
                }
            }
            Step::Descriptors(c, _) => {
                let mut next = None;
                if op == FIND_INFO_RSP {
                    let len = if pdu[1] == 1 { 4 } else { 18 };
                    for e in entries(len) {
                        let handle = le16(e, 0);
                        if e[2..] == CCCD.to_le_bytes()
                            && self.chars[c].props & (PROP_NOTIFY | PROP_INDICATE) != 0
                        {
                            self.cccds.push((c, handle));
                        }
                        next = handle.checked_add(1);
                    }
                }
                match next {
                    Some(n) => self.step = Step::Descriptors(c, n),
                    None => self.next_char(c),
                }
            }
            Step::ReadName => {
                if op == READ_RSP {
                    self.name = Some(String::from_utf8_lossy(&pdu[1..]).into());
                }
                self.step = Step::Subscribe(0);
            }
            Step::Subscribe(i) => self.step = Step::Subscribe(i + 1),
            Step::Done => {}
        }
    }

    /// The characteristic found last ends at `end` (if not known yet).
    fn close_char(&mut self, end: u16) {
        if let Some(prev) = self.chars.last_mut()
            && prev.end == u16::MAX
        {
            prev.end = end;
        }
    }

    /// Service `done` (if any) is explored: on to the next, or to descriptors.
    fn next_service(&mut self, done: Option<usize>) {
        if let Some(s) = done {
            self.close_char(self.services[s].1);
        }
        let next = done.map_or(0, |s| s + 1);
        self.step = match self.services.get(next) {
            Some(svc) => Step::Chars(next, svc.0),
            None if self.chars.is_empty() => Step::Done,
            None => Step::Descriptors(0, self.chars[0].value_handle + 1),
        };
    }

    fn next_char(&mut self, c: usize) {
        self.step = match self.chars.get(c + 1) {
            Some(ch) => Step::Descriptors(c + 1, ch.value_handle + 1),
            None => Step::ReadName,
        };
    }
}

/// A notification of `value`, cut to what fits the MTU.
fn notification(handle: u16, value: &[u8]) -> Vec<u8> {
    let mut pdu = vec![NOTIFY];
    pdu.extend_from_slice(&handle.to_le_bytes());
    pdu.extend_from_slice(&value[..value.len().min(ATT_MTU - 3)]);
    pdu
}

fn range_req(op: u8, start: u16, end: u16, typ: &[u8]) -> Vec<u8> {
    let mut r = vec![op];
    r.extend_from_slice(&start.to_le_bytes());
    r.extend_from_slice(&end.to_le_bytes());
    r.extend_from_slice(typ);
    r
}

/// What a mock device is.
pub enum Kind {
    /// Connects to the firmware and explores it.
    Central,
    /// Advertises the BLE MIDI service and sends MIDI.
    Midi,
    /// Advertises the Battery Service and notifies its level.
    Battery,
    /// An MXW01 thermal printer.
    CatPrinter,
}

/// Per-connection state of a peer.
enum Gatt {
    Server(Server),
    Client(Client),
}

/// One mock device in radio range.
pub struct Peer {
    pub name: String,
    /// Bluetooth address, as sent (little-endian: the last byte of the
    /// human-readable form first).
    pub addr: [u8; 6],
    /// A random (not public) address.
    pub random: bool,
    pub kind: Kind,
    /// Connection handle while connected to the firmware.
    pub conn: Option<u16>,
    gatt: Gatt,
    /// For a peripheral: the value handle of the characteristic it sends
    /// (MIDI I/O, battery level).
    midi_handle: u16,
    /// For the printer: the command and data characteristics' value handles.
    printer_handles: (u16, u16),
    /// The printer itself (kept across connections, paper and all).
    pub printer: Option<CatPrinter>,
    /// Battery level; the next notification is due at this time (µs).
    level: u8,
    level_due: u64,
    /// ATT PDUs waiting to go to the firmware.
    outbox: VecDeque<Vec<u8>>,
    /// Observable happenings for the board's event queue.
    pub notes: Vec<(&'static str, i64)>,
}

impl Peer {
    pub fn new(kind: Kind, name: String, addr: [u8; 6], random: bool) -> Self {
        let printer = matches!(kind, Kind::CatPrinter).then(CatPrinter::new);
        let mut p = Self {
            name,
            addr,
            random,
            kind,
            conn: None,
            gatt: Gatt::Client(Client::new()),
            midi_handle: 0,
            printer_handles: (0, 0),
            printer: None,
            level: 100,
            level_due: 0,
            outbox: VecDeque::new(),
            notes: Vec::new(),
        };
        p.printer = printer;
        p.reset();
        p
    }

    /// Fresh per-connection state (as after a disconnection).
    fn reset(&mut self) {
        self.outbox.clear();
        self.gatt = match self.kind {
            Kind::Central => Gatt::Client(Client::new()),
            Kind::Midi => {
                let mut s = Server::new();
                s.service(&0x1800u16.to_le_bytes());
                s.characteristic(
                    &DEVICE_NAME.to_le_bytes(),
                    PROP_READ,
                    self.name.as_bytes().into(),
                );
                s.service(&MIDI_SERVICE);
                self.midi_handle = s.characteristic(
                    &MIDI_IO,
                    PROP_READ | PROP_WRITE_NO_RSP | PROP_NOTIFY,
                    Vec::new(),
                );
                Gatt::Server(s)
            }
            Kind::Battery => {
                let mut s = Server::new();
                s.service(&0x1800u16.to_le_bytes());
                s.characteristic(
                    &DEVICE_NAME.to_le_bytes(),
                    PROP_READ,
                    self.name.as_bytes().into(),
                );
                s.service(&BATTERY_SERVICE.to_le_bytes());
                self.midi_handle = s.characteristic(
                    &BATTERY_LEVEL.to_le_bytes(),
                    PROP_READ | PROP_NOTIFY,
                    vec![self.level],
                );
                Gatt::Server(s)
            }
            Kind::CatPrinter => {
                let mut s = Server::new();
                s.service(&0x1800u16.to_le_bytes());
                s.characteristic(
                    &DEVICE_NAME.to_le_bytes(),
                    PROP_READ,
                    self.name.as_bytes().into(),
                );
                s.service(&PRINTER_SERVICE.to_le_bytes());
                let control = s.characteristic(
                    &PRINTER_CONTROL.to_le_bytes(),
                    PROP_WRITE | PROP_WRITE_NO_RSP,
                    Vec::new(),
                );
                self.midi_handle =
                    s.characteristic(&PRINTER_REPLY.to_le_bytes(), PROP_NOTIFY, Vec::new());
                let data =
                    s.characteristic(&PRINTER_DATA.to_le_bytes(), PROP_WRITE_NO_RSP, Vec::new());
                self.printer_handles = (control, data);
                if let Some(p) = &mut self.printer {
                    p.disconnected();
                }
                Gatt::Server(s)
            }
        };
    }

    pub fn is_central(&self) -> bool {
        matches!(self.kind, Kind::Central)
    }

    /// Advertising data and scan response, while the peer advertises.
    pub fn advertising(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        if self.conn.is_some() || self.is_central() {
            return None;
        }
        let mut adv = vec![2, AD_FLAGS, 0x06];
        match self.kind {
            Kind::Midi => {
                adv.extend_from_slice(&[17, AD_UUIDS128]);
                adv.extend_from_slice(&MIDI_SERVICE);
            }
            Kind::CatPrinter => {
                adv.extend_from_slice(&[3, AD_UUIDS16]);
                adv.extend_from_slice(&PRINTER_SERVICE.to_le_bytes());
            }
            _ => {
                adv.extend_from_slice(&[3, AD_UUIDS16]);
                adv.extend_from_slice(&BATTERY_SERVICE.to_le_bytes());
            }
        }
        let name = self.name.as_bytes();
        let room = 31 - adv.len() - 2;
        if name.len() <= room {
            adv.extend_from_slice(&[name.len() as u8 + 1, AD_NAME]);
            adv.extend_from_slice(name);
        } else {
            adv.extend_from_slice(&[room as u8 + 1, AD_SHORT_NAME]);
            adv.extend_from_slice(&name[..room]);
        }
        let name = &name[..name.len().min(29)];
        let mut rsp = vec![name.len() as u8 + 1, AD_NAME];
        rsp.extend_from_slice(name);
        Some((adv, rsp))
    }

    pub fn connected(&mut self, handle: u16) {
        self.reset();
        self.conn = Some(handle);
        self.notes.push(("ble_connected", 1));
    }

    pub fn disconnected(&mut self) {
        if self.conn.take().is_some() {
            self.reset();
            self.notes.push(("ble_connected", 0));
        }
    }

    /// An ATT PDU from the firmware.
    pub fn att(&mut self, pdu: &[u8]) {
        let Some(&op) = pdu.first() else { return };
        // Whatever the role, a peer answers requests and takes notifications.
        if op == NOTIFY || op == INDICATE {
            let handle = le16(pdu, 1);
            let value = &pdu[3.min(pdu.len())..];
            if let Gatt::Client(c) = &mut self.gatt
                && let Some(ch) = c.chars.iter_mut().find(|ch| ch.value_handle == handle)
            {
                ch.value = value.into();
            }
            let packed = value
                .iter()
                .take(8)
                .rev()
                .fold(0i64, |acc, &b| acc << 8 | b as i64);
            self.notes.push(("ble_notify", packed));
            if op == INDICATE {
                self.outbox.push_back(vec![CONFIRM]);
            }
            return;
        }
        match &mut self.gatt {
            Gatt::Client(c) if op & 1 == 1 && op != WRITE_CMD => c.response(pdu),
            Gatt::Client(_) => {
                // The central serves nothing.
                if op & 0x40 == 0 && op & 1 == 0 {
                    let rsp = if op == MTU_REQ {
                        let mut r = vec![MTU_RSP];
                        r.extend_from_slice(&(ATT_MTU as u16).to_le_bytes());
                        r
                    } else {
                        att_error(op, le16(pdu, 1), E_NOT_FOUND)
                    };
                    self.outbox.push_back(rsp);
                }
            }
            Gatt::Server(s) => {
                let mut written = Vec::new();
                if let Some(rsp) = s.request(pdu, &mut written) {
                    self.outbox.push_back(rsp);
                }
                for (handle, value) in written {
                    if let Some(p) = &mut self.printer {
                        if handle == self.printer_handles.0 {
                            for reply in p.control(&value, &mut self.notes) {
                                self.outbox
                                    .push_back(notification(self.midi_handle, &reply));
                            }
                        } else if handle == self.printer_handles.1 {
                            p.data(&value);
                        }
                    } else if handle == self.midi_handle {
                        let packed = value
                            .iter()
                            .take(8)
                            .rev()
                            .fold(0i64, |acc, &b| acc << 8 | b as i64);
                        self.notes.push(("ble_midi_out", packed));
                    }
                }
            }
        }
    }

    /// The ATT PDUs to send at this connection event (`now` in µs).
    pub fn poll(&mut self, now: u64) -> Vec<Vec<u8>> {
        if self.conn.is_none() {
            return Vec::new();
        }
        if let (Kind::Battery, Gatt::Server(s)) = (&self.kind, &mut self.gatt)
            && s.subscribed(self.midi_handle)
            && now >= self.level_due
        {
            self.level_due = now + 1_000_000;
            self.level = if self.level == 0 { 100 } else { self.level - 1 };
            s.set(self.midi_handle, vec![self.level]);
            let mut pdu = vec![NOTIFY];
            pdu.extend_from_slice(&self.midi_handle.to_le_bytes());
            pdu.push(self.level);
            self.outbox.push_back(pdu);
        }
        if let Some(p) = &mut self.printer
            && let Some(done) = p.poll(now, &mut self.notes)
        {
            self.outbox.push_back(notification(self.midi_handle, &done));
        }
        if let Gatt::Client(c) = &mut self.gatt
            && let Some(req) = c.next_request()
        {
            self.outbox.push_back(req);
        }
        self.outbox.drain(..).collect()
    }

    /// Send a MIDI message (MIDI peripheral, connected and subscribed):
    /// one BLE MIDI packet stamped with the low 13 bits of `ms`.
    pub fn midi(&mut self, ms: u64, msg: &[u8]) -> bool {
        let (Kind::Midi, Gatt::Server(s)) = (&self.kind, &self.gatt) else {
            return false;
        };
        if self.conn.is_none() || !s.subscribed(self.midi_handle) || msg.is_empty() {
            return false;
        }
        let ts = ms as u16 & 0x1FFF;
        let mut pdu = vec![NOTIFY];
        pdu.extend_from_slice(&self.midi_handle.to_le_bytes());
        pdu.push(0x80 | (ts >> 7) as u8);
        pdu.push(0x80 | (ts & 0x7F) as u8);
        pdu.extend_from_slice(&msg[..msg.len().min(ATT_MTU - 5)]);
        self.outbox.push_back(pdu);
        true
    }

    /// The device name the central read from the firmware.
    pub fn remote_name(&self) -> Option<&str> {
        match &self.gatt {
            Gatt::Client(c) => c.name.as_deref(),
            Gatt::Server(_) => None,
        }
    }

    /// The characteristics the central found on the firmware.
    pub fn found(&self) -> &[Found] {
        match &self.gatt {
            Gatt::Client(c) => &c.chars,
            Gatt::Server(_) => &[],
        }
    }

    /// The central has explored the firmware and subscribed.
    pub fn explored(&self) -> bool {
        matches!(&self.gatt, Gatt::Client(c) if c.step == Step::Done)
    }
}

/// Address of the mock battery: the random address trouble's examples
/// connect to (`ff:e4:05:1a:8f:ff`, as sent).
pub const BATTERY_ADDR: [u8; 6] = [0xff, 0x8f, 0x1a, 0x05, 0xe4, 0xff];
/// Address of the mock MIDI peripheral (public `e2:1e:46:25:24:83`, as sent).
pub const MIDI_ADDR: [u8; 6] = [0x83, 0x24, 0x25, 0x46, 0x1e, 0xe2];
/// Address of the mock cat printer (public `d4:e8:a0:01:57:70`, as sent).
pub const PRINTER_ADDR: [u8; 6] = [0x70, 0x57, 0x01, 0xa0, 0xe8, 0xd4];
/// Address of the mock central (random `c0:de:00:00:00:01`, as sent).
pub const CENTRAL_ADDR: [u8; 6] = [0x01, 0x00, 0x00, 0x00, 0xde, 0xc0];

/// The mock devices `--bt mock` puts in range: a central that connects to
/// the firmware when it advertises, a BLE MIDI peripheral, a battery and
/// an MXW01 cat printer.
pub fn neighbourhood() -> Vec<Peer> {
    vec![
        Peer::new(Kind::Central, "Mock central".into(), CENTRAL_ADDR, true),
        Peer::new(Kind::Midi, "Mock MIDI".into(), MIDI_ADDR, false),
        Peer::new(Kind::Battery, "Mock battery".into(), BATTERY_ADDR, true),
        Peer::new(Kind::CatPrinter, "MXW01".into(), PRINTER_ADDR, false),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a central against a peripheral, connection event by connection
    /// event, for `ms`.
    fn link(c: &mut Peer, p: &mut Peer, ms: u64) {
        c.connected(1);
        p.connected(1);
        for t in (0..ms).step_by(10) {
            for pdu in c.poll(t * 1000) {
                p.att(&pdu);
            }
            for pdu in p.poll(t * 1000) {
                c.att(&pdu);
            }
        }
    }

    #[test]
    fn the_central_explores_and_subscribes_to_the_battery() {
        let (mut c, mut p) = (
            Peer::new(Kind::Central, "c".into(), CENTRAL_ADDR, true),
            Peer::new(Kind::Battery, "Bat".into(), BATTERY_ADDR, true),
        );
        link(&mut c, &mut p, 3500);
        assert!(c.explored());
        assert_eq!(c.remote_name(), Some("Bat"));
        let level = c
            .found()
            .iter()
            .find(|f| f.uuid == BATTERY_LEVEL.to_le_bytes())
            .expect("battery level found");
        assert_eq!(level.props, PROP_READ | PROP_NOTIFY);
        // Subscribed, so the level arrives once a second, dropping.
        let got: Vec<i64> = c
            .notes
            .iter()
            .filter(|n| n.0 == "ble_notify")
            .map(|n| n.1)
            .collect();
        assert_eq!(got, [99, 98, 97, 96]);
        assert_eq!(level.value, [96]);
    }

    #[test]
    fn midi_goes_out_as_ble_midi_notifications_once_subscribed() {
        let (mut c, mut p) = (
            Peer::new(Kind::Central, "c".into(), CENTRAL_ADDR, true),
            Peer::new(Kind::Midi, "Keys".into(), MIDI_ADDR, false),
        );
        c.connected(1);
        p.connected(1);
        assert!(!p.midi(0, &[0x90, 60, 100]), "nobody subscribed yet");
        link(&mut c, &mut p, 500);
        assert!(c.explored());
        assert!(p.midi(0x1234, &[0x90, 60, 100]));
        for pdu in p.poll(0) {
            c.att(&pdu);
        }
        let midi = c.found().iter().find(|f| f.uuid == MIDI_IO).unwrap();
        // Header and timestamp carry the 13-bit millisecond stamp.
        assert_eq!(midi.value, [0x80 | 0x24, 0x80 | 0x34, 0x90, 60, 100]);
    }

    #[test]
    fn the_server_answers_discovery_like_a_gatt_server_should() {
        let mut p = Peer::new(Kind::Midi, "Keys".into(), MIDI_ADDR, false);
        p.connected(1);
        let mut ask = |req: &[u8]| {
            p.att(req);
            p.poll(0).pop().unwrap()
        };
        // Primary services: GAP (1..3) and MIDI (4..7), the 128-bit one alone.
        assert_eq!(
            ask(&[READ_BY_GROUP_REQ, 1, 0, 0xFF, 0xFF, 0x00, 0x28]),
            [READ_BY_GROUP_RSP, 6, 1, 0, 3, 0, 0x00, 0x18]
        );
        let mut find = vec![FIND_BY_TYPE_REQ, 1, 0, 0xFF, 0xFF, 0x00, 0x28];
        find.extend_from_slice(&MIDI_SERVICE);
        assert_eq!(ask(&find), [FIND_BY_TYPE_RSP, 4, 0, 7, 0]);
        assert_eq!(
            ask(&[FIND_INFO_REQ, 7, 0, 7, 0]),
            [FIND_INFO_RSP, 1, 7, 0, 0x02, 0x29]
        );
        assert_eq!(
            ask(&[READ_REQ, 9, 0]),
            [ERROR_RSP, READ_REQ, 9, 0, E_INVALID_HANDLE]
        );
        assert_eq!(
            ask(&[WRITE_REQ, 3, 0, 1]),
            [ERROR_RSP, WRITE_REQ, 3, 0, E_WRITE_NOT_PERMITTED]
        );
        assert_eq!(ask(&[MTU_REQ, 0, 1]), [MTU_RSP, 23, 0]);
    }

    #[test]
    fn the_printer_prints_what_comes_over_its_characteristics() {
        use super::super::catprinter::{FLUSH, PRINT, PRINT_COMPLETE, ROW_BYTES, frame};
        let mut p = Peer::new(Kind::CatPrinter, "MXW01".into(), PRINTER_ADDR, false);
        let (adv, _) = p.advertising().unwrap();
        assert_eq!(adv[3..7], [3, AD_UUIDS16, 0x30, 0xAE]);
        p.connected(1);
        // GAP 1..3, printer service 4, control 6, reply 8 (CCCD 9), data 11.
        let mut send = |pdu: Vec<u8>, now: u64| {
            p.att(&pdu);
            p.poll(now)
        };
        let att = |op: u8, handle: u16, data: &[u8]| {
            let mut v = vec![op];
            v.extend_from_slice(&handle.to_le_bytes());
            v.extend_from_slice(data);
            v
        };
        assert_eq!(send(att(WRITE_REQ, 9, &[1, 0]), 0), [vec![WRITE_RSP]]);
        let ack = send(att(WRITE_CMD, 6, &frame(PRINT, &[1, 0, 0x30, 0])), 0);
        assert_eq!(ack, [att(NOTIFY, 8, &frame(PRINT, &[0]))]);
        let row: Vec<u8> = (0..ROW_BYTES as u8).collect();
        for chunk in row.chunks(ATT_MTU - 3) {
            assert!(send(att(WRITE_CMD, 11, chunk), 0).is_empty());
        }
        send(att(WRITE_CMD, 6, &frame(FLUSH, &[0])), 0);
        let done = send(Vec::new(), 1_000_000);
        assert_eq!(done, [att(NOTIFY, 8, &frame(PRINT_COMPLETE, &[0]))]);
        assert_eq!(p.printer.as_ref().unwrap().paper, row);
        assert!(p.notes.contains(&("printed", 1)));
    }

    #[test]
    fn advertising_fits_in_31_bytes_and_stops_while_connected() {
        let mut p = Peer::new(
            Kind::Midi,
            "A rather long keyboard name".into(),
            MIDI_ADDR,
            false,
        );
        let (adv, rsp) = p.advertising().unwrap();
        assert!(adv.len() <= 31 && rsp.len() <= 31);
        assert_eq!(
            adv[adv.len() - 10..adv.len() - 8],
            [9, AD_SHORT_NAME],
            "a shortened name fills the rest"
        );
        assert!(
            Peer::new(Kind::Central, "c".into(), CENTRAL_ADDR, true)
                .advertising()
                .is_none()
        );
        p.connected(1);
        assert!(p.advertising().is_none());
    }
}
