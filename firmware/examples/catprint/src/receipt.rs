//! THE DAILY BIT: what was gathered, laid out on 48 mm of thermal paper.
use core::fmt::Write as _;

use embassy_time::Instant;
use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_6X13_BOLD, FONT_8X13_BOLD, FONT_9X15_BOLD, FONT_9X18_BOLD};
use heapless::{String, Vec};

use crate::astro;
use crate::paper::{Paper, ROW_BYTES, W};
use crate::web::Probe;

// Bold faces throughout: the head can't heat a lone dot enough, so strokes
// one dot wide print faint.
const BODY: &embedded_graphics::mono_font::MonoFont = &FONT_8X13_BOLD;
const SMALL: &embedded_graphics::mono_font::MonoFont = &FONT_6X13_BOLD;
const HEAD: &embedded_graphics::mono_font::MonoFont = &FONT_9X15_BOLD;
/// Characters per body line.
const COLS: usize = 48;
/// Blank paper after the cut line, so it clears the tear bar.
const TAIL: i32 = 56;

pub struct Geo {
    pub ip: String<40>,
    pub city: String<32>,
    pub region: String<32>,
    pub cc: String<4>,
    pub isp: String<48>,
    pub asn: String<48>,
    pub tz: String<32>,
    pub lat: f32,
    pub lon: f32,
    /// Seconds east of UTC, daylight saving included.
    pub offset: i32,
}

pub struct Weather {
    /// wttr.in's fields: temperature, feels like, condition, humidity,
    /// wind, pressure, precipitation, UV index, sunrise, sunset.
    pub fields: Vec<String<32>, 10>,
}

impl Weather {
    pub fn get(&self, i: usize) -> &str {
        self.fields.get(i).map_or("?", |s| s.as_str())
    }
}

pub struct Crew {
    pub number: u8,
    pub people: Vec<(String<28>, String<12>), 14>,
}

pub struct Report {
    pub edition: u32,
    /// Unix time, and when it was taken.
    pub clock: Option<(u64, Instant)>,
    pub ntp_rtt_us: u32,
    pub ssid: &'static str,
    pub lan_ip: String<16>,
    pub gateway: String<16>,
    pub mac: [u8; 6],
    pub geo: Option<Geo>,
    pub wx: Option<Weather>,
    pub iss: Option<(f32, f32)>,
    pub crew: Option<Crew>,
    pub probes: Vec<Probe, 6>,
}

impl Report {
    pub const fn new(ssid: &'static str) -> Self {
        Self {
            edition: 0,
            clock: None,
            ntp_rtt_us: 0,
            ssid,
            lan_ip: String::new(),
            gateway: String::new(),
            mac: [0; 6],
            geo: None,
            wx: None,
            iss: None,
            crew: None,
            probes: Vec::new(),
        }
    }

    pub fn unix(&self) -> Option<i64> {
        self.clock.map(|(t, at)| (t + at.elapsed().as_secs()) as i64)
    }
}

fn s<const N: usize>(args: core::fmt::Arguments) -> String<N> {
    let mut out = String::new();
    let _ = out.write_fmt(args);
    out
}

/// Cut `v` so `key ... v` fits a body line.
fn fit<'a>(key: &str, v: &'a str) -> &'a str {
    let room = COLS.saturating_sub(key.len() + 2);
    if v.len() <= room {
        v
    } else {
        let mut end = room;
        while !v.is_char_boundary(end) {
            end -= 1;
        }
        &v[..end]
    }
}

fn kv(p: &mut Paper, key: &str, v: &str) {
    p.kv(key, fit(key, v), BODY);
}

/// 38.72N 9.14W
fn latlon(lat: f32, lon: f32) -> String<24> {
    s(format_args!(
        "{:.2}{} {:.2}{}",
        libm::fabsf(lat),
        if lat >= 0.0 { 'N' } else { 'S' },
        libm::fabsf(lon),
        if lon >= 0.0 { 'E' } else { 'W' }
    ))
}

fn hhmm(secs: i64) -> String<8> {
    let d = secs.rem_euclid(86_400);
    s(format_args!("{:02}:{:02}", d / 3600, d / 60 % 60))
}

fn utc_offset(off: i32) -> String<12> {
    let a = off.unsigned_abs();
    s(format_args!("UTC{}{:02}:{:02}", if off < 0 { '-' } else { '+' }, a / 3600, a / 60 % 60))
}

/// "07:42:10" to minutes after midnight.
fn minutes(t: &str) -> Option<i32> {
    let mut it = t.trim().split(':');
    let h: i32 = it.next()?.parse().ok()?;
    let m: i32 = it.next()?.parse().ok()?;
    Some(h * 60 + m)
}

