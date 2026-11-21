//! The Infineon CYW43439 Wi-Fi chip on a Raspberry Pi Pico W, modelled at
//! the pins so unmodified Embassy `cyw43` firmware (with `cyw43-pio`) runs,
//! Wi-Fi and Bluetooth.
//!
//! Wiring (Pico W): `on` is WL_ON (power/reset), `dio` the bidirectional
//! data line that doubles as the host interrupt, `cs` the active-low chip
//! select, `clk` the serial clock. `aps` lists up to eight mock access
//! points; they are what a scan finds and what a join can succeed against.
//!
//! The model has these layers:
//!
//! * [`gspi`]: the SPI-like wire, bit by bit on the clock edges.
//! * [`chip`]: the bus registers, the backplane window and the WLAN
//!   function behind it.
//! * [`backplane`]: chip RAM, chip ID and the core wrappers (so the driver's
//!   firmware download "boots" something).
//! * [`whd`]: the host driver protocol the firmware speaks: SDPCM/CDC/BDC
//!   framing, iovars, events, scan, join and the Ethernet data path.
//! * [`bt`]: the Bluetooth patch download, control registers and HCI ring
//!   buffers.
//!
//! Ethernet frames go to a [`NetBackend`] the host installs with
//! [`Cyw43439::set_backend`]. Without one (the browser) the chip scans no
//! networks and joins fail. HCI packets go to an [`HciBackend`] installed
//! with [`Cyw43439::set_bt_backend`] (a real controller, or the virtual one
//! with mock devices); without one a controller with no radio answers, so
//! Bluetooth scans find nothing.
//!
//! Events: `led` (the on-board LED, CYW43 GPIO0) 1/0; `scan` with the number
//! of networks found; `join` 1 on success, 0 on failure; `link` 1/0 as the
//! station associates or leaves; `unavailable` when a scan or join was
//! attempted with no network behind the chip; `bt_unavailable` when the
//! firmware scans, connects or advertises with no radio behind the chip;
//! from mock Bluetooth devices, `ble_connected` 1/0, `ble_notify` (a value a
//! central received, little-endian in the low bytes) and `ble_midi_out`.
//!
//! Input: `midi=0x903C64` sends a MIDI message from every connected mock
//! MIDI peripheral (the bytes packed big-endian in the value).
pub mod backplane;
pub mod bt;
pub mod chip;
pub mod gspi;
pub mod whd;
pub mod wpa;

use alloc::{boxed::Box, string::String, vec::Vec};

use bt::Bt;
use chip::Chip;
use gspi::Gspi;
use pcb::{Device, Io, Kind, Param, Params};
use whd::Whd;
pub use whd::{Ap, Link};

use crate::{hci::HciBackend, net::NetBackend};

const MAX_APS: usize = 8;

pub struct Cyw43439 {
    on: usize,
    dio: usize,
    cs: usize,
    clk: usize,
    cycles_per_us: u64,
    mac: [u8; 6],
    aps: Vec<Ap>,
    /// Pin levels before the change being processed.
    powered: bool,
    cs_high: bool,
    clk_high: bool,
    /// What the chip last drove on DIO (None = released).
    dio_driven: Option<bool>,
    spi: Gspi,
    chip: Chip,
    /// Wake-up requested for these cycles.
    armed_due: Option<u64>,
    armed_poll: Option<u64>,
    armed_bt: Option<u64>,
}

fn parse_mac(s: &str) -> Result<[u8; 6], String> {
    let bad = || String::from("mac must be six hex bytes like 28:cd:c1:00:00:01");
    let mut mac = [0u8; 6];
    let mut parts = s.split(':');
    for b in &mut mac {
        *b = u8::from_str_radix(parts.next().ok_or_else(bad)?, 16).map_err(|_| bad())?;
    }
    if parts.next().is_some() {
        return Err(bad());
    }
    Ok(mac)
}

fn ap(p: &mut Params, index: usize) -> Result<Ap, String> {
    let ssid = String::from(p.str("ssid"));
    if !(1..=32).contains(&ssid.len()) {
        return Err("ssid must be 1..32 bytes".into());
    }
    let wpa2 = p.str("security") == "wpa2";
    let pass = match (wpa2, p.opt_str("passphrase")) {
        (true, Some(pass)) if (8..=63).contains(&pass.len()) => String::from(pass),
        (true, Some(_)) => return Err("passphrase must be 8..63 characters".into()),
        (true, None) => return Err("a wpa2 network needs a passphrase".into()),
        (false, Some(_)) => return Err("an open network has no passphrase".into()),
        (false, None) => String::new(),
    };
    let (rssi, chan) = (p.int("rssi") as i16, p.int("channel") as u8);
    Ok(Ap {
        ssid,
        wpa2,
        pass,
        rssi,
        chan,
        bssid: [0x02, 0x1A, 0x2B, 0x00, 0x00, index as u8 + 1],
    })
}

