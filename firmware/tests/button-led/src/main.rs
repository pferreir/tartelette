//! Bare-metal test firmware for the board layer: GPIO25 (LED) follows a
//! button on GPIO2 (pull-up, pressed = low). After one press and release it
//! prints "done" on UART0 and stops with `bkpt #1`.
#![no_std]
#![no_main]

use core::arch::{asm, global_asm};

use testrt::*;

global_asm!(
    ".section .vectors, \"a\"",
    ".word _stack_top",
    ".word reset",
);

const BUTTON: u32 = 2;
const LED: u32 = 25;

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
    puts(b"done\n");
    unsafe { asm!("bkpt #1") };
    loop {
        core::hint::spin_loop();
    }
}
