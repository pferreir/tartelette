//! NETSCOPE: an internet sonar for a Pico W on a Pimoroni Pico Display Pack
//! 2.0 (`boards/shields/pico-display-2.toml`: 320x240 ST7789 on SPI0, buttons A/B/X/Y).
//!
//! It joins Wi-Fi, gets a DHCP lease, then keeps probing the real internet:
//! DNS, an NTP query (which sets a clock), an HTTP GET and a TCP handshake
//! with a handful of well-known hosts. Every host is a blip on a radar scope,
//! placed by round-trip time on a log scale, with a sweeping beam. The REACH
//! column is the geeky bit: how far light in fibre (200 km/ms) could have
//! travelled, one way, during the round trip.
//!
//! Pages: 1 sonar, 2 table + console, 3 clock (7-segment and binary).
//! Buttons: A retest now, B/X next/previous page, Y switch ms / km of light.
//!
//! The screen is drawn in 16-row strips, so no frame buffer is needed.
#![no_std]
#![no_main]

use core::cell::RefCell;
use core::convert::Infallible;
use core::fmt::Write as _;

use cyw43::{JoinOptions, aligned_bytes};
use cyw43_pio::{DEFAULT_CLOCK_DIVIDER, PioSpi};
use defmt::*;
use embassy_executor::Spawner;
use embassy_net::dns::DnsQueryType;
use embassy_net::iface::Iface;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::UdpSocket;
use embassy_net::wire::{IpAddr, Ipv4Addr, SocketAddr};
use embassy_net::{Stack, StackStorage, StaticPool};
use embassy_rp::{bind_interrupts, dma};
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::mode::Blocking;
use embassy_rp::spi::{self, Spi};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_5X8, FONT_6X10, FONT_9X15_BOLD};
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::raw::RawU16;
use embedded_graphics::prelude::*;
use embedded_graphics::text::{Baseline, Text};
use embedded_io_async::Write;
use heapless::String;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
});

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "TarteletteNet",
};
const PASS: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "hunter22",
};

// ---------------------------------------------------------------- state ----

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Wait,
    Run,
    Ok,
    Fail,
}

#[derive(Clone)]
struct Host {
    name: &'static str,
    /// Literal address, for hosts probed without a DNS lookup.
    lit: Option<[u8; 4]>,
    port: u16,
    ip: Option<[u8; 4]>,
    dns_us: Option<u32>,
    rtt_us: Option<u32>,
    step: Step,
}

const fn host(name: &'static str, lit: Option<[u8; 4]>, port: u16) -> Host {
    Host { name, lit, port, ip: None, dns_us: None, rtt_us: None, step: Step::Wait }
}

const NHOST: usize = 6;
const LOG_LINES: usize = 5;

#[derive(Clone)]
struct State {
    version: u32,
    wifi: Step,
    dhcp: Step,
    dns: Step,
    ntp: Step,
    http: Step,
    tcp: Step,
    ip: String<16>,
    dns_us: u32,
    ntp_rtt_us: u32,
    http_status: u16,
    http_us: u32,
    /// Unix time in ms at the instant it was received.
    clock: Option<(u64, Instant)>,
    hosts: [Host; NHOST],
    log: [String<48>; LOG_LINES],
    rounds: u32,
}

impl State {
    const fn new() -> Self {
        Self {
            version: 0,
            wifi: Step::Wait,
            dhcp: Step::Wait,
            dns: Step::Wait,
            ntp: Step::Wait,
            http: Step::Wait,
            tcp: Step::Wait,
            ip: String::new(),
            dns_us: 0,
            ntp_rtt_us: 0,
            http_status: 0,
            http_us: 0,
            clock: None,
            hosts: [
                host("1.1.1.1", Some([1, 1, 1, 1]), 443),
                host("dns.google", None, 443),
                host("github.com", None, 443),
                host("kernel.org", None, 443),
                host("wikipedia.org", None, 443),
                host("archive.org", None, 443),
            ],
            log: [String::new(), String::new(), String::new(), String::new(), String::new()],
            rounds: 0,
        }
    }

    fn up(&self) -> usize {
        self.hosts.iter().filter(|h| h.step == Step::Ok).count()
    }

    fn unix_ms(&self) -> Option<u64> {
        self.clock.map(|(ms, at)| ms + at.elapsed().as_millis())
    }
}

static STATE: Mutex<CriticalSectionRawMutex, RefCell<State>> = Mutex::new(RefCell::new(State::new()));
static RETEST: Signal<CriticalSectionRawMutex, ()> = Signal::new();

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.lock(|s| {
        let mut s = s.borrow_mut();
        s.version = s.version.wrapping_add(1);
        f(&mut s)
    })
}

fn snapshot() -> State {
    STATE.lock(|s| s.borrow().clone())
}

fn fmt<const N: usize>(args: core::fmt::Arguments) -> String<N> {
    let mut s = String::new();
    let _ = s.write_fmt(args);
    s
}

