//! THE DAILY BIT: a Pico W on a Pimoroni Pico Display Pack 2.0
//! (`--board pico-w --shield pico-display-2`) that reads a few things off the internet
//! over Wi-Fi and prints them, newspaper-style, on an MXW01 "cat printer"
//! over Bluetooth LE.
//!
//! Wi-Fi: NTP for the clock, ip-api.com for where we are, wttr.in for the
//! weather there, open-notify.org for the ISS and who is aboard. All plain
//! HTTP. The moon and the day/night terminator are computed.
//!
//! The screen shows the pipeline on the left and the receipt on the right,
//! at half scale, coming out of the printer as it is sent.
//! Buttons: A new edition (fetch and print), B print it again, X/Y scroll.
#![no_std]
#![no_main]

mod astro;
mod lcd;
mod paper;
mod printer;
mod receipt;
mod web;

use core::cell::RefCell;
use core::fmt::Write as _;

use cyw43::{aligned_bytes, Cyw43439, JoinOptions};
use cyw43_pio::PioSpi;
use defmt::{info, unwrap};
use embassy_executor::Spawner;
use embassy_net::iface::Iface;
use embassy_net::wire::HardwareAddress;
use embassy_net::{Stack, StackStorage};
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::spi::{self, Spi};
use embassy_rp::{bind_interrupts, dma};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, ThreadModeRawMutex};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::mono_font::ascii::{FONT_6X10, FONT_9X15_BOLD};
use heapless::String;
use static_cell::StaticCell;
use trouble_host::prelude::ExternalController;
use {defmt_rtt as _, panic_probe as _};

use lcd::{mix, rgb, Canvas, Lcd};
use paper::Paper;
use receipt::{Crew, Geo, Report, Weather};

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
pub enum Step {
    Wait,
    Run,
    Ok,
    Fail,
}

#[derive(Clone, Copy, PartialEq, defmt::Format)]
pub enum Stage {
    Wifi,
    Dhcp,
    Ntp,
    Geo,
    Wx,
    Iss,
    Crew,
    Render,
    Scan,
    Link,
    Gatt,
    Send,
    Burn,
}

const STAGES: [&str; 13] = ["WIFI", "DHCP", "NTP", "GEOIP", "WTTR", "ISS", "CREW", "RENDER", "SCAN", "LINK", "GATT", "SEND", "BURN"];

#[derive(Clone)]
pub struct State {
    version: u32,
    steps: [Step; 13],
    ip: String<16>,
    printer: String<20>,
    rssi: i8,
    mtu: u16,
    battery: Option<u8>,
    temp: Option<u8>,
    job_rows: u16,
    sent_rows: u16,
    printing: bool,
    edition: u32,
    log: String<40>,
}

static STATE: Mutex<ThreadModeRawMutex, RefCell<State>> = Mutex::new(RefCell::new(State {
    version: 0,
    steps: [Step::Wait; 13],
    ip: String::new(),
    printer: String::new(),
    rssi: 0,
    mtu: 0,
    battery: None,
    temp: None,
    job_rows: 0,
    sent_rows: 0,
    printing: false,
    edition: 0,
    log: String::new(),
}));

/// The receipt being printed, shared by the renderer, the printer and the
/// screen's preview.
pub static PAPER: Mutex<ThreadModeRawMutex, RefCell<Paper>> = Mutex::new(RefCell::new(Paper::new()));
/// Print what is on the paper.
pub static PRINT: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Fetch a new edition.
static FETCH: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.lock(|s| {
        let mut s = s.borrow_mut();
        s.version = s.version.wrapping_add(1);
        f(&mut s)
    })
}

pub fn set(stage: Stage, step: Step) {
    with(|s| s.steps[stage as usize] = step);
}

pub fn log(msg: &str) {
    info!("{=str}", msg);
    with(|s| {
        s.log.clear();
        let _ = s.log.push_str(&msg[..msg.len().min(40)]);
    });
}

fn fmt<const N: usize>(args: core::fmt::Arguments) -> String<N> {
    let mut s = String::new();
    let _ = s.write_fmt(args);
    s
}

