use std::net::{TcpListener, UdpSocket};

use smoltcp::{socket::tcp, wire::IpEndpoint};

use super::*;

const GUEST_MAC: [u8; 6] = [0x28, 0xCD, 0xC1, 0, 0, 1];

fn up() -> Slirp {
    let mut s = Slirp::new();
    s.link_changed(true);
    s
}

/// Run the pump until `f` yields something or two seconds pass.
#[track_caller]
fn wait_for<T>(s: &mut Slirp, mut f: impl FnMut(&mut Slirp) -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(v) = f(s) {
            return v;
        }
        assert!(std::time::Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn arp_for_the_gateway_and_dns_is_answered() {
    let mut s = up();
    for target in [GATEWAY_IP, net::DNS_IP] {
        let mut arp = vec![0xFF; 6];
        arp.extend_from_slice(&GUEST_MAC);
        arp.extend_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
        arp.extend_from_slice(&GUEST_MAC);
        arp.extend_from_slice(&GUEST_IP);
        arp.extend_from_slice(&[0; 6]);
        arp.extend_from_slice(&target);
        s.send(&arp);
        let reply = wait_for(&mut s, |s| s.recv());
        assert_eq!(&reply[12..14], &[8, 6]);
        assert_eq!(&reply[20..22], &[0, 2], "ARP reply");
        assert_eq!(&reply[22..28], &GATEWAY_MAC);
        assert_eq!(&reply[28..32], &target);
    }
}

#[test]
fn dhcp_hands_out_the_guest_address() {
    let mut s = up();
    let mut d = vec![0u8; 236];
    d[0] = 1;
    d[1] = 1;
    d[2] = 6;
    d[4..8].copy_from_slice(&[9, 9, 9, 9]);
    d[28..34].copy_from_slice(&GUEST_MAC);
    d.extend_from_slice(&[0x63, 0x82, 0x53, 0x63, 53, 1, 3, 255]); // REQUEST
    s.send(&net::udp_frame(
        [0xFF; 6],
        GUEST_MAC,
        ([0; 4], 68),
        ([255; 4], 67),
        &d,
    ));
    let ack = wait_for(&mut s, |s| s.recv());
    let ip = net::parse_ipv4(&ack).unwrap();
    let udp = net::parse_udp(ip.payload).unwrap();
    assert_eq!(&udp.data[16..20], &GUEST_IP);
    assert_eq!(&udp.data[240..243], &[53, 1, 5]);
    assert_eq!(s.lease(), Some(GUEST_IP));
}

#[test]
fn dns_resolves_names_with_the_host_resolver() {
    let mut s = up();
    s.send(&net::udp_frame(
        [0; 6],
        GUEST_MAC,
        (GUEST_IP, 4000),
        (net::DNS_IP, 53),
        &dns_query("localhost", 1),
    ));
    let reply = wait_for(&mut s, |s| s.recv());
    let udp = net::parse_udp(net::parse_ipv4(&reply).unwrap().payload).unwrap();
    assert_eq!(&udp.data[0..2], &[0x12, 0x34]);
    assert_eq!(udp.data[3] & 0xF, 0, "rcode");
    assert_eq!(udp.data[7], 1, "one answer");
    assert_eq!(&udp.data[udp.data.len() - 4..], &[127, 0, 0, 1]);

    // AAAA: no data, but not an error.
    s.send(&net::udp_frame(
        [0; 6],
        GUEST_MAC,
        (GUEST_IP, 4000),
        (net::DNS_IP, 53),
        &dns_query("localhost", 28),
    ));
    let reply = wait_for(&mut s, |s| s.recv());
    let udp = net::parse_udp(net::parse_ipv4(&reply).unwrap().payload).unwrap();
    assert_eq!((udp.data[3] & 0xF, udp.data[7]), (0, 0));
}

fn dns_query(name: &str, qtype: u16) -> Vec<u8> {
    let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&[0, 1]);
    q
}

#[test]
fn udp_is_natted_to_the_host_and_back() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut buf = [0; 100];
        let (n, from) = server.recv_from(&mut buf).unwrap();
        server.send_to(&buf[..n], from).unwrap();
    });
    let mut s = up();
    // 10.0.2.2 is the host's loopback.
    s.send(&net::udp_frame(
        GATEWAY_MAC,
        GUEST_MAC,
        (GUEST_IP, 5000),
        (GATEWAY_IP, port),
        b"ping",
    ));
    let reply = wait_for(&mut s, |s| s.recv());
    let ip = net::parse_ipv4(&reply).unwrap();
    assert_eq!((ip.src, ip.dst), (GATEWAY_IP, GUEST_IP));
    let udp = net::parse_udp(ip.payload).unwrap();
    assert_eq!(
        (udp.src_port, udp.dst_port, udp.data),
        (port, 5000, &b"ping"[..])
    );
}

/// A second smoltcp stack standing in for the firmware's.
struct Guest {
    iface: Interface,
    wire: Wire,
    sockets: SocketSet<'static>,
    start: std::time::Instant,
}

