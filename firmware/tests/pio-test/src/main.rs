//! Bare-metal test firmware that sets up PIO the way a real firmware's
//! Embassy drivers do, with its three programs:
//!
//! * PIO1 SM0: `spi_tx` (bit-banged SPI to the DAC8165 on GPIO4 data, 5 CS,
//!   6 CLK), 24-bit autopull, fed by DMA with a frame per channel;
//! * PIO1 SM1: `ws2812` (GPIO14), 8 pixels fed by DMA at 125/8 MHz / 10;
//! * PIO0 SM0: `encoder` (GPIO21 DT, GPIO22 CLK), counting quadrature steps,
//!   its RX FIFO drained by the PIO0_IRQ_0 interrupt.
//!
//! The host turns the knob once clockwise and once back after the firmware
//! says so. Expected output:
//! "start\ndac sent\nleds sent\nencoder ready\nenc+ 4\nenc- 0\npio done\n"
#![no_std]
#![no_main]

use core::{
    arch::{asm, global_asm},
    ptr::{addr_of_mut, read_volatile, write_volatile},
};

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
    ".rept 7",
    ".word default_handler", // IRQ 0..6
    ".endr",
    ".word pio0_irq0", // IRQ 7
);

#[used]
#[unsafe(link_section = ".boot2")]
static BOOT2: [u8; 256] = [0; 256];

const RESETS: u32 = 0x4000_C000;
const IO_BANK0: u32 = 0x4001_4000;
const PADS_BANK0: u32 = 0x4001_C000;
const TIMER: u32 = 0x4005_4000;
const UART0: u32 = 0x4003_4000;
const DMA: u32 = 0x5000_0000;
const PIO0: u32 = 0x5020_0000;
const PIO1: u32 = 0x5030_0000;
const SIO: u32 = 0xD000_0000;
const NVIC_ISER: u32 = 0xE000_E100;

const FUNCSEL_SIO: u32 = 5;
const FUNCSEL_PIO0: u32 = 6;
const FUNCSEL_PIO1: u32 = 7;

// PIO register offsets
const CTRL: u32 = 0x000;
const FSTAT: u32 = 0x004;
const TXF0: u32 = 0x010;
const RXF0: u32 = 0x020;
const INSTR_MEM: u32 = 0x048;
const SM_BASE: u32 = 0x0C8;
const IRQ0_INTE: u32 = 0x12C;
// per-machine register offsets
const CLKDIV: u32 = 0x00;
const EXECCTRL: u32 = 0x04;
const SHIFTCTRL: u32 = 0x08;
const INSTR: u32 = 0x10;
const PINCTRL: u32 = 0x14;

const SHIFT_AUTOPULL: u32 = 1 << 17;
const SHIFT_OUT_RIGHT: u32 = 1 << 19;
const SHIFT_FJOIN_TX: u32 = 1 << 30;
const SHIFT_FJOIN_RX: u32 = 1 << 31;

// DMA control pieces
const DMA_EN: u32 = 1;
const DMA_WORD: u32 = 2 << 2;
const DMA_INCR_READ: u32 = 1 << 4;
const BUSY: u32 = 1 << 24;
const TREQ_PIO1_TX0: u32 = 8;
const TREQ_PIO1_TX1: u32 = 9;

/// spi_tx.pio: 2 side-set bits (CS, CLK), 24-bit frames, wrap 0..6.
const SPI_TX: [u16; 7] = [0xE836, 0x6841, 0xA002, 0x7141, 0x0042, 0xA002, 0xB142];
/// ws2812.pio relocated to offset 7 (its two jumps are absolute).
const WS2812_OFFSET: u16 = 7;
const WS2812: [u16; 4] = [
    0x6221,
    0x1123 + WS2812_OFFSET,
    0x1400 + WS2812_OFFSET,
    0xA442,
];
/// encoder.pio: a 16-entry jump table, then the loop at 16..27.
const ENCODER: [u16; 28] = [
    0x0010, 0x0015, 0x0017, 0x0010, 0x0017, 0x0010, 0x0010, 0x0015, 0x0015, 0x0010, 0x0010, 0x0017,
    0x0010, 0x0017, 0x0015, 0x0010, 0xA0C3, 0x4042, 0xA040, 0x4042, 0xA0A6, 0x005A, 0x001A, 0xA029,
    0x0059, 0xA029, 0xA0C1, 0x8000,
];

