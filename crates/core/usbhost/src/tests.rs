//! The host against a scripted device with a serial port, a HID interface
//! and a MIDI interface (endpoint 0 is 8 bytes, so descriptors take several
//! packets).
use alloc::{collections::VecDeque, string::String, vec, vec::Vec};

use crate::{
    DevicePort, Handshake, Host,
    descriptor::{self, Setup},
    function::{Function, HidRequest, ReportType},
    log::Kind,
};

const MS: u64 = 125_000;

#[rustfmt::skip]
const DEVICE: [u8; 18] = [18, 1, 0, 2, 0xEF, 2, 1, 8, 0x09, 0x12, 0x01, 0x00, 0, 1, 1, 2, 0, 1];
#[rustfmt::skip]
const REPORT: [u8; 12] = [0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x01, 0x75, 0x08, 0xC0, 0x00];

fn config() -> Vec<u8> {
    #[rustfmt::skip]
    let mut d = vec![
        9, 2, 0, 0, 5, 1, 0, 0x80, 50,
        8, 11, 0, 2, 2, 2, 0, 0,
        9, 4, 0, 0, 1, 2, 2, 0, 0,
        5, 0x24, 6, 0, 1,
        7, 5, 0x83, 3, 16, 0, 10,
        9, 4, 1, 0, 2, 10, 0, 0, 0,
        7, 5, 0x02, 2, 64, 0, 0,
        7, 5, 0x82, 2, 64, 0, 0,
        9, 4, 2, 0, 1, 3, 0, 0, 0,
        9, 0x21, 0x11, 1, 0, 1, 0x22, REPORT.len() as u8, 0,
        7, 5, 0x81, 3, 8, 0, 1,
        9, 4, 3, 0, 0, 1, 1, 0, 0,
        9, 4, 4, 0, 2, 1, 3, 0, 0,
        7, 5, 0x04, 2, 64, 0, 0,
        5, 0x25, 1, 1, 1,
        7, 5, 0x84, 2, 64, 0, 0,
        7, 0x25, 1, 3, 1, 2, 3,
    ];
    let total = d.len() as u16;
    d[2..4].copy_from_slice(&total.to_le_bytes());
    d
}

fn string(s: &str) -> Vec<u8> {
    let mut d = vec![0, 3];
    d.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
    d[0] = d.len() as u8;
    d
}

#[derive(Default)]
struct Fake {
    attached: bool,
    address: u8,
    new_address: Option<u8>,
    configured: bool,
    /// The control transfer in progress: its setup, IN data, OUT data.
    setup: Option<Setup>,
    reply: Vec<u8>,
    sent: usize,
    written: Vec<u8>,
    toggles_in: [bool; 16],
    resets: u32,
    requests: Vec<Setup>,
    line_coding: Vec<u8>,
    line_state: u16,
    serial_out: VecDeque<u8>,
    hid_out: VecDeque<Vec<u8>>,
    midi_out: VecDeque<[u8; 4]>,
    midi_in: Vec<[u8; 4]>,
    feature: Vec<u8>,
}

impl Fake {
    fn answer(&mut self, s: &Setup) -> Option<Vec<u8>> {
        let (kind, index) = ((s.value >> 8) as u8, s.value as u8);
        Some(match (s.request_type, s.request) {
            (0x80, descriptor::GET_DESCRIPTOR) => match kind {
                1 => DEVICE.to_vec(),
                2 => config(),
                3 => match index {
                    0 => vec![4, 3, 0x09, 0x04],
                    1 => string("Tartelette"),
                    2 => string("Fake"),
                    _ => return None,
                },
                _ => return None,
            },
            (0x81, descriptor::GET_DESCRIPTOR) if kind == 0x22 => REPORT.to_vec(),
            (0x00, descriptor::SET_ADDRESS) => {
                self.new_address = Some(s.value as u8);
                vec![]
            }
            (0x00, descriptor::SET_CONFIGURATION) => {
                self.configured = true;
                vec![]
            }
            (0x21, 0x22) => {
                self.line_state = s.value;
                vec![]
            }
            (0x21, 0x0A) => return None, // SET_IDLE: stalled, as many devices do
            (0xA1, 0x01) if s.value == 0x0302 => self.feature.clone(),
            (0x21, 0x20 | 0x09) => vec![],
            _ => return None,
        })
    }

