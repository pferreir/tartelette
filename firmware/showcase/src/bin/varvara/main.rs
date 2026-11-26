//! VARVARA: a Uxn virtual machine running a Varvara program, on a Pico
//! (`boards/varvara.toml`: a 320x240 ST7789, a D-pad, A, B, Select and
//! Start, and a speaker on GPIO16). Uxn is a little stack machine and
//! Varvara the computer built round it (screen, sound, controller, console),
//! from Hundred Rabbits: <https://wiki.xxiivv.com/site/varvara.html>.
//!
//! The program is a ROM built into the firmware: `roms/fourtette.rom` by
//! default (fetch it with `roms/fetch.sh`), or another one named by
//! `VARVARA_ROM` at build time.
//!
//! - The screen, of up to 320x256, sits in the middle of the panel; rows
//!   past the panel's 240 are cut off at the bottom. Only the strips of
//!   rows whose pixels changed are sent.
//! - The controller: the eight buttons, read once a frame.
//! - The console: writes go to UART0, and bytes typed into it come back to
//!   the console's vector, or as keys to the controller's when the program
//!   has no console vector.
//! - Audio: the four voices, mixed to 25 kHz PWM on GPIO16 (the sound path
//!   of `groovebox-audio`).
//! - The Pico runs at 200 MHz: the VM needs the cycles.
//! - DateTime counts from 2026-01-01 at boot; there is no File device.
//!
//! The screen vector runs 60 times a second, after the input vectors.
#![no_std]
#![no_main]

mod audio;
mod screen;
mod uxn;

use core::{cell::RefCell, ptr::addr_of_mut};

use defmt::{info, warn};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_rp::{
    bind_interrupts,
    clocks::ClockConfig,
    gpio::{Input, Level, Output, Pull},
    interrupt,
    interrupt::{InterruptExt, Priority},
    mode::{Async, Blocking},
    pac,
    peripherals::DMA_CH0,
    pwm::{self, Pwm},
    spi::{self, Spi},
    uart::{self, Uart},
};
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Duration, Instant, Timer};
use panic_probe as _;

use crate::{
    audio::{Audio, VOICES},
    screen::Screen,
    uxn::{Devices, Uxn},
};

bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
});

static ROM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/varvara.rom"));

static mut UXN: Uxn = Uxn::new();
static mut SCREEN: Screen = Screen::new();

// ---------------------------------------------------------- sound path ----

/// The system clock: 200 MHz, the most the datasheet documents (at 1.15 V,
/// which embassy sets), as the VM needs the cycles.
const SYS_HZ: u32 = 200_000_000;
/// Samples per second: one per PWM period.
const RATE: u32 = 25_000;
/// The PWM's period, in system clock cycles; a level `x` is high for x of
/// them.
const PERIOD: u32 = SYS_HZ / RATE;
const MID: i32 = PERIOD as i32 / 2;
/// Samples per half of the ring.
const HALF: usize = 64;
/// The DMA channel the sound uses (embassy's drivers have CH0).
const CH: usize = 1;

/// Aligned to its size, for the DMA's address wrap.
#[repr(C, align(512))]
struct Ring([u32; 2 * HALF]);

static mut RING: Ring = Ring([MID as u32; 2 * HALF]);
/// Which half of the ring the interrupt fills next.
static mut HALF_NEXT: usize = 0;
static AUDIO: Mutex<CriticalSectionRawMutex, RefCell<Audio>> =
    Mutex::new(RefCell::new(Audio::new(RATE)));

/// Renders voices into one half of the ring, as PWM levels.
fn render(half: &mut [u32]) {
    let mut mix = [0i32; HALF];
    // The VM may be writing elsewhere in memory meanwhile; a sample byte
    // read mid-update is just a glitch in the sound.
    // SAFETY: in bounds, as `a` is a u16 and the memory 64 KiB.
    let ram = unsafe { addr_of_mut!(UXN.ram) as *const u8 };
    let mem = |a: u16| unsafe { ram.add(a as usize).read_volatile() };
    AUDIO.lock(|a| a.borrow_mut().render(mem, &mut mix));
    // Full scale at two voices at full volume (±16.8 million), clipped
    // beyond, with a multiply and shifts instead of a division.
    const GAIN: i32 = MID * 65536 / 36_000;
    for (o, m) in half.iter_mut().zip(mix) {
        *o = (MID + (((m >> 8) * GAIN) >> 16)).clamp(0, PERIOD as i32) as u32;
    }
}

