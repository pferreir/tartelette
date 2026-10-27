//! Bare-metal test firmware: takes UART0 out of reset, prints a greeting
//! (from .data) and the digits 0-9 (from a .bss counter), calls the bootrom's
//! memcpy via its function table, then halts on BKPT.
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

static mut GREETING: [u8; 20] = *b"Hello from RP2040!\r\n";
static mut COUNTER: u8 = 0;

unsafe extern "C" {
    static mut _sdata: u32;
    static mut _edata: u32;
    static _sidata: u32;
    static mut _sbss: u32;
    static mut _ebss: u32;
}

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

/// pico-sdk's `rom_func_lookup`: the bootrom header holds 16-bit pointers to
/// the function table (0x14) and to the table-lookup routine (0x18).
fn rom_func_lookup(c1: u8, c2: u8) -> usize {
    unsafe {
        let table = read_volatile(0x14 as *const u16) as usize;
        let lookup = read_volatile(0x18 as *const u16) as usize;
        let f: extern "C" fn(usize, u32) -> usize = core::mem::transmute(lookup);
        f(table, c1 as u32 | (c2 as u32) << 8)
    }
}

static SRC: [u8; 8] = *b"romcpy!\n";
static mut DST: [u8; 8] = [0; 8];

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    unsafe {
        let mut src = &raw const _sidata;
        let mut dst = &raw mut _sdata;
        while dst < &raw mut _edata {
            write_volatile(dst, read_volatile(src));
            dst = dst.add(1);
            src = src.add(1);
        }
        let mut p = &raw mut _sbss;
        while p < &raw mut _ebss {
            write_volatile(p, 0);
            p = p.add(1);
        }
    }

    wr(RESETS + 0x3000, 1 << 22); // atomic clear: release UART0
    while rd(RESETS + 0x8) & (1 << 22) == 0 {}
    wr(UART0 + 0x24, 67); // IBRD/FBRD for 115200 @ 125 MHz
    wr(UART0 + 0x28, 52);
    wr(UART0 + 0x2C, 0x70); // 8N1, FIFOs on
    wr(UART0 + 0x30, 0x301); // UARTEN | TXE

    for i in 0..20 {
        putc(unsafe { read_volatile(&raw const GREETING[i]) });
    }
    for _ in 0..10 {
        unsafe {
            let n = read_volatile(&raw const COUNTER);
            putc(b'0' + n);
            write_volatile(&raw mut COUNTER, n + 1);
        }
    }
    putc(b'\n');

    let memcpy = rom_func_lookup(b'M', b'C');
    if memcpy == 0 {
        puts(b"rom memcpy missing\n");
    } else {
        unsafe {
            let f: extern "C" fn(*mut u8, *const u8, u32) -> *mut u8 = core::mem::transmute(memcpy);
            f((&raw mut DST).cast(), SRC.as_ptr(), 8);
            puts(&*(&raw const DST));
        }
    }
    while rd(UART0 + 0x18) & (1 << 3) != 0 {} // BUSY: let the FIFO drain
    unsafe { asm!("bkpt #1") };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
