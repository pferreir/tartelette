//! Bare-metal test firmware for the ADC, with whatever drives GPIO26 (AIN0)
//! and GPIO27 (AIN1) on the board:
//!
//! * a one-shot conversion of AIN0,
//! * the temperature sensor (AIN4),
//! * round-robin AIN0/AIN1 into the FIFO until ADC_IRQ_FIFO wakes WFI,
//! * 32 samples of AIN1 at 48 kS/s, paced by the divider and moved by DMA
//!   on DREQ_ADC, with the time they took.
//!
//! Output, one line each: `ain0 N`, `temp N`, `rr N N N N`, `dma N ... N`
//! (32 values), `us N`, then `adc done`.
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
    ".rept 7",
    ".word 0",
    ".endr",
    ".word default_handler", // SVCall
    ".word 0",
    ".word 0",
    ".word default_handler", // PendSV
    ".word default_handler", // SysTick
    ".rept 22",
    ".word default_handler", // IRQ 0..21
    ".endr",
    ".word adc_irq", // IRQ 22: ADC_IRQ_FIFO
);

const ADC: u32 = 0x4004_C000;
const ADC_CS: u32 = ADC;
const ADC_RESULT: u32 = ADC + 0x04;
const ADC_FCS: u32 = ADC + 0x08;
const ADC_FIFO: u32 = ADC + 0x0C;
const ADC_DIV: u32 = ADC + 0x10;
const ADC_INTE: u32 = ADC + 0x18;

const CS_EN: u32 = 1;
const CS_TS_EN: u32 = 1 << 1;
const CS_START_ONCE: u32 = 1 << 2;
const CS_START_MANY: u32 = 1 << 3;
const CS_READY: u32 = 1 << 8;
const FCS_EN: u32 = 1;
const FCS_DREQ_EN: u32 = 1 << 3;
const FCS_EMPTY: u32 = 1 << 8;
const FCS_UNDER_OVER: u32 = 3 << 10;

const DREQ_ADC: u32 = 36;
const SAMPLES: usize = 32;

static mut FIFO_IRQS: u32 = 0;
static mut CAPTURE: [u16; SAMPLES] = [0; SAMPLES];

fn put_dec(mut n: u32) {
    let mut digits = [0u8; 10];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    puts(&digits[i..]);
}

fn wait_ready() {
    while rd(ADC_CS) & CS_READY == 0 {}
}

/// Empty the FIFO and clear its error flags.
fn drain() {
    while rd(ADC_FCS) & FCS_EMPTY == 0 {
        rd(ADC_FIFO);
    }
    wr(ADC_FCS, FCS_UNDER_OVER);
}

#[unsafe(no_mangle)]
extern "C" fn adc_irq() {
    wr(ADC_INTE, 0);
    put(&raw mut FIFO_IRQS, get(&raw const FIFO_IRQS) + 1);
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    // ADC, DMA, IO_BANK0, PADS_BANK0, TIMER and UART0 out of reset.
    let release = 1 | 1 << 2 | 1 << 5 | 1 << 8 | 1 << 21 | 1 << 22;
    wr(RESETS + 0x3000, release);
    while rd(RESETS + 0x8) & release != release {}
    wr(UART0 + 0x30, 0x301);
    puts(b"start\n");

    // As the SDK's adc_gpio_init: no function, input and pulls off.
    for pin in [26, 27] {
        wr(IO_BANK0 + 8 * pin + 4, 31);
        wr(PADS_BANK0 + 4 + 4 * pin, 1 << 7);
    }
    wr(ADC_CS, CS_EN);
    wait_ready();

    wr(ADC_CS, CS_EN | CS_START_ONCE);
    wait_ready();
    puts(b"ain0 ");
    put_dec(rd(ADC_RESULT));

    wr(ADC_CS, CS_EN | CS_TS_EN | 4 << 12 | CS_START_ONCE);
    wait_ready();
    puts(b"\ntemp ");
    put_dec(rd(ADC_RESULT));
    puts(b"\n");

    // Round robin over AIN0 and AIN1 until four results are in.
    wr(ADC_FCS, FCS_EN | 4 << 24);
    wr(ADC_INTE, 1);
    wr(NVIC_ISER, 1 << 22);
    wr(ADC_CS, CS_EN | 0b11 << 16 | CS_START_MANY);
    while get(&raw const FIFO_IRQS) == 0 {
        unsafe { asm!("wfi") };
    }
    wr(ADC_CS, CS_EN);
    wait_ready();
    puts(b"rr");
    for _ in 0..4 {
        puts(b" ");
        put_dec(rd(ADC_FIFO) & 0xFFF);
    }
    puts(b"\n");
    drain();

    // AIN1 at 48 MHz / 1000, through DMA channel 0.
    wr(ADC_DIV, 999 << 8);
    wr(ADC_FCS, FCS_EN | FCS_DREQ_EN | 1 << 24);
    wr(DMA, ADC_FIFO);
    wr(DMA + 0x04, &raw mut CAPTURE as u32);
    wr(DMA + 0x08, SAMPLES as u32);
    // EN, halfwords, INCR_WRITE, TREQ_SEL; CHAIN_TO itself (no chain).
    wr(DMA + 0x0C, 1 | 1 << 2 | 1 << 5 | DREQ_ADC << 15);
    let t0 = rd(TIMER + 0x28); // TIMERAWL
    wr(ADC_CS, CS_EN | 1 << 12 | CS_START_MANY);
    while rd(DMA + 0x0C) & 1 << 24 != 0 {}
    let took = rd(TIMER + 0x28).wrapping_sub(t0);
    wr(ADC_CS, CS_EN);
    wait_ready();
    drain();
    puts(b"dma");
    for i in 0..SAMPLES {
        puts(b" ");
        put_dec(unsafe { (&raw const CAPTURE as *const u16).add(i).read_volatile() } as u32);
    }
    puts(b"\nus ");
    put_dec(took);
    puts(b"\nadc done\n");
    unsafe { asm!("bkpt #1") };
    loop {
        core::hint::spin_loop();
    }
}
