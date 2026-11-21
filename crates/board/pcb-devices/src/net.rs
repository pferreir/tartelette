//! The network behind a Wi-Fi chip: what the emulated radio hands Ethernet
//! frames to, and where the frames for the firmware come from.
//!
//! [`NetBackend`] is the seam. The `cyw43439` model only speaks 802.11-ish
//! management and shuttles Ethernet II frames; a host provides the world
//! behind it (the `bridge-net` crate does real sockets on native builds; the
//! browser build has none). [`Gateway`] is the part every backend shares: a
//! fixed slirp-style address plan with ARP, DHCP and ICMP echo for the
//! gateway, and [`LoopbackBackend`] is a deterministic backend built on it for
//! tests (adds a UDP echo service on the gateway).
//!
//! Addressing: gateway `10.0.2.2`, DNS `10.0.2.3`, guest `10.0.2.15/24`.
use alloc::{boxed::Box, collections::VecDeque, string::String, vec::Vec};

pub const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
pub const DNS_IP: [u8; 4] = [10, 0, 2, 3];
pub const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
pub const NETMASK: [u8; 4] = [255, 255, 255, 0];
/// MAC the gateway (and DNS) answer ARP with.
pub const GATEWAY_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];

/// Whatever sits behind the radio. One Ethernet II frame per call, no FCS.
pub trait NetBackend: Send {
    /// False when there is no network at all (the browser build): scans come
    /// back empty and joins fail.
    fn available(&self) -> bool;
    /// The station joined (`true`) or left a network.
    fn link_changed(&mut self, _up: bool) {}
    /// Frame from the firmware.
    fn send(&mut self, frame: &[u8]);
    /// Frame for the firmware, if one is waiting. Polled every ~1 ms of
    /// emulated time while the link is up.
    fn recv(&mut self) -> Option<Vec<u8>>;
    /// The address the guest was leased, once it has one.
    fn lease(&self) -> Option<[u8; 4]> {
        None
    }
    /// Why the network is unavailable, for the UI.
    fn unavailable_reason(&self) -> String {
        String::new()
    }
}

/// A backend with nothing behind it.
pub struct NoNetwork;

impl NetBackend for NoNetwork {
    fn available(&self) -> bool {
        false
    }
    fn send(&mut self, _frame: &[u8]) {}
    fn recv(&mut self) -> Option<Vec<u8>> {
        None
    }
}

// ---- frame helpers ----

pub const ETH_IPV4: u16 = 0x0800;
pub const ETH_ARP: u16 = 0x0806;

pub fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

/// RFC 1071 one's-complement sum of `data`, folded, not inverted.
pub fn sum16(data: &[u8], init: u32) -> u32 {
    let mut sum = init;
    let (chunks, rest) = data.as_chunks::<2>();
    for c in chunks {
        sum += u16::from_be_bytes(*c) as u32;
    }
    if let [last] = rest {
        sum += (*last as u32) << 8;
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    sum
}

pub fn checksum(data: &[u8]) -> u16 {
    !(sum16(data, 0) as u16)
}

/// Checksum of a UDP/TCP segment including the IPv4 pseudo header.
pub fn transport_checksum(src: [u8; 4], dst: [u8; 4], proto: u8, segment: &[u8]) -> u16 {
    let mut pseudo = [0u8; 12];
    pseudo[..4].copy_from_slice(&src);
    pseudo[4..8].copy_from_slice(&dst);
    pseudo[9] = proto;
    pseudo[10..12].copy_from_slice(&(segment.len() as u16).to_be_bytes());
    let sum = sum16(segment, sum16(&pseudo, 0));
    match !(sum as u16) {
        0 => 0xFFFF,
        c => c,
    }
}

/// An Ethernet + IPv4 frame around `payload` (protocol `proto`).
pub fn ipv4_frame(
    dst_mac: [u8; 6],
    src_mac: [u8; 6],
    src: [u8; 4],
    dst: [u8; 4],
    proto: u8,
    payload: &[u8],
) -> Vec<u8> {
    let total = 20 + payload.len();
    let mut f = Vec::with_capacity(14 + total);
    f.extend_from_slice(&dst_mac);
    f.extend_from_slice(&src_mac);
    f.extend_from_slice(&ETH_IPV4.to_be_bytes());
    f.extend_from_slice(&[
        0x45,
        0,
        (total >> 8) as u8,
        total as u8,
        0,
        0,
        0x40,
        0,
        64,
        proto,
        0,
        0,
    ]);
    f.extend_from_slice(&src);
    f.extend_from_slice(&dst);
    let c = checksum(&f[14..34]);
    f[24..26].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

pub fn udp_frame(
    dst_mac: [u8; 6],
    src_mac: [u8; 6],
    src: ([u8; 4], u16),
    dst: ([u8; 4], u16),
    data: &[u8],
) -> Vec<u8> {
    let len = 8 + data.len();
    let mut seg = Vec::with_capacity(len);
    seg.extend_from_slice(&src.1.to_be_bytes());
    seg.extend_from_slice(&dst.1.to_be_bytes());
    seg.extend_from_slice(&(len as u16).to_be_bytes());
    seg.extend_from_slice(&[0, 0]);
    seg.extend_from_slice(data);
    let c = transport_checksum(src.0, dst.0, 17, &seg);
    seg[6..8].copy_from_slice(&c.to_be_bytes());
    ipv4_frame(dst_mac, src_mac, src.0, dst.0, 17, &seg)
}

/// A parsed IPv4 packet inside an Ethernet frame.
#[derive(Clone, Debug)]
pub struct Ipv4<'a> {
    pub src_mac: [u8; 6],
    pub src: [u8; 4],
    pub dst: [u8; 4],
    pub proto: u8,
    /// Everything after the IP header (up to the IP total length).
    pub payload: &'a [u8],
}

pub fn parse_ipv4(frame: &[u8]) -> Option<Ipv4<'_>> {
    if frame.len() < 34 || be16(frame, 12) != ETH_IPV4 || frame[14] >> 4 != 4 {
        return None;
    }
    let ihl = (frame[14] & 15) as usize * 4;
    let total = be16(frame, 16) as usize;
    if ihl < 20 || total < ihl || 14 + total > frame.len() {
        return None;
    }
    Some(Ipv4 {
        src_mac: frame[6..12].try_into().ok()?,
        src: frame[26..30].try_into().ok()?,
        dst: frame[30..34].try_into().ok()?,
        proto: frame[23],
        payload: &frame[14 + ihl..14 + total],
    })
}