static mut ENC_COUNT: i32 = 0;
static mut ENC_EVENTS: u32 = 0;
static mut DAC_FRAMES: [u32; 4] = [0; 4];
static mut PIXELS: [u32; 8] = [0; 8];

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
fn put_i32(v: i32) {
    if v < 0 {
        putc(b'-');
    }
    putc(b'0' + v.unsigned_abs() as u8 % 10);
}
fn sm(block: u32, n: u32, reg: u32) -> u32 {
    block + SM_BASE + 0x18 * n + reg
}
fn load(block: u32, at: usize, program: &[u16]) {
    for (i, w) in program.iter().enumerate() {
        wr(block + INSTR_MEM + 4 * (at + i) as u32, *w as u32);
    }
}
fn exec(block: u32, n: u32, instr: u16) {
    wr(sm(block, n, INSTR), instr as u32);
}
fn gpio_func(pin: u32, func: u32) {
    wr(IO_BANK0 + 8 * pin + 4, func);
}
/// Busy-wait `us` microseconds on the 1 MHz timer.
fn delay_us(us: u32) {
    let start = rd(TIMER + 0x28);
    while rd(TIMER + 0x28).wrapping_sub(start) < us {}
}
fn dma_start(channel: u32, read: u32, write: u32, count: u32, treq: u32) {
    let base = DMA + channel * 0x40;
    wr(base, read);
    wr(base + 0x04, write);
    wr(base + 0x08, count);
    wr(
        base + 0x0C,
        DMA_EN | DMA_WORD | DMA_INCR_READ | treq << 15 | channel << 11,
    );
}
fn dma_wait(channel: u32) {
    while rd(DMA + channel * 0x40 + 0x0C) & BUSY != 0 {}
}

#[unsafe(no_mangle)]
extern "C" fn default_handler() {
    puts(b"unexpected exception\n");
    unsafe { asm!("bkpt #2") };
    loop {
        core::hint::spin_loop();
    }
}

/// PIO0 IRQ 0: the encoder pushed a counter value. Drain the RX FIFO.
#[unsafe(no_mangle)]
extern "C" fn pio0_irq0() {
    while rd(PIO0 + FSTAT) & (1 << 8) == 0 {
        let v = rd(PIO0 + RXF0) as i32;
        unsafe {
            write_volatile(&raw mut ENC_COUNT, v);
            write_volatile(
                &raw mut ENC_EVENTS,
                read_volatile(&raw const ENC_EVENTS) + 1,
            );
        }
    }
}