fn log(msg: &str) {
    info!("{=str}", msg);
    with(|s| {
        s.log.rotate_left(1);
        let l = &mut s.log[LOG_LINES - 1];
        l.clear();
        let _ = l.write_str(&msg[..msg.len().min(48)]);
    });
}

// ----------------------------------------------------------- the canvas ----

const W: i32 = 320;
const H: i32 = 240;
const STRIP: i32 = 16;

const fn rgb(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 & 0xF8) << 8) | ((g as u16 & 0xFC) << 3) | (b as u16 >> 3)
}

fn scale(c: (u8, u8, u8), f: f32) -> u16 {
    let f = f.clamp(0.0, 1.0);
    rgb((c.0 as f32 * f) as u8, (c.1 as f32 * f) as u8, (c.2 as f32 * f) as u8)
}

const PHOSPHOR: (u8, u8, u8) = (110, 255, 140);
const AMBER: (u8, u8, u8) = (255, 176, 0);
const RED: (u8, u8, u8) = (255, 70, 60);
const CYAN: (u8, u8, u8) = (80, 220, 255);
const GRID: u16 = rgb(0, 70, 30);
const DIM: u16 = rgb(70, 110, 85);
const INK: u16 = rgb(110, 255, 140);
const WARN: u16 = rgb(255, 176, 0);
const BAD: u16 = rgb(255, 70, 60);
const COOL: u16 = rgb(80, 220, 255);

/// A horizontal slice of the screen, in RGB565 big-endian as the panel wants it.
struct Canvas {
    buf: [u8; (W * STRIP * 2) as usize],
    x0: i32,
    y0: i32,
    w: i32,
    h: i32,
}

