//! The firmware side of the Wi-Fi host driver protocol: SDPCM framing with
//! credits, CDC ioctls and iovars, Broadcom event packets, scans and joins
//! against a list of mock access points, and the Ethernet data path.
//!
//! Layouts follow Embassy `cyw43` (`structs.rs`). Event packets are
//! big-endian; everything else little-endian.
use super::wpa;
use crate::net::NetBackend;
use alloc::{boxed::Box, collections::BTreeMap, collections::VecDeque, string::String, vec, vec::Vec};

// SDPCM channels.
const CONTROL: u8 = 0;
const EVENT: u8 = 1;
const DATA: u8 = 2;
const SDPCM_HEADER: usize = 12;
const CDC_HEADER: usize = 16;

// Ioctl numbers.
const WLC_SET_SSID: u32 = 26;
const WLC_DISASSOC: u32 = 52;
const WLC_SET_WSEC: u32 = 134;
const WLC_SET_WPA_AUTH: u32 = 165;
const WLC_SET_WSEC_PMK: u32 = 268;
const WLC_GET_VAR: u32 = 262;
const WLC_SET_VAR: u32 = 263;

// Event numbers (`events.rs`).
const E_SET_SSID: u32 = 0;
const E_JOIN: u32 = 1;
const E_AUTH: u32 = 3;
const E_DEAUTH_IND: u32 = 6;
const E_ASSOC: u32 = 7;
const E_LINK: u32 = 16;
const E_PSK_SUP: u32 = 46;
const E_ESCAN_RESULT: u32 = 69;

// Event status codes.
const ST_SUCCESS: u32 = 0;
const ST_FAIL: u32 = 1;
const ST_NO_NETWORKS: u32 = 3;
const ST_PARTIAL: u32 = 8;
const WLC_SUP_KEYED: u32 = 6;
const WLC_SUP_KEYXCHANGE_WAIT_M1: u32 = 4;

const BSS_INFO_SIZE: usize = 124;
const SCAN_RESULTS_SIZE: usize = 12;

/// A mock access point from the board file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ap {
    pub ssid: String,
    pub wpa2: bool,
    pub pass: String,
    pub rssi: i16,
    pub chan: u8,
    pub bssid: [u8; 6],
}

/// A frame the chip will send to the host, before SDPCM framing.
struct Frame {
    channel: u8,
    payload: Vec<u8>,
}

enum Later {
    Frame(Frame),
    LinkUp(usize),
    LinkDown,
}

/// Where the station is with respect to the mock network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Down,
    /// A join is in progress.
    Joining,
    Up(usize),
}

pub struct Whd {
    pub mac: [u8; 6],
    pub aps: Vec<Ap>,
    pub backend: Option<Box<dyn NetBackend>>,
    cycles_per_us: u64,
    /// Sequence number of the last frame the host sent (the credit base).
    host_seq: u8,
    tx_seq: u8,
    rx: VecDeque<Frame>,
    later: Vec<(u64, Later)>,
    /// Observable happenings for the board's event queue.
    pub notes: Vec<(&'static str, i64)>,
    /// Last value the host SET for each iovar.
    vars: BTreeMap<String, Vec<u8>>,
    /// iovars and ioctls nobody modelled (each once), for debugging.
    pub unknown: Vec<String>,
    gpio: u32,
    wsec: u32,
    /// WSEC_PMK: (flags, key bytes).
    pmk: Option<(u16, Vec<u8>)>,
    pub link: Link,
    /// SSID last joined or being joined, for the UI.
    pub joined_ssid: Option<String>,
}

fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Variables the driver sets that need no behaviour.
const QUIET_VARS: &[&str] = &[
    "clmload",
    "bus:txglom",
    "apsta",
    "country",
    "ampdu_ba_wsize",
    "ampdu_mpdu",
    "bsscfg:event_msgs",
    "bsscfg:sup_wpa",
    "bsscfg:sup_wpa2_eapver",
    "bsscfg:sup_wpa_tmo",
    "pm2_sleep_ret",
    "bcn_li_bcn",
    "bcn_li_dtim",
    "assoc_listen",
    "mcast_list",
    "bsscfg:ssid",
    "bsscfg:wsec",
    "bsscfg:wpa_auth",
    "2g_mrate",
    "bss",
];

impl Whd {
    pub fn new(mac: [u8; 6], aps: Vec<Ap>, cycles_per_us: u64) -> Self {
        Self {
            mac,
            aps,
            backend: None,
            cycles_per_us,
            host_seq: 0,
            tx_seq: 0,
            rx: VecDeque::new(),
            later: Vec::new(),
            notes: Vec::new(),
            vars: BTreeMap::new(),
            unknown: Vec::new(),
            gpio: 0,
            wsec: 0,
            pmk: None,
            link: Link::Down,
            joined_ssid: None,
        }
    }