fn wait_encoder_events(n: u32) {
    let mut spins = 0;
    while unsafe { read_volatile(&raw const ENC_EVENTS) } < n && spins < 100_000 {
        unsafe { asm!("wfi") };
        spins += 1;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    // DMA, IO_BANK0, PADS_BANK0, PIO0, PIO1, TIMER, UART0 out of reset.
    let release = 1 << 2 | 1 << 5 | 1 << 8 | 1 << 10 | 1 << 11 | 1 << 21 | 1 << 22;
    wr(RESETS + 0x3000, release);
    while rd(RESETS + 0x8) & release != release {}
    wr(UART0 + 0x30, 0x301);
    puts(b"start\n");

    // DAC reset line (GPIO7) high; then hand the serial pins to PIO1.
    gpio_func(7, FUNCSEL_SIO);
    wr(SIO + 0x24, 1 << 7);
    wr(SIO + 0x14, 1 << 7);
    for pin in [4, 5, 6, 14] {
        gpio_func(pin, FUNCSEL_PIO1);
    }
    // Encoder pins: PIO0 inputs with pull-ups.
    for pin in [21, 22] {
        gpio_func(pin, FUNCSEL_PIO0);
        wr(PADS_BANK0 + 4 + 4 * pin, 1 << 6 | 1 << 3);
    }

    // ---- DAC: spi_tx on PIO1 SM0 ----
    load(PIO1, 0, &SPI_TX);
    wr(sm(PIO1, 0, CLKDIV), 1 << 16 | 64 << 8); // 1.25: ~100 MHz PIO clock
    wr(sm(PIO1, 0, EXECCTRL), 6 << 12); // wrap 0..6
    wr(
        sm(PIO1, 0, SHIFTCTRL),
        SHIFT_AUTOPULL | 24 << 25 | SHIFT_FJOIN_TX,
    ); // shift left, 24 bits
    wr(sm(PIO1, 0, PINCTRL), 3 << 26 | 4 << 5); // set pins 4..6, to make them outputs
    exec(PIO1, 0, 0xE087); // set pindirs, 7
    wr(sm(PIO1, 0, PINCTRL), 1 << 20 | 4 | 2 << 29 | 5 << 10); // out pin 4; side-set pins 5, 6
    exec(PIO1, 0, 0x0000); // jmp 0
    wr(PIO1 + CTRL, 1 << 0); // enable SM0
    let regs: [u32; 4] = [0x10_0000, 0x12_0000, 0x14_0000, 0x16_0000];
    let volts: [u32; 4] = [0x8000, 0x4000, 0x1000, 0xFFFC];
    for i in 0..4 {
        unsafe {
            (addr_of_mut!(DAC_FRAMES) as *mut u32)
                .add(i)
                .write_volatile((regs[i] | (volts[i] & 0xFFFC)) << 8)
        };
    }
    dma_start(
        0,
        addr_of_mut!(DAC_FRAMES) as u32,
        PIO1 + TXF0,
        4,
        TREQ_PIO1_TX0,
    );
    dma_wait(0);
    delay_us(20);
    puts(b"dac sent\n");

    // ---- LEDs: ws2812 on PIO1 SM1 ----
    load(PIO1, WS2812_OFFSET as usize, &WS2812);
    wr(sm(PIO1, 1, CLKDIV), 15 << 16 | 160 << 8); // 125 MHz / 15.625 = 8 MHz
    wr(
        sm(PIO1, 1, EXECCTRL),
        (WS2812_OFFSET as u32) << 7 | (WS2812_OFFSET as u32 + 3) << 12,
    );
    wr(
        sm(PIO1, 1, SHIFTCTRL),
        SHIFT_AUTOPULL | 24 << 25 | SHIFT_FJOIN_TX,
    );
    wr(sm(PIO1, 1, PINCTRL), 1 << 26 | 14 << 5);
    exec(PIO1, 1, 0xE081); // set pindirs, 1
    wr(sm(PIO1, 1, PINCTRL), 1 << 20 | 14 | 1 << 29 | 14 << 10); // out pin 14, side-set pin 14
    exec(PIO1, 1, WS2812_OFFSET); // jmp to the program start
    wr(PIO1 + CTRL, 1 << 0 | 1 << 1); // enable SM0 and SM1
    for i in 0..8u32 {
        let (r, g, b) = (i * 16, 255 - i * 16, i);
        let grb = g << 16 | r << 8 | b;
        unsafe {
            (addr_of_mut!(PIXELS) as *mut u32)
                .add(i as usize)
                .write_volatile(grb << 8)
        };
    }
    dma_start(
        1,
        addr_of_mut!(PIXELS) as u32,
        PIO1 + TXF0 + 4,
        8,
        TREQ_PIO1_TX1,
    );
    dma_wait(1);
    delay_us(150); // the last bits go out, then the reset gap latches the frame
    puts(b"leds sent\n");

    // ---- encoder on PIO0 SM0 ----
    load(PIO0, 0, &ENCODER);
    wr(sm(PIO0, 0, EXECCTRL), 16 << 7 | 27 << 12); // wrap 16..27
    wr(sm(PIO0, 0, SHIFTCTRL), SHIFT_OUT_RIGHT | SHIFT_FJOIN_RX); // shift in left, no autopush
    wr(sm(PIO0, 0, PINCTRL), 21 << 15); // IN_BASE 21: DT is bit 0, CLK bit 1
    exec(PIO0, 0, 0xE020); // set x, 0
    exec(PIO0, 0, 0xA040); // mov y, pins
    exec(PIO0, 0, 0x0010); // jmp 16
    wr(PIO0 + IRQ0_INTE, 1 << 0); // SM0 RX FIFO not empty
    wr(NVIC_ISER, 1 << 7);
    wr(PIO0 + CTRL, 1 << 0);
    puts(b"encoder ready\n");

    wait_encoder_events(4);
    puts(b"enc+ ");
    put_i32(unsafe { read_volatile(&raw const ENC_COUNT) });
    putc(b'\n');
    wait_encoder_events(8);
    puts(b"enc- ");
    put_i32(unsafe { read_volatile(&raw const ENC_COUNT) });
    putc(b'\n');

    puts(b"pio done\n");
    unsafe { asm!("bkpt #1") };
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