impl Canvas {
    fn begin(&mut self, x: i32, y: i32, w: i32, h: i32) {
        self.x0 = x;
        self.y0 = y;
        self.w = w;
        self.h = h;
        self.buf[..(w * h * 2) as usize].fill(0);
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..(self.w * self.h * 2) as usize]
    }

    fn px(&mut self, x: i32, y: i32, c: u16) {
        let (lx, ly) = (x - self.x0, y - self.y0);
        if lx >= 0 && ly >= 0 && lx < self.w && ly < self.h {
            let i = ((ly * self.w + lx) * 2) as usize;
            self.buf[i..i + 2].copy_from_slice(&c.to_be_bytes());
        }
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: u16) {
        let xa = x.max(self.x0);
        let xb = (x + w).min(self.x0 + self.w);
        let ya = y.max(self.y0);
        let yb = (y + h).min(self.y0 + self.h);
        if xa >= xb {
            return;
        }
        let b = c.to_be_bytes();
        for yy in ya..yb {
            let i = (((yy - self.y0) * self.w + (xa - self.x0)) * 2) as usize;
            for k in 0..(xb - xa) as usize {
                self.buf[i + 2 * k] = b[0];
                self.buf[i + 2 * k + 1] = b[1];
            }
        }
    }

    fn line(&mut self, mut x0: i32, mut y0: i32, x1: i32, y1: i32, c: u16) {
        if y0.max(y1) < self.y0 || y0.min(y1) >= self.y0 + self.h {
            return;
        }
        let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let mut err = dx + dy;
        loop {
            self.px(x0, y0, c);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    fn circle(&mut self, cx: i32, cy: i32, r: i32, c: u16) {
        if cy + r < self.y0 || cy - r >= self.y0 + self.h {
            return;
        }
        let (mut x, mut y, mut d) = (r, 0, 1 - r);
        while x >= y {
            for (px, py) in [(x, y), (y, x), (-y, x), (-x, y), (-x, -y), (-y, -x), (y, -x), (x, -y)] {
                self.px(cx + px, cy + py, c);
            }
            y += 1;
            if d < 0 {
                d += 2 * y + 1;
            } else {
                x -= 1;
                d += 2 * (y - x) + 1;
            }
        }
    }

    fn disc(&mut self, cx: i32, cy: i32, r: i32, c: u16) {
        for dy in -r..=r {
            let dx = libm::sqrtf((r * r - dy * dy) as f32) as i32;
            self.fill(cx - dx, cy + dy, 2 * dx + 1, 1, c);
        }
    }

    fn text(&mut self, x: i32, y: i32, s: &str, font: &MonoFont, c: u16) {
        let fh = font.character_size.height as i32;
        if y + fh <= self.y0 || y >= self.y0 + self.h || x >= self.x0 + self.w {
            return;
        }
        let style = MonoTextStyle::new(font, embedded_graphics::pixelcolor::Rgb565::from(RawU16::new(c)));
        let _ = Text::with_baseline(s, Point::new(x, y), style, Baseline::Top).draw(self);
    }

    fn text_r(&mut self, right: i32, y: i32, s: &str, font: &MonoFont, c: u16) {
        let w = s.len() as i32 * (font.character_size.width + font.character_spacing) as i32;
        self.text(right - w, y, s, font, c);
    }
}

impl OriginDimensions for Canvas {
    fn size(&self) -> Size {
        Size::new(W as u32, H as u32)
    }
}

impl DrawTarget for Canvas {
    type Color = embedded_graphics::pixelcolor::Rgb565;
    type Error = Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<Self::Color>>>(&mut self, pixels: I) -> Result<(), Infallible> {
        for Pixel(p, c) in pixels {
            self.px(p.x, p.y, c.into_storage());
        }
        Ok(())
    }
}

// --------------------------------------------------------------- the LCD ----

struct Lcd {
    spi: Spi<'static, Blocking>,
    cs: Output<'static>,
    dc: Output<'static>,
}

impl Lcd {
    fn cmd(&mut self, c: u8, args: &[u8]) {
        self.cs.set_low();
        self.dc.set_low();
        let _ = self.spi.blocking_write(&[c]);
        self.dc.set_high();
        if !args.is_empty() {
            let _ = self.spi.blocking_write(args);
        }
        self.cs.set_high();
    }

    async fn init(&mut self) {
        self.cmd(0x01, &[]); // SWRESET
        Timer::after_millis(150).await;
        self.cmd(0x11, &[]); // SLPOUT
        Timer::after_millis(120).await;
        self.cmd(0x3A, &[0x05]); // 16 bpp
        self.cmd(0x36, &[0x60]); // MADCTL: landscape (MX | MV)
        self.cmd(0x13, &[]); // NORON
        self.cmd(0x29, &[]); // DISPON
        Timer::after_millis(20).await;
    }

    /// Render `scene` into the rectangle, one strip at a time.
    fn flush(&mut self, cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, scene: &mut dyn FnMut(&mut Canvas)) {
        let mut yy = y;
        while yy < y + h {
            let rows = STRIP.min(y + h - yy);
            cv.begin(x, yy, w, rows);
            scene(cv);
            let (xe, ye) = (x + w - 1, yy + rows - 1);
            self.cmd(0x2A, &[(x >> 8) as u8, x as u8, (xe >> 8) as u8, xe as u8]);
            self.cmd(0x2B, &[(yy >> 8) as u8, yy as u8, (ye >> 8) as u8, ye as u8]);
            self.cs.set_low();
            self.dc.set_low();
            let _ = self.spi.blocking_write(&[0x2C]);
            self.dc.set_high();
            let _ = self.spi.blocking_write(cv.bytes());
            self.cs.set_high();
            yy += rows;
        }
    }
}

// ---------------------------------------------------------------- scenes ----

fn fmt_ms(us: u32) -> String<10> {
    if us < 10_000 {
        fmt(format_args!("{}.{}ms", us / 1000, us % 1000 / 100))
    } else {
        fmt(format_args!("{}ms", us / 1000))
    }
}

/// How far light in fibre (200 km/ms) gets one way during a round trip.
fn fmt_reach(us: u32) -> String<10> {
    fmt(format_args!("{}km", us / 10))
}

fn step_tag(s: Step) -> (&'static str, u16) {
    match s {
        Step::Wait => ("...", DIM),
        Step::Run => ("RUN", WARN),
        Step::Ok => ("OK ", INK),
        Step::Fail => ("ERR", BAD),
    }
}

fn verdict(st: &State) -> (&'static str, u16) {
    let up = st.up();
    if st.wifi != Step::Ok || st.dhcp != Step::Ok {
        ("LINKING", WARN)
    } else if st.rounds == 0 {
        ("PROBING", WARN)
    } else if up == 0 {
        ("OFFLINE", BAD)
    } else if up < NHOST {
        ("DEGRADED", WARN)
    } else {
        ("ONLINE", INK)
    }
}

fn header(cv: &mut Canvas, st: &State, page: u8) {
    cv.fill(0, 0, W, 15, rgb(0, 40, 18));
    cv.text(4, 0, "NETSCOPE", &FONT_9X15_BOLD, scale(AMBER, 1.0));
    let (v, col) = verdict(st);
    cv.text(100, 2, v, &FONT_6X10, col);
    let n = fmt::<12>(format_args!("{}/{} up", st.up(), NHOST));
    cv.text(160, 2, &n, &FONT_6X10, DIM);
    for p in 0..3 {
        let x = 296 + p * 10;
        if p as u8 == page {
            cv.disc(x, 7, 3, INK);
        } else {
            cv.circle(x, 7, 3, DIM);
        }
    }
}

fn footer(cv: &mut Canvas, units_km: bool) {
    cv.fill(0, 226, W, 14, rgb(0, 20, 10));
    cv.text(4, 229, "A retest  B/X page  Y units:", &FONT_6X10, DIM);
    cv.text(176, 229, if units_km { "reach" } else { "ms" }, &FONT_6X10, COOL);
}

fn ring_radius(ms: f32, r_max: f32) -> f32 {
    r_max * libm::logf(1.0 + ms) / libm::logf(1.0 + 500.0)
}

const CX: i32 = 96;
const CY: i32 = 120;
const R: i32 = 88;

fn host_angle(i: usize) -> f32 {
    30.0 + 60.0 * i as f32
}

fn polar(deg: f32, r: f32) -> (i32, i32) {
    let a = deg * (core::f32::consts::PI / 180.0);
    (CX + libm::roundf(r * libm::sinf(a)) as i32, CY - libm::roundf(r * libm::cosf(a)) as i32)
}

fn radar(cv: &mut Canvas, st: &State, t_ms: u64, km: bool) {
    // Rings and crosshairs.
    for (ms, label) in [(10.0, "10"), (50.0, "50"), (150.0, "150"), (500.0, "500ms")] {
        let r = ring_radius(ms, R as f32) as i32;
        cv.circle(CX, CY, r, GRID);
        cv.text(CX + 2, CY - r + 1, label, &FONT_5X8, rgb(0, 110, 50));
    }
    cv.line(CX - R, CY, CX + R, CY, GRID);
    cv.line(CX, CY - R, CX, CY + R, GRID);

    // The beam and its afterglow.
    let sweep = (t_ms as f32 / 16.0) % 360.0;
    for k in 0..28 {
        let f = 1.0 - k as f32 / 28.0;
        let (x, y) = polar(sweep - k as f32 * 1.6, R as f32);
        cv.line(CX, CY, x, y, scale(PHOSPHOR, 0.5 * f * f * f));
    }

    // Blips.
    for (i, h) in st.hosts.iter().enumerate() {
        let a = host_angle(i);
        let behind = (sweep - a + 360.0) % 360.0;
        let glow = (1.0 - behind / 360.0).max(0.25);
        let digit = fmt::<2>(format_args!("{}", i + 1));
        match (h.step, h.rtt_us) {
            (Step::Ok, Some(us)) => {
                let ms = us as f32 / 1000.0;
                let (x, y) = polar(a, ring_radius(ms, R as f32));
                let c = if ms > 150.0 { AMBER } else { PHOSPHOR };
                cv.disc(x, y, 3, scale(c, glow));
                cv.circle(x, y, 5, scale(c, glow * 0.5));
                cv.text(x + 7, y - 4, &digit, &FONT_5X8, scale(c, 0.4 + glow * 0.6));
            }
            (Step::Run, _) => {
                let (x, y) = polar(a, R as f32 - 4.0);
                let pulse = 3 + ((t_ms / 100) % 4) as i32;
                cv.circle(x, y, pulse, scale(AMBER, 0.9));
                cv.text(x + 7, y - 4, &digit, &FONT_5X8, scale(AMBER, 0.9));
            }
            (Step::Fail, _) => {
                let (x, y) = polar(a, R as f32 - 2.0);
                cv.line(x - 3, y - 3, x + 3, y + 3, scale(RED, 0.8));
                cv.line(x - 3, y + 3, x + 3, y - 3, scale(RED, 0.8));
                cv.text(x + 7, y - 4, &digit, &FONT_5X8, scale(RED, 0.8));
            }
            _ => {}
        }
    }
    cv.disc(CX, CY, 2, scale(CYAN, 1.0));
    let _ = km;
}

fn side(cv: &mut Canvas, st: &State, km: bool) {
    let x = 196;
    let mut y = 20;
    let mut row = |cv: &mut Canvas, name: &str, step: Step, val: &str| {
        let (tag, col) = step_tag(step);
        cv.text(x, y, name, &FONT_6X10, DIM);
        cv.text(x + 30, y, tag, &FONT_6X10, col);
        cv.text(x + 54, y, val, &FONT_6X10, INK);
        y += 12;
    };
    row(cv, "WIFI", st.wifi, &SSID[..SSID.len().min(11)]);
    row(cv, "DHCP", st.dhcp, &st.ip);
    let dns = if st.dns == Step::Ok { fmt_ms(st.dns_us) } else { String::new() };
    row(cv, "DNS", st.dns, &dns);
    let ntp = match (st.ntp, st.unix_ms()) {
        (Step::Ok, Some(ms)) => {
            let s = ms / 1000;
            fmt::<12>(format_args!("{:02}:{:02}:{:02}Z", s / 3600 % 24, s / 60 % 60, s % 60))
        }
        _ => String::new(),
    };
    row(cv, "NTP", st.ntp, &ntp);
    let http = if st.http == Step::Ok { fmt::<14>(format_args!("{} {}", st.http_status, fmt_ms(st.http_us))) } else { String::new() };
    row(cv, "HTTP", st.http, &http);
    let tcp = if st.rounds > 0 || st.tcp != Step::Wait { fmt::<12>(format_args!("{}/{}", st.up(), NHOST)) } else { String::new() };
    row(cv, "TCP", st.tcp, &tcp);

    y += 8;
    cv.text(x, y, if km { "# HOST    REACH" } else { "# HOST      RTT" }, &FONT_6X10, rgb(255, 176, 0));
    y += 12;
    for (i, h) in st.hosts.iter().enumerate() {
        let n = &h.name[..h.name.len().min(9)];
        let l = fmt::<4>(format_args!("{}", i + 1));
        cv.text(x, y, &l, &FONT_6X10, DIM);
        cv.text(x + 12, y, n, &FONT_6X10, if h.step == Step::Fail { BAD } else { INK });
        let v = match (h.step, h.rtt_us) {
            (Step::Ok, Some(us)) => if km { fmt_reach(us) } else { fmt_ms(us) },
            (Step::Fail, _) => fmt::<10>(format_args!("---")),
            (Step::Run, _) => fmt::<10>(format_args!("...")),
            _ => String::new(),
        };
        cv.text_r(W - 4, y, &v, &FONT_6X10, COOL);
        y += 12;
    }
}

fn sonar_page(cv: &mut Canvas, st: &State, t_ms: u64, km: bool) {
    header(cv, st, 0);
    radar(cv, st, t_ms, km);
    side(cv, st, km);
    footer(cv, km);
}

fn table_page(cv: &mut Canvas, st: &State, km: bool) {
    header(cv, st, 1);
    cv.text(4, 20, "HOST", &FONT_6X10, WARN);
    cv.text(94, 20, "IP", &FONT_6X10, WARN);
    cv.text_r(232, 20, "DNS", &FONT_6X10, WARN);
    cv.text_r(272, 20, "RTT", &FONT_6X10, WARN);
    cv.text_r(316, 20, "REACH", &FONT_6X10, WARN);
    cv.fill(4, 31, 312, 1, GRID);
    let mut y = 35;
    for h in st.hosts.iter() {
        let ip = match h.ip.or(h.lit) {
            Some(i) => fmt::<16>(format_args!("{}.{}.{}.{}", i[0], i[1], i[2], i[3])),
            None => String::new(),
        };
        let dns = h.dns_us.map(fmt_ms).unwrap_or_default();
        let (rtt, reach) = match (h.step, h.rtt_us) {
            (Step::Ok, Some(us)) => (fmt_ms(us), fmt_reach(us)),
            (Step::Fail, _) => (fmt::<10>(format_args!("fail")), String::new()),
            _ => (String::new(), String::new()),
        };
        let col = if h.step == Step::Fail { BAD } else { INK };
        cv.text(4, y, h.name, &FONT_6X10, col);
        cv.text(94, y, &ip, &FONT_6X10, DIM);
        cv.text_r(232, y, &dns, &FONT_6X10, COOL);
        cv.text_r(272, y, &rtt, &FONT_6X10, col);
        cv.text_r(316, y, &reach, &FONT_6X10, if km { WARN } else { COOL });
        y += 12;
    }
    cv.text(4, y + 2, "REACH: one-way distance light in fibre", &FONT_6X10, rgb(0, 110, 50));
    cv.text(4, y + 14, "covers during the round trip (200km/ms)", &FONT_6X10, rgb(0, 110, 50));
    // Console.
    let top = 142;
    cv.fill(4, top, 312, 1, GRID);
    for (i, l) in st.log.iter().enumerate() {
        let c = if i == LOG_LINES - 1 { INK } else { DIM };
        cv.text(4, top + 6 + 12 * i as i32, "$", &FONT_6X10, rgb(255, 176, 0));
        cv.text(14, top + 6 + 12 * i as i32, l, &FONT_6X10, c);
    }
    footer(cv, km);
}

const SEGS: [u8; 10] = [0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F];

/// One 7-segment digit; bit 0 = a (top), 1 = b, 2 = c, 3 = d, 4 = e, 5 = f, 6 = g.
fn digit7(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, t: i32, d: u8, on: u16, off: u16) {
    let mid = y + (h - t) / 2;
    let vh = (h - t) / 2 - t;
    let rects = [
        (x + t, y, w - 2 * t, t),
        (x + w - t, y + t, t, vh),
        (x + w - t, mid + t, t, vh),
        (x + t, y + h - t, w - 2 * t, t),
        (x, mid + t, t, vh),
        (x, y + t, t, vh),
        (x + t, mid, w - 2 * t, t),
    ];
    let bits = SEGS[d as usize % 10];
    for (i, (rx, ry, rw, rh)) in rects.into_iter().enumerate() {
        cv.fill(rx, ry, rw, rh, if bits >> i & 1 != 0 { on } else { off });
    }
}

fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + (m <= 2) as i64, m as u32, d as u32)
}

