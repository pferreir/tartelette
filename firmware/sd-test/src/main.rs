//! Bare-metal test firmware for an SD card on SPI1 (SCK 10, TX 11, RX 12,
//! CS GPIO8): the SPI-mode initialisation with CRC checking, then 512-byte
//! blocks moved by DMA (a full-duplex RX/TX pair for reads, TX for writes),
//! with CRC16 verified in both directions, as the BTL-01's driver does.
//!
//! Block 0 of the card holds a known pattern (the host puts it there); the
//! firmware reads it, writes a transformed copy to block 5 and reads that back.
//! Expected output:
//! "start\ncard ready\nocr sdhc\nread ok\ncrc ok\nwrite ok\nverify ok\nsd done\n"
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
    ".rept 12",
    ".word 0",
    ".endr",
);

#[used]
#[unsafe(link_section = ".boot2")]
static BOOT2: [u8; 256] = [0; 256];

const RESETS: u32 = 0x4000_C000;
const IO_BANK0: u32 = 0x4001_4000;
const SPI1: u32 = 0x4004_0000;
const UART0: u32 = 0x4003_4000;
const DMA: u32 = 0x5000_0000;
const SIO: u32 = 0xD000_0000;

const SPI_CR0: u32 = SPI1;
const SPI_CR1: u32 = SPI1 + 0x04;
const SPI_DR: u32 = SPI1 + 0x08;
const SPI_SR: u32 = SPI1 + 0x0C;
const SPI_CPSR: u32 = SPI1 + 0x10;
const SPI_DMACR: u32 = SPI1 + 0x24;

const CS: u32 = 8;
const TREQ_SPI1_TX: u32 = 18;
const TREQ_SPI1_RX: u32 = 19;
const BUSY: u32 = 1 << 24;

static mut BLOCK: [u8; 512] = [0; 512];
static mut BACK: [u8; 512] = [0; 512];
static FILL: [u8; 512] = [0xFF; 512];

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
fn cs(high: bool) {
    wr(if high { SIO + 0x14 } else { SIO + 0x18 }, 1 << CS);
}

/// One byte out, one byte in (the SPI frame size is 8 bits).
fn xfer(b: u8) -> u8 {
    while rd(SPI_SR) & 2 == 0 {}
    wr(SPI_DR, b as u32);
    while rd(SPI_SR) & 4 == 0 {}
    rd(SPI_DR) as u8
}
fn drain_rx() {
    while rd(SPI_SR) & 4 != 0 {
        rd(SPI_DR);
    }
}