    fn endpoint_in(&mut self, ep: u8) -> Option<Vec<u8>> {
        match ep {
            2 if !self.serial_out.is_empty() => {
                let n = self.serial_out.len().min(64);
                Some(self.serial_out.drain(..n).collect())
            }
            1 => self.hid_out.pop_front(),
            4 if !self.midi_out.is_empty() => Some(self.midi_out.drain(..).flatten().collect()),
            _ => None,
        }
    }
}

impl DevicePort for Fake {
    fn attached(&self) -> bool {
        self.attached
    }
    fn bus_reset(&mut self) {
        self.resets += 1;
        self.address = 0;
        self.configured = false;
    }
    fn setup(&mut self, address: u8, packet: [u8; 8]) -> Result<(), Handshake> {
        if address != self.address {
            return Err(Handshake::Timeout);
        }
        let s = Setup::from_bytes(packet);
        self.requests.push(s);
        self.setup = Some(s);
        self.sent = 0;
        self.written.clear();
        self.toggles_in[0] = true;
        self.reply = match self.answer(&s) {
            Some(r) => r,
            None => {
                self.setup = None; // stall the rest
                Vec::new()
            }
        };
        Ok(())
    }
    fn token_in(&mut self, address: u8, ep: u8, max: u16) -> Result<(bool, Vec<u8>), Handshake> {
        if address != self.address {
            return Err(Handshake::Timeout);
        }
        if ep != 0 {
            if !self.configured {
                return Err(Handshake::Timeout);
            }
            let data = self.endpoint_in(ep).ok_or(Handshake::Nak)?;
            let t = self.toggles_in[ep as usize];
            self.toggles_in[ep as usize] ^= true;
            return Ok((t, data));
        }
        let s = self.setup.ok_or(Handshake::Stall)?;
        if !s.is_in() || s.length == 0 {
            // Status stage of an OUT or no-data request.
            if s.request_type == 0x21 && s.request == 0x20 {
                self.line_coding = self.written.clone();
            }
            if let Some(a) = self.new_address.take() {
                self.address = a;
            }
            self.setup = None;
            return Ok((true, Vec::new()));
        }
        // Endpoint 0 sends at most its own 8 bytes per packet.
        let end = (self.sent + (max as usize).min(DEVICE[7] as usize))
            .min(self.reply.len())
            .min(s.length as usize);
        let data = self.reply[self.sent..end].to_vec();
        self.sent = end;
        let t = self.toggles_in[0];
        self.toggles_in[0] ^= true;
        Ok((t, data))
    }
    fn token_out(
        &mut self,
        address: u8,
        ep: u8,
        _data1: bool,
        data: &[u8],
    ) -> Result<(), Handshake> {
        if address != self.address {
            return Err(Handshake::Timeout);
        }
        match ep {
            0 => {
                if self.setup.is_none() {
                    return Err(Handshake::Stall);
                }
                self.written.extend_from_slice(data);
                if self.setup.is_some_and(|s| s.is_in()) && data.is_empty() {
                    self.setup = None; // status stage
                }
            }
            2 => self
                .serial_out
                .extend(data.iter().map(u8::to_ascii_uppercase)),
            4 => self.midi_in.extend(data.as_chunks::<4>().0),
            _ => return Err(Handshake::Timeout),
        }
        Ok(())
    }
}

/// Run the host for `cycles`, waking it when it asks (or every millisecond).
fn run(host: &mut Host, dev: &mut Fake, now: &mut u64, cycles: u64) {
    let until = *now + cycles;
    while *now < until {
        let wake = host.poll(dev, *now, true);
        *now = wake
            .unwrap_or(*now + MS)
            .clamp(*now + 1, until.max(*now + 1));
    }
}

fn enumerated() -> (Host, Fake, u64) {
    let mut host = Host::new(125);
    let mut dev = Fake::default();
    let mut now = 0;
    run(&mut host, &mut dev, &mut now, 5 * MS);
    assert!(
        host.take_events().is_empty(),
        "nothing attached, nothing happens"
    );
    dev.attached = true;
    run(&mut host, &mut dev, &mut now, 60 * MS);
    (host, dev, now)
}

