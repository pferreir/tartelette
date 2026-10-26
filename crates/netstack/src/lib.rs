//! A user-mode network for the emulated Wi-Fi chip, in the manner of QEMU's
//! "slirp": the guest sees a tiny private network and its traffic comes out
//! of the host's ordinary sockets, with no privileges and no TAP device.
//!
//! | address     | what                                                           |
//! |-------------|----------------------------------------------------------------|
//! | `10.0.2.15` | the guest (leased by the built-in DHCP server)                 |
//! | `10.0.2.2`  | the gateway; connecting to it reaches the *host's* `127.0.0.1` |
//! | `10.0.2.3`  | DNS, answered with the host's resolver (A records only)        |
//!
//! * ARP and ICMP echo to the gateway and DNS addresses, and a DHCP server:
//!   the shared [`pcb_devices::net::Gateway`] and `smoltcp`'s interface.
//! * **UDP** is NATed one host socket per flow, idle flows expire.
//! * **TCP** is terminated by `smoltcp` (in `any_ip` mode, so it accepts SYNs
//!   to any address) and spliced onto a real `TcpStream`. The host connection is
//!   made first: if it fails, the guest gets an RST instead of a handshake.
//!
//! Time on the guest runs on emulated time, the host side on real time; they
//! don't have to agree. Not modelled: IPv6, ICMP to the outside, inbound
//! connections (see [`Slirp::forward`]).
use pcb_devices::net::{self, Gateway, GATEWAY_IP, GATEWAY_MAC, GUEST_IP, NetBackend};
use smoltcp::{
    iface::{Config, Interface, SocketHandle, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::tcp,
    time::Instant,
    wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpListenEndpoint, Ipv4Address},
};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

const TCP_BUFFER: usize = 32 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const UDP_IDLE: Duration = Duration::from_secs(60);
const MAX_UDP_FLOWS: usize = 64;
const MAX_TCP_CONNS: usize = 64;
const MAX_DNS_QUERIES: usize = 8;

/// `smoltcp`'s view of the wire: frames in, frames out.
#[derive(Default)]
struct Wire {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut VecDeque<Vec<u8>>);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
        r
    }
}

