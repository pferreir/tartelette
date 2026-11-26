//! The Uxn CPU: 64 KiB of memory, a working and a return stack of 256 bytes
//! each, and 256 bytes of device ports. See
//! <https://wiki.xxiivv.com/site/uxntal_reference.html>; the semantics here
//! follow uxn11, the reference emulator.
//!
//! An instruction byte is an opcode in its low five bits and three mode
//! bits: 0x20 short (16-bit operands), 0x40 return (the stacks swap roles),
//! 0x80 keep (operands are read but left on the stack). Opcode 0 with modes
//! is BRK, the immediate jumps JCI/JMI/JSI and the literals LIT/LIT2/LITr/
//! LIT2r. The eight mode combinations each get their own copy of the
//! interpreter (`op` is generic over them), so the modes cost nothing at
//! run time.

/// What sits behind the device ports. `dei` reads port `port` (the second
/// byte of a short read is taken from `dev` as it is after that), `deo` is
/// told after `dev[port]` was written. A short write tells both ports in
/// turn, each after its own byte, as uxn does: File/delete, at an even port,
/// is written as a short.
pub trait Devices {
    fn dei(&mut self, u: &mut Uxn, port: u8) -> u8;
    fn deo(&mut self, u: &mut Uxn, port: u8);
}

pub struct Stack {
    pub dat: [u8; 256],
    pub ptr: u8,
}

impl Stack {
    const fn new() -> Self {
        Self {
            dat: [0; 256],
            ptr: 0,
        }
    }

    #[inline(always)]
    fn push8(&mut self, v: u8) {
        self.dat[self.ptr as usize] = v;
        self.ptr = self.ptr.wrapping_add(1);
    }

    #[inline(always)]
    fn push<const S: bool>(&mut self, v: u16) {
        if S {
            self.push8((v >> 8) as u8);
        }
        self.push8(v as u8);
    }
}

/// Reads operands down from a stack pointer without moving the stack's own,
/// so that keep mode just doesn't commit.
struct Pop(u8);

impl Pop {
    #[inline(always)]
    fn byte(&mut self, st: &Stack) -> u8 {
        self.0 = self.0.wrapping_sub(1);
        st.dat[self.0 as usize]
    }

    #[inline(always)]
    fn short(&mut self, st: &Stack) -> u16 {
        let lo = self.byte(st) as u16;
        (self.byte(st) as u16) << 8 | lo
    }

    #[inline(always)]
    fn val<const S: bool>(&mut self, st: &Stack) -> u16 {
        if S {
            self.short(st)
        } else {
            self.byte(st) as u16
        }
    }
}

pub struct Uxn {
    pub ram: [u8; 0x10000],
    pub dev: [u8; 0x100],
    pub wst: Stack,
    pub rst: Stack,
}

impl Uxn {
    pub const fn new() -> Self {
        Self {
            ram: [0; 0x10000],
            dev: [0; 0x100],
            wst: Stack::new(),
            rst: Stack::new(),
        }
    }

    /// Back to as `new` left it, in place, for another program.
    pub fn reset(&mut self) {
        self.ram.fill(0);
        self.dev.fill(0);
        for s in [&mut self.wst, &mut self.rst] {
            s.dat.fill(0);
            s.ptr = 0;
        }
    }

    /// Puts a ROM at 0x0100, where programs start.
    pub fn load(&mut self, rom: &[u8]) {
        let n = rom.len().min(0x10000 - 0x100);
        self.ram[0x100..0x100 + n].copy_from_slice(&rom[..n]);
    }

    /// Whether the program has asked to stop (System/state).
    pub fn halted(&self) -> bool {
        self.dev[0x0f] != 0
    }

    #[inline(always)]
    fn peek2(&self, a: u16) -> u16 {
        (self.ram[a as usize] as u16) << 8 | self.ram[a.wrapping_add(1) as usize] as u16
    }