#[test]
fn enumerates_and_binds_the_functions() {
    let (mut host, dev, _) = enumerated();
    assert_eq!(dev.resets, 1);
    assert_eq!(dev.address, 1);
    assert!(dev.configured);
    let info = host.device().expect("configured").clone();
    assert_eq!(info.manufacturer.as_deref(), Some("Tartelette"));
    assert_eq!(info.product.as_deref(), Some("Fake"));
    assert_eq!(info.name(), "1209:0001 \"Fake\"");
    let kinds: Vec<&str> = host
        .functions
        .iter()
        .map(|f| match f {
            Function::Serial(_) => "serial",
            Function::Hid(_) => "hid",
            Function::Midi(_) => "midi",
            Function::Other(_) => "other",
        })
        .collect();
    assert_eq!(kinds, ["serial", "hid", "other", "midi"]);
    let Function::Hid(h) = &host.functions[1] else {
        panic!()
    };
    assert!(h.ready);
    assert_eq!(h.report_descriptor, REPORT);
    let Function::Midi(m) = &host.functions[3] else {
        panic!()
    };
    assert_eq!((m.cables_in, m.cables_out), (3, 1));
    // The serial port is opened: 115200 8N1, DTR and RTS up.
    assert_eq!(dev.line_coding, [0x00, 0xC2, 0x01, 0x00, 0, 0, 8]);
    assert_eq!(dev.line_state, 3);
    // The log tells the story, the stalled SET_IDLE included.
    let log: Vec<String> = host.take_events().iter().map(|e| e.describe()).collect();
    assert_eq!(log[0], "device attached");
    assert_eq!(log[1], "bus reset");
    assert!(
        log[2].contains("GET_DESCRIPTOR device 0 → 8 bytes"),
        "{}",
        log[2]
    );
    assert!(log.iter().any(|l| l.contains("HID SET_IDLE failed: STALL")));
    assert!(
        log.iter()
            .any(|l| l == "configured 1209:0001 \"Fake\": serial (if 0+1) HID (if 2) audio control (if 3, unused) MIDI (if 4)"),
        "{log:#?}"
    );
}

#[test]
fn serial_data_flows_both_ways() {
    let (mut host, mut dev, mut now) = enumerated();
    host.serial().unwrap().tx.extend(b"hello, world");
    run(&mut host, &mut dev, &mut now, 5 * MS);
    let rx: Vec<u8> = host.serial().unwrap().rx.drain(..).collect();
    assert_eq!(rx, b"HELLO, WORLD");
    host.serial().unwrap().dtr = false;
    run(&mut host, &mut dev, &mut now, 5 * MS);
    assert_eq!(dev.line_state, 2);
}

#[test]
fn a_full_receive_queue_stops_reading() {
    let (mut host, mut dev, mut now) = enumerated();
    dev.serial_out
        .extend(vec![b'x'; crate::function::SERIAL_KEEP + 1000]);
    run(&mut host, &mut dev, &mut now, 50 * MS);
    let s = host.serial().unwrap();
    assert!(s.rx.len() >= crate::function::SERIAL_KEEP);
    assert!(
        !dev.serial_out.is_empty(),
        "the device keeps what the host can't take"
    );
    s.rx.clear();
    run(&mut host, &mut dev, &mut now, 50 * MS);
    assert!(dev.serial_out.is_empty());
}

#[test]
fn bulk_transfers_take_bus_time() {
    let (mut host, mut dev, mut now) = enumerated();
    // 64 bytes + overhead is 640 bits: 53.3 us at 12 Mbit/s.
    dev.serial_out.extend(vec![b'x'; 640]);
    let start = now;
    run(&mut host, &mut dev, &mut now, 10 * MS);
    let t: Vec<u64> = host
        .take_events()
        .iter()
        .filter(|e| matches!(&e.kind, Kind::Data { ep: 0x82, .. }))
        .map(|e| e.at - start)
        .collect();
    assert_eq!(t.len(), 10);
    assert!(t.windows(2).all(|w| w[1] - w[0] >= 6667), "{t:?}");
}

#[test]
fn hid_reports_and_requests() {
    let (mut host, mut dev, mut now) = enumerated();
    dev.hid_out.push_back(vec![1, 0x04]);
    dev.feature = vec![2, 0xAA];
    let Function::Hid(h) = &mut host.functions[1] else {
        panic!()
    };
    h.output.push_back(vec![1, 0x02]); // no OUT endpoint: SET_REPORT
    h.requests.push_back(HidRequest {
        id: 7,
        kind: ReportType::Feature,
        report_id: 2,
        set: None,
    });
    h.requests.push_back(HidRequest {
        id: 8,
        kind: ReportType::Feature,
        report_id: 9,
        set: None,
    });
    run(&mut host, &mut dev, &mut now, 20 * MS);
    let Function::Hid(h) = &mut host.functions[1] else {
        panic!()
    };
    assert_eq!(h.input.pop_front(), Some(vec![1, 0x04]));
    assert_eq!(h.replies.pop_front().unwrap().data, Some(vec![2, 0xAA]));
    assert_eq!(h.replies.pop_front().unwrap().data, None); // stalled
    let set = dev
        .requests
        .iter()
        .find(|s| s.request_type == 0x21 && s.request == 0x09)
        .unwrap();
    assert_eq!(set.value, 0x0201, "output report 1");
}