    /// The chip's own GPIO outputs (bit 0 is the Pico W's LED).
    pub fn gpio(&self) -> u32 {
        self.gpio
    }

    fn ms(&self, n: u64) -> u64 {
        n * 1000 * self.cycles_per_us
    }

    fn us(&self, n: u64) -> u64 {
        n * self.cycles_per_us
    }

    fn available(&self) -> bool {
        self.backend.as_ref().is_some_and(|b| b.available())
    }

    // ---- toward the host ----

    pub fn has_frame(&self) -> bool {
        !self.rx.is_empty()
    }

    /// Length of the next frame (SDPCM header included).
    pub fn next_len(&self) -> Option<usize> {
        self.rx.front().map(|f| SDPCM_HEADER + f.payload.len())
    }

    /// Take the next frame, framed for the bus.
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let f = self.rx.pop_front()?;
        let len = SDPCM_HEADER + f.payload.len();
        let mut v = Vec::with_capacity(len);
        v.extend_from_slice(&(len as u16).to_le_bytes());
        v.extend_from_slice(&(!len as u16).to_le_bytes());
        v.push(self.tx_seq);
        self.tx_seq = self.tx_seq.wrapping_add(1);
        v.push(f.channel);
        v.push(0); // next length
        v.push(SDPCM_HEADER as u8); // data offset
        v.push(0); // flow control
        v.push(self.host_seq.wrapping_add(8)); // credit: the host may send 8 more
        v.extend_from_slice(&[0, 0]);
        v.extend_from_slice(&f.payload);
        Some(v)
    }

    pub fn next_due(&self) -> Option<u64> {
        self.later.iter().map(|l| l.0).min()
    }

    /// Deliver everything scheduled at or before `now`.
    pub fn tick(&mut self, now: u64) {
        while let Some(pos) = self.later.iter().enumerate().filter(|(_, l)| l.0 <= now).min_by_key(|(_, l)| l.0).map(|(i, _)| i) {
            let (_, what) = self.later.remove(pos);
            match what {
                Later::Frame(f) => self.rx.push_back(f),
                Later::LinkUp(i) => {
                    self.link = Link::Up(i);
                    if let Some(b) = self.backend.as_mut() {
                        b.link_changed(true);
                    }
                    self.notes.push(("link", 1));
                }
                Later::LinkDown => self.link_down(),
            }
        }
    }

    fn link_down(&mut self) {
        let was_up = matches!(self.link, Link::Up(_));
        self.link = Link::Down;
        self.joined_ssid = None;
        if let Some(b) = self.backend.as_mut() {
            b.link_changed(false);
        }
        if was_up {
            self.notes.push(("link", 0));
        }
    }

    /// Pull frames the network has for the firmware. Returns true if any
    /// arrived.
    pub fn poll_backend(&mut self) -> bool {
        let mut any = false;
        if !matches!(self.link, Link::Up(_)) {
            return false;
        }
        while self.rx.len() < 32 {
            let Some(eth) = self.backend.as_mut().and_then(|b| b.recv()) else { break };
            let mut payload = vec![2 << 4, 0, 0, 0]; // BDC v2, no offset
            payload.extend_from_slice(&eth);
            self.rx.push_back(Frame { channel: DATA, payload });
            any = true;
        }
        any
    }

    // ---- from the host ----

    /// A complete F2 write: one SDPCM frame.
    pub fn host_frame(&mut self, now: u64, bytes: &[u8]) {
        if bytes.len() < SDPCM_HEADER {
            return;
        }
        let len = (le16(bytes, 0) as usize).min(bytes.len());
        let header = (bytes[7] as usize).clamp(SDPCM_HEADER, len.max(SDPCM_HEADER));
        self.host_seq = bytes[4];
        let payload = bytes.get(header..len).unwrap_or(&[]);
        match bytes[5] & 0xF {
            CONTROL => self.ioctl(now, payload),
            DATA => self.data(payload),
            _ => {}
        }
    }

    fn data(&mut self, p: &[u8]) {
        // BDC header, then `data_offset` words of options, then Ethernet.
        let Some(&offset) = p.get(3) else { return };
        let Some(eth) = p.get(4 + 4 * offset as usize..) else { return };
        if matches!(self.link, Link::Up(_))
            && eth.len() >= 14
            && let Some(b) = self.backend.as_mut()
        {
            b.send(eth);
        }
    }