// ------------------------------------------------------------- gathering ----

async fn gather(stack: Stack<'static>, control: &mut cyw43::Control<'static>, r: &mut Report) {
    control.gpio_set(0, true).await;
    for st in [Stage::Ntp, Stage::Geo, Stage::Wx, Stage::Iss, Stage::Crew, Stage::Render] {
        set(st, Step::Wait);
    }
    r.edition += 1;
    r.probes.clear();
    with(|s| s.edition = r.edition);
    let mut buf = [0u8; 4096];

    set(Stage::Ntp, Step::Run);
    log("ntp: pool.ntp.org");
    match web::ntp(stack, "pool.ntp.org").await {
        Some((unix, rtt)) => {
            r.clock = Some((unix, Instant::now()));
            r.ntp_rtt_us = rtt;
            set(Stage::Ntp, Step::Ok);
        }
        None => set(Stage::Ntp, if r.clock.is_some() { Step::Ok } else { Step::Fail }),
    }

    set(Stage::Geo, Step::Run);
    log("http: ip-api.com");
    let path = "/json/?fields=status,countryCode,regionName,city,lat,lon,timezone,offset,isp,as,query";
    let (body, probe) = web::get(stack, "ip-api.com", path, &mut buf).await;
    let _ = r.probes.push(probe);
    r.geo = body.and_then(|(a, b)| core::str::from_utf8(&buf[a..b]).ok()).and_then(|j| {
        (web::json_str(j, "status")? == "success").then_some(())?;
        Some(Geo {
            ip: web::ascii(web::json_str(j, "query")?),
            city: web::ascii(web::json_str(j, "city").unwrap_or("?")),
            region: web::ascii(web::json_str(j, "regionName").unwrap_or("")),
            cc: web::ascii(web::json_str(j, "countryCode").unwrap_or("")),
            isp: web::ascii(web::json_str(j, "isp").unwrap_or("?")),
            asn: web::ascii(web::json_str(j, "as").unwrap_or("?")),
            tz: web::ascii(web::json_str(j, "timezone").unwrap_or("?")),
            lat: web::json_num(j, "lat")?,
            lon: web::json_num(j, "lon")?,
            offset: web::json_num(j, "offset").unwrap_or(0.0) as i32,
        })
    });
    set(Stage::Geo, if r.geo.is_some() { Step::Ok } else { Step::Fail });

    set(Stage::Wx, Step::Run);
    log("http: wttr.in");
    // wttr.in finds the place by itself if we don't know it.
    const FORMAT: &str = "?format=%25t|%25f|%25C|%25h|%25w|%25P|%25p|%25u|%25S|%25s&m";
    let path: String<128> = match &r.geo {
        Some(g) => fmt(format_args!("/{:.2},{:.2}{FORMAT}", g.lat, g.lon)),
        None => fmt(format_args!("/{FORMAT}")),
    };
    let (body, probe) = web::get(stack, "wttr.in", &path, &mut buf).await;
    let _ = r.probes.push(probe);
    r.wx = body.and_then(|(a, b)| core::str::from_utf8(&buf[a..b]).ok()).and_then(|t| {
        let mut fields = heapless::Vec::new();
        for f in t.trim().split('|') {
            let _ = fields.push(web::ascii(f.trim()));
        }
        (fields.len() == 10).then_some(Weather { fields })
    });
    set(Stage::Wx, if r.wx.is_some() { Step::Ok } else { Step::Fail });

    set(Stage::Iss, Step::Run);
    log("http: iss-now");
    let (body, probe) = web::get(stack, "api.open-notify.org", "/iss-now.json", &mut buf).await;
    let _ = r.probes.push(probe);
    r.iss = body.and_then(|(a, b)| core::str::from_utf8(&buf[a..b]).ok()).and_then(|j| {
        // The coordinates come as strings.
        let num = |k| web::json_str(j, k).and_then(|v| v.parse().ok()).or_else(|| web::json_num(j, k));
        Some((num("latitude")?, num("longitude")?))
    });
    set(Stage::Iss, if r.iss.is_some() { Step::Ok } else { Step::Fail });

    set(Stage::Crew, Step::Run);
    log("http: astros");
    let (body, probe) = web::get(stack, "api.open-notify.org", "/astros.json", &mut buf).await;
    let _ = r.probes.push(probe);
    r.crew = body.and_then(|(a, b)| core::str::from_utf8(&buf[a..b]).ok()).and_then(|j| {
        let number = web::json_num(j, "number")? as u8;
        let mut people = heapless::Vec::new();
        let list = &j[j.find("\"people\"")?..];
        for obj in list.split('{').skip(1) {
            let obj = &obj[..obj.find('}').unwrap_or(obj.len())];
            if let (Some(name), Some(craft)) = (web::json_str(obj, "name"), web::json_str(obj, "craft")) {
                let _ = people.push((web::ascii(name), web::ascii(craft)));
            }
        }
        Some(Crew { number, people })
    });
    set(Stage::Crew, if r.crew.is_some() { Step::Ok } else { Step::Fail });
    control.gpio_set(0, false).await;

    set(Stage::Render, Step::Run);
    let t = Instant::now();
    let rows = PAPER.lock(|p| {
        let mut p = p.borrow_mut();
        receipt::render(&mut p, r);
        p.job_rows()
    });
    info!("rendered {} rows in {} ms", rows, t.elapsed().as_millis());
    with(|s| {
        s.job_rows = rows as u16;
        s.sent_rows = 0;
    });
    log(&fmt::<40>(format_args!("render: {rows} rows")));
    set(Stage::Render, Step::Ok);
}