pub const KIND: Kind = Kind {
    name: "cyw43439",
    summary: "Pico W Wi-Fi/Bluetooth chip on a bit-banged gSPI link",
    bus: None,
    params: &[
        Param::pin("on", "WL_ON: power and reset"),
        Param::pin("dio", "data in/out, also the host interrupt"),
        Param::pin("cs", "chip select, active low"),
        Param::pin("clk", "serial clock"),
        Param::str("mac", "MAC address, six hex bytes like 28:cd:c1:00:00:01")
            .default_str("28:cd:c1:00:00:01"),
        Param::int("mhz", 1, 1000, "core clock, to turn time into cycles").default_int(125),
        Param::tables(
            "aps",
            &[
                Param::str("ssid", "network name, 1..32 bytes"),
                Param::choice("security", &["open", "wpa2"], "how a station joins")
                    .default_str("open"),
                Param::str("passphrase", "WPA2 passphrase, 8..63 characters").optional(),
                Param::int("rssi", -120, 0, "signal strength in dBm").default_int(-50),
                Param::int("channel", 1, 14, "2.4 GHz channel").default_int(6),
            ],
            "the mock access points a scan finds (up to 8)",
        )
        .optional(),
    ],
    build: |p| {
        let (on, dio, cs, clk) = (p.pin("on"), p.pin("dio"), p.pin("cs"), p.pin("clk"));
        let mac = parse_mac(p.str("mac"))?;
        let cycles_per_us = p.int("mhz") as u64;
        let tables = p.tables("aps");
        if tables.len() > MAX_APS {
            return Err(alloc::format!("at most {MAX_APS} access points"));
        }
        let mut aps: Vec<Ap> = Vec::new();
        for (i, mut t) in tables.into_iter().enumerate() {
            let ap = ap(&mut t, i).map_err(|e| alloc::format!("aps[{i}]: {e}"))?;
            if aps.iter().any(|a| a.ssid == ap.ssid) {
                return Err(alloc::format!(
                    "aps[{i}]: SSID {:?} is listed twice",
                    ap.ssid
                ));
            }
            aps.push(ap);
        }
        Ok(Box::new(Cyw43439::new(
            on,
            dio,
            cs,
            clk,
            mac,
            aps,
            cycles_per_us,
        )))
    },
};

impl Cyw43439 {
    pub fn new(
        on: usize,
        dio: usize,
        cs: usize,
        clk: usize,
        mac: [u8; 6],
        aps: Vec<Ap>,
        cycles_per_us: u64,
    ) -> Self {
        let whd = Whd::new(mac, aps.clone(), cycles_per_us);
        Self {
            on,
            dio,
            cs,
            clk,
            cycles_per_us,
            mac,
            aps,
            powered: false,
            cs_high: true,
            clk_high: false,
            dio_driven: None,
            spi: Gspi::new(),
            chip: Chip::new(
                whd,
                Bt::new(Bt::no_radio(mac), cycles_per_us),
                cycles_per_us,
            ),
            armed_due: None,
            armed_poll: None,
            armed_bt: None,
        }
    }

    /// Install the network the chip's Ethernet side talks to.
    pub fn set_backend(&mut self, backend: Box<dyn NetBackend>) {
        self.chip.whd.backend = Some(backend);
    }

    /// Install the Bluetooth controller behind the chip's HCI transport.
    pub fn set_bt_backend(&mut self, backend: Box<dyn HciBackend>) {
        self.chip.bt.backend = backend;
    }

    pub fn bt_backend(&self) -> &dyn HciBackend {
        self.chip.bt.backend.as_ref()
    }

    /// The firmware brought Bluetooth up and is talking HCI.
    pub fn bt_up(&self) -> bool {
        self.chip.bt.up
    }

    /// What the radio is doing, as the HCI traffic says: (scanning,
    /// advertising, open connections).
    pub fn bt_activity(&self) -> (bool, bool, u16) {
        let bt = &self.chip.bt;
        (bt.scanning, bt.advertising, bt.links)
    }

    pub fn backend(&self) -> Option<&dyn NetBackend> {
        self.chip.whd.backend.as_deref()
    }

    /// The on-board LED (CYW43 GPIO0).
    pub fn led(&self) -> bool {
        self.chip.whd.gpio() & 1 != 0
    }

    pub fn link(&self) -> Link {
        self.chip.whd.link
    }

    /// SSID being joined or joined.
    pub fn ssid(&self) -> Option<&str> {
        self.chip.whd.joined_ssid.as_deref()
    }

    pub fn aps(&self) -> &[Ap] {
        &self.aps
    }

    /// The address the guest was leased by the network, once it has one.
    pub fn lease(&self) -> Option<[u8; 4]> {
        self.backend().and_then(|b| b.lease())
    }

    /// Whether a network backend that works is installed.
    pub fn network_available(&self) -> bool {
        self.backend().is_some_and(|b| b.available())
    }

    /// iovars and ioctls the firmware used that the model ignores.
    pub fn unknown_requests(&self) -> &[String] {
        &self.chip.whd.unknown
    }

    /// Whether the firmware has been downloaded and started.
    pub fn firmware_running(&self) -> bool {
        self.chip.f2_ready()
    }

