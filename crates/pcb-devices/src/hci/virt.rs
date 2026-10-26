//! A Bluetooth LE controller in software: answers the HCI commands a host
//! stack (trouble, BTstack, NimBLE) issues, and runs a radio "neighbourhood"
//! of [mock peers](super::peers) on a 10 ms tick: advertising reports while
//! the firmware scans, connections both ways, and LE ACL data carrying
//! L2CAP to and from the peers. Everything runs on emulated time, so runs are
//! deterministic.
//!
//! Built with [`VirtualController::no_radio`] it has no peers and reports
//! itself unavailable: scans find nothing, connections never complete, and
//! the first attempt at either (or at advertising) is reported as
//! `bt_unavailable`.
use super::peers::{ATT_CID, Peer};
use super::{ACL, EVENT, HciBackend};
use alloc::{collections::VecDeque, format, string::String, vec, vec::Vec};

/// How often the radio neighbourhood acts: a connection event, more or less.
const TICK_US: u64 = 10_000;
/// A central peer connects this long after the firmware starts advertising.
const CONNECT_DELAY_US: u64 = 100_000;
/// Longest LE data payload either side sends (the LE default).
const LE_MTU: usize = 27;
const ACL_BUFFERS: u8 = 8;
const PEER_RSSI: i8 = -60;
const SIGNAL_CID: u16 = 5;

// Events.
const EV_DISCONNECTION_COMPLETE: u8 = 0x05;
const EV_COMMAND_COMPLETE: u8 = 0x0E;
const EV_COMMAND_STATUS: u8 = 0x0F;
const EV_COMPLETED_PACKETS: u8 = 0x13;
const EV_LE_META: u8 = 0x3E;
const LE_CONN_COMPLETE: u8 = 0x01;
const LE_ADV_REPORT: u8 = 0x02;
const LE_CONN_UPDATE_COMPLETE: u8 = 0x03;
const LE_REMOTE_FEATURES: u8 = 0x04;

// Status codes.
const OK: u8 = 0x00;
const UNKNOWN_COMMAND: u8 = 0x01;
const UNKNOWN_CONNECTION: u8 = 0x02;
const DISALLOWED: u8 = 0x0C;
const LOCAL_HOST_TERMINATED: u8 = 0x16;

// Advertising PDU types in reports.
const ADV_IND: u8 = 0x00;
const SCAN_RSP: u8 = 0x04;

fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

#[derive(Default)]
struct Advertising {
    /// Legacy advertising type (0 = connectable undirected).
    kind: u8,
    data: Vec<u8>,
    scan_rsp: Vec<u8>,
    /// Enabled since (µs).
    since: Option<u64>,
}

struct Scan {
    active: bool,
    filtered: bool,
    no_duplicates: bool,
    /// Peers already reported in this scan.
    reported: Vec<usize>,
}

struct Connecting {
    /// Use the filter accept list instead of `peer`.
    filtered: bool,
    peer: [u8; 6],
    interval: u16,
}

struct Link {
    handle: u16,
    peer: usize,
    /// The firmware is the central.
    central: bool,
    interval: u16,
    /// L2CAP frame being reassembled from the firmware.
    rx: Vec<u8>,
    /// Packets from the firmware not yet reported completed.
    completed: u16,
}

