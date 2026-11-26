//! Bare-metal test firmware: a PS/2 host for a mouse on GPIO14 (CLK) and
//! GPIO15 (DATA), both open-drain with the pads' pull-ups: SIO drives them
//! low by enabling the output (OUT stays 0) and lets them go by disabling it.
//!
//! PIO0 SM0 receives: at 1 MHz it waits for CLK to fall and samples DATA,
//! autopushing every 11 bits. The CPU sends, bit-banging DATA on each CLK
//! fall, with the receiver stopped (it would take the host's own frames and
//! inhibit pulses for bits) and restarted after.
//!
//! It waits for the power-up `AA 00`, resets the mouse, turns the wheel on
//! (sample rates 200, 100, 80, then reads the ID), enables reporting, then
//! inhibits the mouse until 1 s and reads a packet; then reads one more.
//! The host moves the mouse at 0.9 s (while inhibited) and presses the left
//! button and scrolls at 1.2 s. Expected output, with those inputs:
//! "start\nbat aa 00\nreset fa aa 00\nid fa fa fa fa fa fa fa 03\n
//! enable fa\npacket 28 0a fb 00\npacket 09 00 00 ff\nps2 done\n"
#![no_std]
#![no_main]

use core::arch::{asm, global_asm};

use testrt::*;

global_asm!(
    ".section .vectors, \"a\"",
    ".word _stack_top",
    ".word reset",
    ".word default_handler", // NMI
    ".word default_handler", // HardFault
);

const CLK: u32 = 14;
const DATA: u32 = 15;

const PIO0: u32 = 0x5020_0000;
const FUNCSEL_SIO: u32 = 5;

// PIO register offsets
const CTRL: u32 = 0x000;
const FSTAT: u32 = 0x004;
const RXF0: u32 = 0x020;
const INSTR_MEM: u32 = 0x048;
const SM0_CLKDIV: u32 = 0x0C8;
const SM0_EXECCTRL: u32 = 0x0CC;
const SM0_SHIFTCTRL: u32 = 0x0D0;
const SM0_INSTR: u32 = 0x0D8;
const SM0_PINCTRL: u32 = 0x0DC;

const SHIFT_AUTOPUSH: u32 = 1 << 16;
const SHIFT_IN_RIGHT: u32 = 1 << 18;
const SHIFT_FJOIN_RX: u32 = 1 << 31;
const CTRL_SM0_EN: u32 = 1 << 0;
const CTRL_SM0_RESTART: u32 = 1 << 4;
const FSTAT_RXEMPTY0: u32 = 1 << 8;

/// `wait 0 gpio CLK`, `in pins, 1`, `wait 1 gpio CLK`; wrap 0..2.
const RX: [u16; 3] = [0x2000 | CLK as u16, 0x4001, 0x2080 | CLK as u16];

// SIO
const GPIO_IN: u32 = SIO + 0x04;
const GPIO_OE_SET: u32 = SIO + 0x24;
const GPIO_OE_CLR: u32 = SIO + 0x28;

fn now_us() -> u32 {
    rd(TIMER + 0x28)
}

fn delay_us(us: u32) {
    let start = now_us();
    while now_us().wrapping_sub(start) < us {}
}

/// Pull `pin` low, or let it float up.
fn line(pin: u32, low: bool) {
    wr(if low { GPIO_OE_SET } else { GPIO_OE_CLR }, 1 << pin);
}

fn level(pin: u32) -> bool {
    rd(GPIO_IN) >> pin & 1 != 0
}

/// Wait up to 20 ms for `pin` to read `high`.
fn wait_for(pin: u32, high: bool) -> bool {
    let start = now_us();
    while level(pin) != high {
        if now_us().wrapping_sub(start) > 20_000 {
            return false;
        }
    }
    true
}

fn receiver(on: bool) {
    if on {
        wr(PIO0 + SM0_INSTR, 0x0000); // jmp 0
        wr(PIO0 + CTRL, CTRL_SM0_RESTART | CTRL_SM0_EN);
    } else {
        wr(PIO0 + CTRL, 0);
    }
}

fn hex(b: u8) {
    let d = |n: u8| if n < 10 { b'0' + n } else { b'a' + n - 10 };
    putc(d(b >> 4));
    putc(d(b & 15));
}

fn fail(what: &[u8]) -> ! {
    puts(what);
    putc(b'\n');
    unsafe { asm!("bkpt #2") };
    loop {
        core::hint::spin_loop();
    }
}