fn clock_page(cv: &mut Canvas, st: &State, km: bool) {
    header(cv, st, 2);
    let secs = st.unix_ms().map(|ms| ms / 1000);
    let t = secs.map(|s| [(s / 3600 % 24) as u8, (s / 60 % 60) as u8, (s % 60) as u8]);
    let on = scale(PHOSPHOR, 1.0);
    let off = rgb(0, 28, 14);
    let mut x = 10;
    for g in 0..3 {
        for k in 0..2 {
            let d = t.map(|t| if k == 0 { t[g] / 10 } else { t[g] % 10 });
            match d {
                Some(d) => digit7(cv, x, 24, 36, 64, 6, d, on, off),
                None => cv.fill(x + 6, 54, 24, 6, DIM),
            }
            x += 44;
        }
        if g < 2 {
            let c = if t.is_some_and(|t| t[2] % 2 == 0) { on } else { off };
            cv.fill(x - 2, 40, 7, 7, c);
            cv.fill(x - 2, 64, 7, 7, c);
            x += 12;
        }
    }

    match secs {
        Some(s) => {
            let (y, m, d) = civil((s / 86_400) as i64);
            let wd = ["THU", "FRI", "SAT", "SUN", "MON", "TUE", "WED"][(s / 86_400 % 7) as usize];
            let date = fmt::<24>(format_args!("{y:04}-{m:02}-{d:02} {wd} UTC"));
            cv.text(10, 98, &date, &FONT_10X20, scale(AMBER, 1.0));
            let unix = fmt::<24>(format_args!("unix {s}"));
            let hex = fmt::<24>(format_args!("0x{s:08X}"));
            cv.text(10, 128, &unix, &FONT_6X10, COOL);
            cv.text(10, 140, &hex, &FONT_6X10, COOL);
            let rtt = fmt::<24>(format_args!("ntp rtt {}", fmt_ms(st.ntp_rtt_us)));
            cv.text(10, 152, &rtt, &FONT_6X10, DIM);
            cv.text(10, 164, "synced via UDP/123", &FONT_6X10, DIM);
            // Binary clock: BCD columns, bit 3 on top.
            let t = t.unwrap();
            let cols = [t[0] / 10, t[0] % 10, t[1] / 10, t[1] % 10, t[2] / 10, t[2] % 10];
            let rows = [2, 4, 3, 4, 3, 4];
            let (bx, by) = (186, 130);
            for (c, (v, n)) in cols.iter().zip(rows).enumerate() {
                let cx = bx + c as i32 * 22 + (c as i32 / 2) * 6;
                for bit in 0..n {
                    let cy = by + (3 - bit) * 22;
                    if v >> bit & 1 != 0 {
                        cv.disc(cx, cy, 8, on);
                    } else {
                        cv.circle(cx, cy, 8, rgb(0, 60, 30));
                    }
                }
            }
            for (i, l) in ["8", "4", "2", "1"].iter().enumerate() {
                cv.text(bx - 20, by + i as i32 * 22 - 4, l, &FONT_5X8, DIM);
            }
        }
        None => {
            cv.text(10, 98, "NO TIME SYNC", &FONT_10X20, BAD);
            cv.text(10, 128, "waiting for NTP over the internet", &FONT_6X10, DIM);
        }
    }
    footer(cv, km);
}