pub struct VirtualController {
    available: bool,
    pub peers: Vec<Peer>,
    public_addr: [u8; 6],
    random_addr: Option<[u8; 6]>,
    adv: Advertising,
    scan_params: (bool, bool),
    scan: Option<Scan>,
    accept: Vec<[u8; 6]>,
    connecting: Option<Connecting>,
    links: Vec<Link>,
    next_handle: u16,
    out: VecDeque<(u8, Vec<u8>)>,
    next_tick: u64,
    notes: Vec<(&'static str, i64)>,
    /// Commands nobody modelled (each once), for debugging.
    pub unknown: Vec<String>,
}

impl VirtualController {
    /// A controller with these devices in range. `addr` is its public
    /// address (little-endian, as sent).
    pub fn new(addr: [u8; 6], peers: Vec<Peer>) -> Self {
        Self {
            available: true,
            peers,
            public_addr: addr,
            random_addr: None,
            adv: Advertising::default(),
            scan_params: (false, false),
            scan: None,
            accept: Vec::new(),
            connecting: None,
            links: Vec::new(),
            next_handle: 0x40,
            out: VecDeque::new(),
            next_tick: 0,
            notes: Vec::new(),
            unknown: Vec::new(),
        }
    }

    /// What answers when there is no radio: no peers, unavailable.
    pub fn no_radio(addr: [u8; 6]) -> Self {
        Self { available: false, ..Self::new(addr, Vec::new()) }
    }

    fn event(&mut self, code: u8, params: &[u8]) {
        let mut p = vec![code, params.len() as u8];
        p.extend_from_slice(params);
        self.out.push_back((EVENT, p));
    }

    fn le_event(&mut self, sub: u8, params: &[u8]) {
        let mut p = vec![sub];
        p.extend_from_slice(params);
        self.event(EV_LE_META, &p);
    }

    fn complete(&mut self, opcode: u16, ret: &[u8]) {
        let mut p = vec![1];
        p.extend_from_slice(&opcode.to_le_bytes());
        p.extend_from_slice(ret);
        self.event(EV_COMMAND_COMPLETE, &p);
    }

    fn status(&mut self, opcode: u16, status: u8) {
        let o = opcode.to_le_bytes();
        self.event(EV_COMMAND_STATUS, &[status, 1, o[0], o[1]]);
    }

    fn unavailable(&mut self) {
        if !self.available {
            self.notes.push(("bt_unavailable", 1));
        }
    }