    /// Runs from `pc` until BRK, or until the program halts.
    pub fn eval(&mut self, d: &mut impl Devices, mut pc: u16) {
        if pc == 0 || self.halted() {
            return;
        }
        // One arm per instruction byte, each with its modes as constants, so
        // that dispatch is a single jump.
        macro_rules! dispatch {
            ($ins:expr; $($i:literal)*) => {
                match $ins {
                    $($i => self.op::<{ $i & 0x20 != 0 }, { $i & 0x40 != 0 }, { $i & 0x80 != 0 }>(
                        d,
                        $i & 0x1f,
                        pc,
                    ),)*
                }
            };
        }
        loop {
            let ins = self.ram[pc as usize];
            pc = pc.wrapping_add(1);
            let next = dispatch!(ins;
                0x00 0x01 0x02 0x03 0x04 0x05 0x06 0x07 0x08 0x09 0x0a 0x0b 0x0c 0x0d 0x0e 0x0f
                0x10 0x11 0x12 0x13 0x14 0x15 0x16 0x17 0x18 0x19 0x1a 0x1b 0x1c 0x1d 0x1e 0x1f
                0x20 0x21 0x22 0x23 0x24 0x25 0x26 0x27 0x28 0x29 0x2a 0x2b 0x2c 0x2d 0x2e 0x2f
                0x30 0x31 0x32 0x33 0x34 0x35 0x36 0x37 0x38 0x39 0x3a 0x3b 0x3c 0x3d 0x3e 0x3f
                0x40 0x41 0x42 0x43 0x44 0x45 0x46 0x47 0x48 0x49 0x4a 0x4b 0x4c 0x4d 0x4e 0x4f
                0x50 0x51 0x52 0x53 0x54 0x55 0x56 0x57 0x58 0x59 0x5a 0x5b 0x5c 0x5d 0x5e 0x5f
                0x60 0x61 0x62 0x63 0x64 0x65 0x66 0x67 0x68 0x69 0x6a 0x6b 0x6c 0x6d 0x6e 0x6f
                0x70 0x71 0x72 0x73 0x74 0x75 0x76 0x77 0x78 0x79 0x7a 0x7b 0x7c 0x7d 0x7e 0x7f
                0x80 0x81 0x82 0x83 0x84 0x85 0x86 0x87 0x88 0x89 0x8a 0x8b 0x8c 0x8d 0x8e 0x8f
                0x90 0x91 0x92 0x93 0x94 0x95 0x96 0x97 0x98 0x99 0x9a 0x9b 0x9c 0x9d 0x9e 0x9f
                0xa0 0xa1 0xa2 0xa3 0xa4 0xa5 0xa6 0xa7 0xa8 0xa9 0xaa 0xab 0xac 0xad 0xae 0xaf
                0xb0 0xb1 0xb2 0xb3 0xb4 0xb5 0xb6 0xb7 0xb8 0xb9 0xba 0xbb 0xbc 0xbd 0xbe 0xbf
                0xc0 0xc1 0xc2 0xc3 0xc4 0xc5 0xc6 0xc7 0xc8 0xc9 0xca 0xcb 0xcc 0xcd 0xce 0xcf
                0xd0 0xd1 0xd2 0xd3 0xd4 0xd5 0xd6 0xd7 0xd8 0xd9 0xda 0xdb 0xdc 0xdd 0xde 0xdf
                0xe0 0xe1 0xe2 0xe3 0xe4 0xe5 0xe6 0xe7 0xe8 0xe9 0xea 0xeb 0xec 0xed 0xee 0xef
                0xf0 0xf1 0xf2 0xf3 0xf4 0xf5 0xf6 0xf7 0xf8 0xf9 0xfa 0xfb 0xfc 0xfd 0xfe 0xff
            );
            match next {
                Some(n) => pc = n,
                None => return,
            }
        }
    }