// ---------------------------------------------------------- the net tests ----

async fn resolve(stack: Stack<'static>, name: &str) -> Option<([u8; 4], u32)> {
    let t = Instant::now();
    let r = with_timeout(Duration::from_secs(4), stack.dns_query(name, DnsQueryType::A)).await;
    let us = t.elapsed().as_micros() as u32;
    if let Ok(Ok(addrs)) = r {
        for a in addrs.iter() {
            #[allow(irrefutable_let_patterns)]
            if let IpAddr::V4(a) = a {
                return Some((a.octets(), us));
            }
        }
    }
    None
}

async fn tcp_rtt(stack: Stack<'static>, ip: [u8; 4], port: u16) -> Option<u32> {
    let mut rx = [0u8; 256];
    let mut tx = [0u8; 64];
    let Ok(mut sock) = TcpSocket::new(stack, &mut rx, &mut tx) else {
        return None;
    };
    sock.set_timeout(Some(Duration::from_secs(5)));
    let t = Instant::now();
    let r = with_timeout(Duration::from_secs(4), sock.connect(SocketAddr::new(Ipv4Addr::from(ip).into(), port))).await;
    let us = t.elapsed().as_micros() as u32;
    sock.abort();
    Timer::after_millis(30).await;
    matches!(r, Ok(Ok(()))).then_some(us)
}

