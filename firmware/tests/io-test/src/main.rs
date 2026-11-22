//! Bare-metal test firmware for GPIO: SIO output/output-enable registers,
//! pad pull resistors, GPIO_IN loopback of driven pins, and an IO_BANK0 edge
//! interrupt (NVIC IRQ 13). Results go out on UART0, one line per check.
//! Expected output: "start\nfuncsel ok\nout ok\npull ok\nedge ok\nio done\n".
#![no_std]
#![no_main]

use core::{
    arch::{asm, global_asm},
    ptr::{read_volatile, write_volatile},
};

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
    ".rept 13",
    ".word default_handler", // IRQ 0..12
    ".endr",
    ".word io_irq_bank0", // IRQ 13
);

const SIO_GPIO_IN: u32 = SIO + 0x04;
const SIO_GPIO_OUT_SET: u32 = SIO + 0x14;
const SIO_GPIO_OUT_CLR: u32 = SIO + 0x18;
const SIO_GPIO_OUT_XOR: u32 = SIO + 0x1C;
const SIO_GPIO_OE_SET: u32 = SIO + 0x24;
const SIO_GPIO_OE_CLR: u32 = SIO + 0x28;

const FUNCSEL_SIO: u32 = 5;
const PAD_PUE: u32 = 1 << 3;
const PAD_PDE: u32 = 1 << 2;
const PAD_IE: u32 = 1 << 6;
const EDGE_HIGH: u32 = 1 << 3; // per-pin event bit within a 4-bit group

const OUT_PIN: u32 = 16; // driven by SIO
const FLOAT_PIN: u32 = 15; // input only; tested with pull-up / pull-down

static mut EDGE_EVENTS: u32 = 0;
static mut EDGE_STATUS: u32 = 0;

fn pin_in(pin: u32) -> bool {
    rd(SIO_GPIO_IN) >> pin & 1 != 0
}
fn ctrl(pin: u32) -> u32 {
    IO_BANK0 + 8 * pin + 4
}
fn pad(pin: u32) -> u32 {
    PADS_BANK0 + 4 + 4 * pin
}

/// IO_BANK0 interrupt: record which edge events are pending for pins 16-23
/// (INTS2) and acknowledge them (INTR2 is write-1-to-clear for edges).
#[unsafe(no_mangle)]
extern "C" fn io_irq_bank0() {
    let status = rd(IO_BANK0 + 0x120 + 8); // PROC0_INTS2
    wr(IO_BANK0 + 0x0F8, status); // INTR2
    unsafe {
        write_volatile(&raw mut EDGE_STATUS, status);
        write_volatile(
            &raw mut EDGE_EVENTS,
            read_volatile(&raw const EDGE_EVENTS) + 1,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    // IO_BANK0, PADS_BANK0 and UART0 out of reset.
    let release = 1 << 5 | 1 << 8 | 1 << 22;
    wr(RESETS + 0x3000, release);
    while rd(RESETS + 0x8) & release != release {}
    wr(UART0 + 0x30, 0x301);
    puts(b"start\n");

    // Route both pins to SIO and check FUNCSEL reads back.
    wr(ctrl(OUT_PIN), FUNCSEL_SIO);
    wr(ctrl(FLOAT_PIN), FUNCSEL_SIO);
    check(
        rd(ctrl(OUT_PIN)) & 0x1F == FUNCSEL_SIO && rd(ctrl(FLOAT_PIN)) & 0x1F == FUNCSEL_SIO,
        b"funcsel ok\n",
        b"funcsel bad\n",
    );

    // Output: nothing is driven until OE is set, then OUT_SET/CLR/XOR show
    // up on GPIO_IN (it reads the pad, which follows a driven output).
    wr(SIO_GPIO_OUT_CLR, 1 << OUT_PIN);
    wr(SIO_GPIO_OE_SET, 1 << OUT_PIN);
    let low = !pin_in(OUT_PIN);
    wr(SIO_GPIO_OUT_SET, 1 << OUT_PIN);
    let high = pin_in(OUT_PIN);
    wr(SIO_GPIO_OUT_XOR, 1 << OUT_PIN);
    let toggled_low = !pin_in(OUT_PIN);
    wr(SIO_GPIO_OUT_XOR, 1 << OUT_PIN);
    let toggled_high = pin_in(OUT_PIN);
    check(
        low && high && toggled_low && toggled_high,
        b"out ok\n",
        b"out bad\n",
    );

    // Input with pull resistors: an undriven pin follows its pull.
    wr(pad(FLOAT_PIN), PAD_IE | PAD_PUE);
    let pulled_up = pin_in(FLOAT_PIN);
    wr(pad(FLOAT_PIN), PAD_IE | PAD_PDE);
    let pulled_down = !pin_in(FLOAT_PIN);
    check(pulled_up && pulled_down, b"pull ok\n", b"pull bad\n");

    // Edge interrupt: enable rising-edge detection on OUT_PIN (INTE2 covers
    // pins 16-23, 4 bits per pin), then drive the pin low -> high.
    wr(SIO_GPIO_OUT_CLR, 1 << OUT_PIN);
    let shift = (OUT_PIN % 8) * 4;
    wr(IO_BANK0 + 0x0F8, 0xF << shift); // clear stale edge events
    wr(IO_BANK0 + 0x100 + 8, EDGE_HIGH << shift); // PROC0_INTE2
    wr(NVIC_ISER, 1 << 13);
    wr(SIO_GPIO_OUT_SET, 1 << OUT_PIN);
    let mut spins = 0;
    while unsafe { read_volatile(&raw const EDGE_EVENTS) } == 0 && spins < 1000 {
        unsafe { asm!("wfi") };
        spins += 1;
    }
    let (events, status) = unsafe {
        (
            read_volatile(&raw const EDGE_EVENTS),
            read_volatile(&raw const EDGE_STATUS),
        )
    };
    // Exactly one event, reporting the rising edge on OUT_PIN, and it was acknowledged.
    // Only the edge bits latch; the level bits are live and the pin is high.
    let acked = rd(IO_BANK0 + 0x0F8) & (0xC << shift) == 0;
    check(
        events == 1 && status == EDGE_HIGH << shift && acked,
        b"edge ok\n",
        b"edge bad\n",
    );

    wr(SIO_GPIO_OE_CLR, 1 << OUT_PIN);
    puts(b"io done\n");
    unsafe { asm!("bkpt #1") };
    loop {
        core::hint::spin_loop();
    }
}
