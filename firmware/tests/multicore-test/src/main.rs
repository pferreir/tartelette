//! Bare-metal test firmware for the second core. Core 0 resets core 1
//! through the PSM, starts it with the bootrom's FIFO handshake
//! (0, 0, 1, vector table, stack pointer, entry, each echoed back), then both
//! cores run at once:
//!
//! * a counter incremented by both cores under SIO spinlock 3,
//! * each core's own hardware divider checked against software division,
//! * a FIFO echo service on core 1, and a word from core 1 that interrupts
//!   core 0 (SIO_IRQ_PROC0) while it sleeps.
//!
//! Core 1 reports through shared memory only; all output comes from core 0.
//! Expected output:
//! "start\ncore1 launched\ncore1 id 1\nlock ok\ndiv ok\nfifo ok\nfifo irq ok\nmulticore done\n"
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
    ".rept 15",
    ".word default_handler", // IRQ 0..14
    ".endr",
    ".word sio_irq_proc0", // IRQ 15
);

unsafe extern "C" {
    static vectors: u32;
}

const PSM: u32 = 0x4001_0000;

const SIO_CPUID: u32 = SIO;
const FIFO_ST: u32 = SIO + 0x50;
const FIFO_WR: u32 = SIO + 0x54;
const FIFO_RD: u32 = SIO + 0x58;
const UDIVIDEND: u32 = SIO + 0x60;
const UDIVISOR: u32 = SIO + 0x64;
const QUOTIENT: u32 = SIO + 0x70;
const REMAINDER: u32 = SIO + 0x74;
const SPINLOCK3: u32 = SIO + 0x100 + 4 * 3;

const FIFO_VLD: u32 = 1;
const FIFO_RDY: u32 = 2;
const PROC1: u32 = 1 << 16;

const INCREMENTS: u32 = 200;
const DIV_ROUNDS: u32 = 50;
const PING_IRQ: u32 = 0xFFFF_0000;
const IRQ_WORD: u32 = 0xCAFE;

// Shared between the cores (all accesses volatile).
static mut CORE1_ID: u32 = 0xFF;
static mut COUNTER: u32 = 0;
static mut LOCK1_DONE: u32 = 0;
static mut DIV1_ERRORS: u32 = 0;
static mut DIV1_DONE: u32 = 0;
static mut IRQ_VALUE: u32 = 0;
static mut CORE1_STACK: [u32; 256] = [0; 256];

fn sev() {
    unsafe { asm!("sev") };
}
fn wfe() {
    unsafe { asm!("wfe") };
}

// pico-sdk style blocking FIFO access.
fn fifo_push(v: u32) {
    while rd(FIFO_ST) & FIFO_RDY == 0 {}
    wr(FIFO_WR, v);
    sev();
}
fn fifo_pop() -> u32 {
    while rd(FIFO_ST) & FIFO_VLD == 0 {
        wfe();
    }
    rd(FIFO_RD)
}
fn fifo_drain() {
    while rd(FIFO_ST) & FIFO_VLD != 0 {
        rd(FIFO_RD);
    }
}

fn lock() {
    while rd(SPINLOCK3) == 0 {}
}
fn unlock() {
    wr(SPINLOCK3, 0);
}

/// Increment the shared counter `INCREMENTS` times under the spinlock; the
/// read and the write are separated so that without the lock the other core
/// would regularly slip in between them.
fn locked_increments() {
    for _ in 0..INCREMENTS {
        lock();
        let v = get(&raw const COUNTER);
        unsafe { asm!("nop", "nop", "nop") };
        put(&raw mut COUNTER, v + 1);
        unlock();
    }
}

/// Use this core's hardware divider on `ROUNDS` operand pairs, doing a little
/// unrelated work between starting and reading so the other core's divider
/// gets used in the gaps. Returns the number of wrong results.
fn divider_rounds(seed: u32) -> u32 {
    let mut errors = 0;
    for i in 0..DIV_ROUNDS {
        let n = seed.wrapping_mul(2654435761).wrapping_add(i * 977);
        let d = (i % 13) + 2;
        wr(UDIVIDEND, n);
        wr(UDIVISOR, d);
        unsafe { asm!("nop", "nop", "nop", "nop") };
        let (q, r) = (rd(QUOTIENT), rd(REMAINDER));
        if q != n / d || r != n % d {
            errors += 1;
        }
    }
    errors
}