// ----------------------------------------------------------------- tasks ----

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>, Cyw43439>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn ble_task(controller: ExternalController<cyw43::bluetooth::BtDriver<'static>, 10>) {
    printer::run(controller).await
}

#[embassy_executor::task]
async fn app_task(mut control: cyw43::Control<'static>, stack: Stack<'static>, iface: Iface<'static>) {
    set(Stage::Wifi, Step::Run);
    loop {
        log(&fmt::<40>(format_args!("wifi: joining {SSID}")));
        let opts = if PASS.is_empty() { JoinOptions::new_open() } else { JoinOptions::new(PASS.as_bytes()) };
        match control.join(SSID, opts).await {
            Ok(()) => break,
            Err(e) => {
                log(&fmt::<40>(format_args!("wifi: join failed ({:?})", e)));
                set(Stage::Wifi, Step::Fail);
                Timer::after_secs(2).await;
                set(Stage::Wifi, Step::Run);
            }
        }
    }
    set(Stage::Wifi, Step::Ok);
    set(Stage::Dhcp, Step::Run);
    iface.wait_config_up().await;
    let lease = unwrap!(iface.dhcpv4_lease());
    static REPORT: StaticCell<Report> = StaticCell::new();
    let report = REPORT.init(Report::new(SSID));
    report.lan_ip = fmt(format_args!("{}", lease.address.address()));
    report.gateway = lease.router.map(|g| fmt(format_args!("{g}"))).unwrap_or_default();
    #[allow(irrefutable_let_patterns)]
    if let HardwareAddress::Ethernet(mac) = iface.hardware_addr() {
        report.mac = mac.0;
    }
    log(&fmt::<40>(format_args!("dhcp: {}", report.lan_ip)));
    with(|s| s.ip = report.lan_ip.clone());
    set(Stage::Dhcp, Step::Ok);

    loop {
        gather(stack, &mut control, report).await;
        PRINT.signal(());
        FETCH.wait().await;
    }
}

// ---------------------------------------------------------------- screen ----

const INK: u16 = rgb(110, 255, 140);
const DIM: u16 = rgb(70, 110, 85);
const WARN: u16 = rgb(255, 176, 0);
const BAD: u16 = rgb(255, 70, 60);
const PAPER_RGB: (u8, u8, u8) = (238, 232, 214);
const DOT_RGB: (u8, u8, u8) = (25, 25, 35);
/// Where the preview starts on the screen.
const PX: i32 = 128;
const PY: i32 = 16;