    /// The most recent bus transactions the host made, oldest first.
    pub fn transactions(&self) -> impl Iterator<Item = &chip::Transaction> {
        self.chip.trace.iter()
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn power(&mut self, io: &Io, on: bool) {
        self.powered = on;
        // Power-cycling the chip wipes it. The network stays plugged in.
        let backend = self.chip.whd.backend.take();
        let mut whd = Whd::new(self.mac, self.aps.clone(), self.cycles_per_us);
        if let Some(mut b) = backend {
            b.link_changed(false);
            whd.backend = Some(b);
        }
        let radio = core::mem::replace(&mut self.chip.bt.backend, Bt::no_radio(self.mac));
        self.chip = Chip::new(whd, Bt::new(radio, self.cycles_per_us), self.cycles_per_us);
        self.spi = Gspi::new();
        self.armed_due = None;
        self.armed_poll = None;
        self.armed_bt = None;
        if on {
            self.chip.power_on_at = Some(io.cycles());
        }
    }

    /// Settle everything that follows from a change: report happenings,
    /// drive the interrupt line, schedule wake-ups.
    fn settle(&mut self, io: &mut Io) {
        for (label, value) in self
            .chip
            .whd
            .notes
            .drain(..)
            .chain(self.chip.bt.notes.drain(..))
        {
            io.emit(label, value);
        }
        if !self.powered {
            if self.dio_driven.take().is_some() {
                io.drive(self.dio, None);
            }
        } else if self.cs_high {
            let want = Some(self.chip.irq_pending());
            if self.dio_driven != want {
                self.dio_driven = want;
                io.drive(self.dio, want);
            }
        }
        if let Some(t) = self.chip.whd.next_due()
            && self.armed_due != Some(t)
        {
            self.armed_due = Some(t);
            io.wake_at(t);
        }
        if self.chip.whd.link != Link::Down
            && self.armed_poll.is_none()
            && self.chip.whd.backend.is_some()
        {
            let t = io.cycles() + 1000 * self.cycles_per_us;
            self.armed_poll = Some(t);
            io.wake_at(t);
        }
        // Bluetooth: look for controller traffic every millisecond.
        if self.chip.bt.up && self.armed_bt.is_none() {
            let t = io.cycles() + 1000 * self.cycles_per_us;
            self.armed_bt = Some(t);
            io.wake_at(t);
        }
    }
}

impl Device for Cyw43439 {
    fn watched_pins(&self) -> u32 {
        1 << self.on | 1 << self.cs | 1 << self.clk
    }

    fn attach(&mut self, io: &mut Io) {
        self.cs_high = io.level(self.cs);
        self.clk_high = io.level(self.clk);
        if io.level(self.on) {
            self.power(io, true);
        }
    }

    fn pin_changed(&mut self, io: &mut Io, changed: u32) {
        self.chip.now = io.cycles();
        if changed >> self.on & 1 != 0 {
            let on = io.level(self.on);
            if on != self.powered {
                self.power(io, on);
            }
        }
        if changed >> self.cs & 1 != 0 {
            let cs = io.level(self.cs);
            if cs != self.cs_high {
                self.cs_high = cs;
                if cs {
                    self.spi.cs_rise();
                } else {
                    self.spi.cs_fall();
                    // The host drives DIO from here until it turns the line round.
                    if self.dio_driven.take().is_some() {
                        io.drive(self.dio, None);
                    }
                }
            }
        }
        if changed >> self.clk & 1 != 0 {
            let clk = io.level(self.clk);
            if clk != self.clk_high {
                self.clk_high = clk;
                if self.powered && !self.cs_high {
                    if clk {
                        let bit = io.level(self.dio);
                        self.spi.clk_rise(bit, &mut self.chip);
                    } else if let Some(bit) = self.spi.clk_fall()
                        && self.dio_driven != Some(bit)
                    {
                        self.dio_driven = Some(bit);
                        io.drive(self.dio, Some(bit));
                    }
                }
            }
        }
        self.settle(io);
    }

    fn input(&mut self, io: &mut Io, action: &str, value: i64) -> bool {
        let now = io.cycles();
        let ok = self
            .chip
            .bt
            .backend
            .input(now / self.cycles_per_us, action, value);
        if ok {
            self.chip.now = now;
            self.chip.bt.pump(now, &mut self.chip.bp);
            self.settle(io);
        }
        ok
    }

    fn timer(&mut self, io: &mut Io) {
        self.chip.now = io.cycles();
        let now = io.cycles();
        if self.armed_due.is_some_and(|t| t <= now) {
            self.armed_due = None;
        }
        self.chip.whd.tick(now);
        if self.armed_poll.is_some_and(|t| t <= now) {
            self.armed_poll = None;
            self.chip.whd.poll_backend();
        }
        if self.armed_bt.is_some_and(|t| t <= now) {
            self.armed_bt = None;
            self.chip.bt.pump(now, &mut self.chip.bp);
        }
        self.settle(io);
    }
}

#[cfg(test)]
mod tests;
