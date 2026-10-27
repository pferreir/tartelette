//! Bare-metal test firmware for SPI0 and the DMA controller against an
//! ST7789 display (CS=GPIO9, D/C=GPIO13, RST=GPIO17):
//!
//! * PL022 loopback mode,
//! * a CPU-driven fill of a 10x5 window with red (0xF800) at (0,0),
//! * a DMA-driven fill of a 20x10 window with green (0x07E0) at (20,10),
//!   completed by DMA_IRQ_0 while the core sleeps,
//! * a full-duplex DMA read of the display ID (RX and TX channels),
//! * memory-to-memory DMA, and a chained pair of channels.
//!
//! Expected output:
//! "start\nloopback ok\ncpu draw sent\ndma draw ok\ndma rx ok\nmemcpy ok\nchain ok\nspi done\n"
#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::ptr::{addr_of_mut, read_volatile, write_volatile};

global_asm!(
    ".section .vectors, \"a\"",
    ".word _stack_top",
    ".word reset",
    ".word default_handler", // NMI
    ".word default_handler", // HardFault
    ".rept 7",
    ".word 0",
    ".endr",
    ".word default_handler", // SVCall
    ".word 0",
    ".word 0",
    ".word default_handler", // PendSV
    ".word default_handler", // SysTick
    ".rept 11",
    ".word default_handler", // IRQ 0..10
    ".endr",
    ".word dma_irq0", // IRQ 11
);

#[used]
#[unsafe(link_section = ".boot2")]
static BOOT2: [u8; 256] = [0; 256];

const RESETS: u32 = 0x4000_C000;
const IO_BANK0: u32 = 0x4001_4000;
const SPI0: u32 = 0x4003_C000;
const UART0: u32 = 0x4003_4000;
const DMA: u32 = 0x5000_0000;
const SIO: u32 = 0xD000_0000;
const NVIC_ISER: u32 = 0xE000_E100;

const SIO_OUT_SET: u32 = SIO + 0x14;
const SIO_OUT_CLR: u32 = SIO + 0x18;
const SIO_OE_SET: u32 = SIO + 0x24;

const SPI_CR0: u32 = SPI0;
const SPI_CR1: u32 = SPI0 + 0x04;
const SPI_DR: u32 = SPI0 + 0x08;
const SPI_SR: u32 = SPI0 + 0x0C;
const SPI_CPSR: u32 = SPI0 + 0x10;
const SPI_DMACR: u32 = SPI0 + 0x24;

const CS: u32 = 9;
const DC: u32 = 13;
const RST: u32 = 17;

// DMA control word pieces
const EN: u32 = 1;
const INCR_READ: u32 = 1 << 4;
const INCR_WRITE: u32 = 1 << 5;
const TREQ_SPI0_TX: u32 = 16;
const TREQ_SPI0_RX: u32 = 17;
const TREQ_PERMANENT: u32 = 0x3F;
const BUSY: u32 = 1 << 24;

static mut DMA_IRQS: u32 = 0;
static mut FILL: [u8; 400] = [0; 400];
static mut RX: [u8; 4] = [0xEE; 4];
static mut ZEROS: [u8; 4] = [0; 4];
static mut SRC: [u32; 16] = [0; 16];
static mut DST: [u32; 16] = [0; 16];
static mut MID: [u32; 4] = [0; 4];
static mut OUT: [u32; 4] = [0; 4];

fn wr(addr: u32, v: u32) {
    unsafe { write_volatile(addr as *mut u32, v) }
}
fn rd(addr: u32) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}
fn putc(c: u8) {
    while rd(UART0 + 0x18) & (1 << 5) != 0 {}
    wr(UART0, c as u32);
}
fn puts(s: &[u8]) {
    for &c in s {
        putc(c);
    }
}
fn check(ok: bool, pass: &[u8], fail: &[u8]) {
    puts(if ok { pass } else { fail });
}

fn pin_set(pin: u32, high: bool) {
    wr(if high { SIO_OUT_SET } else { SIO_OUT_CLR }, 1 << pin);
}
fn gpio_func(pin: u32, func: u32) {
    wr(IO_BANK0 + 8 * pin + 4, func);
}