fn step_tag(s: Step) -> (&'static str, u16) {
    match s {
        Step::Wait => ("...", DIM),
        Step::Run => ("RUN", WARN),
        Step::Ok => ("OK", INK),
        Step::Fail => ("ERR", BAD),
    }
}

fn screen(cv: &mut Canvas, st: &State, scroll: i32) {
    cv.fill(0, 0, lcd::W, 15, rgb(40, 28, 0));
    cv.text(4, 0, "THE DAILY BIT", &FONT_9X15_BOLD, WARN);
    let ed = fmt::<16>(format_args!("ed.{}", st.edition));
    cv.text_r(PX - 4, 3, &ed, &FONT_6X10, DIM);
    let pos = fmt::<24>(format_args!("{}/{} rows", st.sent_rows, st.job_rows));
    cv.text_r(lcd::W - 4, 3, &pos, &FONT_6X10, DIM);

    // The pipeline.
    for (i, name) in STAGES.iter().enumerate() {
        let y = 19 + i as i32 * 11;
        let (tag, col) = step_tag(st.steps[i]);
        let bt = i >= Stage::Scan as usize;
        cv.text(4, y, if bt { "BT" } else if i < 2 { "L2" } else { "NET" }, &FONT_6X10, rgb(40, 80, 60));
        cv.text(28, y, name, &FONT_6X10, DIM);
        cv.text(76, y, tag, &FONT_6X10, col);
    }
    let mut y = 166;
    let mut line = |cv: &mut Canvas, s: &str, c: u16| {
        cv.text(4, y, s, &FONT_6X10, c);
        y += 11;
    };
    line(cv, &fmt::<24>(format_args!("ip {}", st.ip)), INK);
    if !st.printer.is_empty() {
        line(cv, &st.printer, rgb(80, 220, 255));
        let bat = st.battery.map(|b| fmt::<8>(format_args!("{b}%"))).unwrap_or_default();
        line(cv, &fmt::<24>(format_args!("{}dBm mtu{} {}", st.rssi, st.mtu, bat)), DIM);
    } else {
        line(cv, "printer: -", DIM);
        line(cv, "", DIM);
    }
    // Progress through the job.
    cv.fill(4, y + 1, 118, 6, rgb(20, 40, 30));
    if st.job_rows > 0 {
        cv.fill(4, y + 1, 118 * st.sent_rows as i32 / st.job_rows as i32, 6, INK);
    }
    let log = &st.log[..st.log.len().min(20)];
    cv.text(4, y + 11, log, &FONT_6X10, WARN);
    cv.text(4, 228, "A new B again XY ^v", &FONT_6X10, DIM);

    // The receipt at half scale: sent rows dark, the rest faded, the head red.
    let faded = (mix(PAPER_RGB, DOT_RGB, 0.0), mix(PAPER_RGB, DOT_RGB, 0.35));
    PAPER.lock(|p| {
        let p = p.borrow();
        let rows = p.job_rows() as i32;
        let ya = cv.y0.max(PY);
        let yb = cv.y0 + cv.h;
        for sy in ya..yb {
            let py = scroll + (sy - PY) * 2;
            if p.rows == 0 || py >= rows {
                cv.fill(PX, sy, lcd::W - PX, 1, rgb(10, 14, 12));
                continue;
            }
            let sent = !st.printing || py < st.sent_rows as i32;
            for sx in PX..lcd::W {
                let px = (sx - PX) * 2;
                let n = p.dot(px, py) as u8 + p.dot(px + 1, py) as u8 + p.dot(px, py + 1) as u8 + p.dot(px + 1, py + 1) as u8;
                let c = if sent { mix(PAPER_RGB, DOT_RGB, n as f32 / 4.0) } else if n >= 2 { faded.1 } else { faded.0 };
                cv.px(sx, sy, c);
            }
            if st.printing && (py - st.sent_rows as i32).abs() <= 1 {
                cv.fill(PX, sy, lcd::W - PX, 1, rgb(255, 40, 40));
            }
        }
        if p.rows == 0 {
            cv.text(PX + 30, 110, "no receipt yet", &FONT_6X10, DIM);
        }
    });
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let fw = aligned_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/43439A0.bin"));
    let clm = aligned_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/43439A0_clm.bin"));
    let btfw = aligned_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/43439A0_btfw.bin"));
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
    let buttons = [Input::new(p.PIN_12, Pull::Up), Input::new(p.PIN_20, Pull::Up), Input::new(p.PIN_14, Pull::Up), Input::new(p.PIN_15, Pull::Up)];
    static CANVAS: StaticCell<Canvas> = StaticCell::new();
    let cv = CANVAS.init(Canvas::new());
    let st = STATE.lock(|s| s.borrow().clone());
    lcd.flush(cv, 0, 0, lcd::W, lcd::H, &mut |cv| screen(cv, &st, 0));

    // Radio: Wi-Fi and Bluetooth on the one chip.
    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        cyw43_pio::DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
        dma::Channel::new(p.DMA_CH1, Irqs),
    );
    static RADIO: StaticCell<cyw43::State> = StaticCell::new();
    let (net_device, bt_device, mut control, runner) = cyw43::new_with_bluetooth(RADIO.init(cyw43::State::new()), pwr, spi, fw, btfw, nvram).await;
    spawner.spawn(unwrap!(cyw43_task(runner)));
    control.init(clm).await;
    control.set_power_management(cyw43::PowerManagementMode::PowerSave).await;

    static STACK: StaticCell<StackStorage<'static>> = StaticCell::new();
    let (stack, net_runner) = Stack::new(STACK.init(StackStorage::new()), 0xca7_f00d_0b17_0001);
    static DEVICE: StaticCell<cyw43::NetDriver<'static>> = StaticCell::new();
    let iface = unwrap!(stack.add_iface(DEVICE.init(net_device)).ok());
    iface.set_dhcpv4(Some(Default::default()));
    spawner.spawn(unwrap!(net_task(net_runner)));
    spawner.spawn(unwrap!(app_task(control, stack, iface)));
    spawner.spawn(unwrap!(ble_task(ExternalController::new(bt_device))));

    let mut prev = [false; 4];
    let mut scroll = 0i32;
    let mut shown = (u32::MAX, -1);
    let mut last = Instant::now();
    loop {
        let now = buttons.each_ref().map(|b| b.is_low());
        let pressed = |i: usize| now[i] && !prev[i];
        let (printing, rows) = STATE.lock(|s| (s.borrow().printing, s.borrow().job_rows as i32));
        if pressed(0) && !printing {
            log("button A: new edition");
            FETCH.signal(());
        }
        if pressed(1) && !printing && rows > 0 {
            log("button B: print again");
            PRINT.signal(());
        }
        // The preview shows 448 rows.
        let max_scroll = (rows - 448).max(0) / 2 * 2;
        if pressed(2) {
            scroll = (scroll - 96).max(0);
        }
        if pressed(3) {
            scroll = (scroll + 96).min(max_scroll);
        }
        prev = now;

        let st = STATE.lock(|s| s.borrow().clone());
        if st.printing {
            // Follow the paper out.
            scroll = (st.sent_rows as i32 - 360).clamp(0, max_scroll) / 2 * 2;
        }
        // While rows stream to the printer only the progress bar is
        // repainted, and rarely: a full repaint holds the CPU long enough to
        // leave gaps in the stream, and the printer stutters on gaps.
        let sending = st.printing && st.steps[Stage::Send as usize] == Step::Run;
        if sending {
            if st.version != shown.0 && last.elapsed() >= Duration::from_millis(500) {
                lcd.flush(cv, 0, 196, PX, 24, &mut |cv| screen(cv, &st, scroll));
                shown.0 = st.version;
                last = Instant::now();
            }
        } else if (st.version, scroll) != shown {
            lcd.flush(cv, 0, 0, lcd::W, lcd::H, &mut |cv| screen(cv, &st, scroll));
            shown = (st.version, scroll);
            last = Instant::now();
        }
        Timer::after_millis(40).await;
    }
}
