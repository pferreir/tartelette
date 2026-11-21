//! picosh: a tiny shell on UART0 (TX GPIO0, RX GPIO1, 115200 8N1) for a
//! plain Raspberry Pi Pico.
//!
//! Type `help`. Besides playing with the LED, it puts both cores to work:
//! `mandel` draws the Mandelbrot set in ASCII, once on core 0 alone and
//! then with core 1 doing every other row, and says how long each took
//! (core 1 is started at boot and waits for work in a spin loop).
//! `peek` reads memory, the bootrom included.
#![no_std]
#![no_main]

use core::cell::UnsafeCell;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use defmt::info;
use embassy_executor::Spawner;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::multicore::{spawn_core1, Stack};
use embassy_rp::peripherals::UART0;
use embassy_rp::uart::{self, BufferedInterruptHandler, BufferedUart};
use embassy_rp::bind_interrupts;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Instant, Timer};
use embedded_io_async::{Read, Write};
use heapless::String;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    UART0_IRQ => BufferedInterruptHandler<UART0>;
});

// ------------------------------------------------------------ the mandel ----

const COLS: usize = 72;
const ROWS: usize = 24;
const RAMP: &[u8] = b" .:-=+*#%@";

/// The picture, written by both cores, a row each at a time.
struct Picture(UnsafeCell<[[u8; COLS]; ROWS]>);
unsafe impl Sync for Picture {}
static PICTURE: Picture = Picture(UnsafeCell::new([[b' '; COLS]; ROWS]));

/// Work for core 1: the iteration limit, or 0 for none. Core 1 clears it when
/// it is done.
static JOB: AtomicU32 = AtomicU32::new(0);
static CORE1_UP: AtomicBool = AtomicBool::new(false);

/// One row of the set, in Q13 fixed point (the M0+ has no FPU).
fn mandel_row(row: usize, limit: u32) {
    const ONE: i32 = 1 << 13;
    // Row centres, so that no row lies exactly on the real axis.
    let ci = ((2 * row as i32 + 1) * ONE * 12 / 10) / ROWS as i32 - ONE * 12 / 10;
    let mut out = [b' '; COLS];
    for (col, cell) in out.iter_mut().enumerate() {
        let cr = (col as i32 * 3 * ONE) / COLS as i32 - 2 * ONE - ONE / 4;
        let (mut x, mut y, mut n) = (0i32, 0i32, 0u32);
        while n < limit {
            let (xx, yy) = ((x * x) >> 13, (y * y) >> 13);
            if xx + yy > 4 * ONE {
                break;
            }
            y = ((x * y) >> 12) + ci;
            x = xx - yy + cr;
            n += 1;
        }
        *cell = if n >= limit { b'@' } else { RAMP[(n as usize * (RAMP.len() - 1)) / limit as usize] };
    }
    // SAFETY: each row is written by one core only, and read once both are done.
    unsafe { (*PICTURE.0.get())[row] = out };
}

fn core1_main() -> ! {
    CORE1_UP.store(true, Ordering::Release);
    loop {
        let limit = JOB.load(Ordering::Acquire);
        if limit == 0 {
            // Sleep until core 0 posts work (it sends an event after).
            cortex_m::asm::wfe();
            continue;
        }
        for row in (1..ROWS).step_by(2) {
            mandel_row(row, limit);
        }
        JOB.store(0, Ordering::Release);
    }
}

fn mandel(limit: u32, both: bool) -> u64 {
    let t0 = Instant::now();
    if both {
        JOB.store(limit, Ordering::Release);
        cortex_m::asm::sev();
        for row in (0..ROWS).step_by(2) {
            mandel_row(row, limit);
        }
        while JOB.load(Ordering::Acquire) != 0 {}
    } else {
        for row in 0..ROWS {
            mandel_row(row, limit);
        }
    }
    t0.elapsed().as_micros()
}

// ------------------------------------------------------------------ LED ----

#[derive(Clone, Copy)]
enum Led {
    Set(bool),
    Toggle,
    Blink(u32),
}

static LED: Signal<CriticalSectionRawMutex, Led> = Signal::new();