async fn ntp(stack: Stack<'static>, ip: [u8; 4]) -> Option<(u64, u32)> {
    let mut sock = UdpSocket::new(stack).ok()?;
    sock.bind(40123, SocketAddr::UNSPECIFIED).ok()?;
    let mut pkt = [0u8; 48];
    pkt[0] = 0x1B; // LI 0, version 3, client
    let t = Instant::now();
    sock.send_to(&pkt, SocketAddr::new(Ipv4Addr::from(ip).into(), 123)).await.ok()?;
    let mut buf = [0u8; 64];
    let (n, _) = with_timeout(Duration::from_secs(4), sock.recv_from(&mut buf)).await.ok()?.ok()?;
    let us = t.elapsed().as_micros() as u32;
    if n < 48 {
        return None;
    }
    let secs = u32::from_be_bytes([buf[40], buf[41], buf[42], buf[43]]) as u64;
    secs.checked_sub(2_208_988_800).map(|unix| (unix * 1000, us))
}

async fn http_status(stack: Stack<'static>, ip: [u8; 4], host: &str) -> Option<(u16, u32)> {
    let mut rx = [0u8; 512];
    let mut tx = [0u8; 256];
    let Ok(mut sock) = TcpSocket::new(stack, &mut rx, &mut tx) else {
        return None;
    };
    sock.set_timeout(Some(Duration::from_secs(6)));
    let t = Instant::now();
    let go = async {
        sock.connect(SocketAddr::new(Ipv4Addr::from(ip).into(), 80)).await.ok()?;
        let req = fmt::<128>(format_args!("GET / HTTP/1.0\r\nHost: {host}\r\nUser-Agent: netscope-pico\r\n\r\n"));
        sock.write_all(req.as_bytes()).await.ok()?;
        let mut head = [0u8; 16];
        let mut n = 0;
        while n < 12 {
            let k = sock.read(&mut head[n..]).await.ok()?;
            if k == 0 {
                return None;
            }
            n += k;
        }
        // "HTTP/1.x NNN"
        let code = core::str::from_utf8(&head[9..12]).ok()?.parse::<u16>().ok()?;
        Some(code)
    };
    let code = with_timeout(Duration::from_secs(8), go).await.ok()??;
    let us = t.elapsed().as_micros() as u32;
    sock.abort();
    Some((code, us))
}