#[interrupt]
fn DMA_IRQ_1() {
    pac::DMA.ints(1).write_value(1 << CH);
    // Carry on round the ring, then refill the half just played.
    pac::DMA
        .ch(CH)
        .al1_trans_count_trig()
        .write_value(HALF as u32);
    // SAFETY: only this handler touches HALF_NEXT and the half of RING that
    // DMA has finished reading, once main has started the DMA.
    let (h, ring) = unsafe { (&mut *addr_of_mut!(HALF_NEXT), &mut (*addr_of_mut!(RING)).0) };
    let n = *h;
    *h ^= 1;
    render(&mut ring[n * HALF..(n + 1) * HALF]);
}

/// Start the DMA feeding slice 0's compare register, once per wrap.
fn start_sound() {
    // SAFETY: the DMA interrupt isn't enabled yet.
    let ring = unsafe { &mut (*addr_of_mut!(RING)).0 };
    let dma = pac::DMA;
    let ch = dma.ch(CH);
    ch.read_addr().write_value(ring.as_ptr() as u32);
    ch.write_addr()
        .write_value(pac::PWM.ch(0).cc().as_ptr() as u32);
    ch.trans_count().write_value(HALF as u32);
    dma.inte(1).modify(|v| *v |= 1 << CH);
    interrupt::DMA_IRQ_1.set_priority(Priority::P1);
    // SAFETY: the handler only needs what is set up above.
    unsafe { interrupt::DMA_IRQ_1.enable() };
    ch.ctrl_trig().write(|w| {
        w.set_data_size(pac::dma::vals::DataSize::SizeWord);
        w.set_incr_read(true);
        w.set_incr_write(false);
        // Wrap the read address round the 512-byte ring.
        w.set_ring_size(9);
        w.set_ring_sel(false);
        w.set_treq_sel(pac::dma::vals::TreqSel::PwmWrap0);
        w.set_chain_to(CH as u8);
        w.set_en(true);
    });
}

// ------------------------------------------------------------- display ----

const PW: usize = 320;
const PH: usize = 240;
/// Rows per transfer: two buffers of 10 KiB take turns, one filled while
/// DMA sends the other.
const CHUNK: usize = 16;
const CHUNKS: usize = PH / CHUNK;

/// The 320x240 ST7789, landscape, on SPI0: SCK 18, MOSI 19, CS 9, D/C 13.
struct Panel {
    spi: Spi<'static, Async>,
    cs: Output<'static>,
    dc: Output<'static>,
    /// RGB565, big-endian as the panel takes it.
    bufs: [[u16; PW * CHUNK]; 2],
    /// A hash of each chunk as last sent.
    sent: [u32; CHUNKS],
}

impl Panel {
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

