//! Bare-metal test firmware for the emulator's batched execution: the core
//! never sleeps, and the timer alarm and SysTick are (re)armed while it spins
//! in RAM-only loops, so interrupts must arrive at exactly the right cycle.
//! Prints the timer value at entry to each interrupt handler, in hex.
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
    ".word default_handler",
    ".word systick",
    ".word timer_irq0", // IRQ 0
);

const SYST_CSR: u32 = 0xE000_E010;
const SYST_RVR: u32 = 0xE000_E014;
const SYST_CVR: u32 = 0xE000_E018;

const SAMPLES: usize = 16;
static mut ALARM_AT: [u32; SAMPLES] = [0; SAMPLES];
static mut TICK_AT: [u32; SAMPLES] = [0; SAMPLES];
static mut ALARMS: u32 = 0;
static mut TICKS: u32 = 0;

fn hex(v: u32) {
    for i in (0..8).rev() {
        putc(b"0123456789abcdef"[(v >> (4 * i) & 15) as usize]);
    }
    putc(b'\n');
}

#[unsafe(no_mangle)]
extern "C" fn timer_irq0() {
    let now = rd(TIMER + 0x28);
    wr(TIMER + 0x34, 1); // INTR: clear alarm 0
    unsafe {
        let n = read_volatile(&raw const ALARMS) as usize;
        write_volatile((&raw mut ALARM_AT as *mut u32).add(n), now);
        write_volatile(&raw mut ALARMS, n as u32 + 1);
        if n + 1 < SAMPLES {
            // Re-arm, with a different delay each time, from inside the handler.
            wr(TIMER + 0x10, now.wrapping_add(7 + 5 * n as u32));
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn systick() {
    let now = rd(TIMER + 0x28);
    unsafe {
        let n = read_volatile(&raw const TICKS) as usize;
        write_volatile((&raw mut TICK_AT as *mut u32).add(n), now);
        write_volatile(&raw mut TICKS, n as u32 + 1);
        if n + 1 == SAMPLES {
            wr(SYST_CSR, 0);
        }
    }
}

/// Spin without touching any device until `p` reaches SAMPLES.
fn spin_until_full(p: *const u32) {
    while (get(p) as usize) < SAMPLES {
        unsafe { asm!("nop", "nop", "nop") };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    wr(RESETS + 0x3000, 1 << 22 | 1 << 21); // release UART0 and TIMER
    while rd(RESETS + 0x8) & (1 << 22 | 1 << 21) != 1 << 22 | 1 << 21 {}
    wr(UART0 + 0x30, 0x301);

    // Alarm 0 re-arming itself from its own handler.
    wr(TIMER + 0x38, 1); // INTE alarm 0
    wr(NVIC_ISER, 1);
    wr(TIMER + 0x10, rd(TIMER + 0x28).wrapping_add(50));
    spin_until_full(&raw const ALARMS);

    // SysTick every 400 cycles, spun on after a few register writes.
    wr(SYST_RVR, 399);
    wr(SYST_CVR, 0);
    wr(SYST_CSR, 0b111);
    spin_until_full(&raw const TICKS);

    for i in 0..SAMPLES {
        hex(unsafe { read_volatile((&raw const ALARM_AT as *const u32).add(i)) });
    }
    for i in 0..SAMPLES {
        hex(unsafe { read_volatile((&raw const TICK_AT as *const u32).add(i)) });
    }
    unsafe { asm!("bkpt #1") };
    loop {
        core::hint::spin_loop();
    }
}