async fn round(stack: Stack<'static>, control: &mut cyw43::Control<'static>) {
    control.gpio_set(0, true).await;
    with(|s| {
        s.dns = Step::Run;
        s.ntp = Step::Wait;
        s.http = Step::Wait;
        s.tcp = Step::Wait;
        for h in s.hosts.iter_mut() {
            h.step = Step::Wait;
        }
    });

    log("dns: time.cloudflare.com");
    let time_ip = resolve(stack, "time.cloudflare.com").await;
    match time_ip {
        Some((ip, us)) => {
            log(&fmt::<48>(format_args!("  -> {}.{}.{}.{} in {}", ip[0], ip[1], ip[2], ip[3], fmt_ms(us))));
            with(|s| {
                s.dns = Step::Ok;
                s.dns_us = us;
            });
        }
        None => {
            log("  -> dns failed");
            with(|s| s.dns = Step::Fail);
        }
    }

    with(|s| s.ntp = Step::Run);
    log("ntp: asking what time it is");
    match ntp(stack, time_ip.map_or([162, 159, 200, 1], |t| t.0)).await {
        Some((ms, rtt)) => {
            log(&fmt::<48>(format_args!("  -> unix {} rtt {}", ms / 1000, fmt_ms(rtt))));
            with(|s| {
                s.ntp = Step::Ok;
                s.ntp_rtt_us = rtt;
                s.clock = Some((ms, Instant::now()));
            });
        }
        None => {
            log("  -> no answer");
            with(|s| s.ntp = Step::Fail);
        }
    }

    with(|s| s.http = Step::Run);
    log("http: GET example.com");
    let web = match resolve(stack, "example.com").await {
        Some((ip, _)) => http_status(stack, ip, "example.com").await,
        None => None,
    };
    match web {
        Some((code, us)) => {
            log(&fmt::<48>(format_args!("  -> HTTP {} in {}", code, fmt_ms(us))));
            with(|s| {
                s.http = Step::Ok;
                s.http_status = code;
                s.http_us = us;
            });
        }
        None => {
            log("  -> no response");
            with(|s| s.http = Step::Fail);
        }
    }

    with(|s| s.tcp = Step::Run);
    for i in 0..NHOST {
        let h = with(|s| {
            s.hosts[i].step = Step::Run;
            s.hosts[i].rtt_us = None;
            s.hosts[i].clone()
        });
        log(&fmt::<48>(format_args!("tcp: {}:{}", h.name, h.port)));
        let ip = match h.lit {
            Some(ip) => Some(ip),
            None => match resolve(stack, h.name).await {
                Some((ip, us)) => {
                    with(|s| s.hosts[i].dns_us = Some(us));
                    Some(ip)
                }
                None => None,
            },
        };
        let rtt = match ip {
            Some(ip) => {
                with(|s| s.hosts[i].ip = Some(ip));
                tcp_rtt(stack, ip, h.port).await
            }
            None => None,
        };
        with(|s| {
            s.hosts[i].rtt_us = rtt;
            s.hosts[i].step = if rtt.is_some() { Step::Ok } else { Step::Fail };
        });
        match rtt {
            Some(us) => log(&fmt::<48>(format_args!("  -> up {} ({}km)", fmt_ms(us), us / 10))),
            None => log("  -> unreachable"),
        }
    }
    with(|s| {
        s.tcp = if s.up() > 0 { Step::Ok } else { Step::Fail };
        s.rounds += 1;
    });
    control.gpio_set(0, false).await;
}