    /// Sends the chunks of rows that changed: those the program drew on,
    /// if their pixels did change (programs often redraw the same thing),
    /// or all of them, borders included, after a resize or a new palette.
    async fn flush(&mut self, s: &mut Screen) {
        let all = core::mem::take(&mut s.all_dirty);
        let (w, h) = (s.w, s.h);
        let ox = (PW - w) / 2;
        let oy = PH.saturating_sub(h) / 2;
        // The screen rows on panel row `py`, if any.
        let src = |py: usize| (py >= oy && py - oy < h).then(|| py - oy);
        let mut todo = [all; CHUNKS];
        for sy in 0..h {
            // Rows below the panel are taken too, so they don't pile up.
            if s.take_dirty(sy) && oy + sy < PH {
                todo[(oy + sy) / CHUNK] = true;
            }
        }
        for (c, t) in todo.iter_mut().enumerate() {
            if *t {
                // FNV-1a over the chunk's pixels, never 0 (unknown).
                let mut hash = 0x811c_9dc5u32;
                for sy in (c * CHUNK..(c + 1) * CHUNK).filter_map(src) {
                    for &b in s.row(sy) {
                        hash = (hash ^ b as u32).wrapping_mul(0x0100_0193);
                    }
                }
                let hash = hash | 1;
                *t = all || hash != self.sent[c];
                self.sent[c] = hash;
            }
        }
        let mut lut = [0u16; 16];
        for (v, c) in lut.iter_mut().enumerate() {
            let i = if v >> 2 != 0 { v >> 2 } else { v & 3 };
            *c = s.colors[i].to_be();
        }

        // Round a smaller screen, black.
        let border = 0;
        let Self {
            spi, cs, dc, bufs, ..
        } = self;
        let [a, b] = bufs;
        let (mut ready, mut spare) = (a, b);
        // The chunk in `ready`, waiting to be sent.
        let mut pending: Option<usize> = None;
        let mut chunks = (0..CHUNKS).filter(|&c| todo[c]);
        loop {
            let next = chunks.next();
            if pending.is_none() && next.is_none() {
                break;
            }
            let send = async {
                if let Some(c) = pending {
                    let (y0, ye) = (c * CHUNK, c * CHUNK + CHUNK - 1);
                    let mut cmd = |c: u8, args: &[u8]| {
                        cs.set_low();
                        dc.set_low();
                        let _ = spi.blocking_write(&[c]);
                        dc.set_high();
                        let _ = spi.blocking_write(args);
                        if c != 0x2C {
                            cs.set_high();
                        }
                    };
                    cmd(0x2A, &[0, 0, ((PW - 1) >> 8) as u8, (PW - 1) as u8]);
                    cmd(
                        0x2B,
                        &[(y0 >> 8) as u8, y0 as u8, (ye >> 8) as u8, ye as u8],
                    );
                    cmd(0x2C, &[]);
                    // SAFETY: plain integers, viewed as the bytes they are.
                    let bytes = unsafe {
                        core::slice::from_raw_parts(ready.as_ptr() as *const u8, PW * CHUNK * 2)
                    };
                    let _ = spi.write(bytes).await;
                    cs.set_high();
                }
            };
            let fill = async {
                if let Some(c) = next {
                    for (i, out) in spare.as_chunks_mut::<PW>().0.iter_mut().enumerate() {
                        let Some(sy) = src(c * CHUNK + i) else {
                            out.fill(border);
                            continue;
                        };
                        out[..ox].fill(border);
                        out[ox + w..].fill(border);
                        for (o, &v) in out[ox..ox + w].iter_mut().zip(s.row(sy)) {
                            *o = lut[(v & 15) as usize];
                        }
                    }
                }
            };
            join(send, fill).await;
            pending = next;
            core::mem::swap(&mut ready, &mut spare);
        }
    }
}

// ------------------------------------------------------------- devices ----

/// Varvara's devices round the VM.
struct Varvara {
    screen: &'static mut Screen,
    uart: Uart<'static, Blocking>,
}

fn peek2(m: &[u8], a: usize) -> usize {
    (m[a & 0xffff] as usize) << 8 | m[(a + 1) & 0xffff] as usize
}

impl Varvara {
    /// System/expansion: fill and copy within memory. Only bank 0 (the
    /// 64 KiB the program runs in) exists.
    fn expansion(&mut self, u: &mut Uxn) {
        let ram = &mut u.ram;
        let e = peek2(&u.dev, 2);
        let (cmd, bank, addr) = (ram[e], peek2(ram, e + 3), peek2(ram, e + 5));
        let len = peek2(ram, e + 1).min(0x10000 - addr);
        match cmd {
            0 if bank == 0 => {
                let v = ram[(e + 7) & 0xffff];
                ram[addr..addr + len].fill(v);
            }
            1 | 2 => {
                let (dst_bank, dst) = (peek2(ram, e + 7), peek2(ram, e + 9));
                let len = len.min(0x10000 - dst);
                if bank == 0 && dst_bank == 0 {
                    ram.copy_within(addr..addr + len, dst);
                }
            }
            _ => warn!("varvara: expansion command {=u8:02x} unsupported", cmd),
        }
    }

