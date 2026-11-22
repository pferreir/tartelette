//! The MXW01 over Bluetooth LE (trouble as the host stack): find it by name,
//! connect, and push the receipt through its three characteristics.
//!
//! * `AE01` takes commands, framed `22 21 cmd 00 len_lo len_hi payload..
//!   crc8(payload) FF` (CRC-8, polynomial 7, over the payload only).
//! * `AE02` notifies replies in the same framing.
//! * `AE03` takes the raster as a byte stream, 48 bytes a row.
use core::{cell::Cell, fmt::Write as _};

use bt_hci::{cmd::le::LeSetScanParams, controller::ControllerCmdSync};
use defmt::{info, warn};
use embassy_futures::{join::join, select::select};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use heapless::{String, Vec};
use trouble_host::prelude::*;

use crate::{PAPER, PRINT, Stage, Step, log, paper::ROW_BYTES, set, with};

const NAME: &[u8] = b"MXW01";
const SERVICE: u16 = 0xAE30;
const CONTROL: u16 = 0xAE01;
const REPLY: u16 = 0xAE02;
const DATA: u16 = 0xAE03;

const GET_STATUS: u8 = 0xA1;
const SET_INTENSITY: u8 = 0xA2;
const PRINT_CMD: u8 = 0xA9;
const PRINT_COMPLETE: u8 = 0xAA;
const FLUSH: u8 = 0xAD;

/// Head energy, 0..=100 (`INTENSITY` at build time). The libraries the
/// protocol comes from send 0x5D (93), which prints light on some units.
const INTENSITY: u8 = match option_env!("INTENSITY") {
    Some(s) => parse_u64(s) as u8,
    None => 100,
};
/// Pause after each row (`ROW_GAP_MS` at build time). None by default:
/// the printer burns rows as they arrive, and every gap in the stream stops
/// the motor and leaves the print patchy, so data goes as fast as the link
/// takes it.
const ROW_GAP_MS: u64 = match option_env!("ROW_GAP_MS") {
    Some(s) => parse_u64(s),
    None => 0,
};

const fn parse_u64(s: &str) -> u64 {
    let b = s.as_bytes();
    let (mut i, mut n) = (0, 0);
    while i < b.len() {
        n = n * 10 + (b[i] - b'0') as u64;
        i += 1;
    }
    n
}

pub fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |crc, &b| {
        let mut c = crc ^ b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 { c << 1 ^ 0x07 } else { c << 1 };
        }
        c
    })
}

fn frame(cmd: u8, payload: &[u8]) -> Vec<u8, 24> {
    let mut f = Vec::new();
    let len = (payload.len() as u16).to_le_bytes();
    let _ = f.extend_from_slice(&[0x22, 0x21, cmd, 0x00, len[0], len[1]]);
    let _ = f.extend_from_slice(payload);
    let _ = f.push(crc8(payload));
    let _ = f.push(0xFF);
    f
}

/// Watches advertising for the printer's name.
struct Finder {
    found: Cell<Option<(Address, i8)>>,
}

impl EventHandler for Finder {
    fn on_adv_reports(&self, mut it: LeAdvReportsIter<'_>) {
        while let Some(Ok(r)) = it.next() {
            if self.found.get().is_none() && has_name(r.data, NAME) {
                self.found.set(Some((
                    Address {
                        kind: r.addr_kind,
                        addr: r.addr,
                    },
                    r.rssi,
                )));
            }
        }
    }
}

/// Does the advertising data carry this (complete or shortened) name?
fn has_name(mut ad: &[u8], name: &[u8]) -> bool {
    while let [len, rest @ ..] = ad {
        let len = *len as usize;
        if len == 0 || rest.len() < len {
            break;
        }
        if (rest[0] == 0x08 || rest[0] == 0x09) && &rest[1..len] == name {
            return true;
        }
        ad = &rest[len..];
    }
    false
}

pub async fn run<C: Controller + ControllerCmdSync<LeSetScanParams>>(controller: C) {
    // A static random address (top bits 11).
    let address = Address::random([0x01, 0xCA, 0x7C, 0x0B, 0x17, 0xC0]);
    let mut resources: HostResources<DefaultPacketPool, 1, 3> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(address)
        .build();
    let mut central = stack.central();
    let mut runner = stack.runner();
    let finder = Finder {
        found: Cell::new(None),
    };

    let _ = join(runner.run_with_handler(&finder), async {
        loop {
            PRINT.wait().await;
            with(|s| {
                for st in [
                    Stage::Scan,
                    Stage::Link,
                    Stage::Gatt,
                    Stage::Send,
                    Stage::Burn,
                ] {
                    s.steps[st as usize] = Step::Wait;
                }
                s.sent_rows = 0;
                s.printing = true;
            });
            if let Err((stage, why)) = job(&stack, &mut central, &finder).await {
                warn!("print failed at {}: {}", stage as u8, why);
                set(stage, Step::Fail);
                log(why);
            }
            with(|s| s.printing = false);
        }
    })
    .await;
}

