//! VARVARA: a Uxn virtual machine running a Varvara program, on a Pico
//! (`boards/varvara.toml`: a 320x240 ST7789, a D-pad, A, B, Select and
//! Start, and a speaker on GPIO16). Uxn is a little stack machine and
//! Varvara the computer built round it (screen, sound, controller, console),
//! from Hundred Rabbits: <https://wiki.xxiivv.com/site/varvara.html>.
//!
//! The program is a ROM built into the firmware (`roms/fourtette.rom` by
//! default, fetched by `roms/fetch.sh`, or another one named by
//! `VARVARA_ROM` at build time), or one on the SD card. With a card in, a
//! menu lists the built-in ROM and the card's directories and `.rom` files
//! (`menu.rs`). Holding Select and Start for a second goes back to it, as
//! does a ROM halting; without a card, it starts the built-in ROM again.
//!
//! - The screen, of up to 320x256, sits in the middle of the panel; rows
//!   past the panel's 240 are cut off at the bottom. Only the strips of
//!   rows whose pixels changed are sent.
//! - The controller: the eight buttons, read once a frame.
//! - The mouse: a PS/2 mouse on GPIO20 (CLK) and 21 (DATA), if there is
//!   one (`mouse.rs`), with its wheel; a packet a frame.
//! - The console: writes go to UART0, and bytes typed into it come back to
//!   the console's vector, or as keys to the controller's when the program
//!   has no console vector.
//! - Audio: the four voices, mixed to 25 kHz PWM on GPIO16 (the sound path
//!   of `groovebox-audio`).
//! - File: both devices, on the card's first FAT volume (`file.rs`), an
//!   SDHC card on SPI1: SCK 14, MOSI 15, MISO 12, CS 11.
//! - The Pico runs at 200 MHz: the VM needs the cycles.
//! - DateTime counts from 2026-01-01 at boot, and so do the card's file
//!   times.
//!
//! The screen vector runs 60 times a second, after the input vectors.
#![no_std]
#![no_main]

mod audio;
mod file;
mod menu;
mod mouse;
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
    peripherals::{DMA_CH0, PIO0},
    pio::{self as rp_pio, Pio},
    pwm::{self, Pwm},
    spi::{self, Spi},
    uart::{self, Uart},
};
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Delay, Duration, Instant, Timer};
use embedded_hal_bus::spi::{ExclusiveDevice, NoDelay};
use embedded_sdmmc::{SdCard, TimeSource, Timestamp, sdcard::spi::AcquireOpts};
use panic_probe as _;

use crate::{
    audio::{Audio, VOICES},
    file::Disk,
    menu::{Action, Kind, Menu},
    mouse::{PACKETS, Packet},
    screen::Screen,
    uxn::{Devices, Uxn},
};

bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
    PIO0_IRQ_0 => rp_pio::InterruptHandler<PIO0>;
});

static ROM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/varvara.rom"));
/// The built-in ROM's file name, without `.rom`.
const ROM_NAME: &str = env!("VARVARA_ROM_NAME");

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

/// The SD card on SPI1, and the File devices on it.
type Card = Disk<ExclusiveDevice<Spi<'static, Blocking>, Output<'static>, NoDelay>, Delay, Clock>;

/// Varvara's devices round the VM.
struct Varvara {
    screen: &'static mut Screen,
    uart: Uart<'static, Blocking>,
    disk: Card,
}

fn peek2(m: &[u8], a: usize) -> usize {
    (m[a & 0xffff] as usize) << 8 | m[(a + 1) & 0xffff] as usize
}

/// 2026-01-01 00:00:00, a Thursday, plus the time since boot: days since
/// 1970-01-01, and the seconds into the day.
fn now() -> (i64, u64) {
    let secs = Instant::now().as_secs();
    ((secs / 86400) as i64 + 20454, secs % 86400)
}

/// The card's file times.
struct Clock;

impl TimeSource for Clock {
    fn get_timestamp(&self) -> Timestamp {
        let (days, t) = now();
        let (year, month, day) = civil(days);
        let (h, m, s) = ((t / 3600) as u8, (t / 60 % 60) as u8, (t % 60) as u8);
        Timestamp::from_calendar(year as u16, month as u8, day as u8, h, m, s)
            .unwrap_or(Timestamp::from_fat(0x21, 0))
    }
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
        let (days, t) = now();
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
            0xa0..=0xbf => {
                let base = (port & 0xf0) as usize;
                let d = &mut u.dev[base..base + 16];
                self.disk
                    .deo((port >> 4) as usize - 0xa, &mut u.ram, d, port & 0xf);
            }
            _ => {}
        }
    }
}