impl Guest {
    fn new() -> Self {
        let mut wire = Wire::default();
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GUEST_MAC)));
        config.random_seed = 42;
        let mut iface = Interface::new(config, &mut wire, Instant::from_micros(0));
        iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24));
        });
        let _ = iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2));
        Guest {
            iface,
            wire,
            sockets: SocketSet::new(Vec::new()),
            start: std::time::Instant::now(),
        }
    }

    fn connect(&mut self, addr: ([u8; 4], u16)) -> SocketHandle {
        let socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; 4096]),
            tcp::SocketBuffer::new(vec![0; 4096]),
        );
        let handle = self.sockets.add(socket);
        let remote = IpEndpoint::new(
            IpAddress::v4(addr.0[0], addr.0[1], addr.0[2], addr.0[3]),
            addr.1,
        );
        let cx = self.iface.context();
        self.sockets
            .get_mut::<tcp::Socket>(handle)
            .connect(cx, remote, 49152)
            .unwrap();
        handle
    }

    fn step(&mut self, net: &mut Slirp) {
        let now = Instant::from_micros(self.start.elapsed().as_micros() as i64);
        self.iface.poll(now, &mut self.wire, &mut self.sockets);
        while let Some(f) = self.wire.tx.pop_front() {
            net.send(&f);
        }
        while let Some(f) = net.recv() {
            self.wire.rx.push_back(f);
        }
        self.iface.poll(now, &mut self.wire, &mut self.sockets);
    }

    fn socket(&mut self, h: SocketHandle) -> &mut tcp::Socket<'static> {
        self.sockets.get_mut::<tcp::Socket>(h)
    }
}

#[track_caller]
fn run_guest<T>(g: &mut Guest, s: &mut Slirp, mut f: impl FnMut(&mut Guest) -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        g.step(s);
        if let Some(v) = f(g) {
            return v;
        }
        assert!(std::time::Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn tcp_connects_through_to_a_host_server_and_back() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    let echo = std::thread::spawn(move || {
        let (mut c, _) = server.accept().unwrap();
        let mut buf = [0; 1024];
        let mut total = 0;
        loop {
            let n = c.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            c.write_all(&buf[..n]).unwrap();
            total += n;
        }
        total
    });
    let mut s = up();
    let mut g = Guest::new();
    let h = g.connect((GATEWAY_IP, port));
    run_guest(&mut g, &mut s, |g| {
        (g.socket(h).state() == tcp::State::Established).then_some(())
    });

    // More than the socket buffers hold, so flow control has to work.
    let payload: Vec<u8> = (0..20_000u32).map(|i| (i * 31) as u8).collect();
    let (mut sent, mut got) = (0, Vec::new());
    run_guest(&mut g, &mut s, |g| {
        let sock = g.socket(h);
        if sent < payload.len() && sock.can_send() {
            sent += sock.send_slice(&payload[sent..]).unwrap();
        }
        if sock.can_recv() {
            sock.recv(|d| {
                got.extend_from_slice(d);
                (d.len(), ())
            })
            .unwrap();
        }
        (got.len() == payload.len()).then_some(())
    });
    assert_eq!(got, payload);

    // Closing from the guest closes the host side.
    g.socket(h).close();
    run_guest(&mut g, &mut s, |_| echo.is_finished().then_some(()));
    assert_eq!(echo.join().unwrap(), payload.len());
}

#[test]
fn a_refused_connection_resets_the_guest() {
    // A port nothing listens on.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut s = up();
    let mut g = Guest::new();
    let h = g.connect((GATEWAY_IP, port));
    run_guest(&mut g, &mut s, |g| {
        (g.socket(h).state() == tcp::State::Closed).then_some(())
    });
    assert!(s.tcp.is_empty());
}

#[test]
fn host_closing_first_closes_the_guest() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut c, _) = server.accept().unwrap();
        c.write_all(b"bye").unwrap();
    });
    let mut s = up();
    let mut g = Guest::new();
    let h = g.connect((GATEWAY_IP, port));
    let mut got = Vec::new();
    run_guest(&mut g, &mut s, |g| {
        let sock = g.socket(h);
        if sock.can_recv() {
            sock.recv(|d| {
                got.extend_from_slice(d);
                (d.len(), ())
            })
            .unwrap();
        }
        (got.len() == 3 && !sock.may_recv() && !sock.can_recv()).then_some(())
    });
    assert_eq!(got, b"bye");
}

#[test]
fn inbound_connections_can_be_forwarded_to_the_guest() {
    let mut s = up();
    let addr = s.forward(0, 8080).unwrap();
    // The guest runs a one-shot server on 8080.
    let mut g = Guest::new();
    let socket = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; 4096]),
        tcp::SocketBuffer::new(vec![0; 4096]),
    );
    let h = g.sockets.add(socket);
    g.socket(h).listen(8080).unwrap();
    // The gateway must be reachable for ARP; the guest speaks first so its MAC is learned.
    let mut client = TcpStream::connect(addr).unwrap();
    client.write_all(b"knock").unwrap();
    client.set_nonblocking(true).unwrap();
    // Let the gateway learn the guest's MAC by letting it ARP.
    let mut arp = vec![0xFF; 6];
    arp.extend_from_slice(&GUEST_MAC);
    arp.extend_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
    arp.extend_from_slice(&GUEST_MAC);
    arp.extend_from_slice(&GUEST_IP);
    arp.extend_from_slice(&[0; 6]);
    arp.extend_from_slice(&GATEWAY_IP);
    s.send(&arp);
    let mut got = Vec::new();
    let mut reply = false;
    run_guest(&mut g, &mut s, |g| {
        let sock = g.socket(h);
        if sock.can_recv() {
            sock.recv(|d| {
                got.extend_from_slice(d);
                (d.len(), ())
            })
            .unwrap();
        }
        if !got.is_empty() && !reply && sock.can_send() {
            sock.send_slice(b"who's there").unwrap();
            reply = true;
        }
        let mut buf = [0; 64];
        match client.read(&mut buf) {
            Ok(n) if n > 0 => {
                assert_eq!(&buf[..n], b"who's there");
                Some(())
            }
            _ => None,
        }
    });
    assert_eq!(got, b"knock");
}
