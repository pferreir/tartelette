//! Bare-metal test firmware for a core polling memory while the other one
//! sleeps, a loop the emulator may skip ahead through: whatever ends the
//! wait must find the core where it would have been, on the same cycle.
//! Core 0 waits in turn for
//!
//! * a timer interrupt, polling a counter under a spinlock with interrupts
//!   masked, as the pico-sdk's queues do,
//! * a word the DMA writes, when a PIO machine asks for it,
//! * core 1, woken by its own timer alarm.
//!
//! It prints SysTick (counting down every cycle) as each wait ends, in hex.
#![no_std]
#![no_main]

use core::{
    arch::{asm, global_asm},
    ptr::addr_of_mut,
};

use testrt::*;

global_asm!(
    ".section .vectors, \"a\"",
    ".global vectors",
    "vectors:",
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
    ".word timer_irq0",      // IRQ 0
    ".word timer_irq1",      // IRQ 1
);

unsafe extern "C" {
    static vectors: u32;
}

const PSM: u32 = 0x4001_0000;
const PROC1: u32 = 1 << 16;
const FIFO_ST: u32 = SIO + 0x50;
const FIFO_WR: u32 = SIO + 0x54;
const FIFO_RD: u32 = SIO + 0x58;
const SPINLOCK5: u32 = SIO + 0x100 + 4 * 5;
const SYST_CSR: u32 = 0xE000_E010;
const SYST_RVR: u32 = 0xE000_E014;
const SYST_CVR: u32 = 0xE000_E018;

const PIO0: u32 = 0x5020_0000;
const PIO1: u32 = 0x5030_0000;
const DREQ_PIO0_RX0: u32 = 4;

const TICKS: u32 = 6;
static mut COUNT: u32 = 0;
static mut LANDED: u32 = 0;
static mut CORE1_DONE: u32 = 0;
static mut CORE1_STACK: [u32; 128] = [0; 128];

fn hex(v: u32) {
    for i in (0..8).rev() {
        putc(b"0123456789abcdef"[(v >> (4 * i) & 15) as usize]);
    }
    putc(b'\n');
}

/// Alarm 0, on core 0: counts, and re-arms with a different delay each time.
#[unsafe(no_mangle)]
extern "C" fn timer_irq0() {
    wr(TIMER + 0x34, 1);
    let n = get(&raw const COUNT) + 1;
    put(&raw mut COUNT, n);
    if n < TICKS {
        wr(TIMER + 0x10, rd(TIMER + 0x28).wrapping_add(40 + 30 * n));
    }
}

/// Alarm 1, on core 1.
#[unsafe(no_mangle)]
extern "C" fn timer_irq1() {
    wr(TIMER + 0x34, 2);
    put(&raw mut CORE1_DONE, 1);
}

/// Read `p` under spinlock 5 with interrupts masked.
fn locked_get(p: *const u32) -> u32 {
    unsafe { asm!("cpsid i") };
    while rd(SPINLOCK5) == 0 {}
    let v = get(p);
    wr(SPINLOCK5, 0);
    unsafe { asm!("cpsie i") };
    v
}

fn stamp() {
    hex(rd(SYST_CVR));
}

#[unsafe(no_mangle)]
extern "C" fn core1_main() -> ! {
    wr(NVIC_ISER, 1 << 1);
    wr(TIMER + 0x14, rd(TIMER + 0x28).wrapping_add(300));
    loop {
        unsafe { asm!("wfi") };
    }
}

fn fifo_push(v: u32) {
    while rd(FIFO_ST) & 2 == 0 {}
    wr(FIFO_WR, v);
    unsafe { asm!("sev") };
}

fn fifo_pop() -> u32 {
    while rd(FIFO_ST) & 1 == 0 {
        unsafe { asm!("wfe") };
    }
    rd(FIFO_RD)
}

/// Reset core 1 through the PSM, then run the bootrom's launch handshake.
fn launch_core1() {
    wr(PSM + 0x4, PROC1);
    while rd(PSM + 0x4) & PROC1 == 0 {}
    wr(PSM + 0x4, 0);
    let seq = [
        0,
        0,
        1,
        &raw const vectors as u32,
        addr_of_mut!(CORE1_STACK) as u32 + 4 * 128,
        core1_main as *const () as usize as u32 | 1,
    ];
    let mut i = 0;
    while i < seq.len() {
        if seq[i] == 0 {
            while rd(FIFO_ST) & 1 != 0 {
                rd(FIFO_RD);
            }
            unsafe { asm!("sev") };
        }
        fifo_push(seq[i]);
        i = if fifo_pop() == seq[i] { i + 1 } else { 0 };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    // UART0, TIMER, PIO1, PIO0 and DMA out of reset.
    let release = 1 << 22 | 1 << 21 | 1 << 11 | 1 << 10 | 1 << 2;
    wr(RESETS + 0x3000, release);
    while rd(RESETS + 0x8) & release != release {}
    wr(UART0 + 0x30, 0x301);
    wr(SYST_RVR, 0xFF_FFFF);
    wr(SYST_CVR, 0);
    wr(SYST_CSR, 0b101);

    wr(TIMER + 0x38, 1 << 1 | 1);
    wr(NVIC_ISER, 1);
    wr(TIMER + 0x10, rd(TIMER + 0x28).wrapping_add(20));
    while locked_get(&raw const COUNT) < TICKS {}
    stamp();

    // PIO0's machine 0 waits about 1000 cycles and pushes 7; the DMA,
    // waiting on its RX request, moves it into LANDED.
    let program = [
        0xFF3F, // set x, 31 [31]
        0x1F41, // jmp x--, 1 [31]
        0xE027, // set x, 7
        0x4020, // in x, 32
        0x8020, // push block
    ];
    for (i, op) in program.iter().enumerate() {
        wr(PIO0 + 0x048 + 4 * i as u32, *op);
    }
    wr(DMA + 0x40, PIO0 + 0x020); // channel 1: from RXF0
    wr(DMA + 0x44, &raw mut LANDED as u32);
    wr(DMA + 0x48, 1);
    wr(DMA + 0x4C, DREQ_PIO0_RX0 << 15 | 1 << 11 | 2 << 2 | 1);
    wr(PIO0, 1); // enable machine 0
    while get(&raw const LANDED) != 7 {}
    stamp();

    // PIO1 ticks along on its own, raising and clearing one of its flags
    // (no interrupt), which cuts the core's batches short as busy
    // peripherals do.
    wr(PIO1 + 0x048, 0xDF00); // irq nowait 0 [31]
    wr(PIO1 + 0x04C, 0xDF40); // irq clear 0 [31]
    wr(PIO1, 1);
    launch_core1();
    while locked_get(&raw const CORE1_DONE) == 0 {}
    stamp();

    unsafe { asm!("bkpt #1") };
    loop {
        core::hint::spin_loop();
    }
}