fn crc7(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in data {
        for i in (0..8).rev() {
            let top = crc >> 6 & 1;
            crc = (crc << 1) & 0x7F;
            if top ^ (byte >> i & 1) != 0 {
                crc ^= 0x09;
            }
        }
    }
    crc
}
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { crc << 1 ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// Send a command and return R1.
fn command(c: u8, arg: u32) -> u8 {
    let mut frame = [0x40 | c, (arg >> 24) as u8, (arg >> 16) as u8, (arg >> 8) as u8, arg as u8, 0];
    frame[5] = crc7(&frame[..5]) << 1 | 1;
    for b in frame {
        xfer(b);
    }
    for _ in 0..10 {
        let r = xfer(0xFF);
        if r & 0x80 == 0 {
            return r;
        }
    }
    0xFF
}

fn dma_start(channel: u32, read: u32, write: u32, count: u32, ctrl: u32) {
    let base = DMA + channel * 0x40;
    wr(base, read);
    wr(base + 0x04, write);
    wr(base + 0x08, count);
    wr(base + 0x0C, ctrl | channel << 11);
}
fn dma_wait(channel: u32) {
    while rd(DMA + channel * 0x40 + 0x0C) & BUSY != 0 {}
}
const DMA_EN: u32 = 1;
const DMA_INCR_READ: u32 = 1 << 4;
const DMA_INCR_WRITE: u32 = 1 << 5;

/// Receive 512 bytes with a full-duplex DMA pair: RX channel collects, TX channel clocks 0xFF.
fn dma_read_512(buf: *mut u8) {
    drain_rx();
    wr(SPI_DMACR, 3);
    dma_start(0, SPI_DR, buf as u32, 512, DMA_EN | DMA_INCR_WRITE | TREQ_SPI1_RX << 15);
    dma_start(1, FILL.as_ptr() as u32, SPI_DR, 512, DMA_EN | DMA_INCR_READ | TREQ_SPI1_TX << 15);
    dma_wait(0);
    dma_wait(1);
    wr(SPI_DMACR, 0);
}
/// Send 512 bytes with a TX DMA channel (the received bytes are discarded).
fn dma_write_512(buf: *const u8) {
    wr(SPI_DMACR, 2);
    dma_start(0, buf as u32, SPI_DR, 512, DMA_EN | DMA_INCR_READ | TREQ_SPI1_TX << 15);
    dma_wait(0);
    wr(SPI_DMACR, 0);
    drain_rx();
}

/// CMD17: read block `n` into `buf`; true if the token, the data and the CRC all arrive intact.
fn read_block(n: u32, buf: *mut u8) -> bool {
    if command(17, n) != 0 {
        return false;
    }
    let mut spins = 0;
    while xfer(0xFF) != 0xFE {
        spins += 1;
        if spins > 100 {
            return false;
        }
    }
    dma_read_512(buf);
    let crc = (xfer(0xFF) as u16) << 8 | xfer(0xFF) as u16;
    crc == crc16(unsafe { core::slice::from_raw_parts(buf, 512) })
}

#[unsafe(no_mangle)]
extern "C" fn default_handler() {
    puts(b"unexpected exception\n");
    unsafe { asm!("bkpt #2") };
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    // DMA, IO_BANK0, SPI1 and UART0 out of reset (PADS_BANK0 keeps its defaults).
    let release = 1 << 2 | 1 << 5 | 1 << 8 | 1 << 17 | 1 << 22;
    wr(RESETS + 0x3000, release);
    while rd(RESETS + 0x8) & release != release {}
    wr(UART0 + 0x30, 0x301);
    puts(b"start\n");

    for pin in [10, 11, 12] {
        wr(IO_BANK0 + 8 * pin + 4, 1); // SPI1
    }
    wr(IO_BANK0 + 8 * CS + 4, 5); // SIO
    wr(SIO + 0x24, 1 << CS);
    cs(true);
    wr(SPI_CR0, 7);
    wr(SPI_CPSR, 250); // a slow clock for initialisation (ignored by the model)
    wr(SPI_CR1, 1 << 1);

    // 74+ clocks with CS high, then CS low and the SPI-mode init sequence.
    for _ in 0..10 {
        xfer(0xFF);
    }
    cs(false);
    let mut ok = command(0, 0) == 0x01;
    ok &= command(59, 1) == 0x01; // CRC on
    ok &= command(8, 0x1AA) == 0x01;
    let r7 = [xfer(0xFF), xfer(0xFF), xfer(0xFF), xfer(0xFF)];
    ok &= r7 == [0, 0, 1, 0xAA];
    let mut tries = 0;
    loop {
        command(55, 0);
        if command(41, 0x4000_0000) == 0 {
            break;
        }
        tries += 1;
        if tries > 100 {
            ok = false;
            break;
        }
    }
    check(ok, b"card ready\n", b"card bad\n");
    let r1 = command(58, 0);
    let ocr0 = xfer(0xFF);
    xfer(0xFF);
    xfer(0xFF);
    xfer(0xFF);
    check(r1 == 0 && ocr0 & 0xC0 == 0xC0, b"ocr sdhc\n", b"ocr bad\n");
    cs(true);
    xfer(0xFF);

    // Read block 0 (pattern from the host) with DMA, CRC checked in firmware.
    let block = addr_of_mut!(BLOCK) as *mut u8;
    cs(false);
    let got = read_block(0, block);
    cs(true);
    xfer(0xFF);
    let pattern_ok = (0..512).all(|i| unsafe { *block.add(i) } == (i as u8).wrapping_mul(3));
    check(got && pattern_ok, b"read ok\ncrc ok\n", b"read bad\n");

    // Write a transformed copy to block 5 with TX DMA and a CRC16, then read it back.
    for i in 0..512 {
        unsafe { *block.add(i) = (*block.add(i)) ^ 0x5A };
    }
    let crc = crc16(unsafe { core::slice::from_raw_parts(block, 512) });
    cs(false);
    let mut wrote = command(24, 5) == 0;
    xfer(0xFE);
    dma_write_512(block);
    xfer((crc >> 8) as u8);
    xfer(crc as u8);
    wrote &= xfer(0xFF) & 0x1F == 0x05;
    while xfer(0xFF) != 0xFF {} // busy
    cs(true);
    xfer(0xFF);
    check(wrote, b"write ok\n", b"write bad\n");

    let back = addr_of_mut!(BACK) as *mut u8;
    cs(false);
    let ok = read_block(5, back);
    cs(true);
    let same = (0..512).all(|i| unsafe { *back.add(i) == *block.add(i) });
    check(ok && same, b"verify ok\n", b"verify bad\n");

    puts(b"sd done\n");
    unsafe { asm!("bkpt #1") };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