#[embassy_executor::task]
async fn led_task(mut led: Output<'static>) {
    let mut blinking = 0u32;
    loop {
        let cmd = if blinking > 0 {
            match embassy_futures::select::select(LED.wait(), Timer::after_millis(150)).await {
                embassy_futures::select::Either::First(c) => Some(c),
                embassy_futures::select::Either::Second(()) => None,
            }
        } else {
            Some(LED.wait().await)
        };
        match cmd {
            Some(Led::Set(on)) => {
                blinking = 0;
                led.set_level(Level::from(on));
            }
            Some(Led::Toggle) => {
                blinking = 0;
                led.toggle();
            }
            Some(Led::Blink(n)) => {
                blinking = n * 2;
                led.set_low();
            }
            None => {
                led.toggle();
                blinking -= 1;
            }
        }
    }
}

// ---------------------------------------------------------------- shell ----

type Out = String<4096>;

const BANNER: &str = "\r\n\
   .-~~~~~~~-.     picosh, on tartelette\r\n\
  (  o  o  o  )    an RP2040 shell on UART0\r\n\
   \\_________/     type 'help'\r\n\r\n";

const HELP: &str = "\
help             this\r\n\
led on|off|t     the LED on GPIO25 (t toggles)\r\n\
blink [N]        blink it N times (default 5)\r\n\
mandel [LIMIT]   the Mandelbrot set, on one core then on two\r\n\
primes [N]       the primes below N (default 200, max 20000)\r\n\
fib N            the Nth Fibonacci number (u128, N <= 186)\r\n\
peek ADDR [LEN]  hex dump: try 'peek 0' for the bootrom\r\n\
info             the chip, the clocks, uptime\r\n\
echo TEXT        say it back\r\n";

fn parse_num(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).ok()
    } else {
        s.parse().ok()
    }
}

fn readable(addr: u32, len: u32) -> bool {
    let end = addr.saturating_add(len);
    [(0x0000_0000, 0x0000_4000), (0x1000_0000, 0x1020_0000), (0x2000_0000, 0x2004_2000)]
        .iter()
        .any(|&(a, b)| addr >= a && end <= b)
}

async fn run(line: &str, out: &mut Out) {
    let mut words = line.split_whitespace();
    let Some(cmd) = words.next() else { return };
    let arg = words.next();
    match cmd {
        "help" | "?" => out.push_str(HELP).ok().unwrap_or(()),
        "led" => match arg {
            Some("on") => LED.signal(Led::Set(true)),
            Some("off") => LED.signal(Led::Set(false)),
            Some("t") | Some("toggle") => LED.signal(Led::Toggle),
            _ => out.push_str("led on|off|t\r\n").unwrap_or(()),
        },
        "blink" => {
            let n = arg.and_then(parse_num).unwrap_or(5).clamp(1, 1000);
            LED.signal(Led::Blink(n));
            let _ = write!(out, "blinking {n} times\r\n");
        }
        "mandel" => {
            let limit = arg.and_then(parse_num).unwrap_or(64).clamp(2, 4096);
            let one = mandel(limit, false);
            let two = mandel(limit, true);
            // SAFETY: both cores are done with it.
            let pic = unsafe { &*PICTURE.0.get() };
            for row in pic {
                let _ = out.push_str(core::str::from_utf8(row).unwrap_or(""));
                let _ = out.push_str("\r\n");
            }
            let _ = write!(
                out,
                "limit {limit}: core 0 alone {} ms, cores 0+1 {} ms ({}.{:02}x)\r\n",
                one / 1000,
                two / 1000,
                one / two.max(1),
                (one * 100 / two.max(1)) % 100
            );
        }
        "primes" => {
            const MAX: usize = 20_000;
            static mut SIEVE: [bool; MAX] = [false; MAX];
            // SAFETY: only this command, on this core, ever touches it.
            let sieve = unsafe { &mut *core::ptr::addr_of_mut!(SIEVE) };
            let n = arg.and_then(parse_num).unwrap_or(200).clamp(2, MAX as u32) as usize;
            let t0 = Instant::now();
            sieve[..n].fill(true);
            sieve[0] = false;
            sieve[1] = false;
            let mut i = 2;
            while i * i < n {
                if sieve[i] {
                    (i * i..n).step_by(i).for_each(|j| sieve[j] = false);
                }
                i += 1;
            }
            let us = t0.elapsed().as_micros();
            let count = sieve[..n].iter().filter(|&&p| p).count();
            let mut col = 0;
            for (p, _) in sieve[..n].iter().enumerate().filter(|(_, &p)| p) {
                if out.len() > 3500 {
                    let _ = out.push_str("...");
                    break;
                }
                let _ = write!(out, "{p:>6}");
                col += 1;
                if col % 12 == 0 {
                    let _ = out.push_str("\r\n");
                }
            }
            let _ = write!(out, "\r\n{count} primes below {n}, sieved in {us} us\r\n");
        }
        "fib" => match arg.and_then(parse_num) {
            Some(n) if n <= 186 => {
                let (mut a, mut b) = (0u128, 1u128);
                for _ in 0..n {
                    (a, b) = (b, a + b);
                }
                let _ = write!(out, "fib({n}) = {a}\r\n");
            }
            _ => out.push_str("fib N, with N up to 186\r\n").unwrap_or(()),
        },
        "peek" => {
            let addr = arg.and_then(parse_num);
            let len = words.next().and_then(parse_num).unwrap_or(64).clamp(1, 256);
            match addr {
                Some(addr) if readable(addr, len) => {
                    for line in (addr..addr + len).step_by(16) {
                        let _ = write!(out, "{line:08x} ");
                        let mut ascii: String<16> = String::new();
                        for a in line..(line + 16).min(addr + len) {
                            // SAFETY: inside the bootrom, flash or SRAM.
                            let b = unsafe { core::ptr::read_volatile(a as *const u8) };
                            let _ = write!(out, " {b:02x}");
                            let _ = ascii.push(if (0x20..0x7f).contains(&b) { b as char } else { '.' });
                        }
                        let _ = write!(out, "  {ascii}\r\n");
                    }
                }
                _ => out.push_str("peek ADDR [LEN]: bootrom 0x0, flash 0x10000000, SRAM 0x20000000\r\n").unwrap_or(()),
            }
        }
        "info" => {
            let up = Instant::now().as_millis();
            // The bootrom's version byte and the chip's revision.
            let rom = unsafe { core::ptr::read_volatile(0x13 as *const u8) };
            let chip_id = embassy_rp::pac::SYSINFO.chip_id().read();
            let _ = write!(
                out,
                "RP2040 rev B{}, bootrom v{rom}, part {:#x}\r\n\
                 clk_sys {} MHz, core 1 {}\r\n\
                 up {}.{:03} s\r\n",
                chip_id.revision().saturating_sub(1),
                chip_id.part(),
                embassy_rp::clocks::clk_sys_freq() / 1_000_000,
                if CORE1_UP.load(Ordering::Acquire) { "running" } else { "stopped" },
                up / 1000,
                up % 1000
            );
        }
        "echo" => {
            let rest = line.trim_start().strip_prefix("echo").unwrap_or("").trim();
            let _ = write!(out, "{rest}\r\n");
        }
        _ => {
            let _ = write!(out, "{cmd}: unknown command, try 'help'\r\n");
        }
    }
}