impl Device for Wire {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((Rx(frame), Tx(&mut self.tx)))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        Some(Tx(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps
    }
}

/// Where a connection from the guest stands.
enum Tcp {
    /// The host side is connecting; the guest's SYN waits.
    Connecting { syn: Vec<u8>, result: Receiver<io::Result<TcpStream>> },
    /// Spliced.
    Open { handle: SocketHandle, stream: TcpStream, shut_remote: bool, remote_eof: bool },
}

#[derive(Hash, PartialEq, Eq, Clone, Copy, Debug)]
struct Flow {
    src: ([u8; 4], u16),
    dst: ([u8; 4], u16),
}

struct UdpFlow {
    socket: UdpSocket,
    last_used: std::time::Instant,
}

/// Where the guest's traffic to `10.0.2.2` goes on the host.
fn host_side(ip: [u8; 4]) -> Ipv4Addr {
    if ip == GATEWAY_IP { Ipv4Addr::LOCALHOST } else { Ipv4Addr::from(ip) }
}

pub struct Slirp {
    start: std::time::Instant,
    gateway: Gateway,
    iface: Interface,
    wire: Wire,
    sockets: SocketSet<'static>,
    tcp: HashMap<Flow, Tcp>,
    udp: HashMap<Flow, UdpFlow>,
    guest_mac: Option<[u8; 6]>,
    /// Replies built here (DHCP, DNS, NATed UDP, RSTs), ahead of `smoltcp`'s.
    out: VecDeque<Vec<u8>>,
    dns_tx: Sender<(Flow, Vec<u8>)>,
    dns_rx: Receiver<(Flow, Vec<u8>)>,
    dns_pending: usize,
    up: bool,
    /// Host listeners for inbound connections: (listener, guest port).
    forwards: Vec<(TcpListener, u16)>,
    next_port: u16,
}

impl Default for Slirp {
    fn default() -> Self {
        Self::new()
    }
}

impl Slirp {
    pub fn new() -> Self {
        let mut wire = Wire::default();
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
        config.random_seed = 0x7461_7274_656c_6574;
        let mut iface = Interface::new(config, &mut wire, Instant::from_micros(0));
        iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::new(IpAddress::v4(10, 0, 2, 2), 24));
            let _ = a.push(IpCidr::new(IpAddress::v4(10, 0, 2, 3), 24));
        });
        iface.set_any_ip(true);
        let _ = iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2));
        let (dns_tx, dns_rx) = mpsc::channel();
        Self {
            start: std::time::Instant::now(),
            gateway: Gateway::new(),
            iface,
            wire,
            sockets: SocketSet::new(Vec::new()),
            tcp: HashMap::new(),
            udp: HashMap::new(),
            guest_mac: None,
            out: VecDeque::new(),
            dns_tx,
            dns_rx,
            dns_pending: 0,
            up: false,
            forwards: Vec::new(),
            next_port: 40000,
        }
    }

    pub fn boxed() -> Box<dyn NetBackend> {
        Box::new(Self::new())
    }

    /// Forward connections to `host_port` on the host's loopback to `guest_port`
    /// on the guest. Returns the address actually bound.
    pub fn forward(&mut self, host_port: u16, guest_port: u16) -> io::Result<SocketAddr> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, host_port))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        self.forwards.push((listener, guest_port));
        Ok(addr)
    }

    fn now(&self) -> Instant {
        Instant::from_micros(self.start.elapsed().as_micros() as i64)
    }

    // ---- from the guest ----

    fn handle_frame(&mut self, frame: &[u8]) {
        if frame.len() < 14 {
            return;
        }
        self.guest_mac = frame[6..12].try_into().ok();
        match net::be16(frame, 12) {
            net::ETH_IPV4 => {}
            // ARP is smoltcp's: it answers for the gateway and DNS addresses.
            net::ETH_ARP => return self.wire.rx.push_back(frame.to_vec()),
            _ => return,
        }
        let Some(ip) = net::parse_ipv4(frame) else { return };
        match ip.proto {
            17 => self.udp_from_guest(frame, &ip),
            6 => self.tcp_from_guest(frame, &ip),
            1 => self.wire.rx.push_back(frame.to_vec()), // echo to the gateway
            _ => {}
        }
    }

    fn udp_from_guest(&mut self, frame: &[u8], ip: &net::Ipv4) {
        let Some(udp) = net::parse_udp(ip.payload) else { return };
        if udp.dst_port == 67 {
            let _ = self.gateway.handle(frame);
            while let Some(f) = self.gateway.out.pop_front() {
                self.out.push_back(f);
            }
            return;
        }
        let flow = Flow { src: (ip.src, udp.src_port), dst: (ip.dst, udp.dst_port) };
        if ip.dst == net::DNS_IP && udp.dst_port == 53 || ip.dst == GATEWAY_IP && udp.dst_port == 53 {
            return self.dns_query(flow, udp.data);
        }
        // Broadcast and multicast don't leave the machine.
        if ip.dst[3] == 255 || ip.dst[0] >= 224 {
            return;
        }
        if !self.udp.contains_key(&flow) {
            if self.udp.len() >= MAX_UDP_FLOWS {
                self.expire_udp(true);
            }
            let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else { return };
            if socket.set_nonblocking(true).is_err() {
                return;
            }
            self.udp.insert(flow, UdpFlow { socket, last_used: std::time::Instant::now() });
        }
        if let Some(f) = self.udp.get_mut(&flow) {
            f.last_used = std::time::Instant::now();
            let _ = f.socket.send_to(udp.data, SocketAddrV4::new(host_side(ip.dst), udp.dst_port));
        }
    }

    fn expire_udp(&mut self, force_one: bool) {
        let now = std::time::Instant::now();
        self.udp.retain(|_, f| now.duration_since(f.last_used) < UDP_IDLE);
        if force_one
            && self.udp.len() >= MAX_UDP_FLOWS
            && let Some(oldest) = self.udp.iter().min_by_key(|(_, f)| f.last_used).map(|(k, _)| *k)
        {
            self.udp.remove(&oldest);
        }
    }

    fn dns_query(&mut self, flow: Flow, query: &[u8]) {
        if self.dns_pending >= MAX_DNS_QUERIES {
            return;
        }
        let Some(q) = dns::Query::parse(query) else { return };
        self.dns_pending += 1;
        let tx = self.dns_tx.clone();
        let query = query.to_vec();
        std::thread::spawn(move || {
            let answers: Option<Vec<Ipv4Addr>> = if q.qtype == dns::A {
                (q.name.as_str(), 0).to_socket_addrs().ok().map(|it| {
                    it.filter_map(|a| match a {
                        SocketAddr::V4(v4) => Some(*v4.ip()),
                        _ => None,
                    })
                    .collect()
                })
            } else {
                Some(Vec::new())
            };
            let _ = tx.send((flow, dns::response(&query, &q, answers.as_deref())));
        });
    }

    fn tcp_from_guest(&mut self, frame: &[u8], ip: &net::Ipv4) {
        let seg = ip.payload;
        if seg.len() < 20 {
            return;
        }
        let flow = Flow { src: (ip.src, net::be16(seg, 0)), dst: (ip.dst, net::be16(seg, 2)) };
        let flags = seg[13];
        let (syn, ack, rst) = (flags & 0x02 != 0, flags & 0x10 != 0, flags & 0x04 != 0);
        match self.tcp.get(&flow).map(|t| matches!(t, Tcp::Open { .. })) {
            Some(true) => self.wire.rx.push_back(frame.to_vec()),
            // Retransmitted SYN while the host side is still connecting: nothing to do.
            Some(false) => {
                if rst {
                    self.tcp.remove(&flow);
                }
            }
            None if syn && !ack => {
                if self.tcp.len() >= MAX_TCP_CONNS {
                    return self.reset(&flow, seg);
                }
                let addr = SocketAddr::V4(SocketAddrV4::new(host_side(ip.dst), flow.dst.1));
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT));
                });
                self.tcp.insert(flow, Tcp::Connecting { syn: frame.to_vec(), result: rx });
            }
            // A segment for a connection we don't have: tell the guest.
            None if !rst => self.reset(&flow, seg),
            None => {}
        }
    }

    /// RST|ACK back to the guest for a segment `seg` it sent on `flow`.
    fn reset(&mut self, flow: &Flow, seg: &[u8]) {
        let Some(mac) = self.guest_mac else { return };
        let flags = seg[13];
        let seq = u32::from_be_bytes([seg[4], seg[5], seg[6], seg[7]]);
        let ack = u32::from_be_bytes([seg[8], seg[9], seg[10], seg[11]]);
        let data_off = (seg[12] >> 4) as usize * 4;
        let payload = seg.len().saturating_sub(data_off) as u32;
        let syn_fin = (flags & 0x02 != 0) as u32 + (flags & 0x01 != 0) as u32;
        let (r_seq, r_ack, r_flags) =
            if flags & 0x10 != 0 { (ack, 0, 0x04u8) } else { (0, seq.wrapping_add(payload + syn_fin), 0x14u8) };
        let mut tcp = vec![0u8; 20];
        tcp[0..2].copy_from_slice(&flow.dst.1.to_be_bytes());
        tcp[2..4].copy_from_slice(&flow.src.1.to_be_bytes());
        tcp[4..8].copy_from_slice(&r_seq.to_be_bytes());
        tcp[8..12].copy_from_slice(&r_ack.to_be_bytes());
        tcp[12] = 5 << 4;
        tcp[13] = r_flags;
        let c = net::transport_checksum(flow.dst.0, flow.src.0, 6, &tcp);
        tcp[16..18].copy_from_slice(&c.to_be_bytes());
        self.out.push_back(net::ipv4_frame(mac, GATEWAY_MAC, flow.dst.0, flow.src.0, 6, &tcp));
    }

    // ---- the pump ----

    /// Move everything that can move: DNS answers, UDP replies, connection
    /// progress, bytes between the guest's sockets and the host's.
    fn pump(&mut self) {
        self.accept_inbound();
        self.finish_connects();
        let now = self.now();
        self.iface.poll(now, &mut self.wire, &mut self.sockets);
        self.splice();
        self.iface.poll(now, &mut self.wire, &mut self.sockets);
        self.udp_replies();
        self.dns_replies();
    }

    fn finish_connects(&mut self) {
        let mut done = Vec::new();
        for (flow, conn) in &self.tcp {
            if let Tcp::Connecting { result, .. } = conn {
                match result.try_recv() {
                    Ok(r) => done.push((*flow, Some(r))),
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => done.push((*flow, None)),
                }
            }
        }
        for (flow, result) in done {
            let Some(Tcp::Connecting { syn, .. }) = self.tcp.remove(&flow) else { continue };
            match result {
                Some(Ok(stream)) if stream.set_nonblocking(true).is_ok() => {
                    let _ = stream.set_nodelay(true);
                    let mut socket = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; TCP_BUFFER]), tcp::SocketBuffer::new(vec![0; TCP_BUFFER]));
                    socket.set_nagle_enabled(false);
                    let ep = IpListenEndpoint { addr: Some(IpAddress::v4(flow.dst.0[0], flow.dst.0[1], flow.dst.0[2], flow.dst.0[3])), port: flow.dst.1 };
                    if socket.listen(ep).is_ok() {
                        let handle = self.sockets.add(socket);
                        // Hand smoltcp the SYN the guest sent while we were connecting.
                        self.wire.rx.push_back(syn);
                        self.tcp.insert(flow, Tcp::Open { handle, stream, shut_remote: false, remote_eof: false });
                    }
                }
                _ => {
                    // Connection refused, timed out, ...: reset the guest's attempt.
                    if let Some(seg) = net::parse_ipv4(&syn).map(|ip| ip.payload.to_vec()) {
                        self.reset(&flow, &seg);
                    }
                }
            }
        }
    }

    fn splice(&mut self) {
        let mut closed = Vec::new();
        let mut buf = [0u8; 8192];
        for (flow, conn) in self.tcp.iter_mut() {
            let Tcp::Open { handle, stream, shut_remote, remote_eof } = conn else { continue };
            let socket = self.sockets.get_mut::<tcp::Socket>(*handle);
            // Guest -> host.
            let mut broken = false;
            while socket.can_recv() {
                let wrote = socket.recv(|data| match stream.write(data) {
                    Ok(n) => (n, n),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => (0, 0),
                    Err(_) => {
                        broken = true;
                        (0, 0)
                    }
                });
                if broken || !matches!(wrote, Ok(n) if n > 0) {
                    break;
                }
            }
            // The guest said FIN (and everything before it has been passed on).
            let guest_fin = matches!(socket.state(), tcp::State::CloseWait | tcp::State::LastAck | tcp::State::Closing | tcp::State::TimeWait);
            if guest_fin && socket.recv_queue() == 0 && !*shut_remote {
                let _ = stream.shutdown(std::net::Shutdown::Write);
                *shut_remote = true;
            }
            // Host -> guest.
            while !*remote_eof && socket.may_send() {
                let room = socket.send_capacity() - socket.send_queue();
                if room == 0 {
                    break;
                }
                let want = room.min(buf.len());
                match stream.read(&mut buf[..want]) {
                    Ok(0) => {
                        *remote_eof = true;
                        socket.close();
                    }
                    Ok(n) => {
                        let _ = socket.send_slice(&buf[..n]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        broken = true;
                        break;
                    }
                }
            }
            if broken {
                socket.abort();
            }
            if socket.state() == tcp::State::Closed || (socket.state() == tcp::State::TimeWait && *remote_eof) {
                closed.push(*flow);
            }
        }
        for flow in closed {
            if let Some(Tcp::Open { handle, .. }) = self.tcp.remove(&flow) {
                self.sockets.remove(handle);
            }
        }
    }

    fn udp_replies(&mut self) {
        let Some(mac) = self.guest_mac else { return };
        let mut buf = [0u8; 2048];
        let mut replies = Vec::new();
        for (flow, f) in &self.udp {
            while let Ok((n, _)) = f.socket.recv_from(&mut buf) {
                replies.push(net::udp_frame(mac, GATEWAY_MAC, flow.dst, flow.src, &buf[..n]));
            }
        }
        self.out.extend(replies);
        self.expire_udp(false);
    }

    fn dns_replies(&mut self) {
        let Some(mac) = self.guest_mac else { return };
        while let Ok((flow, response)) = self.dns_rx.try_recv() {
            self.dns_pending = self.dns_pending.saturating_sub(1);
            self.out.push_back(net::udp_frame(mac, GATEWAY_MAC, flow.dst, flow.src, &response));
        }
    }

    // ---- inbound ----

    fn accept_inbound(&mut self) {
        if self.forwards.is_empty() || self.guest_mac.is_none() {
            return;
        }
        let mut new = Vec::new();
        for (listener, guest_port) in &self.forwards {
            while let Ok((stream, _)) = listener.accept() {
                new.push((stream, *guest_port));
            }
        }
        for (stream, guest_port) in new {
            if stream.set_nonblocking(true).is_err() || self.tcp.len() >= MAX_TCP_CONNS {
                continue;
            }
            let mut socket = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; TCP_BUFFER]), tcp::SocketBuffer::new(vec![0; TCP_BUFFER]));
            socket.set_nagle_enabled(false);
            let local_port = self.next_port;
            self.next_port = if self.next_port == u16::MAX { 40000 } else { self.next_port + 1 };
            let remote = (IpAddress::Ipv4(Ipv4Address::from(GUEST_IP)), guest_port);
            if socket.connect(self.iface.context(), remote, (IpAddress::v4(10, 0, 2, 2), local_port)).is_err() {
                continue;
            }
            let handle = self.sockets.add(socket);
            // Keyed the way the guest's own segments will arrive. The connection is
            // spliced as soon as the guest's handshake completes.
            let flow = Flow { src: (GUEST_IP, guest_port), dst: (GATEWAY_IP, local_port) };
            self.tcp.insert(flow, Tcp::Open { handle, stream, shut_remote: false, remote_eof: false });
        }
    }
}