/// The next byte from the mouse, waiting up to `timeout_us`.
fn read(timeout_us: u32) -> Option<u8> {
    let start = now_us();
    while rd(PIO0 + FSTAT) & FSTAT_RXEMPTY0 != 0 {
        if now_us().wrapping_sub(start) > timeout_us {
            return None;
        }
    }
    // Shifted in from the top: the frame is the word's top 11 bits.
    let frame = rd(PIO0 + RXF0) >> 21;
    let byte = (frame >> 1) as u8;
    let parity = frame >> 9 & 1;
    if frame & 1 != 0 || frame >> 10 & 1 != 1 || (byte.count_ones() + parity).is_multiple_of(2) {
        fail(b"frame error");
    }
    Some(byte)
}

fn print(b: u8) {
    putc(b' ');
    hex(b);
}

fn next(timeout_us: u32) -> u8 {
    read(timeout_us).unwrap_or_else(|| fail(b" timeout"))
}

/// Read `n` bytes and print them after `label`.
fn expect(label: &[u8], n: usize, timeout_us: u32) {
    puts(label);
    for _ in 0..n {
        print(next(timeout_us));
    }
    putc(b'\n');
}

/// Send `byte` to the mouse; true if it acknowledged.
fn send(byte: u8) -> bool {
    receiver(false);
    // Request to send: inhibit, start bit, let the mouse clock.
    line(CLK, true);
    delay_us(120);
    line(DATA, true);
    line(CLK, false);
    let parity = byte.count_ones().is_multiple_of(2);
    let frame = byte as u32 | (parity as u32) << 8 | 1 << 9;
    for i in 0..10 {
        if !wait_for(CLK, false) {
            return false;
        }
        line(DATA, frame >> i & 1 == 0);
        if !wait_for(CLK, true) {
            return false;
        }
    }
    // The acknowledge: DATA low through one more pulse.
    let acked = wait_for(CLK, false) && !level(DATA);
    let released = wait_for(CLK, true) && wait_for(DATA, true);
    receiver(true);
    acked && released
}

/// Send `bytes`, each once the mouse has answered the last (a new command
/// would make it drop what it hasn't sent), and print what it says after
/// `label`: a byte each, and `more` after the last.
fn command(label: &[u8], bytes: &[u8], more: usize, timeout_us: u32) {
    puts(label);
    for &b in bytes {
        if !send(b) {
            fail(b" no ack");
        }
        print(next(50_000));
    }
    for _ in 0..more {
        print(next(timeout_us));
    }
    putc(b'\n');
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    // IO_BANK0, PADS_BANK0, PIO0, TIMER, UART0 out of reset.
    let release = 1 << 5 | 1 << 8 | 1 << 10 | 1 << 21 | 1 << 22;
    wr(RESETS + 0x3000, release);
    while rd(RESETS + 0x8) & release != release {}
    wr(UART0 + 0x30, 0x301);
    puts(b"start\n");

    for pin in [CLK, DATA] {
        wr(IO_BANK0 + 8 * pin + 4, FUNCSEL_SIO);
        wr(PADS_BANK0 + 4 + 4 * pin, 1 << 6 | 1 << 3); // input enable, pull-up
    }

    for (i, w) in RX.iter().enumerate() {
        wr(PIO0 + INSTR_MEM + 4 * i as u32, *w as u32);
    }
    wr(PIO0 + SM0_CLKDIV, 125 << 16); // 1 MHz
    wr(PIO0 + SM0_EXECCTRL, 2 << 12); // wrap 0..2
    wr(
        PIO0 + SM0_SHIFTCTRL,
        SHIFT_AUTOPUSH | SHIFT_IN_RIGHT | SHIFT_FJOIN_RX | 11 << 20,
    );
    wr(PIO0 + SM0_PINCTRL, DATA << 15); // IN_BASE
    receiver(true);

    expect(b"bat", 2, 1_000_000);
    // The self-test takes a while.
    command(b"reset", &[0xFF], 2, 1_000_000);
    command(b"id", &[0xF3, 200, 0xF3, 100, 0xF3, 80, 0xF2], 1, 50_000);
    command(b"enable", &[0xF4], 0, 50_000);

    // Hold the mouse off until 1 s: what the host does meanwhile waits.
    receiver(false);
    line(CLK, true);
    while now_us() < 1_000_000 {}
    line(CLK, false);
    receiver(true);
    expect(b"packet", 4, 50_000);
    expect(b"packet", 4, 1_000_000);

    puts(b"ps2 done\n");
    unsafe { asm!("bkpt #1") };
    loop {
        core::hint::spin_loop();
    }
}