fn vector(u: &Uxn, port: usize) -> u16 {
    (u.dev[port] as u16) << 8 | u.dev[port + 1] as u16
}

/// The Mouse device: the pointer moves within the screen, and the vector
/// runs with the buttons and the scroll, which is cleared after.
fn mouse(vm: &mut Varvara, u: &mut Uxn, p: Packet) {
    let (w, h) = (vm.screen.w as i32, vm.screen.h as i32);
    let x = (peek2(&u.dev, 0x92) as i32 + p.dx as i32).clamp(0, w - 1);
    let y = (peek2(&u.dev, 0x94) as i32 + p.dy as i32).clamp(0, h - 1);
    u.dev[0x92..0x94].copy_from_slice(&(x as u16).to_be_bytes());
    u.dev[0x94..0x96].copy_from_slice(&(y as u16).to_be_bytes());
    u.dev[0x96] = p.buttons;
    u.dev[0x9c..0x9e].copy_from_slice(&p.scroll.to_be_bytes());
    u.eval(vm, vector(u, 0x90));
    u.dev[0x9c..0x9e].fill(0);
}

// ---------------------------------------------------------------- main ----

const FRAME: Duration = Duration::from_micros(16_667);
/// The card's clock once it is up (it starts at 400 kHz, as SD cards must).
const SD_HZ: u32 = 20_000_000;