#[test]
fn midi_messages_are_packed_and_unpacked() {
    let (mut host, mut dev, mut now) = enumerated();
    dev.midi_out.push_back([0x29, 0x90, 60, 100]);
    dev.midi_out.push_back([0x04, 0xF0, 1, 2]);
    dev.midi_out.push_back([0x06, 3, 0xF7, 0]);
    let Function::Midi(m) = &mut host.functions[3] else {
        panic!()
    };
    m.tx.push_back((0, vec![0x80, 60, 0]));
    m.tx.push_back((0, vec![0xF8]));
    run(&mut host, &mut dev, &mut now, 5 * MS);
    assert_eq!(dev.midi_in, [[0x08, 0x80, 60, 0], [0x0F, 0xF8, 0, 0]]);
    let Function::Midi(m) = &mut host.functions[3] else {
        panic!()
    };
    let rx: Vec<_> = m.rx.drain(..).collect();
    assert_eq!(
        rx,
        [(2, vec![0x90, 60, 100]), (0, vec![0xF0, 1, 2, 3, 0xF7])]
    );
}

#[test]
fn detaching_forgets_the_device_and_reattaching_starts_over() {
    let (mut host, mut dev, mut now) = enumerated();
    let g = host.generation();
    dev.attached = false;
    run(&mut host, &mut dev, &mut now, MS);
    assert!(host.device().is_none());
    assert!(host.functions.is_empty());
    assert_ne!(host.generation(), g);
    dev.attached = true;
    run(&mut host, &mut dev, &mut now, 60 * MS);
    assert!(host.device().is_some());
    assert_eq!(dev.resets, 2);
}

#[test]
fn a_silent_device_fails_enumeration_after_retries() {
    struct Mute;
    impl DevicePort for Mute {
        fn attached(&self) -> bool {
            true
        }
        fn bus_reset(&mut self) {}
        fn setup(&mut self, _: u8, _: [u8; 8]) -> Result<(), Handshake> {
            Err(Handshake::Timeout)
        }
        fn token_in(&mut self, _: u8, _: u8, _: u16) -> Result<(bool, Vec<u8>), Handshake> {
            Err(Handshake::Timeout)
        }
        fn token_out(&mut self, _: u8, _: u8, _: bool, _: &[u8]) -> Result<(), Handshake> {
            Err(Handshake::Timeout)
        }
    }
    let mut host = Host::new(125);
    let mut now = 0;
    let mut wakes = 0;
    while let Some(w) = host.poll(&mut Mute, now, false) {
        now = w;
        wakes += 1;
        assert!(wakes < 100);
    }
    let log: Vec<String> = host.take_events().iter().map(|e| e.describe()).collect();
    assert_eq!(
        log.iter().filter(|l| l.contains("failed: timeout")).count(),
        3
    );
    assert_eq!(
        log.last().unwrap(),
        "enumeration failed: GET_DESCRIPTOR timeout"
    );
}

#[test]
fn a_nak_waits_for_a_kick() {
    let (mut host, mut dev, mut now) = enumerated();
    // Nothing to read: the bulk IN endpoint NAKs and the host waits for a kick.
    host.poll(&mut dev, now, true);
    now += MS;
    let wake = host.poll(&mut dev, now, false);
    // Only the interrupt endpoints' polling is left to wait for.
    assert!(wake.is_some_and(|w| w > now));
    dev.serial_out.extend(b"late");
    run(&mut host, &mut dev, &mut now, 2 * MS);
    let rx: Vec<u8> = host.serial().unwrap().rx.drain(..).collect();
    assert_eq!(rx, b"late");
}

#[test]
fn report_ids_are_found() {
    assert!(crate::host::uses_report_ids(&REPORT));
    assert!(!crate::host::uses_report_ids(&[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0xC0
    ]));
}
