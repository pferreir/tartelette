//! Bare-metal test firmware for interrupts: TIMER alarm 0 (NVIC IRQ 0),
//! SysTick and PendSV, with the main loop sleeping in WFI between events.
//! Expected UART0 output: "start\nA ok\nTTT\nP\nirq done\n".
#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::ptr::{read_volatile, write_volatile};

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
    ".word pendsv",
    ".word systick",
    ".word timer_irq0", // IRQ 0
);

#[used]
#[unsafe(link_section = ".boot2")]
static BOOT2: [u8; 256] = [0; 256];

const RESETS: u32 = 0x4000_C000;
const UART0: u32 = 0x4003_4000;
const TIMER: u32 = 0x4005_4000;
const SYST_CSR: u32 = 0xE000_E010;
const SYST_RVR: u32 = 0xE000_E014;
const SYST_CVR: u32 = 0xE000_E018;
const NVIC_ISER: u32 = 0xE000_E100;
const ICSR: u32 = 0xE000_ED04;

static mut ALARM_FIRED: u32 = 0;
static mut TICKS: u32 = 0;
static mut PENDSV_RAN: u32 = 0;

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
fn wfi() {
    unsafe { asm!("wfi") }
}
fn get(p: *const u32) -> u32 {
    unsafe { read_volatile(p) }
}
fn bump(p: *mut u32) -> u32 {
    unsafe {
        let v = read_volatile(p) + 1;
        write_volatile(p, v);
        v
    }
}

#[unsafe(no_mangle)]
extern "C" fn default_handler() {
    puts(b"unexpected exception\n");
    unsafe { asm!("bkpt #2") };
    loop {}
}

#[unsafe(no_mangle)]
extern "C" fn timer_irq0() {
    wr(TIMER + 0x34, 1); // INTR: clear alarm 0
    bump(&raw mut ALARM_FIRED);
}

#[unsafe(no_mangle)]
extern "C" fn systick() {
    if bump(&raw mut TICKS) == 3 {
        wr(SYST_CSR, 0);
    }
    putc(b'T');
}

#[unsafe(no_mangle)]
extern "C" fn pendsv() {
    putc(b'P');
    bump(&raw mut PENDSV_RAN);
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    wr(RESETS + 0x3000, 1 << 22 | 1 << 21); // release UART0 and TIMER
    while rd(RESETS + 0x8) & (1 << 22 | 1 << 21) != 1 << 22 | 1 << 21 {}
    wr(UART0 + 0x30, 0x301);
    puts(b"start\n");

    // Alarm 0 in 1000 us; sleep until its interrupt wakes the core.
    let t0 = rd(TIMER + 0x28);
    wr(TIMER + 0x38, 1); // INTE alarm 0
    wr(NVIC_ISER, 1);
    wr(TIMER + 0x10, t0.wrapping_add(1000));
    while get(&raw const ALARM_FIRED) == 0 {
        wfi();
    }
    let dt = rd(TIMER + 0x28).wrapping_sub(t0);
    puts(if (1000..1100).contains(&dt) { b"A ok\n" } else { b"A bad\n" });

    // SysTick on the CPU clock: 3 interrupts, 100 us apart.
    wr(SYST_RVR, 12_499);
    wr(SYST_CVR, 0);
    wr(SYST_CSR, 0b111);
    while get(&raw const TICKS) < 3 {
        wfi();
    }
    putc(b'\n');

    // PendSV with interrupts masked runs only after cpsie.
    unsafe { asm!("cpsid i") };
    wr(ICSR, 1 << 28);
    if get(&raw const PENDSV_RAN) != 0 {
        puts(b"pendsv ran while masked\n");
    }
    unsafe { asm!("cpsie i") };
    unsafe { asm!("nop", "nop") };
    putc(b'\n');

    puts(b"irq done\n");
    unsafe { asm!("bkpt #1") };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