pub fn render(p: &mut Paper, r: &Report) {
    p.clear();
    let unix = r.unix();
    let offset = r.geo.as_ref().map_or(0, |g| g.offset);
    masthead(p, r, unix, offset);
    whoami(p, r);
    weather(p, r, unix, offset);
    if let Some(t) = unix {
        moon(p, t);
    }
    orbit(p, r, unix);
    crew(p, r);
    telemetry(p, r);
    footer(p, unix);
}

fn masthead(p: &mut Paper, r: &Report, unix: Option<i64>, offset: i32) {
    // Black on white in a frame: solid black areas print blotchy (see
    // `Paper::section`).
    p.hline(0, W - 1, 0, 0);
    p.hline(0, W - 1, 1, 0);
    p.hline(0, W - 1, 4, 0);
    p.text_center(9, "THE DAILY BIT", &FONT_9X18_BOLD, 2, true);
    p.hline(0, W - 1, 46, 0);
    p.hline(0, W - 1, 49, 0);
    p.hline(0, W - 1, 50, 0);
    p.y = 56;
    p.text_center(p.y, "ALL THE BITS THAT FIT TO PRINT", SMALL, 1, true);
    p.y += 16;
    p.hline(0, W - 1, p.y, 0);
    p.hline(0, W - 1, p.y + 2, 0);
    p.y += 6;
    let line: String<64> = match unix {
        Some(t) => {
            let local = t + offset as i64;
            let (y, m, d) = astro::civil(local.div_euclid(86_400));
            s(format_args!(
                "NO.{}  {y:04}-{m:02}-{d:02} {}  {} {}",
                t / 86_400,
                astro::weekday(local),
                hhmm(local),
                utc_offset(offset)
            ))
        }
        None => s(format_args!("NO.???  EDITION {}  (CLOCK NOT SET)", r.edition)),
    };
    p.text_center(p.y, &line, BODY, 1, true);
    p.y += 16;
    p.hline(0, W - 1, p.y, 0);
    p.hline(0, W - 1, p.y + 2, 0);
    p.y += 10;
}

fn whoami(p: &mut Paper, r: &Report) {
    p.section("$ WHOAMI", "ip-api.com", HEAD);
    match &r.geo {
        Some(g) => {
            kv(p, "public ip", &g.ip);
            kv(p, "isp", &g.isp);
            kv(p, "as", &g.asn);
            let place: String<72> = s(format_args!("{}, {} {}", g.city, g.region, g.cc));
            kv(p, "place", &place);
            kv(p, "lat/lon", &latlon(g.lat, g.lon));
            let tz: String<48> = s(format_args!("{} {}", g.tz, utc_offset(g.offset)));
            kv(p, "tz", &tz);
        }
        None => p.line_text("geoip: NO CARRIER", BODY),
    }
    kv(p, "ssid", r.ssid);
    let lan: String<40> = s(format_args!("{} via {}", r.lan_ip, r.gateway));
    kv(p, "lan", &lan);
    let m = r.mac;
    let mac: String<20> = s(format_args!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]));
    kv(p, "mac", &mac);
    p.gap(8);
}