    fn ioctl(&mut self, now: u64, p: &[u8]) {
        if p.len() < CDC_HEADER {
            return;
        }
        let cmd = le32(p, 0);
        let req_len = le32(p, 4) as usize;
        let flags = le16(p, 8);
        let id = le16(p, 10);
        let data = p.get(CDC_HEADER..).unwrap_or(&[]);
        let data = &data[..req_len.min(data.len())];
        let set = flags & 2 != 0;
        let resp = self.dispatch(now, cmd, set, data, req_len);
        // The host copies the response over its request buffer, so it must
        // not be longer than the request.
        let resp = &resp[..resp.len().min(req_len)];
        let mut payload = Vec::with_capacity(CDC_HEADER + resp.len());
        payload.extend_from_slice(&cmd.to_le_bytes());
        payload.extend_from_slice(&(resp.len() as u32).to_le_bytes());
        payload.extend_from_slice(&flags.to_le_bytes());
        payload.extend_from_slice(&id.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes()); // status: the driver panics on anything else
        payload.extend_from_slice(resp);
        let at = now + self.us(30);
        self.later.push((at, Later::Frame(Frame { channel: CONTROL, payload })));
    }

    fn dispatch(&mut self, now: u64, cmd: u32, set: bool, data: &[u8], req_len: usize) -> Vec<u8> {
        match cmd {
            WLC_SET_VAR => {
                let (name, value) = split_name(data);
                self.set_var(now, name, value);
                Vec::new()
            }
            WLC_GET_VAR => {
                let (name, _) = split_name(data);
                self.get_var(name, req_len)
            }
            WLC_SET_SSID => {
                self.join(now, data);
                Vec::new()
            }
            WLC_SET_WSEC => {
                self.wsec = le32(data, 0);
                Vec::new()
            }
            WLC_SET_WSEC_PMK => {
                // PassphraseInfo: len u16, flags u16, key[64]
                let n = (le16(data, 0) as usize).min(64);
                self.pmk = Some((le16(data, 2), data.get(4..4 + n).unwrap_or(&[]).to_vec()));
                Vec::new()
            }
            WLC_DISASSOC => {
                if self.link != Link::Down {
                    let bssid = self.joined_bssid();
                    self.link_down();
                    self.event(now + self.ms(1), E_DEAUTH_IND, ST_SUCCESS, 3, 0, 0, bssid, &[]);
                }
                Vec::new()
            }
            // Everything else the driver sets is accepted; a GET returns zeros.
            _ if set => Vec::new(),
            _ => {
                if cmd != WLC_SET_WPA_AUTH {
                    self.note_unknown(alloc::format!("ioctl {cmd}"));
                }
                vec![0; req_len]
            }
        }
    }

    fn note_unknown(&mut self, what: String) {
        if self.unknown.len() < 64 && !self.unknown.contains(&what) {
            self.unknown.push(what);
        }
    }

    fn get_var(&mut self, name: &str, req_len: usize) -> Vec<u8> {
        let mut v = match name {
            "cur_etheraddr" => self.mac.to_vec(),
            "clmload_status" => vec![0; 4],
            "gpioout" => self.gpio.to_le_bytes().to_vec(),
            _ => match self.vars.get(name) {
                Some(v) => v.clone(),
                None => {
                    self.note_unknown(alloc::format!("get {name}"));
                    Vec::new()
                }
            },
        };
        v.resize(req_len, 0);
        v
    }

    fn set_var(&mut self, now: u64, name: &str, value: &[u8]) {
        match name {
            "gpioout" => {
                let (mask, val) = (le32(value, 0), le32(value, 4));
                let new = self.gpio & !mask | val & mask;
                if (new ^ self.gpio) & 1 != 0 {
                    self.notes.push(("led", (new & 1) as i64));
                }
                self.gpio = new;
            }
            "escan" => self.escan(now, value),
            _ => {
                if !QUIET_VARS.contains(&name) {
                    self.note_unknown(alloc::format!("set {name}"));
                }
                self.vars.insert(name.into(), value.to_vec());
            }
        }
    }

    // ---- events ----