type Fail = (Stage, &'static str);

async fn job<C: Controller + ControllerCmdSync<LeSetScanParams>>(
    stack: &Stack<'_, C, DefaultPacketPool>,
    central: &mut Central<'_, C, DefaultPacketPool>,
    finder: &Finder,
) -> Result<(), Fail> {
    // Find it.
    set(Stage::Scan, Step::Run);
    log("ble: scanning for MXW01");
    finder.found.set(None);
    let (target, rssi) = {
        let mut scanner = Scanner::new(central);
        let config = ScanConfig {
            active: true,
            interval: Duration::from_millis(100),
            window: Duration::from_millis(100),
            ..Default::default()
        };
        let _session = scanner
            .scan(&config)
            .await
            .map_err(|_| (Stage::Scan, "ble: scan failed"))?;
        let t = Instant::now();
        loop {
            if let Some(f) = finder.found.get() {
                break f;
            }
            if t.elapsed() > Duration::from_secs(20) {
                return Err((Stage::Scan, "ble: no MXW01 in range"));
            }
            Timer::after_millis(50).await;
        }
    };
    let a = target.addr.raw();
    with(|s| {
        s.printer.clear();
        let _ = write!(
            s.printer,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            a[5], a[4], a[3], a[2], a[1], a[0]
        );
        s.rssi = rssi;
    });
    set(Stage::Scan, Step::Ok);

    // Connect.
    set(Stage::Link, Step::Run);
    // A short connection interval, so rows flow steadily instead of in
    // bursts (trouble asks for 80 ms by default).
    let params = RequestedConnParams {
        min_connection_interval: Duration::from_micros(7_500),
        max_connection_interval: Duration::from_millis(15),
        max_latency: 0,
        min_event_length: Duration::from_secs(0),
        max_event_length: Duration::from_millis(15),
        supervision_timeout: Duration::from_secs(4),
    };
    let config = ConnectConfig {
        scan_config: ScanConfig {
            filter_accept_list: &[target],
            ..Default::default()
        },
        connect_params: params,
    };
    let conn = with_timeout(Duration::from_secs(15), central.connect(&config))
        .await
        .map_err(|_| (Stage::Link, "ble: connect timed out"))?
        .map_err(|_| (Stage::Link, "ble: connect failed"))?;
    log("ble: connected");
    set(Stage::Link, Step::Ok);

    set(Stage::Gatt, Step::Run);
    let client = GattClient::<C, DefaultPacketPool, 4>::new(stack, &conn)
        .await
        .map_err(|_| (Stage::Gatt, "gatt: no client"))?;
    let mtu = conn.att_mtu();
    with(|s| s.mtu = mtu);
    let result = select(client.task(), session(&client, mtu)).await;
    conn.disconnect();
    match result {
        embassy_futures::select::Either::First(_) => Err((Stage::Gatt, "gatt: link lost")),
        embassy_futures::select::Either::Second(r) => r,
    }
}

async fn session<C: Controller>(
    client: &GattClient<'_, C, DefaultPacketPool, 4>,
    mtu: u16,
) -> Result<(), Fail> {
    let g = |_| (Stage::Gatt, "gatt: printer service missing");
    let services = client
        .services_by_uuid(&Uuid::new_short(SERVICE))
        .await
        .map_err(g)?;
    let svc = services
        .first()
        .ok_or((Stage::Gatt, "gatt: printer service missing"))?
        .clone();
    let control: Characteristic<[u8]> = client
        .characteristic_by_uuid(&svc, &Uuid::new_short(CONTROL))
        .await
        .map_err(g)?;
    let notify: Characteristic<[u8]> = client
        .characteristic_by_uuid(&svc, &Uuid::new_short(REPLY))
        .await
        .map_err(g)?;
    let data: Characteristic<[u8]> = client
        .characteristic_by_uuid(&svc, &Uuid::new_short(DATA))
        .await
        .map_err(g)?;
    let mut replies = client
        .subscribe(&notify, false)
        .await
        .map_err(|_| (Stage::Gatt, "gatt: subscribe failed"))?;

    ask(
        client,
        &control,
        &mut replies,
        SET_INTENSITY,
        &[INTENSITY],
        None,
    )
    .await
    .map_err(|_| (Stage::Gatt, "mxw01: write failed"))?;
    let status = ask(
        client,
        &control,
        &mut replies,
        GET_STATUS,
        &[0],
        Some(Duration::from_secs(5)),
    )
    .await
    .map_err(|_| (Stage::Gatt, "mxw01: no status"))?;
    if let (Some(&battery), Some(&temp)) = (status.get(3), status.get(4)) {
        with(|s| {
            s.battery = Some(battery);
            s.temp = Some(temp);
        });
    }
    if status.get(6).is_some_and(|&e| e != 0) {
        return Err((Stage::Gatt, "mxw01: printer error"));
    }
    set(Stage::Gatt, Step::Ok);
    let mut line: String<48> = String::new();
    let _ = write!(
        line,
        "mxw01: mtu {mtu}, bat {}%, heat {INTENSITY}",
        status.get(3).copied().unwrap_or(0)
    );
    log(&line);

    // The job.
    set(Stage::Send, Step::Run);
    let rows = PAPER.lock(|p| p.borrow().job_rows());
    with(|s| s.job_rows = rows as u16);
    let n = (rows as u16).to_le_bytes();
    let ack = ask(
        client,
        &control,
        &mut replies,
        PRINT_CMD,
        &[n[0], n[1], 0x30, 0x00],
        Some(Duration::from_secs(5)),
    )
    .await
    .map_err(|_| (Stage::Send, "mxw01: print refused"))?;
    if ack.first() != Some(&0) {
        return Err((Stage::Send, "mxw01: print refused"));
    }
    let chunk = (mtu as usize).saturating_sub(3).clamp(20, ROW_BYTES);
    let t = Instant::now();
    let mut row = [0u8; ROW_BYTES];
    for y in 0..rows {
        PAPER.lock(|p| row.copy_from_slice(p.borrow().row(y)));
        for part in row.chunks(chunk) {
            client
                .write_characteristic_without_response(&data, part)
                .await
                .map_err(|_| (Stage::Send, "ble: write failed"))?;
        }
        if y % 8 == 7 || y + 1 == rows {
            with(|s| s.sent_rows = y as u16 + 1);
        }
        #[allow(clippy::absurd_extreme_comparisons)] // ROW_GAP_MS is 0 unless set at build time
        if ROW_GAP_MS > 0 {
            Timer::after_millis(ROW_GAP_MS).await;
        }
    }
    info!("sent {} rows in {} ms", rows, t.elapsed().as_millis());
    ask(client, &control, &mut replies, FLUSH, &[0], None)
        .await
        .map_err(|_| (Stage::Send, "ble: write failed"))?;
    set(Stage::Send, Step::Ok);

    set(Stage::Burn, Step::Run);
    log("mxw01: burning paper");
    let wait = Duration::from_millis(10_000 + rows as u64 * 30);
    let done = reply(&mut replies, PRINT_COMPLETE, wait).await;
    if done.is_err() {
        return Err((Stage::Burn, "mxw01: no print-complete"));
    }
    set(Stage::Burn, Step::Ok);
    log("mxw01: printed. tear here!");
    Ok(())
}

/// Send a command; with `wait`, return the payload of its reply.
async fn ask<C: Controller, const M: usize>(
    client: &GattClient<'_, C, DefaultPacketPool, 4>,
    control: &Characteristic<[u8]>,
    replies: &mut NotificationListener<'_, M>,
    cmd: u8,
    payload: &[u8],
    wait: Option<Duration>,
) -> Result<Vec<u8, 32>, ()> {
    client
        .write_characteristic_without_response(control, &frame(cmd, payload))
        .await
        .map_err(|_| ())?;
    match wait {
        Some(wait) => reply(replies, cmd, wait).await,
        None => Ok(Vec::new()),
    }
}

/// The payload of the next reply to `cmd`.
async fn reply<const M: usize>(
    replies: &mut NotificationListener<'_, M>,
    cmd: u8,
    wait: Duration,
) -> Result<Vec<u8, 32>, ()> {
    with_timeout(wait, async {
        loop {
            let n = replies.next().await;
            let f = n.as_ref();
            if f.len() >= 6 && f[0] == 0x22 && f[1] == 0x21 && f[2] == cmd {
                let len = (u16::from_le_bytes([f[4], f[5]]) as usize)
                    .min(f.len() - 6)
                    .min(32);
                return Vec::from_slice(&f[6..6 + len]).map_err(|_| ());
            }
        }
    })
    .await
    .map_err(|_| ())?
}
