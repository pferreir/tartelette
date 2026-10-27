//! Bare-metal test firmware for the board layer: GPIO25 (LED) follows a
//! button on GPIO2 (pull-up, pressed = low). After one press and release it
//! prints "done" on UART0 and stops with `bkpt #1`.
#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::ptr::{read_volatile, write_volatile};

global_asm!(
    ".section .vectors, \"a\"",
    ".word _stack_top",
    ".word reset",
);

#[used]
#[unsafe(link_section = ".boot2")]
static BOOT2: [u8; 256] = [0; 256];

const RESETS: u32 = 0x4000_C000;
const IO_BANK0: u32 = 0x4001_4000;
const PADS_BANK0: u32 = 0x4001_C000;
const UART0: u32 = 0x4003_4000;
const SIO: u32 = 0xD000_0000;

const BUTTON: u32 = 2;
const LED: u32 = 25;

fn wr(addr: u32, v: u32) {
    unsafe { write_volatile(addr as *mut u32, v) }
}
fn rd(addr: u32) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    let release = 1 << 5 | 1 << 8 | 1 << 22;
    wr(RESETS + 0x3000, release);
    while rd(RESETS + 0x8) & release != release {}
    wr(UART0 + 0x30, 0x301);

    for pin in [BUTTON, LED] {
        wr(IO_BANK0 + 8 * pin + 4, 5); // SIO
    }
    wr(PADS_BANK0 + 4 + 4 * BUTTON, 1 << 6 | 1 << 3); // input enable + pull-up
    wr(SIO + 0x24, 1 << LED); // OE_SET

    let mut was_pressed = false;
    loop {
        let pressed = rd(SIO + 0x04) >> BUTTON & 1 == 0;
        wr(if pressed { SIO + 0x14 } else { SIO + 0x18 }, 1 << LED);
        if was_pressed && !pressed {
            break;
        }
        was_pressed = pressed;
    }
    for &c in b"done\n" {
        while rd(UART0 + 0x18) & (1 << 5) != 0 {}
        wr(UART0, c as u32);
    }
    unsafe { asm!("bkpt #1") };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