/// A parsed UDP datagram.
pub struct Udp<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub data: &'a [u8],
}

pub fn parse_udp(payload: &[u8]) -> Option<Udp<'_>> {
    if payload.len() < 8 {
        return None;
    }
    let len = (be16(payload, 4) as usize).clamp(8, payload.len());
    Some(Udp {
        src_port: be16(payload, 0),
        dst_port: be16(payload, 2),
        data: &payload[8..len],
    })
}

// ---- the gateway ----

/// ARP, DHCP and ICMP echo for the gateway and DNS addresses.
pub struct Gateway {
    /// What the guest asked to be called / was leased.
    leased: Option<[u8; 4]>,
    guest_mac: Option<[u8; 6]>,
    pub out: VecDeque<Vec<u8>>,
}

/// What [`Gateway::handle`] did with a frame.
pub enum Handled {
    /// Dealt with (a reply may be queued in [`Gateway::out`]).
    Yes,
    /// Not for the gateway: the backend's business.
    No,
}

impl Default for Gateway {
    fn default() -> Self {
        Self::new()
    }
}

impl Gateway {
    pub fn new() -> Self {
        Self {
            leased: None,
            guest_mac: None,
            out: VecDeque::new(),
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn lease(&self) -> Option<[u8; 4]> {
        self.leased
    }

    pub fn guest_mac(&self) -> Option<[u8; 6]> {
        self.guest_mac
    }

    pub fn handle(&mut self, frame: &[u8]) -> Handled {
        if frame.len() < 14 {
            return Handled::Yes;
        }
        self.guest_mac = frame[6..12].try_into().ok();
        match be16(frame, 12) {
            ETH_ARP => {
                self.arp(frame);
                Handled::Yes
            }
            ETH_IPV4 => {
                let Some(ip) = parse_ipv4(frame) else {
                    return Handled::Yes;
                };
                if ip.proto == 17
                    && let Some(udp) = parse_udp(ip.payload)
                    && udp.dst_port == 67
                {
                    self.dhcp(&ip, &udp);
                    return Handled::Yes;
                }
                if ip.proto == 1 && ip.dst == GATEWAY_IP && ip.payload.first() == Some(&8) {
                    let mut reply = ip.payload.to_vec();
                    reply[0] = 0;
                    reply[2..4].copy_from_slice(&[0, 0]);
                    let c = checksum(&reply);
                    reply[2..4].copy_from_slice(&c.to_be_bytes());
                    self.out.push_back(ipv4_frame(
                        ip.src_mac,
                        GATEWAY_MAC,
                        GATEWAY_IP,
                        ip.src,
                        1,
                        &reply,
                    ));
                    return Handled::Yes;
                }
                Handled::No
            }
            _ => Handled::Yes,
        }
    }

    fn arp(&mut self, f: &[u8]) {
        // Ethernet/IPv4 request ("who has")
        if f.len() < 42 || be16(f, 14) != 1 || be16(f, 16) != ETH_IPV4 || be16(f, 20) != 1 {
            return;
        }
        let target: [u8; 4] = f[38..42].try_into().unwrap_or_default();
        let sender_mac: [u8; 6] = f[22..28].try_into().unwrap_or_default();
        let sender_ip: [u8; 4] = f[28..32].try_into().unwrap_or_default();
        // The gateway and DNS answer. Anything else on the subnet (the
        // guest's own gratuitous ARP, say) is left alone; off-subnet traffic
        // goes through the gateway so is never ARPed for.
        if target != GATEWAY_IP && target != DNS_IP {
            return;
        }
        let mut r = Vec::with_capacity(42);
        r.extend_from_slice(&sender_mac);
        r.extend_from_slice(&GATEWAY_MAC);
        r.extend_from_slice(&ETH_ARP.to_be_bytes());
        r.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 2]);
        r.extend_from_slice(&GATEWAY_MAC);
        r.extend_from_slice(&target);
        r.extend_from_slice(&sender_mac);
        r.extend_from_slice(&sender_ip);
        self.out.push_back(r);
    }