static mut CORE1_STACK: Stack<4096> = Stack::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    spawn_core1(p.CORE1, unsafe { &mut *core::ptr::addr_of_mut!(CORE1_STACK) }, core1_main);
    spawner.spawn(led_task(Output::new(p.PIN_25, Level::Low)).unwrap());

    static TX: StaticCell<[u8; 1024]> = StaticCell::new();
    static RX: StaticCell<[u8; 64]> = StaticCell::new();
    let mut uart = BufferedUart::new(
        p.UART0,
        p.PIN_0,
        p.PIN_1,
        Irqs,
        TX.init([0; 1024]),
        RX.init([0; 64]),
        uart::Config::default(),
    );
    info!("picosh up");

    let _ = uart.write_all(BANNER.as_bytes()).await;
    LED.signal(Led::Blink(2));
    let mut line: String<80> = String::new();
    let mut out = Out::new();
    let mut last_cr = false;
    let _ = uart.write_all(b"> ").await;
    loop {
        let mut buf = [0u8; 16];
        let Ok(n) = uart.read(&mut buf).await else { continue };
        for &c in &buf[..n] {
            match c {
                b'\n' if last_cr => {}
                b'\r' | b'\n' => {
                    let _ = uart.write_all(b"\r\n").await;
                    info!("> {}", line.as_str());
                    out.clear();
                    run(&line, &mut out).await;
                    // Out in chunks, so the TX buffer never has to hold it all.
                    for chunk in out.as_bytes().chunks(256) {
                        let _ = uart.write_all(chunk).await;
                    }
                    line.clear();
                    let _ = uart.write_all(b"> ").await;
                }
                0x08 | 0x7f => {
                    if line.pop().is_some() {
                        let _ = uart.write_all(b"\x08 \x08").await;
                    }
                }
                0x20..=0x7e => {
                    if line.push(c as char).is_ok() {
                        let _ = uart.write_all(&[c]).await;
                    }
                }
                _ => {}
            }
            last_cr = c == b'\r';
        }
    }
}