    #[allow(clippy::too_many_arguments)]
    fn event(&mut self, at: u64, event: u32, status: u32, reason: u32, auth_type: u32, flags: u16, addr: [u8; 6], data: &[u8]) {
        let mut p = Vec::with_capacity(4 + 72 + data.len());
        p.extend_from_slice(&[2 << 4, 0, 0, 0]); // BDC
        // Ethernet header: the tunnelled-event ethertype.
        p.extend_from_slice(&self.mac);
        p.extend_from_slice(&addr);
        p.extend_from_slice(&0x886Cu16.to_be_bytes());
        // Broadcom event header.
        p.extend_from_slice(&32769u16.to_be_bytes()); // BCMILCP_SUBTYPE_VENDOR_LONG
        p.extend_from_slice(&((48 + data.len()) as u16).to_be_bytes());
        p.push(0);
        p.extend_from_slice(&[0x00, 0x10, 0x18]);
        p.extend_from_slice(&1u16.to_be_bytes()); // BCMILCP_BCM_SUBTYPE_EVENT
        // Event message.
        p.extend_from_slice(&2u16.to_be_bytes());
        p.extend_from_slice(&flags.to_be_bytes());
        p.extend_from_slice(&event.to_be_bytes());
        p.extend_from_slice(&status.to_be_bytes());
        p.extend_from_slice(&reason.to_be_bytes());
        p.extend_from_slice(&auth_type.to_be_bytes());
        p.extend_from_slice(&(data.len() as u32).to_be_bytes());
        p.extend_from_slice(&addr);
        let mut ifname = [0u8; 16];
        ifname[..5].copy_from_slice(b"wlan0");
        p.extend_from_slice(&ifname);
        p.extend_from_slice(&[0, 0]); // ifidx, bsscfgidx
        p.extend_from_slice(data);
        self.later.push((at, Later::Frame(Frame { channel: EVENT, payload: p })));
    }

    // ---- scan ----

    fn escan(&mut self, now: u64, p: &[u8]) {
        // ScanParams: version u32, action u16, sync_id u16, ssid_len u32, ssid[32], bssid[6], ...
        if le16(p, 4) != 1 {
            return; // only "start" is modelled
        }
        let sync_id = le16(p, 6);
        let want_len = (le32(p, 8) as usize).min(32);
        let want_ssid = p.get(12..12 + want_len).unwrap_or(&[]).to_vec();
        let want_bssid: [u8; 6] = p.get(44..50).and_then(|b| b.try_into().ok()).unwrap_or([0xFF; 6]);
        let mut at = now + self.ms(8);
        let mut found = 0;
        if self.available() {
            let aps: Vec<Ap> = self
                .aps
                .iter()
                .filter(|a| want_len == 0 || a.ssid.as_bytes() == want_ssid.as_slice())
                .filter(|a| want_bssid == [0xFF; 6] || a.bssid == want_bssid)
                .cloned()
                .collect();
            for ap in &aps {
                let mut data = Vec::with_capacity(SCAN_RESULTS_SIZE + BSS_INFO_SIZE);
                data.extend_from_slice(&((SCAN_RESULTS_SIZE + BSS_INFO_SIZE) as u32).to_le_bytes());
                data.extend_from_slice(&109u32.to_le_bytes());
                data.extend_from_slice(&sync_id.to_le_bytes());
                data.extend_from_slice(&1u16.to_le_bytes());
                data.extend_from_slice(&bss_info(ap));
                self.event(at, E_ESCAN_RESULT, ST_PARTIAL, 0, 0, 0, ap.bssid, &data);
                at += self.ms(4);
                found += 1;
            }
        } else {
            self.notes.push(("unavailable", 1));
        }
        self.event(at, E_ESCAN_RESULT, ST_SUCCESS, 0, 0, 0, [0; 6], &[]);
        self.notes.push(("scan", found));
    }

    // ---- join ----

    fn joined_bssid(&self) -> [u8; 6] {
        match self.link {
            Link::Up(i) => self.aps.get(i).map_or([0; 6], |a| a.bssid),
            _ => [0; 6],
        }
    }

    fn password_ok(&self, ap: &Ap) -> bool {
        match &self.pmk {
            Some((flags, key)) if flags & 1 != 0 => key.as_slice() == ap.pass.as_bytes(),
            Some((_, key)) => key.as_slice() == wpa::psk(ap.pass.as_bytes(), ap.ssid.as_bytes()),
            None => false,
        }
    }

