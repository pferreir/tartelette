//! Bare-metal test firmware for the I2C1 controller against an MCP23008 at
//! 0x20 (INT on GPIO16): register writes, a write-restart-read, an address
//! NACK, the RX_FULL interrupt (NVIC IRQ 24) and the expander's INT line
//! reaching GPIO16 as an edge interrupt (NVIC IRQ 13).
//!
//! The host must provide the board (see the test) and, once the firmware
//! prints "waiting", pull expander pin gp0 low.
//! Expected output:
//! "start\nwrite ok\nread A0\nstop ok\nrx irq ok\nnack ok\nwaiting\nint AE\ni2c done\n"
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
    ".rept 10",
    ".word default_handler", // IRQ 14..23
    ".endr",
    ".word i2c1_irq", // IRQ 24
);

const I2C1: u32 = 0x4004_8000;

// I2C registers
const IC_CON: u32 = I2C1;
const IC_TAR: u32 = I2C1 + 0x04;
const IC_DATA_CMD: u32 = I2C1 + 0x10;
const IC_INTR_MASK: u32 = I2C1 + 0x30;
const IC_RAW_INTR_STAT: u32 = I2C1 + 0x34;
const IC_RX_TL: u32 = I2C1 + 0x38;
const IC_CLR_STOP_DET: u32 = I2C1 + 0x60;
const IC_CLR_TX_ABRT: u32 = I2C1 + 0x54;
const IC_ENABLE: u32 = I2C1 + 0x6C;
const IC_RXFLR: u32 = I2C1 + 0x78;
const IC_TX_ABRT_SOURCE: u32 = I2C1 + 0x80;

const CMD_READ: u32 = 1 << 8;
const CMD_STOP: u32 = 1 << 9;
const CMD_RESTART: u32 = 1 << 10;
const INT_RX_FULL: u32 = 1 << 2;
const INT_TX_ABRT: u32 = 1 << 6;
const INT_STOP_DET: u32 = 1 << 9;

// MCP23008 registers
const MCP_IODIR: u32 = 0x00;
const MCP_GPINTEN: u32 = 0x02;
const MCP_GPPU: u32 = 0x06;
const MCP_GPIO: u32 = 0x09;
const MCP_OLAT: u32 = 0x0A;

const INT_PIN: u32 = 16;
const EDGE_LOW: u32 = 1 << 2; // per-pin event bit within a 4-bit group

static mut RX_IRQS: u32 = 0;
static mut INT_EDGES: u32 = 0;

fn hex(b: u8) {
    putc(b"0123456789ABCDEF"[(b >> 4) as usize]);
    putc(b"0123456789ABCDEF"[(b & 15) as usize]);
}

fn cmd(v: u32) {
    while rd(I2C1 + 0x70) & 2 == 0 {} // TFNF
    wr(IC_DATA_CMD, v);
}
fn set_target(addr: u32) {
    wr(IC_ENABLE, 0);
    wr(IC_TAR, addr);
    wr(IC_ENABLE, 1);
}
fn write_reg(reg: u32, val: u32) {
    cmd(reg);
    cmd(val | CMD_STOP);
}
fn wait_stop() {
    while rd(IC_RAW_INTR_STAT) & INT_STOP_DET == 0 {}
    rd(IC_CLR_STOP_DET);
}
fn read_reg(reg: u32) -> u8 {
    cmd(reg);
    cmd(CMD_READ | CMD_RESTART | CMD_STOP);
    wait_stop();
    rd(IC_DATA_CMD) as u8
}

#[unsafe(no_mangle)]
extern "C" fn i2c1_irq() {
    wr(IC_INTR_MASK, 0); // level interrupt: mask it until the main code asks again
    unsafe { write_volatile(&raw mut RX_IRQS, read_volatile(&raw const RX_IRQS) + 1) };
}

