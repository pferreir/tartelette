//! The chip against a host that bit-bangs the gSPI protocol the way
//! Embassy's `cyw43-pio` program does, and talks the WHD protocol the way
//! `cyw43`'s runner and control do (or, with [`Host::big_endian`], with the
//! bus in big-endian mode as pico-sdk's driver sets it).
use alloc::{format, vec};

use pcb::{Board, Pins};

use super::{chip::Framing, *};
use crate::{
    net::{self, LoopbackBackend},
    testutil::FakePins,
};

const ON: usize = 23;
const DIO: usize = 24;
const CS: usize = 25;
const CLK: usize = 29;

const BOARD: &str = r#"
[parts.wifi]
kind = "cyw43439"
on = 23
dio = 24
cs = 25
clk = 29
aps = [
  { ssid = "TarteletteNet", security = "wpa2", passphrase = "hunter22", rssi = -45, channel = 6 },
  { ssid = "FreeCafe", rssi = -70, channel = 11 },
]
"#;

fn cmd_word(write: bool, func: u32, addr: u32, len: usize) -> u32 {
    (write as u32) << 31 | 1 << 30 | func << 28 | (addr & 0x1FFFF) << 11 | (len as u32 & 0x7FF)
}

#[derive(Debug, Clone)]
struct Event {
    kind: u32,
    status: u32,
    flags: u16,
    data: Vec<u8>,
}

