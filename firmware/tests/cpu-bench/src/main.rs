//! Bare-metal test firmware that keeps the CPU busy with the kind of code
//! real firmware runs: a bitwise CRC, a sort, integer multiplies, block
//! copies, a bytecode interpreter loop (as MicroPython's VM) and 64-bit
//! division through compiler-builtins. Prints a checksum on UART0 and halts
//! on BKPT. It is the CPU-bound part of the PGO training runs
//! (`scripts/pgo-build.sh`), and a benchmark:
//! `tartelette cpu-bench --headless` and time it.
#![no_std]
#![no_main]

use core::{
    arch::{asm, global_asm},
    hint::black_box,
    ptr::{read_volatile, write_volatile},
};

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
/// Rounds of the whole mix: about 300 M cycles (2.4 s emulated).
const ROUNDS: u32 = 500;

unsafe extern "C" {
    static mut _sdata: u32;
    static mut _edata: u32;
    static _sidata: u32;
    static mut _sbss: u32;
    static mut _ebss: u32;
}

static mut BUF: [u8; 2048] = [0; 2048];
static mut WORDS: [u32; 256] = [0; 256];
static mut COPY: [u32; 512] = [0; 512];

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

fn put_hex(v: u32) {
    for i in (0..8).rev() {
        putc(b"0123456789abcdef"[(v >> (4 * i) & 0xF) as usize]);
    }
}

struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { crc >> 1 ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn insertion_sort(v: &mut [u32]) {
    for i in 1..v.len() {
        let x = v[i];
        let mut j = i;
        while j > 0 && v[j - 1] > x {
            v[j] = v[j - 1];
            j -= 1;
        }
        v[j] = x;
    }
}

fn matmul(a: &[[i32; 12]; 12], b: &[[i32; 12]; 12]) -> i32 {
    let mut sum = 0i32;
    for i in 0..12 {
        for j in 0..12 {
            let mut acc = 0i32;
            for k in 0..12 {
                acc = acc.wrapping_add(a[i][k].wrapping_mul(b[k][j]));
            }
            sum = sum.wrapping_add(acc);
        }
    }
    sum
}

#[derive(Clone, Copy)]
enum Op {
    Push(i32),
    Add,
    Mul,
    Dup,
    Swap,
    JumpIfPos(u8),
    Dec,
    Store,
    Halt,
}

/// A stack machine counting a loop down, as an interpreter's dispatch loop.
fn interpret(program: &[Op], fuel: u32) -> i32 {
    let mut stack = [0i32; 16];
    let (mut sp, mut pc, mut acc) = (0usize, 0usize, 0i32);
    for _ in 0..fuel {
        match program[pc] {
            Op::Push(v) => {
                stack[sp] = v;
                sp += 1;
            }
            Op::Add => {
                sp -= 1;
                stack[sp - 1] = stack[sp - 1].wrapping_add(stack[sp]);
            }
            Op::Mul => {
                sp -= 1;
                stack[sp - 1] = stack[sp - 1].wrapping_mul(stack[sp]);
            }
            Op::Dup => {
                stack[sp] = stack[sp - 1];
                sp += 1;
            }
            Op::Swap => stack.swap(sp - 1, sp - 2),
            Op::JumpIfPos(to) => {
                sp -= 1;
                if stack[sp] > 0 {
                    pc = to as usize;
                    continue;
                }
            }
            Op::Dec => stack[sp - 1] -= 1,
            Op::Store => acc = acc.wrapping_add(stack[sp - 1]),
            Op::Halt => break,
        }
        pc += 1;
    }
    acc
}

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

    let (buf, words, copy) = unsafe { (&mut *&raw mut BUF, &mut *&raw mut WORDS, &mut *&raw mut COPY) };
    let program = [
        Op::Push(0),
        Op::Push(400),
        // loop: counter on top
        Op::Dup,
        Op::Dup,
        Op::Mul,
        Op::Store,
        Op::Swap,
        Op::Push(3),
        Op::Add,
        Op::Swap,
        Op::Dec,
        Op::Dup,
        Op::JumpIfPos(2),
        Op::Halt,
    ];
    let mut rng = Rng(0x1234_5678);
    let mut check = 0u32;
    for round in 0..ROUNDS {
        for b in buf.iter_mut() {
            *b = rng.next() as u8;
        }
        check ^= crc32(black_box(&buf[..]));
        for w in words.iter_mut() {
            *w = rng.next();
        }
        insertion_sort(black_box(&mut words[..]));
        check = check.wrapping_add(words[round as usize % 256]);
        let mut a = [[0i32; 12]; 12];
        let mut b = [[0i32; 12]; 12];
        for i in 0..12 {
            for j in 0..12 {
                a[i][j] = rng.next() as i32 >> 20;
                b[i][j] = rng.next() as i32 >> 20;
            }
        }
        check = check.wrapping_add(matmul(black_box(&a), black_box(&b)) as u32);
        for k in 0..8 {
            copy[..256].copy_from_slice(&words[..]);
            copy.copy_within(0..256, 256);
            check = check.wrapping_add(copy[k * 37 % 512]);
        }
        check = check.wrapping_add(interpret(black_box(&program), 100_000) as u32);
        let mut q = 0u64;
        for _ in 0..200 {
            let n = (rng.next() as u64) << 32 | rng.next() as u64;
            let d = (rng.next() | 1) as u64;
            q = q.wrapping_add(black_box(n) / black_box(d));
        }
        check = check.wrapping_add(q as u32 ^ (q >> 32) as u32);
    }

    puts(b"cpu-bench ");
    put_hex(check);
    putc(b'\n');
    while rd(UART0 + 0x18) & (1 << 3) != 0 {} // BUSY: let the FIFO drain
    unsafe { asm!("bkpt #1") };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