    fn join(&mut self, now: u64, data: &[u8]) {
        let n = (le32(data, 0) as usize).min(32);
        let ssid = data.get(4..4 + n).unwrap_or(&[]).to_vec();
        if self.link != Link::Down {
            self.link_down();
        }
        if !self.available() {
            self.notes.push(("unavailable", 1));
            self.notes.push(("join", 0));
            self.event(now + self.ms(50), E_SET_SSID, ST_NO_NETWORKS, 0, 0, 0, [0; 6], &[]);
            return;
        }
        let Some(i) = self.aps.iter().position(|a| a.ssid.as_bytes() == ssid.as_slice()) else {
            // The chip scans every channel before giving up.
            self.notes.push(("join", 0));
            self.event(now + self.ms(300), E_SET_SSID, ST_NO_NETWORKS, 0, 0, 0, [0; 6], &[]);
            return;
        };
        let ap = self.aps[i].clone();
        // Security has to agree on both sides: AES (4) with a matching key
        // for WPA2, nothing for an open network.
        let ok = if ap.wpa2 { self.wsec & 4 != 0 && self.password_ok(&ap) } else { self.wsec & 7 == 0 };
        self.joined_ssid = Some(ap.ssid.clone());
        self.link = Link::Joining;
        let b = ap.bssid;
        self.event(now + self.ms(30), E_AUTH, ST_SUCCESS, 0, 0, 0, b, &[]);
        if ok {
            self.event(now + self.ms(40), E_ASSOC, ST_SUCCESS, 0, 0, 0, b, &[]);
            if ap.wpa2 {
                self.event(now + self.ms(60), E_PSK_SUP, WLC_SUP_KEYED, 0, 0, 0, b, &[]);
            }
            self.event(now + self.ms(70), E_LINK, ST_SUCCESS, 0, 0, 1, b, &[]);
            self.event(now + self.ms(80), E_SET_SSID, ST_SUCCESS, 0, 0, 0, b, &[]);
            self.later.push((now + self.ms(80), Later::LinkUp(i)));
            // Newer drivers (`cyw43` 0.7) report the link up on JOIN, so it
            // comes once the link is (frames sent before would be lost).
            self.event(now + self.ms(80), E_JOIN, ST_SUCCESS, 0, 0, 0, b, &[]);
            self.notes.push(("join", 1));
        } else {
            // A wrong key stalls the four-way handshake until the chip gives up.
            if ap.wpa2 {
                self.event(now + self.ms(60), E_PSK_SUP, WLC_SUP_KEYXCHANGE_WAIT_M1, 0, 0, 0, b, &[]);
            }
            self.event(now + self.ms(100), E_SET_SSID, ST_FAIL, 0, 0, 0, b, &[]);
            self.later.push((now + self.ms(100), Later::LinkDown));
            self.notes.push(("join", 0));
        }
    }
}

fn split_name(data: &[u8]) -> (&str, &[u8]) {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let name = core::str::from_utf8(&data[..end]).unwrap_or("");
    (name, data.get(end + 1..).unwrap_or(&[]))
}

/// A `wl_bss_info_t` for `ap`.
fn bss_info(ap: &Ap) -> [u8; BSS_INFO_SIZE] {
    let mut b = [0u8; BSS_INFO_SIZE];
    let put16 = |b: &mut [u8], at: usize, v: u16| b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    let put32 = |b: &mut [u8], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    put32(&mut b, 0, 109); // version
    put32(&mut b, 4, BSS_INFO_SIZE as u32);
    b[8..14].copy_from_slice(&ap.bssid);
    put16(&mut b, 14, 100); // beacon period
    put16(&mut b, 16, 0x0001 | if ap.wpa2 { 0x0010 } else { 0 }); // ESS, privacy
    let ssid = ap.ssid.as_bytes();
    let n = ssid.len().min(32);
    b[18] = n as u8;
    b[19..19 + n].copy_from_slice(&ssid[..n]);
    put32(&mut b, 52, 8); // rateset count
    b[56..64].copy_from_slice(&[0x82, 0x84, 0x8B, 0x96, 0x0C, 0x12, 0x18, 0x24]);
    // Chanspec: channel, 20 MHz, 2.4 GHz band.
    put16(&mut b, 72, ap.chan as u16 | 0x1000 | if ap.chan > 14 { 0xC000 } else { 0 });
    b[76] = 1; // DTIM period
    put16(&mut b, 78, ap.rssi as u16);
    b[80] = (-92i8) as u8; // noise
    b[81] = 1; // n_cap
    b[88] = ap.chan; // control channel
    put16(&mut b, 116, BSS_INFO_SIZE as u16); // IEs start after the struct...
    put32(&mut b, 118, 0); // ...and there are none
    put16(&mut b, 122, (ap.rssi + 92) as u16); // SNR
    b
}