struct Host {
    board: Board,
    pins: FakePins,
    now: u64,
    framing: Framing,
    /// Ask for big-endian words when setting up the bus.
    big_endian: bool,
    bp_pad: usize,
    status: u32,
    window: u32,
    seq: u8,
    seq_max: u8,
    ioctl_id: u16,
    events: Vec<Event>,
    data_frames: Vec<Vec<u8>>,
    /// Response to the ioctl being waited for.
    ioctl_resp: Option<Vec<u8>>,
    led_events: Vec<i64>,
    /// Every other board event, in order.
    notes: Vec<(&'static str, i64)>,
}

impl Host {
    fn new(board: &str) -> Self {
        let mut board = Board::parse_with(board, &crate::registry()).unwrap();
        let mut pins = FakePins::default();
        pins.set_chip(CS, true);
        pins.set_chip(CLK, false);
        pins.set_chip(ON, false);
        board.attach(&mut pins);
        Host {
            board,
            pins,
            now: 0,
            framing: Framing::Swapped16,
            big_endian: false,
            bp_pad: 0,
            status: 0,
            window: 0xAAAA_AAAA,
            seq: 0,
            seq_max: 1,
            ioctl_id: 0,
            events: Vec::new(),
            data_frames: Vec::new(),
            ioctl_resp: None,
            led_events: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn chip(&self) -> &Cyw43439 {
        self.board.device::<Cyw43439>("wifi").unwrap()
    }

    fn chip_mut(&mut self) -> &mut Cyw43439 {
        self.board.device_mut::<Cyw43439>("wifi").unwrap()
    }

    /// Change pins the way the host side does, one service call per cycle.
    fn set(&mut self, levels: &[(usize, Option<bool>)]) {
        for &(p, l) in levels {
            match l {
                Some(l) => self.pins.set_chip(p, l),
                None => self.pins.release_chip(p),
            }
        }
        let changed = self.pins.take_changes();
        self.board.service(&mut self.pins, changed, self.now);
        self.now += 1;
    }

    fn drain_events(&mut self) {
        for e in std::mem::take(&mut self.board.events) {
            if e.label == "led" {
                self.led_events.push(e.value);
            } else {
                self.notes.push((e.label, e.value));
            }
        }
    }

    /// Let `cycles` of time pass, running device timers.
    fn advance(&mut self, cycles: u64) {
        let end = self.now + cycles;
        while let Some(t) = self.board.next_timer().filter(|&t| t <= end) {
            self.now = self.now.max(t);
            self.board.fire_timers(&mut self.pins, t);
        }
        self.now = end;
        self.drain_events();
    }

    /// One transaction exactly as the PIO program shifts it.
    fn xfer(&mut self, out: &[u32], read_words: usize) -> Vec<u32> {
        self.set(&[(CS, Some(false))]);
        for w in out {
            for i in (0..32).rev() {
                self.set(&[(DIO, Some(w >> i & 1 != 0)), (CLK, Some(false))]);
                self.set(&[(CLK, Some(true))]);
            }
        }
        // set pindirs 0 (CLK low), nop
        self.set(&[(CLK, Some(false)), (DIO, None)]);
        self.set(&[]);
        let mut words = Vec::new();
        for _ in 0..read_words {
            let mut w = 0u32;
            for _ in 0..32 {
                w = w << 1 | self.pins.level(DIO) as u32; // sampled by the instruction that raises CLK
                self.set(&[(CLK, Some(true))]);
                self.set(&[(CLK, Some(false))]);
            }
            words.push(w);
        }
        self.set(&[(CS, Some(true))]);
        self.drain_events();
        words
    }

    fn read(&mut self, func: u32, addr: u32, len: usize) -> Vec<u8> {
        let cmd = self.framing.wire(cmd_word(false, func, addr, len));
        let pad = if func == 1 { self.bp_pad } else { 0 };
        let n = len.div_ceil(4);
        let words = self.xfer(&[cmd], pad + n + 1);
        let w = |x: u32| self.framing.wire(x);
        self.status = w(words[pad + n]);
        let mut bytes: Vec<u8> = words[pad..pad + n]
            .iter()
            .flat_map(|&x| w(x).to_le_bytes())
            .collect();
        bytes.truncate(len);
        bytes
    }

    fn write(&mut self, func: u32, addr: u32, data: &[u8]) {
        let mut words = vec![cmd_word(true, func, addr, data.len())];
        words.extend(data.chunks(4).map(|c| {
            let mut b = [0u8; 4];
            b[..c.len()].copy_from_slice(c);
            u32::from_le_bytes(b)
        }));
        for w in &mut words {
            *w = self.framing.wire(*w);
        }
        let st = self.xfer(&words, 1)[0];
        self.status = self.framing.wire(st);
    }

    fn read32(&mut self, func: u32, addr: u32) -> u32 {
        u32::from_le_bytes(self.read(func, addr, 4).try_into().unwrap())
    }

    fn read16(&mut self, func: u32, addr: u32) -> u16 {
        u16::from_le_bytes(self.read(func, addr, 2).try_into().unwrap())
    }

    fn read8(&mut self, func: u32, addr: u32) -> u8 {
        self.read(func, addr, 1)[0]
    }

    fn write8(&mut self, func: u32, addr: u32, v: u8) {
        self.write(func, addr, &[v]);
    }

    fn write16(&mut self, func: u32, addr: u32, v: u16) {
        self.write(func, addr, &v.to_le_bytes());
    }

    fn write32(&mut self, func: u32, addr: u32, v: u32) {
        self.write(func, addr, &v.to_le_bytes());
    }

    fn set_window(&mut self, addr: u32) {
        let new = addr & !0x7FFF;
        for (shift, reg) in [(24, 0x1000C), (16, 0x1000B), (8, 0x1000A)] {
            if (new >> shift) as u8 != (self.window >> shift) as u8 {
                self.write8(1, reg, (new >> shift) as u8);
            }
        }
        self.window = new;
    }

    fn bp_write(&mut self, addr: u32, data: &[u8]) {
        let mut addr = addr;
        for chunk in data.chunks(64) {
            self.set_window(addr);
            self.write(1, addr & 0x7FFF, chunk);
            addr += chunk.len() as u32;
        }
    }

    fn bp_read(&mut self, addr: u32, len: usize) -> Vec<u8> {
        self.set_window(addr);
        self.read(1, addr & 0x7FFF, len)
    }

    fn bp_write32(&mut self, addr: u32, v: u32) {
        self.set_window(addr);
        self.write32(1, addr & 0x7FFF | 0x8000, v);
    }

    fn bp_read8(&mut self, addr: u32) -> u8 {
        self.set_window(addr);
        self.read8(1, addr & 0x7FFF)
    }

    fn bp_write8(&mut self, addr: u32, v: u8) {
        self.set_window(addr);
        self.write8(1, addr & 0x7FFF, v);
    }

    fn core_reset(&mut self, base: u32) {
        // core_disable
        if self.bp_read8(base + 0x800) & 1 == 0 {
            self.bp_write8(base + 0x408, 0);
            self.advance(125_000);
            self.bp_write8(base + 0x800, 1);
        }
        self.bp_write8(base + 0x408, 3);
        self.bp_write8(base + 0x800, 0);
        self.advance(125_000);
        self.bp_write8(base + 0x408, 1);
        self.advance(125_000);
    }

    /// `Bus::init` and `Runner::init`, with a small stand-in for the firmware.
    fn bring_up(&mut self) {
        self.set(&[(ON, Some(false))]);
        self.advance(20 * 125_000);
        self.set(&[(ON, Some(true))]);
        self.advance(250 * 125_000);

        assert_eq!(self.read32(0, 0x14), 0xFEED_BEAD);
        self.write32(0, 0x18, 0x1234_5678);
        assert_eq!(self.read32(0, 0x18), 0x1234_5678);
        // 32-bit words, high speed, IRQ high, wake up, 4 byte response delay, status enable
        let big = (self.big_endian as u32) << 1;
        let ctrl = 0x1 | big | 0x10 | 0x20 | 0x80 | 0x4 << 8 | 0x3 << 16;
        self.write32(0, 0, ctrl);
        self.framing = if self.big_endian {
            Framing::BigEndian
        } else {
            Framing::Word32
        };
        assert_eq!(self.read8(0, 0), 0xB1 | big as u8);
        assert_eq!(self.read32(0, 0x14), 0xFEED_BEAD);
        assert_eq!(self.read32(0, 0x18), 0x1234_5678);
        self.write8(0, 0x1D, 4);
        self.bp_pad = 1;
        self.write8(0, 4, 0xFF); // clear error interrupts
        self.write16(0, 6, 0x0004 | 0x0008 | 0x0010 | 0x0020 | 0x0080 | 0x0002);

        self.write8(1, 0x1000E, 0x08);
        self.write8(1, 0x10008, 0x10);
        assert_eq!(self.read8(1, 0x10008), 0x10);
        assert_ne!(self.read8(1, 0x1000E) & 0x40, 0);
        self.write8(1, 0x1000E, 0);
        assert_eq!(self.bp_read(0x1800_0000, 2), 43439u16.to_le_bytes());

        // Upload "firmware" and NVRAM, then start the core.
        self.core_reset(0x1810_4000); // SOCSRAM
        self.bp_write32(0x1800_4000 + 0x10, 3);
        self.bp_write32(0x1800_4000 + 0x44, 0);
        let fw: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        self.bp_write(0, &fw);
        assert_eq!(self.bp_read(0, 1000), fw);
        let nvram = b"NVRAMRev=$Rev$\0macaddr=00:A0:50:b5:59:5e\0\0\0";
        let len = nvram.len().div_ceil(4) * 4;
        self.bp_write(0x80000 - 4 - len as u32, nvram);
        let words = len as u32 / 4;
        self.bp_write32(0x80000 - 4, !words << 16 | words);
        self.core_reset(0x1810_3000); // the ARM core
        assert_eq!(self.bp_read8(0x1810_3000 + 0x408) & 3, 1);
        assert_eq!(self.bp_read8(0x1810_3000 + 0x800) & 1, 0);
        // HT clock comes up a little later.
        for _ in 0..1000 {
            if self.read8(1, 0x1000E) & 0x80 != 0 {
                break;
            }
            self.advance(10_000);
        }
        assert_ne!(self.read8(1, 0x1000E) & 0x80, 0);
        self.bp_write32(0x1800_2000 + 0x24, 0xF0);
        self.write16(0, 6, 0x0020);
        self.write8(1, 0x10008, 0x20);
        for _ in 0..100 {
            if self.read32(0, 8) & 0x20 != 0 {
                break;
            }
            self.advance(10_000);
        }
        assert_ne!(self.read32(0, 8) & 0x20, 0, "F2 never became ready");
        self.write8(1, 0x1000F, 0);
        self.write8(1, 0x1000E, 0x10);
        assert_ne!(self.read8(1, 0x1000E) & 0x80, 0);
    }

    // ---- WHD ----

    fn credit(&self) -> bool {
        self.seq != self.seq_max && self.seq_max.wrapping_sub(self.seq) & 0x80 == 0
    }

    fn send_frame(&mut self, channel: u8, header_len: usize, payload: &[u8]) {
        assert!(self.credit(), "host has no credit");
        let total = header_len + payload.len();
        let mut f = Vec::new();
        f.extend_from_slice(&(total as u16).to_le_bytes());
        f.extend_from_slice(&(!total as u16).to_le_bytes());
        f.push(self.seq);
        self.seq = self.seq.wrapping_add(1);
        f.push(channel);
        f.push(0);
        f.push(header_len as u8);
        f.extend_from_slice(&[0, 0, 0, 0]);
        f.resize(header_len, 0);
        f.extend_from_slice(payload);
        f.resize(f.len().div_ceil(4) * 4, 0);
        self.write(2, 0, &f);
    }

    /// Receive and dispatch frames while the status register says there are any.
    fn pump(&mut self) {
        while self.status & 0x100 != 0 {
            let len = (self.status >> 9 & 0x7FF) as usize;
            let frame = self.read(2, 0, len);
            self.rx(&frame);
        }
    }

    fn rx(&mut self, f: &[u8]) {
        assert_eq!(u16::from_le_bytes([f[0], f[1]]) as usize, f.len());
        assert_eq!(
            u16::from_le_bytes([f[0], f[1]]),
            !u16::from_le_bytes([f[2], f[3]])
        );
        let ch = f[5] & 0xF;
        let credit = f[9];
        if ch < 3 {
            self.seq_max = if credit.wrapping_sub(self.seq) > 0x40 {
                self.seq + 2
            } else {
                credit
            };
        }
        let p = &f[f[7] as usize..];
        match ch {
            // A frame with no payload only carries credit.
            0 if p.is_empty() => {}
            0 => {
                // pico-sdk's driver reads the CDC header as words.
                assert_eq!(f[7] % 4, 0, "ioctl response alignment");
                let len = u32::from_le_bytes(p[4..8].try_into().unwrap()) as usize;
                let id = u16::from_le_bytes([p[10], p[11]]);
                assert_eq!(
                    u32::from_le_bytes(p[12..16].try_into().unwrap()),
                    0,
                    "ioctl status"
                );
                if id == self.ioctl_id {
                    self.ioctl_resp = Some(p[16..16 + len].to_vec());
                }
            }
            1 => {
                let eth_at = f[7] as usize + 4 + 4 * p[3] as usize;
                // pico-sdk's driver needs the event message at 2 mod 4.
                assert_eq!((eth_at + 24) % 4, 2, "event message alignment");
                let bdc = &p[4 + 4 * p[3] as usize..];
                assert_eq!(&bdc[12..14], &[0x88, 0x6C]);
                assert_eq!(&bdc[14..16], &[0x80, 0x01]);
                assert_eq!(&bdc[19..22], &[0x00, 0x10, 0x18]);
                let msg = &bdc[24..];
                let be32 = |at: usize| u32::from_be_bytes(msg[at..at + 4].try_into().unwrap());
                let datalen = be32(20) as usize;
                self.events.push(Event {
                    kind: be32(4),
                    status: be32(8),
                    flags: u16::from_be_bytes([msg[2], msg[3]]),
                    data: msg[48..48 + datalen].to_vec(),
                });
            }
            2 => self.data_frames.push(p[4 + 4 * p[3] as usize..].to_vec()),
            _ => {}
        }
    }

    /// Run until DIO goes high (the chip wants attention) or time runs out.
    fn wait_irq(&mut self, max: u64) -> bool {
        let mut waited = 0;
        while waited < max {
            if self.pins.level(DIO) {
                return true;
            }
            self.advance(2_000);
            waited += 2_000;
        }
        self.pins.level(DIO)
    }

    /// Service the chip's interrupt: read the interrupt register, pull frames.
    fn service_irq(&mut self) {
        let irq = self.read16(0, 4);
        if irq & 0x20 != 0 {
            self.pump();
        }
    }

    fn ioctl(&mut self, set: bool, cmd: u32, data: &[u8]) -> Vec<u8> {
        self.ioctl_id += 1;
        self.ioctl_resp = None;
        let mut p = Vec::new();
        p.extend_from_slice(&cmd.to_le_bytes());
        p.extend_from_slice(&(data.len() as u32).to_le_bytes());
        p.extend_from_slice(&(if set { 2u16 } else { 0 }).to_le_bytes());
        p.extend_from_slice(&self.ioctl_id.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(data);
        self.send_frame(0, 12, &p);
        assert!(
            self.wait_irq(125_000 * 100),
            "no interrupt after ioctl {cmd}"
        );
        self.service_irq();
        self.ioctl_resp
            .take()
            .unwrap_or_else(|| panic!("no response to ioctl {cmd}"))
    }

    fn set_var(&mut self, name: &str, val: &[u8]) {
        let mut d = Vec::from(name.as_bytes());
        d.push(0);
        d.extend_from_slice(val);
        self.ioctl(true, 263, &d);
    }

    fn get_var(&mut self, name: &str, want: usize) -> Vec<u8> {
        let mut d = Vec::from(name.as_bytes());
        d.push(0);
        d.resize(d.len().max(want), 0);
        let mut r = self.ioctl(false, 262, &d);
        r.truncate(want);
        r
    }

    /// Wait up to `ms` for events to arrive.
    fn collect_events(&mut self, ms: u64, until: impl Fn(&Host) -> bool) {
        for _ in 0..ms {
            if until(self) {
                return;
            }
            self.advance(125_000);
            if self.pins.level(DIO) {
                self.service_irq();
            }
        }
    }

    fn init_control(&mut self) {
        for (i, chunk) in [&[1u8; 64][..], &[2u8; 10][..]].iter().enumerate() {
            let flag: u16 = 0x1000 | if i == 0 { 2 } else { 4 };
            let mut b = Vec::from(&b"clmload\0"[..]);
            b.extend_from_slice(&flag.to_le_bytes());
            b.extend_from_slice(&2u16.to_le_bytes());
            b.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(chunk);
            self.ioctl(true, 263, &b);
        }
        assert_eq!(self.get_var("clmload_status", 4)[..4], [0; 4]);
        self.set_var("bus:txglom", &0u32.to_le_bytes());
        self.set_var("apsta", &1u32.to_le_bytes());
        self.set_var("country", b"XX\0\0XX\0\0\xff\xff\xff\xff");
        self.set_var("bsscfg:event_msgs", &[0xFF; 28]);
        self.ioctl(true, 2, &[]); // UP
    }

    fn escan(&mut self, ssid: Option<&str>) -> Vec<Event> {
        self.events.clear();
        let mut p = Vec::new();
        p.extend_from_slice(&1u32.to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes());
        let s = ssid.unwrap_or("");
        p.extend_from_slice(&(s.len() as u32).to_le_bytes());
        let mut name = [0u8; 32];
        name[..s.len()].copy_from_slice(s.as_bytes());
        p.extend_from_slice(&name);
        p.extend_from_slice(&[0xFF; 6]);
        p.extend_from_slice(&[2, 1]);
        p.extend_from_slice(&[0xFF; 4 * 5]);
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(&[0; 2]);
        self.set_var("escan", &p);
        self.collect_events(200, |h| {
            h.events.iter().any(|e| e.kind == 69 && e.status == 0)
        });
        std::mem::take(&mut self.events)
    }

    fn join(&mut self, ssid: &str, pass: Option<&str>) -> Vec<Event> {
        self.events.clear();
        match pass {
            None => {
                self.ioctl(true, 134, &0u32.to_le_bytes());
                self.set_var("bsscfg:sup_wpa", &[0; 8]);
            }
            Some(pass) => {
                self.ioctl(true, 134, &4u32.to_le_bytes());
                self.set_var("bsscfg:sup_wpa", &[0, 0, 0, 0, 1, 0, 0, 0]);
                let mut pfi = Vec::new();
                pfi.extend_from_slice(&(pass.len() as u16).to_le_bytes());
                pfi.extend_from_slice(&1u16.to_le_bytes());
                let mut k = [0u8; 64];
                k[..pass.len()].copy_from_slice(pass.as_bytes());
                pfi.extend_from_slice(&k);
                self.ioctl(true, 268, &pfi);
                self.ioctl(true, 165, &0x80u32.to_le_bytes());
            }
        }
        self.ioctl(true, 20, &1u32.to_le_bytes());
        self.ioctl(true, 22, &0u32.to_le_bytes());
        let mut i = Vec::new();
        i.extend_from_slice(&(ssid.len() as u32).to_le_bytes());
        let mut n = [0u8; 32];
        n[..ssid.len()].copy_from_slice(ssid.as_bytes());
        i.extend_from_slice(&n);
        self.ioctl(true, 26, &i);
        self.collect_events(1000, |h| h.events.iter().any(|e| e.kind == 0));
        std::mem::take(&mut self.events)
    }
}

fn booted(board: &str) -> Host {
    let mut h = Host::new(board);
    h.bring_up();
    h.init_control();
    h
}

#[test]
fn bring_up_reaches_f2_ready() {
    let h = {
        let mut h = Host::new(BOARD);
        h.bring_up();
        h
    };
    assert!(h.chip().firmware_running());
    // The firmware's shared-memory descriptor is where the driver looks for it.
    let ram = h.chip().chip.bp.ram();
    assert_ne!(
        u32::from_le_bytes(ram[0x6FFFC..0x70000].try_into().unwrap()),
        0
    );
}

#[test]
fn big_endian_bus_reaches_f2_ready() {
    let mut h = Host::new(BOARD);
    h.big_endian = true;
    h.bring_up();
    assert!(h.chip().firmware_running());
    // Keep-SDIO-on reads back as on, which pico-sdk's driver waits for.
    h.write8(1, 0x1001F, 1);
    assert_eq!(h.read8(1, 0x1001F), 0x03);
}

#[test]
fn chip_does_not_answer_until_powered_and_settled() {
    let mut h = Host::new(BOARD);
    assert_eq!(h.read32(0, 0x14), 0, "unpowered chip answered");
    h.set(&[(ON, Some(true))]);
    assert_eq!(h.read32(0, 0x14), 0, "chip answered before it settled");
    h.advance(30 * 125_000);
    assert_eq!(h.read32(0, 0x14), 0xFEED_BEAD);
    // Power-cycling wipes the bus configuration.
    h.set(&[(ON, Some(false))]);
    h.set(&[(ON, Some(true))]);
    h.advance(30 * 125_000);
    assert_eq!(h.read32(0, 0x14), 0xFEED_BEAD);
}

#[test]
fn control_init_and_led() {
    let mut h = booted(BOARD);
    let mac = h.get_var("cur_etheraddr", 6);
    assert_eq!(mac, [0x28, 0xCD, 0xC1, 0, 0, 1]);
    h.set_var("gpioout", &[1, 0, 0, 0, 1, 0, 0, 0]);
    h.set_var("gpioout", &[1, 0, 0, 0, 0, 0, 0, 0]);
    h.set_var("gpioout", &[1, 0, 0, 0, 1, 0, 0, 0]);
    assert_eq!(h.led_events, [1, 0, 1]);
    assert!(h.chip().led());
    assert!(
        h.chip().unknown_requests().is_empty(),
        "{:?}",
        h.chip().unknown_requests()
    );
}

#[test]
fn credits_keep_the_host_sending() {
    let mut h = booted(BOARD);
    for _ in 0..40 {
        h.get_var("cur_etheraddr", 6);
    }
}

#[test]
fn a_host_out_of_credit_gets_more_without_asking() {
    // Data frames get no answer (the link is down, so they go nowhere); once
    // the host spends its last credit the chip sends an empty frame with more,
    // so a driver that only reads on the interrupt doesn't stall.
    let mut h = booted(BOARD);
    let eth = [0u8; 4 + 14];
    while h.credit() {
        h.send_frame(2, 14, &eth);
    }
    assert!(h.wait_irq(10_000), "the chip asks for attention");
    h.service_irq();
    assert!(h.credit(), "credit came back");
}

#[test]
fn scan_lists_the_configured_networks() {
    let mut h = booted(BOARD);
    h.chip_mut().set_backend(LoopbackBackend::boxed());
    let ev = h.escan(None);
    let found: Vec<_> = ev.iter().filter(|e| e.status == 8).collect();
    assert_eq!(found.len(), 2);
    let info = |e: &Event| {
        let b = &e.data[12..];
        let n = b[18] as usize;
        let ssid = String::from(core::str::from_utf8(&b[19..19 + n]).unwrap());
        let rssi = i16::from_le_bytes([b[78], b[79]]);
        let chanspec = u16::from_le_bytes([b[72], b[73]]);
        let cap = u16::from_le_bytes([b[16], b[17]]);
        (
            ssid,
            rssi,
            chanspec & 0xFF,
            cap & 0x10 != 0,
            b[8..14].to_vec(),
        )
    };
    assert_eq!(info(found[0]).0, "TarteletteNet");
    assert_eq!(info(found[0]).1, -45);
    assert_eq!(info(found[0]).2, 6);
    assert!(info(found[0]).3, "WPA2 network should advertise privacy");
    assert_eq!(info(found[1]).0, "FreeCafe");
    assert!(!info(found[1]).3);
    assert_eq!(
        u32::from_le_bytes(found[0].data[4..8].try_into().unwrap()),
        109
    );
    // The IEs, where pico-sdk's driver finds them: wl_bss_info_t laid out
    // with natural alignment (ie_offset at 116, ie_length at 120).
    let ies = |e: &Event| {
        let b = &e.data[12..];
        let off = u16::from_le_bytes([b[116], b[117]]) as usize;
        let len = u32::from_le_bytes(b[120..124].try_into().unwrap()) as usize;
        assert!(off + len <= u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize);
        b[off..off + len].to_vec()
    };
    let wpa2 = ies(found[0]);
    assert_eq!(wpa2[..2], [0, 13]);
    assert_eq!(&wpa2[2..15], b"TarteletteNet");
    assert_eq!(wpa2[15], 0x30, "RSN element");
    assert_eq!(
        ies(found[1])[2..],
        *b"FreeCafe",
        "an open network has no RSN"
    );
    assert_eq!(ev.last().unwrap().status, 0);
}

#[test]
fn with_no_network_scans_are_empty_and_say_so() {
    let mut h = booted(BOARD);
    let ev = h.escan(None);
    assert!(
        ev.iter().all(|e| e.status != 8),
        "got a result with no network"
    );
    assert_eq!(ev.last().unwrap().status, 0);
}

#[test]
fn with_a_network_scan_finds_the_access_points() {
    let mut h = booted(BOARD);
    h.chip_mut().set_backend(LoopbackBackend::boxed());
    assert_eq!(h.escan(None).iter().filter(|e| e.status == 8).count(), 2);
    let ev = h.escan(Some("FreeCafe"));
    assert_eq!(ev.iter().filter(|e| e.status == 8).count(), 1);
    assert_eq!(
        h.escan(Some("Nope"))
            .iter()
            .filter(|e| e.status == 8)
            .count(),
        0
    );
}

#[test]
fn joining_without_a_network_fails() {
    let mut h = booted(BOARD);
    let ev = h.join("FreeCafe", None);
    assert_eq!(ev.last().unwrap().kind, 0);
    assert_ne!(ev.last().unwrap().status, 0);
}

#[test]
fn join_open_and_wpa2_succeed_and_wrong_password_fails_cleanly() {
    let mut h = booted(BOARD);
    h.chip_mut().set_backend(LoopbackBackend::boxed());

    let ev = h.join("TarteletteNet", Some("hunter22"));
    assert_eq!(
        ev.iter().map(|e| e.kind).collect::<Vec<_>>(),
        [3, 7, 46, 16, 0, 1],
        "auth, assoc, psk_sup, link, set_ssid, join"
    );
    assert!(ev.iter().all(|e| e.status == 0 || e.kind == 46));
    assert_eq!(ev[3].flags & 1, 1, "link-up flag");
    assert_eq!(h.chip().link(), Link::Up(0));
    assert_eq!(h.chip().ssid(), Some("TarteletteNet"));

    let ev = h.join("TarteletteNet", Some("wrongpass1"));
    let last = ev.last().unwrap();
    assert_eq!((last.kind, last.status), (0, 1));
    assert_eq!(h.chip().link(), Link::Down);

    // An open join to a WPA2 network is refused.
    let ev = h.join("TarteletteNet", None);
    assert_ne!(ev.last().unwrap().status, 0);
    // An unknown network is not found.
    let ev = h.join("Nowhere", None);
    assert_eq!(ev.last().unwrap().status, 3);

    let ev = h.join("FreeCafe", None);
    assert!(
        ev.iter().any(|e| (e.kind, e.status) == (0, 0)),
        "SET_SSID success"
    );
    assert_eq!(
        (ev.last().unwrap().kind, ev.last().unwrap().status),
        (1, 0),
        "then JOIN"
    );
    assert_eq!(h.chip().link(), Link::Up(1));
}

#[test]
fn a_precomputed_psk_joins_too() {
    let mut h = booted(BOARD);
    h.chip_mut().set_backend(LoopbackBackend::boxed());
    h.events.clear();
    h.ioctl(true, 134, &4u32.to_le_bytes());
    let psk = wpa::psk(b"hunter22", b"TarteletteNet");
    let mut pfi = Vec::new();
    pfi.extend_from_slice(&32u16.to_le_bytes());
    pfi.extend_from_slice(&0u16.to_le_bytes());
    let mut k = [0u8; 64];
    k[..32].copy_from_slice(&psk);
    pfi.extend_from_slice(&k);
    h.ioctl(true, 268, &pfi);
    let mut i = Vec::new();
    i.extend_from_slice(&13u32.to_le_bytes());
    let mut n = [0u8; 32];
    n[..13].copy_from_slice(b"TarteletteNet");
    i.extend_from_slice(&n);
    h.ioctl(true, 26, &i);
    h.collect_events(500, |h| h.events.iter().any(|e| e.kind == 0));
    assert_eq!(h.events.last().unwrap().status, 0);
}

#[test]
fn dhcp_and_udp_echo_over_the_data_channel() {
    let mut h = booted(BOARD);
    h.chip_mut().set_backend(LoopbackBackend::boxed());
    h.join("FreeCafe", None);
    let mac = [0x28, 0xCD, 0xC1, 0, 0, 1];

    // DHCP DISCOVER from 0.0.0.0:68 to the broadcast address.
    let mut d = vec![0u8; 236];
    d[0] = 1;
    d[1] = 1;
    d[2] = 6;
    d[4..8].copy_from_slice(&[1, 2, 3, 4]);
    d[28..34].copy_from_slice(&mac);
    d.extend_from_slice(&[0x63, 0x82, 0x53, 0x63, 53, 1, 1, 255]);
    let frame = net::udp_frame([0xFF; 6], mac, ([0; 4], 68), ([255; 4], 67), &d);
    let mut payload = vec![0x20, 0, 0, 0];
    payload.extend_from_slice(&frame);
    // Host frames carry two bytes of padding before the BDC header.
    let mut padded = vec![0, 0];
    padded.extend_from_slice(&payload);
    h.send_frame(2, 14, &payload);
    h.collect_events(50, |h| !h.data_frames.is_empty());
    let offer = h.data_frames.remove(0);
    let ip = net::parse_ipv4(&offer).unwrap();
    assert_eq!(ip.src, net::GATEWAY_IP);
    let udp = net::parse_udp(ip.payload).unwrap();
    assert_eq!((udp.src_port, udp.dst_port), (67, 68));
    assert_eq!(&udp.data[16..20], &net::GUEST_IP);
    assert_eq!(udp.data[240..243], [53, 1, 2]);

    // ARP for the gateway, then echo a UDP datagram.
    let mut arp = vec![0xFF; 6];
    arp.extend_from_slice(&mac);
    arp.extend_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
    arp.extend_from_slice(&mac);
    arp.extend_from_slice(&net::GUEST_IP);
    arp.extend_from_slice(&[0; 6]);
    arp.extend_from_slice(&net::GATEWAY_IP);
    let mut p = vec![0x20, 0, 0, 0];
    p.extend_from_slice(&arp);
    h.send_frame(2, 14, &p);
    h.collect_events(50, |h| !h.data_frames.is_empty());
    let reply = h.data_frames.remove(0);
    assert_eq!(&reply[12..14], &[8, 6]);
    assert_eq!(&reply[20..22], &[0, 2]);
    assert_eq!(&reply[22..28], &net::GATEWAY_MAC);

    let echo = net::udp_frame(
        net::GATEWAY_MAC,
        mac,
        (net::GUEST_IP, 5000),
        (net::GATEWAY_IP, 7),
        b"hello pico",
    );
    let mut p = vec![0x20, 0, 0, 0];
    p.extend_from_slice(&echo);
    h.send_frame(2, 14, &p);
    h.collect_events(50, |h| !h.data_frames.is_empty());
    let back = h.data_frames.remove(0);
    let ip = net::parse_ipv4(&back).unwrap();
    let udp = net::parse_udp(ip.payload).unwrap();
    assert_eq!(
        (udp.src_port, udp.dst_port, udp.data),
        (7, 5000, &b"hello pico"[..])
    );
    assert_eq!(h.chip().lease(), None, "no DHCPREQUEST yet");
}

#[test]
fn board_file_errors() {
    let e = |t: &str| Board::parse_with(t, &crate::registry()).unwrap_err().msg;
    let w = |extra: &str| {
        format!("parts.w = {{ kind = \"cyw43439\", on = 23, dio = 24, cs = 25, clk = 29{extra} }}")
    };
    assert!(Board::parse_with(&w(""), &crate::registry()).is_ok());
    assert_eq!(
        e("parts.w = { kind = \"cyw43439\", dio = 24, cs = 25, clk = 29 }"),
        "missing on (WL_ON: power and reset)"
    );
    assert!(e(&w(", mac = \"1:2:3\"")).starts_with("w: mac must be"));
    assert_eq!(
        e(&w(", aps = [{ ssid = \"x\", security = \"wpa2\" }]")),
        "w: aps[0]: a wpa2 network needs a passphrase"
    );
    assert_eq!(
        e(&w(
            ", aps = [{ ssid = \"x\", security = \"wpa2\", passphrase = \"short\" }]"
        )),
        "w: aps[0]: passphrase must be 8..63 characters"
    );
    assert_eq!(
        e(&w(
            ", aps = [{ ssid = \"x\", passphrase = \"long enough\" }]"
        )),
        "w: aps[0]: an open network has no passphrase"
    );
    assert_eq!(
        e(&w(", aps = [{ ssid = \"x\", security = \"sort\" }]")),
        "aps[0]: security must be open or wpa2"
    );
    assert_eq!(
        e(&w(", aps = [{ ssid = \"x\", channel = 99 }]")),
        "aps[0]: channel must be 1..14"
    );
    assert_eq!(
        e(&w(", aps = [{ ssid = \"x\" }, { ssid = \"x\" }]")),
        "w: aps[1]: SSID \"x\" is listed twice"
    );
    assert_eq!(
        e(&w(", ap1 = \"x\"")),
        "unknown parameter ap1 (did you mean aps?)"
    );
}

// ---- Bluetooth, the way `cyw43`'s bluetooth.rs drives it ----

const MS: u64 = 125_000;
const BT_CTRL: u32 = 0x1800_0C7C;
const WLAN_RAM_BASE: u32 = 0x1800_0D68;
const HOST_CTRL: u32 = 0x1800_0D6C;
const SDIO_INT_STATUS: u32 = 0x1800_2020;
const SDIO_INT_HOST_MASK: u32 = 0x1800_2024;

/// The host side of the Bluetooth rings.
struct BtHost {
    base: u32,
    h2b: u32,
    b2h: u32,
}

impl Host {
    fn bp_read32(&mut self, addr: u32) -> u32 {
        u32::from_le_bytes(self.bp_read(addr, 4).try_into().unwrap())
    }

    /// `init_bluetooth`: power-up, patch download, wait for ready, rings,
    /// host ready.
    fn bt_init(&mut self) -> BtHost {
        self.bp_write32(SDIO_INT_HOST_MASK, 1 << 5);
        self.bp_write32(0x1964_0894, 3);
        self.advance(2 * MS);
        self.bp_write(0x1900_1000, &[0xA5; 64]);
        assert_eq!(
            self.bp_read32(BT_CTRL) & 1 << 24,
            0,
            "not ready the moment the patch is in"
        );
        self.advance(3 * MS);
        assert_eq!(
            self.bp_read32(BT_CTRL) & (1 << 24 | 1 << 8),
            1 << 24 | 1 << 8,
            "ready and awake"
        );
        let base = self.bp_read32(WLAN_RAM_BASE);
        assert_ne!(base, 0);
        for off in [0x2000, 0x2004, 0x2008, 0x200C] {
            self.bp_write32(base + off, 0);
        }
        let v = self.bp_read32(HOST_CTRL);
        self.bp_write32(HOST_CTRL, v | 1 << 24);
        BtHost {
            base,
            h2b: 0,
            b2h: 0,
        }
    }

    /// `hci_write`: header and padded payload into the ring, advance the
    /// index, toggle data-valid.
    fn hci_write(&mut self, bt: &mut BtHost, kind: u8, payload: &[u8]) {
        let len = payload.len() as u32;
        self.bp_write(bt.base + bt.h2b, &(len | (kind as u32) << 24).to_le_bytes());
        bt.h2b = (bt.h2b + 4) % 0x1000;
        let mut padded = payload.to_vec();
        padded.resize(payload.len().div_ceil(4) * 4, 0);
        for (i, b) in padded.chunks(4).enumerate() {
            self.bp_write(bt.base + (bt.h2b + 4 * i as u32) % 0x1000, b);
        }
        bt.h2b = (bt.h2b + padded.len() as u32) % 0x1000;
        self.bp_write32(bt.base + 0x2000, bt.h2b);
        let v = self.bp_read32(HOST_CTRL);
        self.bp_write32(HOST_CTRL, v ^ 1 << 1);
    }

    /// `handle_irq`: if the SDIO core flags ring activity, clear it and read
    /// every packet.
    fn hci_read(&mut self, bt: &mut BtHost) -> Vec<(u8, Vec<u8>)> {
        let mut got = Vec::new();
        let st = self.bp_read32(SDIO_INT_STATUS);
        if st & 1 << 5 == 0 {
            return got;
        }
        self.bp_write32(SDIO_INT_STATUS, st & 1 << 5);
        loop {
            let write = self.bp_read32(bt.base + 0x2008);
            if write == bt.b2h {
                break;
            }
            let header = self.bp_read32(bt.base + 0x1000 + bt.b2h);
            let len = header & 0xFF_FFFF;
            bt.b2h = (bt.b2h + 4) % 0x1000;
            let mut p = Vec::new();
            for i in 0..len.div_ceil(4) {
                p.extend(self.bp_read(bt.base + 0x1000 + (bt.b2h + 4 * i) % 0x1000, 4));
            }
            p.truncate(len as usize);
            bt.b2h = (bt.b2h + len.div_ceil(4) * 4) % 0x1000;
            self.bp_write32(bt.base + 0x200C, bt.b2h);
            got.push(((header >> 24) as u8, p));
        }
        got
    }

    /// Send a command and collect what comes back within `ms`.
    fn hci_command(
        &mut self,
        bt: &mut BtHost,
        opcode: u16,
        params: &[u8],
        ms: u64,
    ) -> Vec<(u8, Vec<u8>)> {
        let mut p = opcode.to_le_bytes().to_vec();
        p.push(params.len() as u8);
        p.extend_from_slice(params);
        self.hci_write(bt, 1, &p);
        let mut got = self.hci_read(bt);
        for _ in 0..ms {
            self.advance(MS);
            got.extend(self.hci_read(bt));
        }
        got
    }
}

#[test]
fn bluetooth_comes_up_and_carries_hci_both_ways() {
    let mut h = booted(BOARD);
    let mut bt = h.bt_init();
    assert!(h.chip().bt_up());
    let got = h.hci_command(&mut bt, 0x0C03, &[], 0);
    assert_eq!(
        got,
        [(4, vec![0x0E, 4, 1, 0x03, 0x0C, 0])],
        "Reset completes"
    );
    // The interrupt line went up with the data and down with the clear.
    assert_eq!(h.read16(0, 0x04) & 0x2000, 0);
    let mut p = 0x1009u16.to_le_bytes().to_vec();
    p.push(0);
    h.hci_write(&mut bt, 1, &p);
    assert_ne!(
        h.read16(0, 0x04) & 0x2000,
        0,
        "F1_INTR while the ring has data"
    );
    let got = h.hci_read(&mut bt);
    assert_eq!(
        got[0].1[..7],
        [0x0E, 10, 1, 0x09, 0x10, 0, 0x02],
        "BD_ADDR is the MAC plus one"
    );
    assert_eq!(h.read16(0, 0x04) & 0x2000, 0);
}

#[test]
fn many_packets_wrap_around_the_rings() {
    let mut h = booted(BOARD);
    let mut bt = h.bt_init();
    // Each Read Local Supported Commands answer is 72 bytes: 100 of them go
    // round the 4 KiB ring more than once in each direction.
    for _ in 0..100 {
        let got = h.hci_command(&mut bt, 0x1002, &[], 0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1.len(), 2 + 4 + 64);
    }
}

#[test]
fn without_a_radio_bluetooth_scans_find_nothing_and_say_so() {
    let mut h = booted(BOARD);
    let mut bt = h.bt_init();
    h.hci_command(&mut bt, 0x200B, &[1, 0x10, 0, 0x10, 0, 0, 0], 0);
    let got = h.hci_command(&mut bt, 0x200C, &[1, 1], 50);
    assert_eq!(got.len(), 1, "only the command completes: {got:?}");
    assert!(h.notes.contains(&("bt_unavailable", 1)));
    assert!(!h.chip().bt_backend().available());
    assert_eq!(h.chip().bt_activity(), (true, false, 0));
}

#[test]
fn mock_devices_show_up_in_scans_and_survive_a_power_cycle() {
    use crate::hci::{peers, virt::VirtualController};
    let mut h = Host::new(BOARD);
    let addr = h.chip().mac();
    h.chip_mut().set_bt_backend(Box::new(VirtualController::new(
        bt::bt_address(addr),
        peers::neighbourhood(),
    )));
    h.bring_up();
    h.init_control();
    let mut bt = h.bt_init();
    h.hci_command(&mut bt, 0x200B, &[0, 0x10, 0, 0x10, 0, 0, 0], 0);
    let got = h.hci_command(&mut bt, 0x200C, &[1, 1], 30);
    let reports: Vec<_> = got
        .iter()
        .filter(|(k, p)| *k == 4 && p[0] == 0x3E && p[2] == 0x02)
        .map(|(_, p)| p[6..12].to_vec())
        .collect();
    assert_eq!(
        reports,
        [
            peers::MIDI_ADDR.to_vec(),
            peers::BATTERY_ADDR.to_vec(),
            peers::PRINTER_ADDR.to_vec()
        ],
        "one passive report each"
    );
    assert!(h.chip().bt_backend().available());
}