fn weather(p: &mut Paper, r: &Report, unix: Option<i64>, offset: i32) {
    let place = r.geo.as_ref().map_or("here", |g| g.city.as_str());
    let note: String<40> = s(format_args!("wttr.in/{}", &place[..place.len().min(20)]));
    p.section("$ WEATHER", &note, HEAD);
    let Some(w) = &r.wx else {
        p.line_text("wttr.in: NO CARRIER", BODY);
        p.gap(8);
        return;
    };
    // The temperature, big, and the sky next to it.
    let top = p.y;
    let temp = w.get(0);
    let tw = p.text(0, top, temp, &FONT_10X20, 3, true);
    let x = tw + 12;
    p.y = top + 4;
    let mut wrap: String<32> = String::new();
    for word in w.get(2).split(' ') {
        if !wrap.is_empty() && (wrap.len() + 1 + word.len()) as i32 * 9 > W - x {
            p.text(x, p.y, &wrap, &FONT_8X13_BOLD, 1, true);
            p.y += 15;
            wrap.clear();
        }
        if !wrap.is_empty() {
            let _ = wrap.push(' ');
        }
        let _ = wrap.push_str(word);
    }
    p.text(x, p.y, &wrap, &FONT_8X13_BOLD, 1, true);
    p.y += 15;
    let feels: String<32> = s(format_args!("feels like {}", w.get(1)));
    p.text(x, p.y, &feels, BODY, 1, true);
    p.y = (top + 64).max(p.y + 15);

    kv(p, "humidity", w.get(3));
    kv(p, "wind (from)", w.get(4));
    kv(p, "pressure", w.get(5));
    kv(p, "precipitation", w.get(6));
    kv(p, "uv index", w.get(7));
    p.gap(4);

    // 24 hours, 16 dots each: the daylight shaded, now marked.
    let (rise, set) = (minutes(w.get(8)), minutes(w.get(9)));
    let label: String<48> = s(format_args!("daylight {} - {}", hhmm(rise.unwrap_or(0) as i64 * 60), hhmm(set.unwrap_or(0) as i64 * 60)));
    p.line_text(&label, SMALL);
    p.gap(4);
    let (y0, h) = (p.y, 18);
    if let (Some(a), Some(b)) = (rise, set) {
        p.shade(a * 16 / 60, y0, (b - a) * 16 / 60, h, 5);
    }
    p.hline(0, W - 1, y0, 0);
    p.hline(0, W - 1, y0 + h, 0);
    for hour in 0..=24 {
        let x = (hour * 16).min(W - 1);
        let tick = if hour % 6 == 0 { 6 } else { 3 };
        p.vline(x, y0 + h - tick, y0 + h, 0);
        if hour % 6 == 0 {
            let l: String<4> = s(format_args!("{:02}", hour));
            let lx = (x - 6).clamp(0, W - 12);
            p.text(lx, y0 + h + 3, &l, SMALL, 1, true);
        }
    }
    if let Some(t) = unix {
        let now = (t + offset as i64).rem_euclid(86_400) as i32 / 60;
        let x = now * 16 / 60;
        // Within the bar, clear of the label above and the hours below.
        p.fill(x - 1, y0 - 2, 3, h + 4, true);
        let lx = if x > W - 24 { x - 22 } else { x + 4 };
        p.fill(lx - 1, y0 + 2, 20, 15, false);
        p.text(lx, y0 + 3, "NOW", SMALL, 1, true);
    }
    p.y = y0 + h + 18;
    p.gap(6);
}

fn moon(p: &mut Paper, t: i64) {
    p.section("$ MOON", "computed, no net", HEAD);
    let age = astro::moon_age(t);
    let theta = astro::moon_angle(age);
    let (r, cx, cy) = (30, W - 40, p.y + 32);
    // Lit part white, the rest densely dithered.
    for dy in -r..=r {
        let w = libm::sqrtf((r * r - dy * dy) as f32);
        let c = libm::cosf(theta);
        let (l0, l1) = if theta < core::f32::consts::PI { (w * c, w) } else { (-w, -w * c) };
        for dx in -(w as i32)..=(w as i32) {
            let x = dx as f32;
            if !(x >= l0 && x <= l1) {
                p.shade(cx + dx, cy + dy, 1, 1, 12);
            }
        }
    }
    p.circle(cx, cy, r);
    let half = 29.530_588 / 2.0;
    let to_full = if age < half { half - age } else { 2.0 * half - age + half };
    p.line_text(astro::moon_name(age), &FONT_8X13_BOLD);
    p.line_text(&s::<32>(format_args!("age  {:.1} days", age)), BODY);
    p.line_text(&s::<32>(format_args!("lit  {:.0}%", astro::moon_lit(age) * 100.0)), BODY);
    p.line_text(&s::<32>(format_args!("full in {:.1} d", to_full)), BODY);
    p.line_text(&s::<32>(format_args!("new  in {:.1} d", 2.0 * half - age)), BODY);
    p.y = p.y.max(cy + r + 4);
    p.gap(8);
}

