//! Bare-metal test firmware: says "ready" on UART0 with its receiver still
//! off, waits a while, turns the receiver on and echoes what arrives in upper
//! case, asleep (WFI, woken by the RX and receive-timeout interrupts) while
//! nothing does. A '.' ends it: "bye", then BKPT.
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
const UART0: u32 = 0x4003_4000;
const NVIC_ISER: u32 = 0xE000_E100;
const IRQ_UART0: u32 = 20;

fn wr(addr: u32, v: u32) {
    unsafe { write_volatile(addr as *mut u32, v) }
}
fn rd(addr: u32) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

fn putc(c: u8) {
    while rd(UART0 + 0x18) & (1 << 5) != 0 {} // TXFF
    wr(UART0, c as u32);
}

fn puts(s: &[u8]) {
    for &c in s {
        putc(c);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    wr(RESETS + 0x3000, 1 << 22); // atomic clear: release UART0
    while rd(RESETS + 0x8) & (1 << 22) == 0 {}
    wr(UART0 + 0x24, 67); // IBRD/FBRD for 115200 @ 125 MHz
    wr(UART0 + 0x28, 52);
    wr(UART0 + 0x2C, 0x70); // 8N1, FIFOs on
    wr(UART0 + 0x30, 0x101); // UARTEN | TXE: not receiving yet
    puts(b"ready\n");
    // A few milliseconds deaf: what the host sends meanwhile must wait.
    for _ in 0..200_000 {
        unsafe { asm!("nop") };
    }
    wr(UART0 + 0x38, 1 << 4 | 1 << 6); // RX and receive timeout interrupts
    wr(NVIC_ISER, 1 << IRQ_UART0);
    // Masked: the interrupt wakes WFI without a handler running.
    unsafe { asm!("cpsid i") };
    wr(UART0 + 0x30, 0x301); // | RXE

    loop {
        while rd(UART0 + 0x18) & (1 << 4) != 0 {
            unsafe { asm!("wfi") }; // RXFE
        }
        let c = rd(UART0) as u8;
        if c == b'.' {
            break;
        }
        putc(c.to_ascii_uppercase());
    }
    puts(b"bye\n");
    while rd(UART0 + 0x18) & (1 << 3) != 0 {} // BUSY: let the FIFO drain
    unsafe { asm!("bkpt #1") };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
