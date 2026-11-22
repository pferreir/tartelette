//! What the bare-metal test firmware shares: a zeroed boot2 (the emulator
//! boots without one), the panic handler, a handler for exceptions a test
//! doesn't expect, register access and output on UART0. Each test has its own
//! vector table and `reset`, and takes UART0 out of reset itself.
#![no_std]

use core::{
    arch::asm,
    ptr::{read_volatile, write_volatile},
};

pub const RESETS: u32 = 0x4000_C000;
pub const IO_BANK0: u32 = 0x4001_4000;
pub const PADS_BANK0: u32 = 0x4001_C000;
pub const UART0: u32 = 0x4003_4000;
pub const TIMER: u32 = 0x4005_4000;
pub const DMA: u32 = 0x5000_0000;
pub const SIO: u32 = 0xD000_0000;
pub const NVIC_ISER: u32 = 0xE000_E100;

#[used]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".boot2")]
static BOOT2: [u8; 256] = [0; 256];

#[inline(always)]
pub fn wr(addr: u32, v: u32) {
    unsafe { write_volatile(addr as *mut u32, v) }
}

#[inline(always)]
pub fn rd(addr: u32) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

/// Reads a word shared with an interrupt handler or the other core. Safe
/// to call because the tests only pass pointers to their own statics.
#[inline(always)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn get(p: *const u32) -> u32 {
    unsafe { read_volatile(p) }
}

#[inline(always)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn put(p: *mut u32, v: u32) {
    unsafe { write_volatile(p, v) }
}

#[inline]
pub fn putc(c: u8) {
    while rd(UART0 + 0x18) & (1 << 5) != 0 {} // TXFF
    wr(UART0, c as u32);
}

#[inline]
pub fn puts(s: &[u8]) {
    for &c in s {
        putc(c);
    }
}

#[inline]
pub fn check(ok: bool, pass: &[u8], fail: &[u8]) {
    puts(if ok { pass } else { fail });
}

#[unsafe(no_mangle)]
extern "C" fn default_handler() -> ! {
    puts(b"unexpected exception\n");
    unsafe { asm!("bkpt #2") };
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