fn orbit(p: &mut Paper, r: &Report, unix: Option<i64>) {
    p.section("$ ORBIT", "open-notify.org", HEAD);
    // Plate carree, 1 dot per degree; night dithered.
    let (x0, y0) = (12, p.y);
    let sun = unix.map(astro::subsolar);
    if let Some(sun) = sun {
        // The sun is up where sin(lat)sin(d) + cos(lat)cos(d)cos(lon - ls) > 0;
        // the cosine per column once, the rest per row.
        let rad = core::f32::consts::PI / 180.0;
        let mut cols = [0f32; 360];
        for (i, c) in cols.iter_mut().enumerate() {
            *c = libm::cosf((i as f32 - 179.5 - sun.1) * rad);
        }
        let (sd, cd) = (libm::sinf(sun.0 * rad), libm::cosf(sun.0 * rad));
        for lat in -90..90 {
            let la = (lat as f32 + 0.5) * rad;
            let (a, b) = (libm::sinf(la) * sd, libm::cosf(la) * cd);
            for (i, c) in cols.iter().enumerate() {
                if a + b * c <= 0.0 {
                    p.shade(x0 + i as i32, y0 + 90 - lat - 1, 1, 1, 4);
                }
            }
        }
    }
    for lon in (-180..=180).step_by(30) {
        p.vline(x0 + lon + 180, y0, y0 + 180, if lon == 0 { 3 } else { 1 });
    }
    for lat in (-90..=90).step_by(30) {
        p.hline(x0, x0 + 360, y0 + 90 - lat, if lat == 0 { 3 } else { 1 });
    }
    p.vline(x0, y0, y0 + 180, 0);
    p.vline(x0 + 360, y0, y0 + 180, 0);
    p.hline(x0, x0 + 360, y0, 0);
    p.hline(x0, x0 + 360, y0 + 180, 0);
    let at = |lat: f32, lon: f32| (x0 + 180 + lon as i32, y0 + 90 - lat as i32);
    // A label beside a marker: to the right unless asked (or out of room).
    let tag = |p: &mut Paper, x: i32, y: i32, t: &str, left: Option<bool>| {
        let w = t.len() as i32 * 6;
        let left = left.unwrap_or(x > x0 + 300);
        let tx = if left { (x - 8 - w).max(x0 + 1) } else { (x + 8).min(x0 + 359 - w) };
        p.fill(tx - 1, y - 7, w + 2, 15, false);
        p.text(tx, y - 6, t, SMALL, 1, true);
    };
    if let Some((lat, lon)) = sun {
        let (x, y) = at(lat, lon);
        p.disc(x, y, 3, true);
        for k in 0..8 {
            let a = k as f32 * core::f32::consts::PI / 4.0;
            let (c, s) = (libm::cosf(a), libm::sinf(a));
            p.line(x + (c * 5.0) as i32, y + (s * 5.0) as i32, x + (c * 7.0) as i32, y + (s * 7.0) as i32);
        }
        tag(p, x, y, "SUN", None);
    }
    let you = r.geo.as_ref().map(|g| at(g.lat, g.lon));
    let iss = r.iss.map(|(lat, lon)| at(lat, lon));
    // The station passing overhead: labels on opposite sides, so neither
    // covers the other.
    let iss_left = match (you, iss) {
        (Some(u), Some(i)) if (u.0 - i.0).abs() < 48 && (u.1 - i.1).abs() < 16 => Some(i.0 < u.0),
        _ => None,
    };
    // Right over you: you become a ring around it (below), labels outside.
    let over = matches!((you, iss), (Some(u), Some(i)) if (i.0 - u.0).abs() < 9 && (i.1 - u.1).abs() < 7);
    let past_ring = |x: i32, left: Option<bool>| if over { x + if left == Some(true) { -6 } else { 6 } } else { x };
    if let Some((x, y)) = iss {
        p.fill(x - 7, y - 4, 15, 9, false);
        p.fill(x - 1, y - 1, 3, 3, true);
        p.hline(x - 6, x + 6, y, 0);
        p.fill(x - 6, y - 3, 3, 7, true);
        p.fill(x + 4, y - 3, 3, 7, true);
        tag(p, past_ring(x, iss_left), y, "ISS", iss_left);
    }
    // You last, so you're never hidden.
    if let Some((x, y)) = you {
        if over {
            p.circle(x, y, 10);
            p.circle(x, y, 11);
        } else {
            p.disc(x, y, 5, false);
            p.circle(x, y, 5);
            p.disc(x, y, 1, true);
        }
        let side = iss_left.map(|l| !l);
        tag(p, past_ring(x, side), y, "YOU", side);
    }
    p.y = y0 + 184;
    p.text(x0 - 6, p.y, "180W", SMALL, 1, true);
    p.text_center(p.y, "0", SMALL, 1, true);
    p.text_right(x0 + 366, p.y, "180E", SMALL, 1, true);
    p.y += 16;

    match r.iss {
        Some((lat, lon)) => {
            kv(p, "iss over", &latlon(lat, lon));
            if let Some(g) = &r.geo {
                let ground = astro::distance_km((g.lat, g.lon), (lat, lon));
                // Straight line to a station 420 km up.
                let (re, ro) = (6371.0f32, 6371.0f32 + 420.0);
                let ang = ground / re;
                let slant = libm::sqrtf(re * re + ro * ro - 2.0 * re * ro * libm::cosf(ang));
                kv(p, "ground distance", &s::<16>(format_args!("{:.0} km", ground)));
                kv(p, "slant range", &s::<16>(format_args!("{:.0} km", slant)));
                kv(p, "light time", &s::<16>(format_args!("{:.2} ms", slant / 299.792)));
            }
        }
        None => p.line_text("iss: NO CARRIER", BODY),
    }
    if let Some((lat, lon)) = sun {
        kv(p, "sun overhead", &latlon(lat, lon));
    }
    p.gap(8);
}