    fn dhcp(&mut self, ip: &Ipv4, udp: &Udp) {
        let d = udp.data;
        // BOOTP header is 236 bytes, then the magic cookie and options.
        if d.len() < 240 || d[0] != 1 || d[236..240] != [0x63, 0x82, 0x53, 0x63] {
            return;
        }
        let mut msg_type = 0;
        let mut i = 240;
        while i < d.len() && d[i] != 255 {
            if d[i] == 0 {
                i += 1;
                continue;
            }
            let (code, len) = (d[i], *d.get(i + 1).unwrap_or(&0) as usize);
            if code == 53 && len == 1 {
                msg_type = *d.get(i + 2).unwrap_or(&0);
            }
            i += 2 + len;
        }
        let reply_type = match msg_type {
            1 => 2, // DISCOVER -> OFFER
            3 => 5, // REQUEST -> ACK
            _ => return,
        };
        if reply_type == 5 {
            self.leased = Some(GUEST_IP);
        }
        let mut b = alloc::vec![0u8; 236];
        b[0] = 2;
        b[1] = 1;
        b[2] = 6;
        b[4..8].copy_from_slice(&d[4..8]); // xid
        b[10..12].copy_from_slice(&d[10..12]); // flags
        b[16..20].copy_from_slice(&GUEST_IP); // yiaddr
        b[20..24].copy_from_slice(&GATEWAY_IP); // siaddr
        b[28..44].copy_from_slice(&d[28..44]); // chaddr
        b.extend_from_slice(&[0x63, 0x82, 0x53, 0x63]);
        b.extend_from_slice(&[53, 1, reply_type]);
        b.extend_from_slice(&[54, 4]);
        b.extend_from_slice(&GATEWAY_IP);
        b.extend_from_slice(&[51, 4, 0, 1, 0x51, 0x80]); // lease: 86400 s
        b.extend_from_slice(&[58, 4, 0, 0, 0xA8, 0xC0]); // T1
        b.extend_from_slice(&[59, 4, 0, 1, 0x27, 0x50]); // T2
        b.extend_from_slice(&[1, 4]);
        b.extend_from_slice(&NETMASK);
        b.extend_from_slice(&[3, 4]);
        b.extend_from_slice(&GATEWAY_IP);
        b.extend_from_slice(&[6, 4]);
        b.extend_from_slice(&DNS_IP);
        b.push(255);
        while b.len() < 300 {
            b.push(0);
        }
        // Always broadcast: the guest has no address yet, and its stack
        // accepts broadcast replies.
        self.out.push_back(udp_frame(
            [0xFF; 6],
            GATEWAY_MAC,
            (GATEWAY_IP, 67),
            ([255; 4], 68),
            &b,
        ));
        let _ = ip;
    }
}

/// A deterministic network for tests: the [`Gateway`] services plus a UDP
/// echo on the gateway (port 7). No sockets, no clocks.
pub struct LoopbackBackend {
    gw: Gateway,
    up: bool,
}

impl LoopbackBackend {
    pub fn new() -> Self {
        Self {
            gw: Gateway::new(),
            up: false,
        }
    }

    pub fn boxed() -> Box<dyn NetBackend> {
        Box::new(Self::new())
    }
}

impl Default for LoopbackBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NetBackend for LoopbackBackend {
    fn available(&self) -> bool {
        true
    }

    fn link_changed(&mut self, up: bool) {
        self.up = up;
        if !up {
            self.gw.reset();
        }
    }

    fn send(&mut self, frame: &[u8]) {
        if !self.up {
            return;
        }
        if let Handled::No = self.gw.handle(frame)
            && let Some(ip) = parse_ipv4(frame)
            && ip.proto == 17
            && ip.dst == GATEWAY_IP
            && let Some(udp) = parse_udp(ip.payload)
            && udp.dst_port == 7
        {
            let reply = udp_frame(
                ip.src_mac,
                GATEWAY_MAC,
                (GATEWAY_IP, 7),
                (ip.src, udp.src_port),
                udp.data,
            );
            self.gw.out.push_back(reply);
        }
    }

    fn recv(&mut self) -> Option<Vec<u8>> {
        self.gw.out.pop_front()
    }

    fn lease(&self) -> Option<[u8; 4]> {
        self.gw.lease()
    }
}