fn spi_put(b: u8) {
    while rd(SPI_SR) & 2 == 0 {} // TNF
    wr(SPI_DR, b as u32);
}
fn spi_wait_idle() {
    while rd(SPI_SR) & 1 == 0 || rd(SPI_SR) & 0x10 != 0 {} // TFE and not BSY
}
fn spi_drain() {
    while rd(SPI_SR) & 4 != 0 {
        rd(SPI_DR);
    }
}

fn command(c: u8) {
    pin_set(DC, false);
    pin_set(CS, false);
    spi_put(c);
    spi_wait_idle();
    pin_set(CS, true);
}
fn command_with(c: u8, data: &[u8]) {
    pin_set(DC, false);
    pin_set(CS, false);
    spi_put(c);
    spi_wait_idle();
    pin_set(DC, true);
    for &b in data {
        spi_put(b);
    }
    spi_wait_idle();
    pin_set(CS, true);
}
fn window(x0: u8, x1: u8, y0: u8, y1: u8) {
    command_with(0x2A, &[0, x0, 0, x1]);
    command_with(0x2B, &[0, y0, 0, y1]);
}

/// Start `channel` copying `count` items.
fn dma_start(channel: u32, read: u32, write: u32, count: u32, ctrl: u32) {
    let base = DMA + channel * 0x40;
    wr(base, read);
    wr(base + 0x04, write);
    wr(base + 0x08, count);
    wr(base + 0x0C, ctrl); // CTRL_TRIG
}
fn dma_wait(channel: u32) {
    while rd(DMA + channel * 0x40 + 0x0C) & BUSY != 0 {}
}

#[unsafe(no_mangle)]
extern "C" fn default_handler() {
    puts(b"unexpected exception\n");
    unsafe { asm!("bkpt #2") };
    loop {}
}