impl NetBackend for Slirp {
    fn available(&self) -> bool {
        true
    }

    fn link_changed(&mut self, up: bool) {
        self.up = up;
        if !up {
            // Drop everything in flight; the guest lost its association.
            let mut fresh = Slirp::new();
            fresh.forwards = std::mem::take(&mut self.forwards);
            *self = fresh;
        }
    }

    fn send(&mut self, frame: &[u8]) {
        if !self.up {
            return;
        }
        self.handle_frame(frame);
        self.pump();
    }

    fn recv(&mut self) -> Option<Vec<u8>> {
        if !self.up {
            return None;
        }
        if self.out.is_empty() && self.wire.tx.is_empty() {
            self.pump();
        }
        self.out.pop_front().or_else(|| self.wire.tx.pop_front())
    }

    fn lease(&self) -> Option<[u8; 4]> {
        self.gateway.lease()
    }
}

/// Just enough DNS for a stub resolver: one question, A records.
mod dns {
    use std::net::Ipv4Addr;

    pub const A: u16 = 1;

    pub struct Query {
        pub name: String,
        pub qtype: u16,
        /// End of the question section in the message.
        pub end: usize,
    }

    impl Query {
        pub fn parse(msg: &[u8]) -> Option<Query> {
            if msg.len() < 17 || msg[2] & 0x80 != 0 || u16::from_be_bytes([msg[4], msg[5]]) != 1 {
                return None;
            }
            let (mut i, mut name) = (12, String::new());
            loop {
                let len = *msg.get(i)? as usize;
                if len == 0 {
                    i += 1;
                    break;
                }
                if len & 0xC0 != 0 {
                    return None; // no compression in a question
                }
                if !name.is_empty() {
                    name.push('.');
                }
                name.push_str(std::str::from_utf8(msg.get(i + 1..i + 1 + len)?).ok()?);
                i += 1 + len;
            }
            let qtype = u16::from_be_bytes([*msg.get(i)?, *msg.get(i + 1)?]);
            Some(Query { name, qtype, end: i + 4 })
        }
    }

    /// The reply to `query`: `answers` of `None` means the name doesn't exist.
    pub fn response(query: &[u8], q: &Query, answers: Option<&[Ipv4Addr]>) -> Vec<u8> {
        let mut r = Vec::with_capacity(q.end + 16 * answers.map_or(0, |a| a.len()));
        r.extend_from_slice(&query[0..2]); // id
        let rcode = if answers.is_none() { 3 } else { 0 };
        r.push(0x80 | (query[2] & 0x01)); // QR, RD copied
        r.push(0x80 | rcode); // RA
        r.extend_from_slice(&1u16.to_be_bytes());
        let n = answers.map_or(0, |a| a.len()) as u16;
        r.extend_from_slice(&n.to_be_bytes());
        r.extend_from_slice(&[0, 0, 0, 0]);
        r.extend_from_slice(&query[12..q.end]);
        for ip in answers.unwrap_or(&[]) {
            r.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
            r.extend_from_slice(&ip.octets());
        }
        r
    }
}

#[cfg(test)]
mod tests;