    /// Opcode 0, by its modes: BRK, the immediate jumps JCI, JMI and JSI
    /// (relative to after their 16-bit operand), and the literals.
    #[inline(always)]
    fn special<const S: bool, const R: bool, const K: bool>(&mut self, pc: u16) -> Option<u16> {
        match (K, S, R) {
            (false, false, false) => None,
            (false, true, false) => {
                let a = self.peek2(pc);
                self.wst.ptr = self.wst.ptr.wrapping_sub(1);
                let taken = self.wst.dat[self.wst.ptr as usize] != 0;
                Some(pc.wrapping_add(2).wrapping_add(if taken { a } else { 0 }))
            }
            (false, false, true) => Some(pc.wrapping_add(2).wrapping_add(self.peek2(pc))),
            (false, true, true) => {
                let a = self.peek2(pc);
                let pc = pc.wrapping_add(2);
                self.rst.push::<true>(pc);
                Some(pc.wrapping_add(a))
            }
            _ => {
                let st = if R { &mut self.rst } else { &mut self.wst };
                st.push8(self.ram[pc as usize]);
                if S {
                    st.push8(self.ram[pc.wrapping_add(1) as usize]);
                }
                Some(pc.wrapping_add(1 + S as u16))
            }
        }
    }

    /// One instruction: returns the next pc, or None on BRK or when the
    /// program halted.
    #[inline(always)]
    fn op<const S: bool, const R: bool, const K: bool>(
        &mut self,
        d: &mut impl Devices,
        op: u8,
        pc: u16,
    ) -> Option<u16> {
        if op == 0x00 {
            return self.special::<S, R, K>(pc);
        }
        if op == 0x16 || op == 0x17 {
            return self.dev_op::<S, R, K>(d, op, pc);
        }
        let jump = |a: u16| {
            if S {
                a
            } else {
                pc.wrapping_add(a as i8 as u16)
            }
        };
        let Self { ram, wst, rst, .. } = self;
        let (src, dst) = if R { (rst, wst) } else { (wst, rst) };
        let mut p = Pop(src.ptr);
        macro_rules! commit {
            () => {
                if !K {
                    src.ptr = p.0;
                }
            };
        }
        macro_rules! binary {
            (|$a:ident, $b:ident| $e:expr) => {{
                let $b = p.val::<S>(src);
                let $a = p.val::<S>(src);
                commit!();
                src.push::<S>($e);
            }};
        }
        macro_rules! compare {
            (|$a:ident, $b:ident| $e:expr) => {{
                let $b = p.val::<S>(src);
                let $a = p.val::<S>(src);
                commit!();
                src.push8($e as u8);
            }};
        }
        match op {
            // INC POP NIP SWP ROT DUP OVR
            0x01 => {
                let a = p.val::<S>(src);
                commit!();
                src.push::<S>(a.wrapping_add(1));
            }
            0x02 => {
                p.val::<S>(src);
                commit!();
            }
            0x03 => {
                let b = p.val::<S>(src);
                p.val::<S>(src);
                commit!();
                src.push::<S>(b);
            }
            0x04 => {
                let b = p.val::<S>(src);
                let a = p.val::<S>(src);
                commit!();
                src.push::<S>(b);
                src.push::<S>(a);
            }
            0x05 => {
                let c = p.val::<S>(src);
                let b = p.val::<S>(src);
                let a = p.val::<S>(src);
                commit!();
                src.push::<S>(b);
                src.push::<S>(c);
                src.push::<S>(a);
            }
            0x06 => {
                let a = p.val::<S>(src);
                commit!();
                src.push::<S>(a);
                src.push::<S>(a);
            }
            0x07 => {
                let b = p.val::<S>(src);
                let a = p.val::<S>(src);
                commit!();
                src.push::<S>(a);
                src.push::<S>(b);
                src.push::<S>(a);
            }
            // EQU NEQ GTH LTH
            0x08 => compare!(|a, b| a == b),
            0x09 => compare!(|a, b| a != b),
            0x0a => compare!(|a, b| a > b),
            0x0b => compare!(|a, b| a < b),
            // JMP JCN JSR STH
            0x0c => {
                let a = p.val::<S>(src);
                commit!();
                return Some(jump(a));
            }
            0x0d => {
                let a = p.val::<S>(src);
                let c = p.byte(src);
                commit!();
                return Some(if c != 0 { jump(a) } else { pc });
            }
            0x0e => {
                let a = p.val::<S>(src);
                commit!();
                dst.push::<true>(pc);
                return Some(jump(a));
            }
            0x0f => {
                let a = p.val::<S>(src);
                commit!();
                dst.push::<S>(a);
            }
            // LDZ STZ LDR STR LDA STA
            0x10 | 0x12 | 0x14 => {
                let a = match op {
                    0x10 => p.byte(src) as u16,
                    0x12 => pc.wrapping_add(p.byte(src) as i8 as u16),
                    _ => p.short(src),
                };
                commit!();
                src.push8(ram[a as usize]);
                if S {
                    // The zero page wraps round on itself.
                    let b = if op == 0x10 {
                        (a + 1) & 0xff
                    } else {
                        a.wrapping_add(1)
                    };
                    src.push8(ram[b as usize]);
                }
            }
            0x11 | 0x13 | 0x15 => {
                let a = match op {
                    0x11 => p.byte(src) as u16,
                    0x13 => pc.wrapping_add(p.byte(src) as i8 as u16),
                    _ => p.short(src),
                };
                let v = p.val::<S>(src);
                commit!();
                if S {
                    let b = if op == 0x11 {
                        (a + 1) & 0xff
                    } else {
                        a.wrapping_add(1)
                    };
                    ram[a as usize] = (v >> 8) as u8;
                    ram[b as usize] = v as u8;
                } else {
                    ram[a as usize] = v as u8;
                }
            }
            // ADD SUB MUL DIV AND ORA EOR
            0x18 => binary!(|a, b| a.wrapping_add(b)),
            0x19 => binary!(|a, b| a.wrapping_sub(b)),
            0x1a => binary!(|a, b| a.wrapping_mul(b)),
            0x1b => binary!(|a, b| a.checked_div(b).unwrap_or(0)),
            0x1c => binary!(|a, b| a & b),
            0x1d => binary!(|a, b| a | b),
            0x1e => binary!(|a, b| a ^ b),
            // SFT: the shift is a byte in both modes, right by its low
            // nibble then left by its high one.
            _ => {
                let s = p.byte(src);
                let a = p.val::<S>(src);
                commit!();
                src.push::<S>((a >> (s & 0xf)) << (s >> 4));
            }
        }
        Some(pc)
    }

    /// DEI and DEO, which hand the whole machine to the devices.
    fn dev_op<const S: bool, const R: bool, const K: bool>(
        &mut self,
        d: &mut impl Devices,
        op: u8,
        pc: u16,
    ) -> Option<u16> {
        let src = if R { &mut self.rst } else { &mut self.wst };
        let mut p = Pop(src.ptr);
        let port = p.byte(src);
        let v = if op == 0x17 { p.val::<S>(src) } else { 0 };
        if !K {
            src.ptr = p.0;
        }
        if op == 0x16 {
            let v = d.dei(self, port);
            let src = if R { &mut self.rst } else { &mut self.wst };
            src.push8(v);
            if S {
                src.push8(self.dev[port.wrapping_add(1) as usize]);
            }
        } else {
            let port = if S {
                self.dev[port as usize] = (v >> 8) as u8;
                d.deo(self, port);
                if self.halted() {
                    return None;
                }
                port.wrapping_add(1)
            } else {
                port
            };
            self.dev[port as usize] = v as u8;
            d.deo(self, port);
            if self.halted() {
                return None;
            }
        }
        Some(pc)
    }
}