    fn command(&mut self, now: u64, pkt: &[u8]) {
        let opcode = le16(pkt, 0);
        let p = pkt.get(3..).unwrap_or(&[]);
        match opcode {
            // Reset
            0x0C03 => {
                for l in core::mem::take(&mut self.links) {
                    self.peers[l.peer].disconnected();
                }
                self.adv = Advertising::default();
                self.scan = None;
                self.connecting = None;
                self.accept.clear();
                self.random_addr = None;
                self.complete(opcode, &[OK]);
            }
            // Settings with nothing to model: event masks, host buffer
            // size and flow control, LE host support, default data length
            // and PHY, filter list removals, address resolution.
            0x0C01 | 0x0C31 | 0x0C33 | 0x0C63 | 0x0C6D | 0x2001 | 0x2012 | 0x2024 | 0x2027 | 0x2029 | 0x202D | 0x202E | 0x2031 => {
                self.complete(opcode, &[OK])
            }
            // Read Local Version Information: Bluetooth 5.0, Cypress.
            0x1001 => self.complete(opcode, &[OK, 9, 0, 0, 9, 0x31, 0x01, 0, 0]),
            // Read Local Supported Commands: claim them all.
            0x1002 => {
                let mut r = vec![OK];
                r.extend_from_slice(&[0xFF; 64]);
                self.complete(opcode, &r);
            }
            // Read Local Supported Features: LE, no BR/EDR.
            0x1003 => self.complete(opcode, &[OK, 0, 0, 0, 0, 0x60, 0, 0, 0]),
            // Read BD_ADDR
            0x1009 => {
                let mut r = vec![OK];
                r.extend_from_slice(&self.public_addr);
                self.complete(opcode, &r);
            }
            // Read RSSI
            0x1405 => {
                let h = le16(p, 0).to_le_bytes();
                self.complete(opcode, &[OK, h[0], h[1], PEER_RSSI as u8]);
            }
            // LE Read Buffer Size (v1, v2)
            0x2002 => self.complete(opcode, &[OK, LE_MTU as u8, 0, ACL_BUFFERS]),
            0x2060 => self.complete(opcode, &[OK, LE_MTU as u8, 0, ACL_BUFFERS, 0, 0, 0]),
            // LE Read Local Supported Features
            0x2003 => self.complete(opcode, &[OK, 0, 0, 0, 0, 0, 0, 0, 0]),
            // LE Set Random Address
            0x2005 => {
                self.random_addr = p.get(..6).and_then(|a| a.try_into().ok());
                self.complete(opcode, &[OK]);
            }
            // LE Set Advertising Parameters
            0x2006 => {
                self.adv.kind = p.get(4).copied().unwrap_or(0);
                self.complete(opcode, &[OK]);
            }
            // LE Read Advertising Physical Channel Tx Power
            0x2007 => self.complete(opcode, &[OK, 0]),
            // LE Set Advertising Data / Scan Response Data
            0x2008 | 0x2009 => {
                let n = (p.first().copied().unwrap_or(0) as usize).min(31);
                let data = p.get(1..1 + n).unwrap_or(&[]).into();
                if opcode == 0x2008 {
                    self.adv.data = data;
                } else {
                    self.adv.scan_rsp = data;
                }
                self.complete(opcode, &[OK]);
            }
            // LE Set Advertising Enable
            0x200A => {
                if p.first() == Some(&1) {
                    self.unavailable();
                    self.adv.since.get_or_insert(now);
                } else {
                    self.adv.since = None;
                }
                self.complete(opcode, &[OK]);
            }
            // LE Set Scan Parameters
            0x200B => {
                self.scan_params = (p.first() == Some(&1), p.get(6).is_some_and(|f| f & 1 != 0));
                self.complete(opcode, &[OK]);
            }
            // LE Set Scan Enable
            0x200C => {
                if p.first() == Some(&1) {
                    self.unavailable();
                    let (active, filtered) = self.scan_params;
                    self.scan = Some(Scan { active, filtered, no_duplicates: p.get(1) == Some(&1), reported: Vec::new() });
                } else {
                    self.scan = None;
                }
                self.complete(opcode, &[OK]);
            }
            // LE Create Connection
            0x200D => {
                if self.connecting.is_some() {
                    return self.status(opcode, DISALLOWED);
                }
                self.unavailable();
                let peer = p.get(6..12).and_then(|a| a.try_into().ok()).unwrap_or_default();
                self.connecting = Some(Connecting { filtered: p.get(4) == Some(&1), peer, interval: le16(p, 13) });
                self.status(opcode, OK);
            }
            // LE Create Connection Cancel
            0x200E => {
                if self.connecting.take().is_none() {
                    return self.complete(opcode, &[DISALLOWED]);
                }
                self.complete(opcode, &[OK]);
                self.le_event(LE_CONN_COMPLETE, &[UNKNOWN_CONNECTION, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            }
            // LE Read Filter Accept List Size
            0x200F => self.complete(opcode, &[OK, 8]),
            // LE Clear / Add Device To Filter Accept List
            0x2010 => {
                self.accept.clear();
                self.complete(opcode, &[OK]);
            }
            0x2011 => {
                if let Some(a) = p.get(1..7).and_then(|a| a.try_into().ok()) {
                    self.accept.push(a);
                }
                self.complete(opcode, &[OK]);
            }
            // LE Connection Update
            0x2013 => {
                let handle = le16(p, 0);
                if !self.links.iter().any(|l| l.handle == handle) {
                    return self.status(opcode, UNKNOWN_CONNECTION);
                }
                self.status(opcode, OK);
                let mut e = vec![OK];
                e.extend_from_slice(&handle.to_le_bytes());
                e.extend_from_slice(&p[4..6.min(p.len())]); // interval: the minimum asked for
                e.extend_from_slice(p.get(6..10).unwrap_or(&[0; 4])); // latency, timeout
                self.le_event(LE_CONN_UPDATE_COMPLETE, &e);
            }
            // LE Read Remote Features
            0x2016 => {
                let handle = le16(p, 0);
                self.status(opcode, OK);
                let mut e = vec![OK];
                e.extend_from_slice(&handle.to_le_bytes());
                e.extend_from_slice(&[0; 8]);
                self.le_event(LE_REMOTE_FEATURES, &e);
            }
            // LE Read Supported States
            0x201C => {
                let mut r = vec![OK];
                r.extend_from_slice(&[0xFF; 8]);
                self.complete(opcode, &r);
            }
            // LE Set Data Length
            0x2022 => {
                let h = le16(p, 0).to_le_bytes();
                self.complete(opcode, &[OK, h[0], h[1]]);
            }
            // LE Read Suggested Default / Maximum Data Length
            0x2023 => self.complete(opcode, &[OK, LE_MTU as u8, 0, 0x48, 0x01]),
            0x202F => self.complete(opcode, &[OK, LE_MTU as u8, 0, 0x48, 0x01, LE_MTU as u8, 0, 0x48, 0x01]),
            // Disconnect
            0x0406 => {
                let handle = le16(p, 0);
                let Some(i) = self.links.iter().position(|l| l.handle == handle) else {
                    return self.status(opcode, UNKNOWN_CONNECTION);
                };
                self.status(opcode, OK);
                let link = self.links.remove(i);
                self.peers[link.peer].disconnected();
                let h = handle.to_le_bytes();
                self.event(EV_DISCONNECTION_COMPLETE, &[OK, h[0], h[1], LOCAL_HOST_TERMINATED]);
            }
            _ => {
                let name = format!("HCI command {opcode:#06x}");
                if !self.unknown.contains(&name) {
                    self.unknown.push(name);
                }
                self.complete(opcode, &[UNKNOWN_COMMAND]);
            }
        }
    }

    /// LE ACL data from the firmware: reassemble L2CAP and hand it on.
    fn acl(&mut self, pkt: &[u8]) {
        let word = le16(pkt, 0);
        let (handle, boundary) = (word & 0xFFF, word >> 12 & 3);
        let len = le16(pkt, 2) as usize;
        let data = pkt.get(4..4 + len).unwrap_or(&[]);
        let Some(link) = self.links.iter_mut().find(|l| l.handle == handle) else { return };
        link.completed += 1;
        if boundary == 0b01 {
            link.rx.extend_from_slice(data);
        } else {
            link.rx = data.into();
        }
        let frame_len = le16(&link.rx, 0) as usize + 4;
        if link.rx.len() < frame_len || link.rx.len() < 4 {
            return;
        }
        let frame = core::mem::take(&mut link.rx);
        let (peer, cid) = (link.peer, le16(&frame, 2));
        let payload = &frame[4..frame_len];
        match cid {
            ATT_CID => self.peers[peer].att(payload),
            SIGNAL_CID => self.signal(handle, payload),
            _ => {}
        }
    }

    /// LE signalling from the firmware: accept parameter updates, reject
    /// the rest.
    fn signal(&mut self, handle: u16, cmd: &[u8]) {
        let (code, id) = (cmd.first().copied().unwrap_or(0), cmd.get(1).copied().unwrap_or(0));
        let reply = match code {
            // Connection Parameter Update Request: accepted.
            0x12 => vec![0x13, id, 2, 0, 0, 0],
            // Responses need no answer.
            0x01 | 0x13 => return,
            // Command Reject: not understood.
            _ => vec![0x01, id, 2, 0, 0, 0],
        };
        self.send_l2cap(handle, SIGNAL_CID, &reply);
    }

    /// An L2CAP frame to the firmware, in LE-sized ACL fragments.
    fn send_l2cap(&mut self, handle: u16, cid: u16, payload: &[u8]) {
        let mut frame = Vec::with_capacity(payload.len() + 4);
        frame.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        frame.extend_from_slice(&cid.to_le_bytes());
        frame.extend_from_slice(payload);
        for (i, chunk) in frame.chunks(LE_MTU).enumerate() {
            let boundary: u16 = if i == 0 { 0b10 } else { 0b01 };
            let mut p = Vec::with_capacity(chunk.len() + 4);
            p.extend_from_slice(&(handle | boundary << 12).to_le_bytes());
            p.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
            p.extend_from_slice(chunk);
            self.out.push_back((ACL, p));
        }
    }

    fn connect(&mut self, peer: usize, central: bool, interval: u16) {
        let handle = self.next_handle;
        self.next_handle += 1;
        let p = &mut self.peers[peer];
        p.connected(handle);
        let mut e = vec![OK];
        e.extend_from_slice(&handle.to_le_bytes());
        e.push(if central { 0 } else { 1 });
        e.push(p.random as u8);
        e.extend_from_slice(&p.addr);
        e.extend_from_slice(&interval.to_le_bytes());
        e.extend_from_slice(&[0, 0]); // latency
        e.extend_from_slice(&800u16.to_le_bytes()); // supervision timeout: 8 s
        e.push(0); // clock accuracy
        self.le_event(LE_CONN_COMPLETE, &e);
        self.links.push(Link { handle, peer, central, interval, rx: Vec::new(), completed: 0 });
    }

    /// One step of the radio neighbourhood.
    fn tick(&mut self, now: u64) {
        // Advertising reports for the peers that advertise.
        if let Some(scan) = &self.scan {
            let mut reports = Vec::new();
            for (i, peer) in self.peers.iter().enumerate() {
                let Some((adv, rsp)) = peer.advertising() else { continue };
                if (scan.no_duplicates && scan.reported.contains(&i)) || (scan.filtered && !self.accept.contains(&peer.addr)) {
                    continue;
                }
                reports.push((i, ADV_IND, peer.random, peer.addr, adv));
                if scan.active {
                    reports.push((i, SCAN_RSP, peer.random, peer.addr, rsp));
                }
            }
            for (i, kind, random, addr, data) in reports {
                let mut e = vec![1, kind, random as u8];
                e.extend_from_slice(&addr);
                e.push(data.len() as u8);
                e.extend_from_slice(&data);
                e.push(PEER_RSSI as u8);
                self.le_event(LE_ADV_REPORT, &e);
                if let Some(s) = &mut self.scan
                    && !s.reported.contains(&i)
                {
                    s.reported.push(i);
                }
            }
        }
        // The firmware connecting to an advertising peer.
        if let Some(c) = &self.connecting {
            let target = self.peers.iter().position(|p| {
                p.advertising().is_some() && if c.filtered { self.accept.contains(&p.addr) } else { p.addr == c.peer }
            });
            if let Some(i) = target {
                let interval = c.interval;
                self.connecting = None;
                self.connect(i, true, interval);
            }
        }
        // A central peer connecting to the advertising firmware.
        if let Some(since) = self.adv.since
            && self.adv.kind == 0
            && now >= since + CONNECT_DELAY_US
            && let Some(i) = self.peers.iter().position(|p| p.is_central() && p.conn.is_none())
        {
            // Legacy advertising stops when a connection is made.
            self.adv.since = None;
            self.connect(i, false, 40);
        }
        // Connection events: credits back, then what the peers have to say.
        let mut done = Vec::new();
        for l in &mut self.links {
            if l.completed > 0 {
                done.push((l.handle, core::mem::take(&mut l.completed)));
            }
        }
        for (handle, n) in done {
            let mut e = vec![1];
            e.extend_from_slice(&handle.to_le_bytes());
            e.extend_from_slice(&n.to_le_bytes());
            self.event(EV_COMPLETED_PACKETS, &e);
        }
        for i in 0..self.links.len() {
            let (handle, peer) = (self.links[i].handle, self.links[i].peer);
            for pdu in self.peers[peer].poll(now) {
                self.send_l2cap(handle, ATT_CID, &pdu);
            }
        }
        for p in &mut self.peers {
            self.notes.append(&mut p.notes);
        }
    }

    /// The firmware's own address as peers see it.
    pub fn address(&self) -> [u8; 6] {
        self.random_addr.unwrap_or(self.public_addr)
    }

    /// Whether a link with the firmware as central (`true`) or peripheral exists.
    pub fn has_link(&self, central: bool) -> bool {
        self.links.iter().any(|l| l.central == central)
    }

    /// Connection interval of the first link, in 1.25 ms units.
    pub fn interval(&self) -> Option<u16> {
        self.links.first().map(|l| l.interval)
    }
}

impl HciBackend for VirtualController {
    fn available(&self) -> bool {
        self.available
    }

    fn send(&mut self, now: u64, kind: u8, packet: &[u8]) {
        match kind {
            super::COMMAND => self.command(now, packet),
            ACL => self.acl(packet),
            _ => {}
        }
    }

    fn recv(&mut self, now: u64) -> Option<(u8, Vec<u8>)> {
        if now >= self.next_tick {
            self.next_tick = now + TICK_US;
            self.tick(now);
        }
        self.out.pop_front()
    }

    fn unavailable_reason(&self) -> String {
        if self.available { String::new() } else { "Bluetooth unavailable: no radio".into() }
    }

    fn take_notes(&mut self) -> Vec<(&'static str, i64)> {
        core::mem::take(&mut self.notes)
    }

    /// `midi` with a message packed big-endian in the low bytes of `value`
    /// (`0x903C64` is note on, middle C): sent by every connected MIDI peer.
    fn input(&mut self, now: u64, action: &str, value: i64) -> bool {
        if action != "midi" {
            return false;
        }
        let bytes = (value as u32).to_be_bytes();
        let start = bytes.iter().position(|&b| b & 0x80 != 0).unwrap_or(3);
        for p in &mut self.peers {
            p.midi(now / 1000, &bytes[start..]);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::{COMMAND, peers};
    use super::*;

    fn cmd(c: &mut VirtualController, now: u64, opcode: u16, params: &[u8]) {
        let mut p = opcode.to_le_bytes().to_vec();
        p.push(params.len() as u8);
        p.extend_from_slice(params);
        c.send(now, COMMAND, &p);
    }

    fn drain(c: &mut VirtualController, now: u64) -> Vec<(u8, Vec<u8>)> {
        core::iter::from_fn(|| c.recv(now)).collect()
    }

    fn le_events(got: &[(u8, Vec<u8>)], sub: u8) -> Vec<Vec<u8>> {
        got.iter().filter(|(k, p)| *k == EVENT && p[0] == EV_LE_META && p[2] == sub).map(|(_, p)| p[3..].to_vec()).collect()
    }

    #[test]
    fn unknown_commands_fail_and_are_listed() {
        let mut c = VirtualController::new([1; 6], Vec::new());
        cmd(&mut c, 0, 0x2036, &[]);
        assert_eq!(drain(&mut c, 0), [(EVENT, vec![EV_COMMAND_COMPLETE, 4, 1, 0x36, 0x20, UNKNOWN_COMMAND])]);
        assert_eq!(c.unknown, ["HCI command 0x2036"]);
    }

    #[test]
    fn the_firmware_connects_to_a_peripheral_and_talks_att() {
        let mut c = VirtualController::new([1; 6], peers::neighbourhood());
        cmd(&mut c, 0, 0x2010, &[]);
        let mut add = vec![1];
        add.extend_from_slice(&peers::BATTERY_ADDR);
        cmd(&mut c, 0, 0x2011, &add);
        let mut create = vec![0x10, 0, 0x10, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0x40, 0, 0x40, 0, 0, 0, 0x80, 0x0C, 0, 0, 0, 0];
        create.truncate(25);
        cmd(&mut c, 0, 0x200D, &create);
        let got = drain(&mut c, 0);
        assert!(got.iter().any(|(_, p)| p[..3] == [EV_COMMAND_STATUS, 4, OK]));
        // The battery advertises, so the next radio tick connects.
        let conn = &le_events(&got, LE_CONN_COMPLETE)[0];
        assert_eq!((conn[0], conn[3], &conn[5..11]), (OK, 0, &peers::BATTERY_ADDR[..]), "connected as central to the battery");
        let handle = le16(conn, 1);
        // Read By Group Type over L2CAP/ACL, in one packet.
        let mut acl = (handle | 0b10 << 12).to_le_bytes().to_vec();
        acl.extend_from_slice(&[11, 0, 7, 0, 4, 0, 0x10, 1, 0, 0xFF, 0xFF, 0x00, 0x28]);
        c.send(0, ACL, &acl);
        let got = drain(&mut c, 2 * TICK_US);
        assert!(got.iter().any(|(k, p)| *k == EVENT && p[0] == EV_COMPLETED_PACKETS), "the packet's buffer comes back");
        let rsp: Vec<_> = got.iter().filter(|(k, _)| *k == ACL).collect();
        assert_eq!(rsp.len(), 1);
        assert_eq!(rsp[0].1[8..10], [0x11, 6], "a Read By Group Type response");
        assert!(c.has_link(true));
        cmd(&mut c, 0, 0x0406, &[handle as u8, (handle >> 8) as u8, 0x13]);
        let got = drain(&mut c, 3 * TICK_US);
        assert!(got.iter().any(|(_, p)| p[0] == EV_DISCONNECTION_COMPLETE));
        assert!(!c.has_link(true));
    }

    #[test]
    fn a_central_connects_to_the_advertising_firmware() {
        let mut c = VirtualController::new([1; 6], peers::neighbourhood());
        cmd(&mut c, 0, 0x2006, &[0x20, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7, 0]);
        cmd(&mut c, 0, 0x200A, &[1]);
        drain(&mut c, 0);
        assert!(le_events(&drain(&mut c, CONNECT_DELAY_US / 2), LE_CONN_COMPLETE).is_empty(), "not straight away");
        let got = drain(&mut c, CONNECT_DELAY_US);
        let conn = &le_events(&got, LE_CONN_COMPLETE)[0];
        assert_eq!((conn[3], &conn[5..11]), (1, &peers::CENTRAL_ADDR[..]), "connected as peripheral to the central");
        assert!(c.has_link(false));
        // The central's first move, at that same connection event: an MTU exchange.
        let acl: Vec<_> = got.iter().filter(|(k, _)| *k == ACL).collect();
        assert_eq!(acl[0].1[4..], [3, 0, 4, 0, 0x02, 23, 0]);
    }

    #[test]
    fn long_frames_go_out_in_le_sized_fragments() {
        let mut c = VirtualController::new([1; 6], Vec::new());
        c.send_l2cap(0x40, 4, &[7; 40]);
        let got = drain(&mut c, 0);
        assert_eq!(got.len(), 2);
        assert_eq!((le16(&got[0].1, 0) >> 12, le16(&got[0].1, 2)), (0b10, 27));
        assert_eq!((le16(&got[1].1, 0) >> 12, le16(&got[1].1, 2)), (0b01, 17));
    }

    #[test]
    fn with_no_radio_nothing_is_found_and_the_attempt_is_reported() {
        let mut c = VirtualController::no_radio([1; 6]);
        cmd(&mut c, 0, 0x200C, &[1, 0]);
        let got = drain(&mut c, TICK_US);
        assert!(le_events(&got, LE_ADV_REPORT).is_empty());
        assert_eq!(c.take_notes(), [("bt_unavailable", 1)]);
        assert!(!c.available());
    }
}