    fn datetime(&mut self, u: &mut Uxn, port: u8) -> u8 {
        // 2026-01-01 00:00:00, a Thursday, plus the time since boot.
        let secs = Instant::now().as_secs();
        let days = (secs / 86400) as i64 + 20454;
        let t = secs % 86400;
        let (year, month, day) = civil(days);
        let doty = days - days_from_civil(year, 1, 1);
        let v: u16 = match port {
            0xc0 | 0xc1 => year as u16,
            0xc2 => month as u16 - 1,
            0xc3 => day as u16,
            0xc4 => (t / 3600) as u16,
            0xc5 => (t / 60 % 60) as u16,
            0xc6 => (t % 60) as u16,
            0xc7 => ((days + 4) % 7) as u16,
            0xc8 | 0xc9 => doty as u16,
            _ => 0,
        };
        if port == 0xc0 || port == 0xc8 {
            // A short: its second byte is read from the next port.
            u.dev[port as usize + 1] = v as u8;
            (v >> 8) as u8
        } else {
            v as u8
        }
    }
}

/// Days since 1970-01-01 to (year, month 1..=12, day 1..=31), after Howard
/// Hinnant's `civil_from_days`.
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl Devices for Varvara {
    fn dei(&mut self, u: &mut Uxn, port: u8) -> u8 {
        match port {
            0x04 => u.wst.ptr,
            0x05 => u.rst.ptr,
            0x20..=0x2f => self.screen.dei(&mut u.dev, port),
            0x32 | 0x42 | 0x52 | 0x62 => {
                let n = (port >> 4) as usize - 3;
                let pos = AUDIO.lock(|a| a.borrow().position(n));
                u.dev[port as usize + 1] = pos as u8;
                (pos >> 8) as u8
            }
            0x34 | 0x44 | 0x54 | 0x64 => {
                let n = (port >> 4) as usize - 3;
                AUDIO.lock(|a| a.borrow_mut().vu(n))
            }
            0xc0..=0xca => self.datetime(u, port),
            _ => u.dev[port as usize],
        }
    }

    fn deo(&mut self, u: &mut Uxn, port: u8) {
        match port {
            0x03 => self.expansion(u),
            0x04 => u.wst.ptr = u.dev[4],
            0x05 => u.rst.ptr = u.dev[5],
            0x09 | 0x0b | 0x0d => self.screen.colorize(&u.dev),
            0x0e if u.dev[0x0e] & 1 != 0 => {
                let top = |s: &uxn::Stack| {
                    let mut b = [0u8; 8];
                    for (i, v) in b.iter_mut().enumerate() {
                        *v = s.dat[s.ptr.wrapping_sub(8 - i as u8) as usize];
                    }
                    b
                };
                info!(
                    "varvara: WST {=[u8]:02x} <{=u8:02x}, RST {=[u8]:02x} <{=u8:02x}",
                    top(&u.wst),
                    u.wst.ptr,
                    top(&u.rst),
                    u.rst.ptr
                );
            }
            0x18 | 0x19 => {
                let _ = self.uart.blocking_write(&[u.dev[port as usize]]);
            }
            0x20..=0x2f => self.screen.deo(&mut u.dev, &u.ram, port),
            0x3f | 0x4f | 0x5f | 0x6f => {
                let n = (port >> 4) as usize - 3;
                let d = &u.dev[(n + 3) * 16..(n + 4) * 16];
                AUDIO.lock(|a| a.borrow_mut().start(n, d));
            }
            _ => {}
        }
    }
}

fn vector(u: &Uxn, port: usize) -> u16 {
    (u.dev[port] as u16) << 8 | u.dev[port + 1] as u16
}