fn crew(p: &mut Paper, r: &Report) {
    let Some(c) = &r.crew else {
        p.section("$ HUMANS IN ORBIT", "?", HEAD);
        p.line_text("astros: NO CARRIER", BODY);
        p.gap(8);
        return;
    };
    let note: String<16> = s(format_args!("{} up there", c.number));
    p.section("$ HUMANS IN ORBIT", &note, HEAD);
    for (name, craft) in &c.people {
        kv(p, name, craft);
    }
    if c.number as usize > c.people.len() {
        p.line_text(&s::<32>(format_args!("...and {} more", c.number as usize - c.people.len())), BODY);
    }
    p.gap(8);
}

fn telemetry(p: &mut Paper, r: &Report) {
    let up: String<24> = s(format_args!("uptime {}s", Instant::now().as_secs()));
    p.section("$ TELEMETRY", &up, HEAD);
    p.line_text("ST   BYTES     TIME  WHAT", SMALL);
    for pr in &r.probes {
        let st: String<4> = if pr.status == 0 { s(format_args!("---")) } else { s(format_args!("{}", pr.status)) };
        let line: String<64> = s(format_args!("{st:<3} {:>6}B {:>6}ms  {}", pr.bytes, pr.ms, pr.what));
        p.line_text(&line, SMALL);
    }
    if r.clock.is_some() {
        let us = r.ntp_rtt_us;
        let line: String<64> = s(format_args!("UDP {:>6}B {:>3}.{}ms  pool.ntp.org:123", 48, us / 1000, us / 100 % 10));
        p.line_text(&line, SMALL);
    }
    p.gap(10);
}

/// Interleaved 2 of 5: bars from one digit, spaces from the next. `true`
/// is a wide element.
const ITF: [[bool; 5]; 10] = [
    [false, false, true, true, false],
    [true, false, false, false, true],
    [false, true, false, false, true],
    [true, true, false, false, false],
    [false, false, true, false, true],
    [true, false, true, false, false],
    [false, true, true, false, false],
    [false, false, false, true, true],
    [true, false, false, true, false],
    [false, true, false, true, false],
];

fn barcode(p: &mut Paper, digits: &[u8], h: i32) {
    let (n, wide) = (2, 5);
    let elems = |pairs: &mut dyn FnMut(i32, bool)| {
        for &w in &[n, n, n, n] {
            pairs(w, false);
        }
        for pair in digits.chunks(2) {
            let (a, b) = (ITF[pair[0] as usize], ITF[pair[1] as usize]);
            for k in 0..5 {
                pairs(if a[k] { wide } else { n }, false);
                pairs(if b[k] { wide } else { n }, false);
            }
        }
        for &w in &[wide, n, n] {
            pairs(w, false);
        }
    };
    let mut total = 0;
    elems(&mut |w, _| total += w);
    let mut x = (W - total) / 2;
    let mut bar = true;
    let y = p.y;
    elems(&mut |w, _| {
        if bar {
            p.fill(x, y, w, h, true);
        }
        x += w;
        bar = !bar;
    });
    p.y += h + 2;
}

fn footer(p: &mut Paper, unix: Option<i64>) {
    if let Some(t) = unix {
        let digits: String<12> = s(format_args!("{:010}", t));
        let d: Vec<u8, 12> = digits.bytes().map(|b| b - b'0').collect();
        barcode(p, &d, 48);
        p.text_center(p.y, &s::<40>(format_args!("unix {}  0x{:08X}", t, t)), SMALL, 1, true);
        p.y += 16;
    }
    // The size line knows how long the receipt ends up.
    let rows = p.y + 16 + 16 + TAIL;
    let line: String<64> = s(format_args!(
        "{} rows = {}.{} mm = {} bytes over BLE",
        rows,
        rows / 8,
        rows % 8 * 10 / 8,
        rows as usize * ROW_BYTES
    ));
    p.text_center(p.y, &line, SMALL, 1, true);
    p.y += 16;
    p.hline(0, W - 1, p.y + 6, 4);
    p.fill(W / 2 - 22, p.y, 44, 15, false);
    p.text_center(p.y, "8< TEAR", SMALL, 1, true);
    p.y += 16 + TAIL;
}