#[unsafe(no_mangle)]
extern "C" fn dma_irq0() {
    wr(DMA + 0x400, 1); // INTR: write 1 to clear channel 0
    unsafe { write_volatile(&raw mut DMA_IRQS, read_volatile(&raw const DMA_IRQS) + 1) };
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    // DMA, IO_BANK0, PADS_BANK0, SPI0 and UART0 out of reset.
    let release = 1 << 2 | 1 << 5 | 1 << 8 | 1 << 16 | 1 << 22;
    wr(RESETS + 0x3000, release);
    while rd(RESETS + 0x8) & release != release {}
    wr(UART0 + 0x30, 0x301);
    puts(b"start\n");

    // Pins: SPI0 SCK/TX/RX on 18/19/16, the rest plain outputs.
    for pin in [16, 18, 19] {
        gpio_func(pin, 1);
    }
    for pin in [CS, DC, RST] {
        gpio_func(pin, 5);
        wr(SIO_OE_SET, 1 << pin);
    }
    pin_set(CS, true);
    pin_set(DC, true);
    pin_set(RST, true);

    // 8-bit frames, enabled.
    wr(SPI_CR0, 7);
    wr(SPI_CPSR, 2);
    wr(SPI_CR1, 1 << 1);

    // Loopback: what goes out comes straight back.
    wr(SPI_CR1, 1 << 1 | 1);
    wr(SPI_DR, 0xA5);
    spi_wait_idle();
    let looped = rd(SPI_SR) & 4 != 0 && rd(SPI_DR) == 0xA5;
    wr(SPI_CR1, 1 << 1);
    check(looped, b"loopback ok\n", b"loopback bad\n");

    // Reset the panel with its pin, then wake it with RGB565 pixels.
    pin_set(RST, false);
    pin_set(RST, true);
    command(0x01); // SWRESET
    command(0x11); // SLPOUT
    command_with(0x3A, &[0x55]); // COLMOD: 16 bpp
    command(0x29); // DISPON

    // CPU-driven fill: 10x5 red at (0,0).
    window(0, 9, 0, 4);
    pin_set(DC, false);
    pin_set(CS, false);
    spi_put(0x2C); // RAMWR
    spi_wait_idle();
    pin_set(DC, true);
    for _ in 0..50 {
        spi_put(0xF8);
        spi_put(0x00);
    }
    spi_wait_idle();
    pin_set(CS, true);
    spi_drain();
    puts(b"cpu draw sent\n");

    // DMA-driven fill: 20x10 green at (20,10), 400 bytes through the TX FIFO.
    unsafe {
        let fill = addr_of_mut!(FILL) as *mut u8;
        for i in 0..200 {
            *fill.add(2 * i) = 0x07;
            *fill.add(2 * i + 1) = 0xE0;
        }
    }
    window(20, 39, 10, 19);
    pin_set(DC, false);
    pin_set(CS, false);
    spi_put(0x2C);
    spi_wait_idle();
    pin_set(DC, true);
    wr(SPI_DMACR, 2); // TXDMAE
    wr(DMA + 0x404, 1); // INTE0: channel 0
    wr(NVIC_ISER, 1 << 11);
    dma_start(0, addr_of_mut!(FILL) as u32, SPI_DR, 400, EN | INCR_READ | TREQ_SPI0_TX << 15);
    let mut spins = 0;
    while unsafe { read_volatile(&raw const DMA_IRQS) } == 0 && spins < 1000 {
        unsafe { asm!("wfi") };
        spins += 1;
    }
    spi_wait_idle();
    pin_set(CS, true);
    spi_drain();
    check(
        unsafe { read_volatile(&raw const DMA_IRQS) } == 1 && rd(DMA + 0x400) == 0 && rd(DMA + 0x0C) & BUSY == 0,
        b"dma draw ok\n",
        b"dma draw bad\n",
    );
    wr(DMA + 0x404, 0);

    // Full duplex: send RDDID, then clock four bytes in while four zeros go
    // out, with CS held low throughout. RX channel first, then TX.
    pin_set(DC, false);
    pin_set(CS, false);
    spi_put(0x04);
    spi_wait_idle();
    pin_set(DC, true);
    spi_drain();
    wr(SPI_DMACR, 3);
    dma_start(1, SPI_DR, addr_of_mut!(RX) as u32, 4, EN | INCR_WRITE | TREQ_SPI0_RX << 15 | 1 << 11);
    dma_start(2, addr_of_mut!(ZEROS) as u32, SPI_DR, 4, EN | INCR_READ | TREQ_SPI0_TX << 15 | 2 << 11);
    dma_wait(1);
    spi_wait_idle();
    pin_set(CS, true);
    wr(SPI_DMACR, 0);
    let rx = unsafe { read_volatile(&raw const RX) };
    check(rx == [0x85, 0x85, 0x52, 0x00], b"dma rx ok\n", b"dma rx bad\n");

    // Memory to memory, word sized, always ready.
    unsafe {
        let src = addr_of_mut!(SRC) as *mut u32;
        for i in 0..16 {
            write_volatile(src.add(i), 0x1000_0000 + i as u32 * 0x0101);
        }
    }
    dma_start(
        3,
        addr_of_mut!(SRC) as u32,
        addr_of_mut!(DST) as u32,
        16,
        EN | 2 << 2 | INCR_READ | INCR_WRITE | TREQ_PERMANENT << 15 | 3 << 11,
    );
    dma_wait(3);
    let same = unsafe { (0..16).all(|i| read_volatile((addr_of_mut!(SRC) as *const u32).add(i)) == read_volatile((addr_of_mut!(DST) as *const u32).add(i))) };
    check(same && rd(DMA + 3 * 0x40) == addr_of_mut!(SRC) as u32 + 64, b"memcpy ok\n", b"memcpy bad\n");

    // Chain: channel 4 copies SRC[0..4] -> MID and chains to channel 5,
    // which copies MID -> OUT. Only channel 4 is triggered.
    let base5 = DMA + 5 * 0x40;
    wr(base5, addr_of_mut!(MID) as u32);
    wr(base5 + 0x04, addr_of_mut!(OUT) as u32);
    wr(base5 + 0x08, 4);
    wr(base5 + 0x10, EN | 2 << 2 | INCR_READ | INCR_WRITE | TREQ_PERMANENT << 15 | 5 << 11); // CTRL, no trigger
    dma_start(
        4,
        addr_of_mut!(SRC) as u32,
        addr_of_mut!(MID) as u32,
        4,
        EN | 2 << 2 | INCR_READ | INCR_WRITE | TREQ_PERMANENT << 15 | 5 << 11,
    );
    dma_wait(4);
    dma_wait(5);
    let chained = unsafe { (0..4).all(|i| read_volatile((addr_of_mut!(OUT) as *const u32).add(i)) == 0x1000_0000 + i as u32 * 0x0101) };
    check(chained, b"chain ok\n", b"chain bad\n");

    puts(b"spi done\n");
    unsafe { asm!("bkpt #1") };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
