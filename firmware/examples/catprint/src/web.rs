//! Just enough internet: DNS, NTP, plain HTTP/1.0 GETs (no TLS, so only
//! services that still answer on port 80), and a JSON field picker.
use core::fmt::Write as _;

use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_net::udp::UdpSocket;
use embassy_net::wire::{IpAddress, IpEndpoint, Ipv4Address};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use embedded_io_async::Write;
use heapless::String;

pub async fn resolve(stack: Stack<'_>, name: &str) -> Option<Ipv4Address> {
    let r = with_timeout(Duration::from_secs(5), stack.dns_query(name, DnsQueryType::A)).await;
    match r {
        Ok(Ok(addrs)) => addrs.iter().find_map(|a| match a {
            #[allow(unreachable_patterns)]
            IpAddress::Ipv4(a) => Some(*a),
            #[allow(unreachable_patterns)]
            _ => None,
        }),
        _ => None,
    }
}

/// Unix time in seconds, and the round trip in µs.
pub async fn ntp(stack: Stack<'_>, server: &str) -> Option<(u64, u32)> {
    let ip = resolve(stack, server).await?;
    let mut sock = UdpSocket::new(stack).ok()?;
    sock.bind(40123).ok()?;
    let mut pkt = [0u8; 48];
    pkt[0] = 0x1B; // LI 0, version 3, client
    let t = Instant::now();
    sock.send_to(&pkt, IpEndpoint::new(ip.into(), 123)).await.ok()?;
    let reply = with_timeout(Duration::from_secs(4), sock.recv()).await.ok()?.ok()?;
    let us = t.elapsed().as_micros() as u32;
    let buf: &[u8] = &reply;
    if buf.len() < 48 {
        return None;
    }
    let secs = u32::from_be_bytes([buf[40], buf[41], buf[42], buf[43]]) as u64;
    secs.checked_sub(2_208_988_800).map(|unix| (unix, us))
}

/// How a request went, for the telemetry table.
#[derive(Clone)]
pub struct Probe {
    pub what: String<28>,
    pub status: u16,
    pub bytes: u32,
    pub ms: u32,
}

/// `GET http://host/path` into `buf`. Returns the body's range in `buf`
/// and the probe record; the body is cut short if `buf` fills up.
pub async fn get(stack: Stack<'_>, host: &str, path: &str, buf: &mut [u8]) -> (Option<(usize, usize)>, Probe) {
    let mut probe = Probe { what: String::new(), status: 0, bytes: 0, ms: 0 };
    let _ = write!(probe.what, "{host}{}", &path[..path.find('?').unwrap_or(path.len())]);
    probe.what.truncate(probe.what.len().min(28));
    let t = Instant::now();
    // One retry: a fresh connection now and then goes nowhere.
    let mut body = None;
    for _ in 0..2 {
        body = with_timeout(Duration::from_secs(12), fetch(stack, host, path, buf, &mut probe)).await.ok().flatten();
        if body.is_some() {
            break;
        }
        Timer::after_millis(300).await;
    }
    probe.ms = t.elapsed().as_millis() as u32;
    defmt::info!("GET {=str}{=str}: status {} {} bytes {} ms", host, path, probe.status, probe.bytes, probe.ms);
    (body, probe)
}

async fn fetch(stack: Stack<'_>, host: &str, path: &str, buf: &mut [u8], probe: &mut Probe) -> Option<(usize, usize)> {
    let ip = resolve(stack, host).await?;
    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 512];
    let mut sock = TcpSocket::new(stack, &mut rx, &mut tx).ok()?;
    sock.set_timeout(Some(Duration::from_secs(10)));
    sock.connect(IpEndpoint::new(ip.into(), 80)).await.ok()?;
    let mut req: String<384> = String::new();
    let _ = write!(req, "GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: curl/8.5.0 (catprint; pico-w)\r\nAccept: */*\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).await.ok()?;
    let mut n = 0;
    while n < buf.len() {
        match sock.read(&mut buf[n..]).await {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    // The server closed its side; close ours properly so the next
    // connection doesn't trip over this one.
    sock.close();
    let _ = with_timeout(Duration::from_millis(500), sock.flush()).await;
    sock.abort();
    probe.bytes = n as u32;
    let head = core::str::from_utf8(&buf[..n.min(12)]).ok()?;
    probe.status = head.get(9..12)?.parse().ok()?;
    let split = buf[..n].windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    (probe.status == 200).then_some((split, n))
}

/// The raw text of `"key": "value"` (escapes left as they are).
pub fn json_str<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let rest = after_key(s, key)?;
    let rest = rest.strip_prefix('"')?;
    let mut prev = ' ';
    for (i, c) in rest.char_indices() {
        if c == '"' && prev != '\\' {
            return Some(&rest[..i]);
        }
        prev = c;
    }
    None
}

/// The number in `"key": 12.5`.
pub fn json_num(s: &str, key: &str) -> Option<f32> {
    let rest = after_key(s, key)?;
    let end = rest.find(|c: char| !(c.is_ascii_digit() || "+-.eE".contains(c))).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn after_key<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let mut from = 0;
    loop {
        let at = from + s[from..].find(key)?;
        from = at + key.len();
        if at == 0 || s.as_bytes()[at - 1] != b'"' || s.as_bytes().get(from) != Some(&b'"') {
            continue;
        }
        let rest = s[from + 1..].trim_start();
        if let Some(r) = rest.strip_prefix(':') {
            return Some(r.trim_start());
        }
    }
}

/// Text in the printer's font: ASCII, with accents dropped, wttr.in's wind
/// arrows turned into where the wind comes from, and the rest as `?`.
pub fn ascii<const N: usize>(s: &str) -> String<N> {
    let mut out = String::new();
    let (mut escaped, mut skip) = (false, 0);
    for c in s.chars() {
        if skip > 0 {
            skip -= 1;
            continue;
        }
        if escaped {
            escaped = false;
            let _ = match c {
                'u' => {
                    skip = 4;
                    out.push('?')
                }
                'n' | 't' | 'r' => out.push(' '),
                _ => out.push(c),
            };
            continue;
        }
        let r: &str = match c {
            '\\' => {
                escaped = true;
                ""
            }
            ' '..='~' => {
                let _ = out.push(c);
                ""
            }
            'à'..='å' | 'ā' | 'ă' | 'ą' => "a",
            'À'..='Å' => "A",
            'ç' | 'ć' | 'č' => "c",
            'Ç' | 'Č' => "C",
            'è'..='ë' | 'ē' | 'ę' | 'ě' => "e",
            'È'..='Ë' => "E",
            'ì'..='ï' => "i",
            'ñ' | 'ń' => "n",
            'ò'..='ö' | 'ø' | 'ő' => "o",
            'Ö' | 'Ø' => "O",
            'ù'..='ü' | 'ů' | 'ű' => "u",
            'Ü' => "U",
            'ý' | 'ÿ' => "y",
            'ß' => "ss",
            'š' => "s",
            'Š' => "S",
            'ž' => "z",
            'Ž' => "Z",
            '°' => "",
            '↓' => "N ",
            '↙' => "NE ",
            '←' => "E ",
            '↖' => "SE ",
            '↑' => "S ",
            '↗' => "SW ",
            '→' => "W ",
            '↘' => "NW ",
            _ => "?",
        };
        let _ = out.push_str(r);
    }
    out
}