#[unsafe(no_mangle)]
extern "C" fn sio_irq_proc0() {
    // Reading the word empties the FIFO, which drops the level interrupt.
    put(&raw mut IRQ_VALUE, rd(FIFO_RD));
}

/// Core 1 starts here, with the stack and vector table core 0 gave it.
#[unsafe(no_mangle)]
extern "C" fn core1_main() -> ! {
    put(&raw mut CORE1_ID, rd(SIO_CPUID));
    locked_increments();
    put(&raw mut LOCK1_DONE, 1);
    put(&raw mut DIV1_ERRORS, divider_rounds(7));
    put(&raw mut DIV1_DONE, 1);
    // Echo service: add one, and answer the special ping with an interrupting word.
    loop {
        let v = fifo_pop();
        fifo_push(if v == PING_IRQ { IRQ_WORD } else { v + 1 });
    }
}

/// Reset core 1 through the PSM, then run the bootrom's launch handshake.
fn launch_core1() {
    wr(PSM + 0x4, PROC1); // FRCE_OFF.PROC1
    while rd(PSM + 0x4) & PROC1 == 0 {}
    wr(PSM + 0x4, 0);

    let stack_top = addr_of_mut!(CORE1_STACK) as u32 + 4 * 256;
    let seq = [
        0,
        0,
        1,
        &raw const vectors as u32,
        stack_top,
        core1_main as *const () as usize as u32 | 1,
    ];
    let mut i = 0;
    while i < seq.len() {
        let cmd = seq[i];
        if cmd == 0 {
            fifo_drain();
            sev();
        }
        fifo_push(cmd);
        // The bootrom echoes every word back; anything else restarts the sequence.
        i = if fifo_pop() == cmd { i + 1 } else { 0 };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    wr(RESETS + 0x3000, 1 << 22); // UART0 out of reset
    while rd(RESETS + 0x8) & (1 << 22) == 0 {}
    wr(UART0 + 0x30, 0x301);
    puts(b"start\n");

    launch_core1();
    puts(b"core1 launched\n");
    while get(&raw const CORE1_ID) == 0xFF {}
    puts(b"core1 id ");
    putc(b'0' + get(&raw const CORE1_ID) as u8);
    putc(b'\n');

    // Both cores are running now: count together, then divide together.
    locked_increments();
    while get(&raw const LOCK1_DONE) == 0 {}
    check(
        get(&raw const COUNTER) == 2 * INCREMENTS,
        b"lock ok\n",
        b"lock bad\n",
    );

    let mine = divider_rounds(3);
    while get(&raw const DIV1_DONE) == 0 {}
    check(
        mine == 0 && get(&raw const DIV1_ERRORS) == 0,
        b"div ok\n",
        b"div bad\n",
    );

    // Echo service on core 1.
    let mut echoed_ok = true;
    for v in [10, 20, 30] {
        fifo_push(v);
        echoed_ok &= fifo_pop() == v + 1;
    }
    check(echoed_ok, b"fifo ok\n", b"fifo bad\n");

    // Core 1's word interrupts core 0 while it sleeps.
    wr(NVIC_ISER, 1 << 15); // SIO_IRQ_PROC0
    fifo_push(PING_IRQ);
    let mut spins = 0;
    while get(&raw const IRQ_VALUE) == 0 && spins < 1000 {
        unsafe { asm!("wfi") };
        spins += 1;
    }
    check(
        get(&raw const IRQ_VALUE) == IRQ_WORD,
        b"fifo irq ok\n",
        b"fifo irq bad\n",
    );

    puts(b"multicore done\n");
    unsafe { asm!("bkpt #1") };
    loop {
        core::hint::spin_loop();
    }
}