/// The eight buttons, in the order of the controller's bits: A, B, Select,
/// Start, Up, Down, Left, Right.
struct Buttons([Input<'static>; 8]);

impl Buttons {
    fn held(&self) -> u8 {
        self.0
            .iter()
            .enumerate()
            .fold(0u8, |b, (i, k)| b | (k.is_low() as u8) << i)
    }
}

/// Select and Start held together for a second: back to the menu.
struct Back(Option<Instant>);

impl Back {
    const BOTH: u8 = 0x04 | menu::START;

    fn check(&mut self, held: u8) -> bool {
        if held & Self::BOTH != Self::BOTH {
            self.0 = None;
            return false;
        }
        let since = *self.0.get_or_insert_with(Instant::now);
        Instant::now() - since >= Duration::from_secs(1)
    }
}

/// Where the ROM to run comes from.
enum Source {
    Builtin,
    /// The card: it is in memory already.
    Card,
}

/// Lists the menu's directory: its subdirectories and `.rom` files. False
/// if it isn't there (any more).
fn list(menu: &mut Menu, disk: &Card, builtin: Option<&str>) -> bool {
    menu.clear(builtin);
    let dir = menu.dir.clone();
    let ok = disk.list(dir.as_bytes(), |e, name| {
        let rom = name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(b".rom");
        let hidden = e.attributes.is_hidden() || name.first() == Some(&b'.');
        if let Ok(name) = core::str::from_utf8(name)
            && !hidden
        {
            if e.attributes.is_directory() {
                menu.add(Kind::Dir, name);
            } else if rom {
                menu.add(Kind::Rom, name);
            }
        }
        core::ops::ControlFlow::Continue(())
    });
    menu.sort();
    ok
}

/// Shows the menu until a ROM is picked; a card's is loaded into memory.
async fn choose(
    menu: &mut Menu,
    panel: &mut Panel,
    buttons: &Buttons,
    vm: &mut Varvara,
    u: &mut Uxn,
    builtin: Option<&str>,
) -> Source {
    u.reset();
    vm.screen.reset();
    vm.screen.boot(&mut u.dev, PW, PH);
    // Its own palette: dark blue, white, light blue and grey.
    for (port, v) in [(0x08, 0x1e37u16), (0x0a, 0x1eb7), (0x0c, 0x2ef7)] {
        u.dev[port..port + 2].copy_from_slice(&v.to_be_bytes());
    }
    vm.screen.colorize(&u.dev);
    // Back on the ROM that ran last, unless the card changed so much that
    // the directory the menu was in is gone.
    let last: Option<heapless::String<{ menu::NAME }>> =
        menu.entries.get(menu.sel).map(|e| e.name.clone());
    if !list(menu, &vm.disk, builtin) {
        menu.dir.clear();
        list(menu, &vm.disk, builtin);
    }
    if let Some(name) = last {
        menu.select(&name);
    }

    let mut note: heapless::String<64> = heapless::String::new();
    // Buttons still held from before (A, or Select and Start) do nothing
    // until let go.
    let mut prev = buttons.held();
    let mut repeat_at = Instant::now();
    let mut redraw = true;
    loop {
        let held = buttons.held();
        let pressed = held & !prev;
        prev = held;
        // Up and Down repeat after 400 ms held, every 80 ms.
        let updown = held & (menu::UP | menu::DOWN);
        let mut repeat = 0;
        if pressed & updown != 0 {
            repeat_at = Instant::now() + Duration::from_millis(400);
        } else if updown != 0 && Instant::now() >= repeat_at {
            repeat_at = Instant::now() + Duration::from_millis(80);
            repeat = updown;
        }
        if pressed | repeat != 0 {
            redraw = true;
            note.clear();
        }
        let mut up = false;
        match menu.input(pressed, repeat) {
            Action::None => {}
            Action::Back => up = true,
            Action::Open(i) => {
                let e = &menu.entries[i];
                match e.kind {
                    Kind::Builtin => return Source::Builtin,
                    Kind::Up => up = true,
                    Kind::Dir => {
                        let name = e.name.clone();
                        let len = menu.dir.len();
                        let ok = (len == 0 || menu.dir.push('/').is_ok())
                            && menu.dir.push_str(&name).is_ok();
                        if !ok || !list(menu, &vm.disk, builtin) {
                            menu.dir.truncate(len);
                            list(menu, &vm.disk, builtin);
                            let _ = note.push_str("Can't open that directory");
                        }
                    }
                    Kind::Rom => {
                        let mut path: heapless::String<320> = heapless::String::new();
                        if !menu.dir.is_empty() {
                            let _ = path.push_str(&menu.dir);
                            let _ = path.push('/');
                        }
                        let _ = path.push_str(&e.name);
                        u.reset();
                        match vm.disk.load(path.as_bytes(), &mut u.ram[0x100..]) {
                            Some(n) if n > 0 => {
                                info!("varvara: {=str}, {=usize} bytes", path.as_str(), n);
                                return Source::Card;
                            }
                            _ => {
                                let _ = note.push_str("Can't read ");
                                for c in e.name.chars() {
                                    if note.push(c).is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if up {
            // Back to the parent, on the directory just left.
            let cut = menu.dir.rfind('/').unwrap_or(0);
            let skip = (cut > 0) as usize;
            let left: heapless::String<{ menu::NAME }> =
                menu.dir[cut + skip..].chars().take(menu::NAME).collect();
            menu.dir.truncate(cut);
            list(menu, &vm.disk, builtin);
            menu.select(&left);
            redraw = true;
        }
        if redraw {
            menu.draw(vm.screen, &note);
            redraw = false;
        }
        panel.flush(vm.screen).await;
        Timer::after(FRAME).await;
    }
}

/// What `run` stopped for.
enum Stop {
    Halted,
    Back,
}

/// Runs the program in memory from its start, 60 frames a second, until it
/// halts or Select and Start are held.
async fn run(panel: &mut Panel, buttons: &Buttons, vm: &mut Varvara, u: &mut Uxn) -> Stop {
    AUDIO.lock(|a| *a.borrow_mut() = Audio::new(RATE));
    vm.screen.reset();
    vm.screen.boot(&mut u.dev, PW, PH);
    // The pointer starts in the middle, and the mouse's moves from before
    // (in the menu) are dropped.
    while PACKETS.try_receive().is_ok() {}
    let (x, y) = (vm.screen.w as u16 / 2, vm.screen.h as u16 / 2);
    u.dev[0x92..0x94].copy_from_slice(&x.to_be_bytes());
    u.dev[0x94..0x96].copy_from_slice(&y.to_be_bytes());
    // A blank screen meanwhile, as some programs draw a while before
    // their first frame (mandelbrot takes seconds).
    panel.flush(vm.screen).await;
    u.eval(vm, 0x100);

    /// How often the time a frame takes is logged.
    const FRAMES: u32 = 600;
    let mut back = Back(None);
    let mut next = Instant::now();
    let (mut vm_time, mut draw_time, mut frames) = (0u64, 0u64, 0u32);
    loop {
        let t0 = Instant::now();
        let held = buttons.held();
        if back.check(held) {
            return Stop::Back;
        }
        if held != u.dev[0x82] {
            u.dev[0x82] = held;
            u.eval(vm, vector(u, 0x80));
        }
        while let Ok(p) = PACKETS.try_receive() {
            mouse(vm, u, p);
        }
        while vm.uart.read_ready() {
            let mut c = [0u8];
            if vm.uart.blocking_read(&mut c).is_err() {
                break;
            }
            if vector(u, 0x10) != 0 {
                (u.dev[0x12], u.dev[0x17]) = (c[0], 1);
                u.eval(vm, vector(u, 0x10));
            } else {
                u.dev[0x83] = c[0];
                u.eval(vm, vector(u, 0x80));
                u.dev[0x83] = 0;
            }
        }
        let done = AUDIO.lock(|a| a.borrow_mut().take_done());
        for n in 0..VOICES {
            if done & 1 << n != 0 {
                u.eval(vm, vector(u, 0x30 + 0x10 * n));
            }
        }
        u.eval(vm, vector(u, 0x20));
        let t1 = Instant::now();
        panel.flush(vm.screen).await;
        vm_time += (t1 - t0).as_micros();
        draw_time += (Instant::now() - t1).as_micros();

        if u.halted() {
            return Stop::Halted;
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

#[embassy_executor::main]
async fn main(spawner: Spawner) {
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

    spawner.spawn(mouse::task(Pio::new(p.PIO0, Irqs), p.PIN_20, p.PIN_21, SYS_HZ).unwrap());

    let buttons = Buttons([
        Input::new(p.PIN_6, Pull::Up),
        Input::new(p.PIN_7, Pull::Up),
        Input::new(p.PIN_8, Pull::Up),
        Input::new(p.PIN_10, Pull::Up),
        Input::new(p.PIN_2, Pull::Up),
        Input::new(p.PIN_3, Pull::Up),
        Input::new(p.PIN_4, Pull::Up),
        Input::new(p.PIN_5, Pull::Up),
    ]);

    let mut sd_cfg = spi::Config::default();
    sd_cfg.frequency = embassy_rp::time::Hertz(400_000);
    let sd_spi = Spi::new_blocking(p.SPI1, p.PIN_14, p.PIN_15, p.PIN_12, sd_cfg).unwrap();
    let sd = ExclusiveDevice::new_no_delay(sd_spi, Output::new(p.PIN_11, Level::High)).unwrap();
    // A card answers within a try or two; each takes about 300 ms, so an
    // empty slot holds the built-in ROM back for a second, not 15 (the
    // default 50 tries).
    let opts = AcquireOpts {
        acquire_retries: 3,
        ..AcquireOpts::default()
    };
    let disk = Disk::new(SdCard::new_with_options(sd, Delay, opts), Clock);

    let uart = Uart::new_blocking(p.UART0, p.PIN_0, p.PIN_1, uart::Config::default());
    // SAFETY: main is the only one to take these; the sound interrupt reads
    // memory through a raw pointer.
    let (u, screen) = unsafe { (&mut *addr_of_mut!(UXN), &mut *addr_of_mut!(SCREEN)) };
    let mut vm = Varvara { screen, uart, disk };
    let mut menu = Menu::new();
    let builtin = (!ROM.is_empty()).then_some(ROM_NAME);
    if let Some(name) = builtin {
        info!(
            "VARVARA: {=str}, a {=usize}-byte ROM, built in",
            name,
            ROM.len()
        );
    }

    let mut told = false;
    loop {
        let set_hz = |vm: &Varvara, hz| {
            let _ = vm
                .disk
                .with_spi(|d| d.bus_mut().set_frequency(embassy_rp::time::Hertz(hz)));
        };
        set_hz(&vm, 400_000);
        let source = if vm.disk.mount() {
            set_hz(&vm, SD_HZ);
            choose(&mut menu, &mut panel, &buttons, &mut vm, u, builtin).await
        } else if builtin.is_some() {
            Source::Builtin
        } else {
            // Nothing to run: look for a card again in a second.
            if !told {
                warn!(
                    "varvara: no ROM built in (run roms/fetch.sh, or set VARVARA_ROM) and no card"
                );
                let _ = vm
                    .uart
                    .blocking_write(b"varvara: no ROM built in, and no card\r\n");
                told = true;
            }
            Timer::after_secs(1).await;
            continue;
        };
        if let Source::Builtin = source {
            u.reset();
            u.load(ROM);
        }
        if let Stop::Halted = run(&mut panel, &buttons, &mut vm, u).await {
            info!("varvara: halted ({=u8:02x})", u.dev[0x0f]);
            let _ = vm.uart.blocking_write(b"varvara: halted\r\n");
            // With no menu to go back to, it stays as it ended until asked
            // to start again.
            if !vm.disk.mounted() {
                let mut back = Back(None);
                while !back.check(buttons.held()) {
                    Timer::after(FRAME).await;
                }
            }
        }
    }
}