// ---------------------------------------------------------------- main ----

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut config = embassy_rp::config::Config::default();
    config.clocks = ClockConfig::system_freq(SYS_HZ).unwrap();
    let p = embassy_rp::init(config);

    // Slice 0 on GPIO16, at the resting level; the DMA channel is reserved
    // for the sound, which drives it through its registers.
    let mut cfg = pwm::Config::default();
    cfg.top = (PERIOD - 1) as u16;
    cfg.compare_a = MID as u16;
    let _pwm = Pwm::new_output_a(p.PWM_SLICE0, p.PIN_16, cfg);
    let _ch = p.DMA_CH1;
    start_sound();

    let mut scfg = spi::Config::default();
    scfg.frequency = embassy_rp::time::Hertz(62_500_000);
    let spi = Spi::new_txonly(p.SPI0, p.PIN_18, p.PIN_19, p.DMA_CH0, Irqs, scfg).unwrap();
    let mut panel = Panel {
        spi,
        cs: Output::new(p.PIN_9, Level::High),
        dc: Output::new(p.PIN_13, Level::Low),
        bufs: [[0; PW * CHUNK]; 2],
        sent: [0; CHUNKS],
    };
    let _bl = Output::new(p.PIN_17, Level::High);
    panel.init().await;

    // In the order of the controller's bits: A, B, Select, Start, Up, Down,
    // Left, Right.
    let buttons = [
        Input::new(p.PIN_6, Pull::Up),
        Input::new(p.PIN_7, Pull::Up),
        Input::new(p.PIN_8, Pull::Up),
        Input::new(p.PIN_10, Pull::Up),
        Input::new(p.PIN_2, Pull::Up),
        Input::new(p.PIN_3, Pull::Up),
        Input::new(p.PIN_4, Pull::Up),
        Input::new(p.PIN_5, Pull::Up),
    ];

    let uart = Uart::new_blocking(p.UART0, p.PIN_0, p.PIN_1, uart::Config::default());
    // SAFETY: main is the only one to take these; the sound interrupt reads
    // memory through a raw pointer.
    let (u, screen) = unsafe { (&mut *addr_of_mut!(UXN), &mut *addr_of_mut!(SCREEN)) };
    let mut vm = Varvara { screen, uart };

    if ROM.is_empty() {
        warn!("varvara: no ROM built in: run roms/fetch.sh, or set VARVARA_ROM");
        let _ = vm.uart.blocking_write(b"varvara: no ROM built in\r\n");
        return;
    }
    info!("VARVARA: a {=usize}-byte ROM", ROM.len());
    u.load(ROM);
    vm.screen.boot(&mut u.dev, PW, PH);
    u.eval(&mut vm, 0x100);

    const FRAME: Duration = Duration::from_micros(16_667);
    /// How often the time a frame takes is logged.
    const FRAMES: u32 = 600;
    let mut next = Instant::now();
    let (mut vm_time, mut draw_time, mut frames) = (0u64, 0u64, 0u32);
    loop {
        let t0 = Instant::now();
        let held = buttons
            .iter()
            .enumerate()
            .fold(0u8, |b, (i, k)| b | (k.is_low() as u8) << i);
        if held != u.dev[0x82] {
            u.dev[0x82] = held;
            u.eval(&mut vm, vector(u, 0x80));
        }
        while vm.uart.read_ready() {
            let mut c = [0u8];
            if vm.uart.blocking_read(&mut c).is_err() {
                break;
            }
            if vector(u, 0x10) != 0 {
                (u.dev[0x12], u.dev[0x17]) = (c[0], 1);
                u.eval(&mut vm, vector(u, 0x10));
            } else {
                u.dev[0x83] = c[0];
                u.eval(&mut vm, vector(u, 0x80));
                u.dev[0x83] = 0;
            }
        }
        let done = AUDIO.lock(|a| a.borrow_mut().take_done());
        for n in 0..VOICES {
            if done & 1 << n != 0 {
                u.eval(&mut vm, vector(u, 0x30 + 0x10 * n));
            }
        }
        u.eval(&mut vm, vector(u, 0x20));
        let t1 = Instant::now();
        panel.flush(vm.screen).await;
        vm_time += (t1 - t0).as_micros();
        draw_time += (Instant::now() - t1).as_micros();

        if u.halted() {
            info!("varvara: halted ({=u8:02x})", u.dev[0x0f]);
            let _ = vm.uart.blocking_write(b"varvara: halted\r\n");
            return;
        }

        frames += 1;
        if frames == FRAMES {
            info!(
                "varvara: a frame in {=u64} us running, {=u64} us drawing",
                vm_time / FRAMES as u64,
                draw_time / FRAMES as u64
            );
            (vm_time, draw_time, frames) = (0, 0, 0);
        }
        // Behind, give up the frames missed instead of rushing through them.
        next = (next + FRAME).max(Instant::now());
        Timer::at(next).await;
    }
}