// ----------------------------------------------------------------- tasks ----

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>, cyw43::Cyw43439>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn diag_task(mut control: cyw43::Control<'static>, stack: Stack<'static>, iface: Iface<'static>) -> ! {
    with(|s| s.wifi = Step::Run);
    loop {
        log(&fmt::<48>(format_args!("wifi: joining {SSID}")));
        let opts = if PASS.is_empty() { JoinOptions::new_open() } else { JoinOptions::new(PASS.as_bytes()) };
        match control.join(SSID, opts).await {
            Ok(()) => break,
            Err(e) => {
                log(&fmt::<48>(format_args!("wifi: join failed ({:?})", e)));
                with(|s| s.wifi = Step::Fail);
                Timer::after_secs(2).await;
                with(|s| s.wifi = Step::Run);
            }
        }
    }
    with(|s| {
        s.wifi = Step::Ok;
        s.dhcp = Step::Run;
    });
    iface.wait_config_up().await;
    let a = unwrap!(iface.dhcpv4_lease()).address.address().octets();
    log(&fmt::<48>(format_args!("dhcp: {}.{}.{}.{}", a[0], a[1], a[2], a[3])));
    with(|s| {
        s.dhcp = Step::Ok;
        s.ip = fmt(format_args!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]));
    });

    loop {
        round(stack, &mut control).await;
        let s = snapshot();
        info!("round {} done: {}/{} hosts up", s.rounds, s.up(), NHOST);
        RETEST.reset();
        let _ = with_timeout(Duration::from_secs(20), RETEST.wait()).await;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let fw = aligned_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/43439A0.bin"));
    let clm = aligned_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/43439A0_clm.bin"));
    let nvram = aligned_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/nvram_rp2040.bin"));

    // Screen first, so the boot is visible.
    let mut cfg = spi::Config::default();
    cfg.frequency = embassy_rp::time::Hertz(31_250_000);
    let mut lcd = Lcd {
        spi: unwrap!(Spi::new_blocking_txonly(p.SPI0, p.PIN_18, p.PIN_19, cfg).ok()),
        cs: Output::new(p.PIN_9, Level::High),
        dc: Output::new(p.PIN_13, Level::Low),
    };
    let _bl = Output::new(p.PIN_17, Level::High);
    lcd.init().await;
    let (a, b, x, y) = (
        Input::new(p.PIN_12, Pull::Up),
        Input::new(p.PIN_20, Pull::Up),
        Input::new(p.PIN_14, Pull::Up),
        Input::new(p.PIN_15, Pull::Up),
    );

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
        dma::Channel::new(p.DMA_CH1, Irqs),
    );

    static STATE_CELL: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE_CELL.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw, nvram).await;
    spawner.spawn(unwrap!(cyw43_task(runner)));
    control.init(clm, cyw43::Config::default()).await;
    control.set_power_management(cyw43::PowerManagementMode::PowerSave).await;

    static STORAGE: StaticCell<StackStorage<'static>> = StaticCell::new();
    // cyw43 receives each frame, bus headers included, straight into these.
    static POOL: StaticPool<{ cyw43::RX_BUF_SIZE }, 8, { cyw43::RX_BUF_ALIGN }> = StaticPool::new();
    let (stack, net_runner) = Stack::new(STORAGE.init(StackStorage::new()), &POOL, 0x5eed_cafe_f00d_0001);
    static DEVICE: StaticCell<cyw43::NetDriver<'static>> = StaticCell::new();
    let iface = unwrap!(stack.add_iface_borrowed(DEVICE.init(net_device)).ok());
    unwrap!(iface.set_dhcpv4(Some(Default::default())).ok());
    spawner.spawn(unwrap!(net_task(net_runner)));
    spawner.spawn(unwrap!(diag_task(control, stack, iface)));

    static CANVAS: StaticCell<Canvas> = StaticCell::new();
    let cv = CANVAS.init(Canvas { buf: [0; (W * STRIP * 2) as usize], x0: 0, y0: 0, w: 0, h: 0 });

    let mut prev = [false; 4];
    let mut page = 0u8;
    let mut km = false;
    let mut shown_version = u32::MAX;
    let mut shown_second = u64::MAX;
    let mut force = true;
    loop {
        let now = [a.is_low(), b.is_low(), x.is_low(), y.is_low()];
        let pressed = |i: usize| now[i] && !prev[i];
        if pressed(0) {
            log("button A: retest");
            RETEST.signal(());
        }
        if pressed(1) {
            page = (page + 1) % 3;
            force = true;
        }
        if pressed(2) {
            page = (page + 2) % 3;
            force = true;
        }
        if pressed(3) {
            km = !km;
            force = true;
        }
        prev = now;

        let st = snapshot();
        let t_ms = Instant::now().as_millis();
        let second = st.unix_ms().map_or(u64::MAX - 1, |ms| ms / 1000);
        let dirty = force || st.version != shown_version || (page == 2 && second != shown_second);
        if dirty || page == 0 {
            let mut scene = |cv: &mut Canvas| match page {
                0 => sonar_page(cv, &st, t_ms, km),
                1 => table_page(cv, &st, km),
                _ => clock_page(cv, &st, km),
            };
            if dirty {
                lcd.flush(cv, 0, 0, W, H, &mut scene);
            } else {
                lcd.flush(cv, 0, 16, 192, 208, &mut scene);
            }
            shown_version = st.version;
            shown_second = second;
            force = false;
        }
        Timer::after_millis(40).await;
    }
}