#[unsafe(no_mangle)]
extern "C" fn io_irq_bank0() {
    let status = rd(IO_BANK0 + 0x120 + 8); // PROC0_INTS2
    wr(IO_BANK0 + 0x0F8, status); // INTR2: acknowledge edges
    unsafe { write_volatile(&raw mut INT_EDGES, read_volatile(&raw const INT_EDGES) + 1) };
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    // IO_BANK0, PADS_BANK0, I2C1 and UART0 out of reset.
    let release = 1 << 5 | 1 << 8 | 1 << 4 | 1 << 22;
    wr(RESETS + 0x3000, release);
    while rd(RESETS + 0x8) & release != release {}
    wr(UART0 + 0x30, 0x301);
    puts(b"start\n");

    // GPIO2/3 to I2C1 (funcsel 3) with pull-ups; INT pin 16 as a pulled-up input.
    for pin in [2u32, 3] {
        wr(IO_BANK0 + 8 * pin + 4, 3);
        wr(PADS_BANK0 + 4 + 4 * pin, 1 << 6 | 1 << 3);
    }
    wr(PADS_BANK0 + 4 + 4 * INT_PIN, 1 << 6 | 1 << 3);

    // Master, fast mode, restarts enabled, slave disabled.
    wr(IC_ENABLE, 0);
    wr(IC_CON, 0x65);
    set_target(0x20);

    // Registers: IODIR = 0x0F (pins 4-7 outputs), OLAT = 0xA0, pull-ups on 0-3.
    write_reg(MCP_IODIR, 0x0F);
    write_reg(MCP_OLAT, 0xA0);
    write_reg(MCP_GPPU, 0x0F);
    wait_stop();
    check(
        rd(IC_RAW_INTR_STAT) & INT_TX_ABRT == 0,
        b"write ok\n",
        b"write bad\n",
    );

    puts(b"read ");
    hex(read_reg(MCP_OLAT));
    putc(b'\n');
    check(rd(IC_RXFLR) == 0, b"stop ok\n", b"stop bad\n");

    // Interrupt-driven read: ask for RX_FULL, sleep until the handler runs.
    wr(IC_RX_TL, 0);
    wr(IC_INTR_MASK, INT_RX_FULL);
    wr(NVIC_ISER, 1 << 24);
    cmd(MCP_GPPU);
    cmd(CMD_READ | CMD_RESTART | CMD_STOP);
    let mut spins = 0;
    while unsafe { read_volatile(&raw const RX_IRQS) } == 0 && spins < 1000 {
        unsafe { asm!("wfi") };
        spins += 1;
    }
    let got = rd(IC_DATA_CMD) as u8;
    check(
        unsafe { read_volatile(&raw const RX_IRQS) } == 1 && got == 0x0F,
        b"rx irq ok\n",
        b"rx irq bad\n",
    );
    wait_stop();

    // Nobody home at 0x21: the controller aborts with an address NACK and
    // flushes the rest of the transfer.
    set_target(0x21);
    cmd(0x00);
    cmd(0x55 | CMD_STOP);
    while rd(IC_RAW_INTR_STAT) & INT_TX_ABRT == 0 {}
    let src = rd(IC_TX_ABRT_SOURCE);
    rd(IC_CLR_TX_ABRT);
    check(
        src & 1 == 1 && rd(IC_RAW_INTR_STAT) & INT_TX_ABRT == 0,
        b"nack ok\n",
        b"nack bad\n",
    );
    wait_stop();
    set_target(0x20);

    // Expander interrupt: INT (GPIO16, active low) falls when gp0 does.
    write_reg(MCP_GPINTEN, 0x01);
    wait_stop();
    let shift = (INT_PIN % 8) * 4;
    wr(IO_BANK0 + 0x0F8, 0xF << shift);
    wr(IO_BANK0 + 0x100 + 8, EDGE_LOW << shift); // PROC0_INTE2
    wr(NVIC_ISER, 1 << 13);
    puts(b"waiting\n");
    let mut spins = 0;
    while unsafe { read_volatile(&raw const INT_EDGES) } == 0 && spins < 1000 {
        unsafe { asm!("wfi") };
        spins += 1;
    }
    puts(b"int ");
    hex(read_reg(MCP_GPIO)); // reading GPIO releases INT
    putc(b'\n');
    puts(b"i2c done\n");
    unsafe { asm!("bkpt #1") };
    loop {
        core::hint::spin_loop();
    }
}
