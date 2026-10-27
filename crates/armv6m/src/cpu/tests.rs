use alloc::vec::Vec;

use super::*;
use crate::bus::{Bus, Ram};

struct TestBus(Ram);

impl Bus for TestBus {
    fn read8(&mut self, a: u32) -> u8 {
        self.0.read8(a)
    }
    fn read16(&mut self, a: u32) -> u16 {
        self.0.read16(a)
    }
    fn read32(&mut self, a: u32) -> u32 {
        self.0.read32(a)
    }
    fn write8(&mut self, a: u32, v: u8) {
        self.0.write8(a, v)
    }
    fn write16(&mut self, a: u32, v: u16) {
        self.0.write16(a, v)
    }
    fn write32(&mut self, a: u32, v: u32) {
        self.0.write32(a, v)
    }
}

const CODE: u32 = 0x100;
const STACK: u32 = 0x8000;

// ---- encoders --------------------------------------------------------------
fn movs(rd: u16, imm: u16) -> u16 {
    0x2000 | rd << 8 | imm
}
fn cmp_i(rd: u16, imm: u16) -> u16 {
    0x2800 | rd << 8 | imm
}
fn adds_i8(rd: u16, imm: u16) -> u16 {
    0x3000 | rd << 8 | imm
}
fn subs_i8(rd: u16, imm: u16) -> u16 {
    0x3800 | rd << 8 | imm
}
fn adds(rd: u16, rn: u16, rm: u16) -> u16 {
    0x1800 | rm << 6 | rn << 3 | rd
}
fn subs(rd: u16, rn: u16, rm: u16) -> u16 {
    0x1A00 | rm << 6 | rn << 3 | rd
}
fn lsls_i(rd: u16, rm: u16, imm: u16) -> u16 {
    imm << 6 | rm << 3 | rd
}
fn lsrs_i(rd: u16, rm: u16, imm: u16) -> u16 {
    0x0800 | imm << 6 | rm << 3 | rd
}
fn asrs_i(rd: u16, rm: u16, imm: u16) -> u16 {
    0x1000 | imm << 6 | rm << 3 | rd
}
/// data-processing group: op in 0..16
fn alu(op: u16, rdn: u16, rm: u16) -> u16 {
    0x4000 | op << 6 | rm << 3 | rdn
}
fn mov_hi(rd: u16, rm: u16) -> u16 {
    0x4600 | (rd & 8) << 4 | rm << 3 | (rd & 7)
}
fn bx(rm: u16) -> u16 {
    0x4700 | rm << 3
}
fn blx(rm: u16) -> u16 {
    0x4780 | rm << 3
}
fn ldr_lit(rt: u16, imm4: u16) -> u16 {
    0x4800 | rt << 8 | imm4
}
fn str_i(rt: u16, rn: u16, imm4: u16) -> u16 {
    0x6000 | imm4 << 6 | rn << 3 | rt
}
fn ldr_i(rt: u16, rn: u16, imm4: u16) -> u16 {
    0x6800 | imm4 << 6 | rn << 3 | rt
}
fn strb_i(rt: u16, rn: u16, imm: u16) -> u16 {
    0x7000 | imm << 6 | rn << 3 | rt
}
fn ldrb_i(rt: u16, rn: u16, imm: u16) -> u16 {
    0x7800 | imm << 6 | rn << 3 | rt
}
fn ldrh_i(rt: u16, rn: u16, imm2: u16) -> u16 {
    0x8800 | imm2 << 6 | rn << 3 | rt
}
fn ldrsb_r(rt: u16, rn: u16, rm: u16) -> u16 {
    0x5600 | rm << 6 | rn << 3 | rt
}
fn ldrsh_r(rt: u16, rn: u16, rm: u16) -> u16 {
    0x5E00 | rm << 6 | rn << 3 | rt
}
fn str_sp(rt: u16, imm4: u16) -> u16 {
    0x9000 | rt << 8 | imm4
}
fn ldr_sp(rt: u16, imm4: u16) -> u16 {
    0x9800 | rt << 8 | imm4
}
fn add_sp_i(imm4: u16) -> u16 {
    0xB000 | imm4
}
fn sub_sp_i(imm4: u16) -> u16 {
    0xB080 | imm4
}
fn push(list: u16, lr: bool) -> u16 {
    0xB400 | (lr as u16) << 8 | list
}
fn pop(list: u16, pc: bool) -> u16 {
    0xBC00 | (pc as u16) << 8 | list
}
fn stm(rn: u16, list: u16) -> u16 {
    0xC000 | rn << 8 | list
}
fn ldm(rn: u16, list: u16) -> u16 {
    0xC800 | rn << 8 | list
}
fn bcond(cond: u16, off: i8) -> u16 {
    0xD000 | cond << 8 | (off as u8 as u16)
}
fn b(off: i16) -> u16 {
    0xE000 | (off as u16 & 0x7FF)
}
fn svc(imm: u16) -> u16 {
    0xDF00 | imm
}
const NOP: u16 = 0xBF00;
const BEQ: u16 = 0;
const BNE: u16 = 1;
const BGE: u16 = 10;
const BLT: u16 = 11;

fn new_bus() -> TestBus {
    TestBus(Ram::new(0x10000))
}

fn load(bus: &mut TestBus, at: u32, code: &[u16]) {
    for (i, hw) in code.iter().enumerate() {
        bus.0.write16(at + 2 * i as u32, *hw);
    }
}

/// Run until pc leaves the program (falls off the end) or 10k steps.
fn run(code: &[u16]) -> (Cpu, TestBus) {
    let mut bus = new_bus();
    load(&mut bus, CODE, code);
    let end = CODE + 2 * code.len() as u32;
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, CODE | 1);
    let mut n = 0;
    while cpu.pc < end && n < 10_000 {
        cpu.step(&mut bus);
        n += 1;
    }
    assert!(n < 10_000, "program did not terminate");
    (cpu, bus)
}

fn run_cpu(code: &[u16]) -> Cpu {
    run(code).0
}

// ---- tests -----------------------------------------------------------------

#[test]
fn mov_add_sub_flags() {
    let c = run_cpu(&[movs(0, 5), movs(1, 7), adds(2, 0, 1), subs(3, 0, 1)]);
    assert_eq!(c.r[2], 12);
    assert_eq!(c.r[3], (-2i32) as u32);
    assert!(c.n && !c.z && !c.c); // 5-7: borrow => C clear
}

#[test]
fn add_overflow_and_carry() {
    // 0x7FFFFFFF + 1 => V set, N set
    let c = run_cpu(&[movs(0, 1), lsls_i(0, 0, 31), subs_i8(0, 1), adds_i8(0, 1)]);
    assert_eq!(c.r[0], 0x8000_0000);
    assert!(c.v && c.n && !c.c);
    // 0xFFFFFFFF + 1 => C set, Z set
    let c = run_cpu(&[movs(0, 0), subs_i8(0, 1), adds_i8(0, 1)]);
    assert_eq!(c.r[0], 0);
    assert!(c.c && c.z && !c.v);
}

#[test]
fn shifts_immediate() {
    let c = run_cpu(&[
        movs(0, 0x81),
        lsls_i(1, 0, 4),
        lsrs_i(2, 0, 1),
        asrs_i(3, 1, 2),
    ]);
    assert_eq!(c.r[1], 0x810);
    assert_eq!(c.r[2], 0x40);
    assert_eq!(c.r[3], 0x204);
    // LSRS #32 via imm 0: result 0, carry = bit31
    let c = run_cpu(&[movs(0, 1), lsls_i(0, 0, 31), lsrs_i(1, 0, 0)]);
    assert_eq!(c.r[1], 0);
    assert!(c.c && c.z);
    // ASRS #32 of a negative number
    let c = run_cpu(&[movs(0, 1), lsls_i(0, 0, 31), asrs_i(1, 0, 0)]);
    assert_eq!(c.r[1], 0xFFFF_FFFF);
    assert!(c.c && c.n);
    // LSLS carry-out
    let c = run_cpu(&[
        movs(0, 3),
        lsls_i(0, 0, 0),
        movs(1, 1),
        lsls_i(1, 1, 31),
        lsls_i(2, 1, 1),
    ]);
    assert_eq!(c.r[2], 0);
    assert!(c.c && c.z);
}

#[test]
fn shifts_register_over_32() {
    let c = run_cpu(&[movs(0, 0xFF), movs(1, 33), alu(2, 0, 1)]);
    assert_eq!(c.r[0], 0);
    assert!(!c.c && c.z);
}

#[test]
fn lsl_reg_32_carry() {
    let c = run_cpu(&[movs(0, 0xFF), movs(1, 32), alu(2, 0, 1)]);
    assert!(c.c);
    assert_eq!(c.r[0], 0);
}

#[test]
fn ror_reg() {
    let c = run_cpu(&[movs(0, 1), movs(1, 1), alu(7, 0, 1)]);
    assert_eq!(c.r[0], 0x8000_0000);
    assert!(c.c && c.n);
}

#[test]
fn alu_ops() {
    let c = run_cpu(&[
        movs(0, 0xF0),
        movs(1, 0x3C),
        mov_hi(2, 0),
        alu(0x0, 2, 1), // r2 = 0xF0 & 0x3C = 0x30
        mov_hi(3, 0),
        alu(0x1, 3, 1), // eor => 0xCC
        mov_hi(4, 0),
        alu(0xC, 4, 1), // orr => 0xFC
        mov_hi(5, 0),
        alu(0xE, 5, 1), // bic => 0xC0
        alu(0xF, 6, 1), // mvn r6 = !0x3C
        alu(0x9, 7, 1), // rsb r7 = -0x3C
    ]);
    assert_eq!(c.r[2], 0x30);
    assert_eq!(c.r[3], 0xCC);
    assert_eq!(c.r[4], 0xFC);
    assert_eq!(c.r[5], 0xC0);
    assert_eq!(c.r[6], !0x3Cu32);
    assert_eq!(c.r[7], 0u32.wrapping_sub(0x3C));
}

#[test]
fn mul_and_tst_cmn() {
    let c = run_cpu(&[movs(0, 200), movs(1, 100), alu(0xD, 0, 1)]);
    assert_eq!(c.r[0], 20_000);
    let c = run_cpu(&[movs(0, 0x0F), movs(1, 0xF0), alu(0x8, 0, 1)]);
    assert!(c.z);
    let c = run_cpu(&[movs(0, 1), movs(1, 0), subs_i8(1, 1), alu(0xB, 0, 1)]); // 1 + (-1)
    assert!(c.z && c.c);
}

#[test]
fn adc_sbc_chain() {
    // 64-bit add: (0xFFFFFFFF,0) + (1,0) => (0,1)
    let c = run_cpu(&[
        movs(0, 0),
        subs_i8(0, 1),
        movs(1, 0), // lo=0xFFFFFFFF hi=0
        movs(2, 1),
        movs(3, 0),
        adds(0, 0, 2),
        alu(5, 1, 3),
    ]);
    assert_eq!((c.r[0], c.r[1]), (0, 1));
}

#[test]
fn hi_register_ops() {
    // ADD r8, r0 ; MOV r9, r8 ; CMP r9, r0
    let c = run_cpu(&[movs(0, 9), mov_hi(8, 0), 0x4480, mov_hi(9, 8), 0x4581]);
    // 0x4480 = ADD r8, r0 (rdn=8, rm=0)  => r8 = 18
    assert_eq!(c.r[8], 18);
    assert_eq!(c.r[9], 18);
    assert!(!c.z); // 18 vs 9
}

#[test]
fn load_store_word_byte_half() {
    let (c, mut bus) = run(&[
        movs(0, 0x80),
        lsls_i(0, 0, 4), // r0 = 0x800 (data area)
        movs(1, 0xAB),
        lsls_i(1, 1, 8),
        adds_i8(1, 0xCD), // r1 = 0xABCD
        str_i(1, 0, 0),
        ldrb_i(2, 0, 0),
        ldrb_i(3, 0, 1),
        ldrh_i(4, 0, 0),
        movs(5, 0x7F),
        strb_i(5, 0, 3),
        ldr_i(6, 0, 0),
    ]);
    assert_eq!(c.r[2], 0xCD);
    assert_eq!(c.r[3], 0xAB);
    assert_eq!(c.r[4], 0xABCD);
    assert_eq!(c.r[6], 0x7F00_ABCD);
    assert_eq!(bus.read32(0x800), 0x7F00_ABCD);
}

#[test]
fn sign_extending_loads() {
    let mut bus = new_bus();
    bus.write32(0x800, 0x0000_80FF);
    let code = [
        movs(0, 0x80),
        lsls_i(0, 0, 4),
        movs(1, 0),
        ldrsb_r(2, 0, 1),
        ldrsh_r(3, 0, 1),
    ];
    load(&mut bus, CODE, &code);
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, CODE | 1);
    for _ in 0..code.len() {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.r[2], 0xFFFF_FFFF);
    assert_eq!(cpu.r[3], 0xFFFF_80FF);
}

#[test]
fn ldr_literal_aligned_pc() {
    let mut bus = new_bus();
    // at 0x100: ldr r0,[pc,#4] -> addr = align(0x104)+4 = 0x108
    load(&mut bus, CODE, &[ldr_lit(0, 1), NOP, NOP, NOP]);
    bus.write32(0x108, 0xDEAD_BEEF);
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, CODE | 1);
    cpu.step(&mut bus);
    assert_eq!(cpu.r[0], 0xDEAD_BEEF);
}

#[test]
fn push_pop_and_sp_relative() {
    let c = run_cpu(&[
        movs(0, 1),
        movs(1, 2),
        movs(2, 3),
        push(0b111, false),
        movs(0, 0),
        movs(1, 0),
        movs(2, 0),
        pop(0b111, false),
    ]);
    assert_eq!((c.r[0], c.r[1], c.r[2]), (1, 2, 3));
    assert_eq!(c.sp(), STACK);

    let c = run_cpu(&[
        movs(0, 42),
        sub_sp_i(2),
        str_sp(0, 1),
        movs(0, 0),
        ldr_sp(1, 1),
        add_sp_i(2),
    ]);
    assert_eq!(c.r[1], 42);
    assert_eq!(c.sp(), STACK);
}

#[test]
fn push_lr_pop_pc_returns() {
    // main: BL func ; movs r1,2 ; (end)   func: push{lr}; movs r0,7; pop{pc}
    let (c, _) = run(&[
        0xF000,
        0xF802,     // BL +4 bytes => target = pc+4+4 = func
        movs(1, 2), // executed after return
        b(2),       // skip over func (jump to end)
        push(0, true),
        movs(0, 7),
        pop(0, true),
    ]);
    assert_eq!(c.r[0], 7);
    assert_eq!(c.r[1], 2);
}

#[test]
fn stm_ldm() {
    let c = run_cpu(&[
        movs(0, 0x80),
        lsls_i(0, 0, 4),
        movs(1, 11),
        movs(2, 22),
        movs(3, 33),
        stm(0, 0b1110),
        mov_hi(4, 0), // keep r0 after writeback
        movs(1, 0),
        movs(2, 0),
        movs(3, 0),
        movs(5, 0x80),
        lsls_i(5, 5, 4),
        ldm(5, 0b1110),
    ]);
    assert_eq!(c.r[4], 0x80 << 4 | 0xC); // writeback after 3 words
    assert_eq!((c.r[1], c.r[2], c.r[3]), (11, 22, 33));
    assert_eq!(c.r[5], 0x80 << 4 | 0xC);
}

#[test]
fn conditional_branch_loop_sums_1_to_10() {
    // r0 = 0 ; r1 = 10 ; loop: r0 += r1 ; r1 -= 1 ; bne loop
    let c = run_cpu(&[
        movs(0, 0),
        movs(1, 10),
        adds(0, 0, 1),
        subs_i8(1, 1),
        bcond(BNE, -4),
    ]);
    assert_eq!(c.r[0], 55);
}

#[test]
fn signed_conditions() {
    // -1 < 1 (BLT taken), then BGE not taken
    let c = run_cpu(&[
        movs(0, 0),
        subs_i8(0, 1),
        cmp_i(0, 1),
        bcond(BLT, 0),
        movs(2, 99), // skipped
        movs(3, 5),
    ]);
    assert_eq!(c.r[2], 0);
    assert_eq!(c.r[3], 5);
    let c = run_cpu(&[
        movs(0, 5),
        cmp_i(0, 5),
        bcond(BEQ, 0),
        movs(2, 99),
        bcond(BGE, 0),
        movs(4, 1),
    ]);
    assert_eq!(c.r[2], 0);
    assert_eq!(c.r[4], 0);
}

#[test]
fn unconditional_backward_branch() {
    // 0: movs r0,0 ; 1: adds r0,#1 ; 2: cmp r0,#3 ; 3: bne -> 1 ; (falls)
    let c = run_cpu(&[movs(0, 0), adds_i8(0, 1), cmp_i(0, 3), bcond(BNE, -4)]);
    assert_eq!(c.r[0], 3);
}

#[test]
fn bl_forward_and_backward() {
    // BL to +0x100 bytes then check lr
    let mut bus = new_bus();
    // BL imm: offset = 0x100 => imm11 = 0x80, imm10 = 0 ; hw2 = 0xF800 | imm11
    load(&mut bus, CODE, &[0xF000, 0xF800 | 0x80]);
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, CODE | 1);
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, CODE + 4 + 0x100);
    assert_eq!(cpu.lr, (CODE + 4) | 1);
    // backward: offset -0x20 => S=1, imm10=0x3FF, imm11=0x7F0 >> ... compute
    let off: i32 = -0x20;
    let u = off as u32;
    let s = (u >> 24) & 1;
    let i1 = (u >> 23) & 1;
    let i2 = (u >> 22) & 1;
    let j1 = (!(i1 ^ s)) & 1;
    let j2 = (!(i2 ^ s)) & 1;
    let hw1 = 0xF000 | (s << 10) | ((u >> 12) & 0x3FF);
    let hw2 = 0xD000 | (j1 << 13) | (j2 << 11) | ((u >> 1) & 0x7FF);
    load(&mut bus, 0x400, &[hw1 as u16, hw2 as u16]);
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, 0x400 | 1);
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, (0x404i32 - 0x20) as u32);
}

#[test]
fn bx_and_blx() {
    let mut bus = new_bus();
    // r0 = 0x201 ; blx r0 ; at 0x200: bx lr
    load(
        &mut bus,
        CODE,
        &[
            movs(0, 0x80),
            lsls_i(0, 0, 2),
            adds_i8(0, 1),
            blx(0),
            movs(1, 5),
        ],
    );
    load(&mut bus, 0x200, &[bx(14)]);
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, CODE | 1);
    for _ in 0..6 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.r[1], 5);
}

#[test]
fn bx_to_arm_state_faults() {
    let mut bus = new_bus();
    // vector table at 0: HardFault handler (exc 3) at 0x300
    bus.write32(12, 0x301);
    load(&mut bus, CODE, &[movs(0, 0x40), bx(0)]);
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, CODE | 1);
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(cpu.ipsr, EXC_HARDFAULT);
    assert_eq!(cpu.pc, 0x300);
}

#[test]
fn unaligned_access_faults() {
    let mut bus = new_bus();
    bus.write32(12, 0x301);
    load(
        &mut bus,
        CODE,
        &[
            movs(0, 0x81),
            lsls_i(0, 0, 4),
            adds_i8(0, 2),
            ldr_i(1, 0, 0),
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, CODE | 1);
    for _ in 0..4 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.ipsr, EXC_HARDFAULT);
}

#[test]
fn extend_and_reverse() {
    let mk = |op: u16, rd: u16, rm: u16| -> u16 { op | rm << 3 | rd };
    let c = run_cpu(&[
        movs(0, 0x80),
        lsls_i(0, 0, 8),
        adds_i8(0, 0x81),
        mk(0xB200, 1, 0), // sxth
        mk(0xB240, 2, 0), // sxtb
        mk(0xB280, 3, 0), // uxth
        mk(0xB2C0, 4, 0), // uxtb
    ]);
    assert_eq!(c.r[1], 0xFFFF_8081);
    assert_eq!(c.r[2], 0xFFFF_FF81);
    assert_eq!(c.r[3], 0x8081);
    assert_eq!(c.r[4], 0x81);
    let c = run_cpu(&[
        movs(0, 0x12),
        lsls_i(0, 0, 8),
        adds_i8(0, 0x34),
        lsls_i(0, 0, 8),
        adds_i8(0, 0x56),
        lsls_i(0, 0, 8),
        adds_i8(0, 0x78),
        mk(0xBA00, 1, 0),
        mk(0xBA40, 2, 0),
        mk(0xBAC0, 3, 0),
    ]);
    assert_eq!(c.r[0], 0x1234_5678);
    assert_eq!(c.r[1], 0x7856_3412);
    assert_eq!(c.r[2], 0x3412_7856);
    assert_eq!(c.r[3], 0x0000_7856);
}

#[test]
fn msr_mrs_primask_and_cps() {
    let c = run_cpu(&[0xB672]); // cpsid i
    assert!(c.primask);
    let c = run_cpu(&[0xB672, 0xB662]);
    assert!(!c.primask);
    // mrs r0, primask after cpsid ; msr psp, r1
    let c = run_cpu(&[0xB672, 0xF3EF, 0x8010, movs(1, 0x80), 0xF381, 0x8809]);
    assert_eq!(c.r[0], 1);
    assert_eq!(c.psp, 0x80);
}

#[test]
fn barriers_are_nops() {
    let c = run_cpu(&[0xF3BF, 0x8F4F, 0xF3BF, 0x8F5F, 0xF3BF, 0x8F6F, movs(0, 1)]);
    assert_eq!(c.r[0], 1);
}

#[test]
fn wfi_sleeps_and_sev_wfe() {
    let mut bus = new_bus();
    load(&mut bus, CODE, &[0xBF40, 0xBF20, 0xBF20, movs(0, 1)]); // sev ; wfe ; wfe ; movs
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, CODE | 1);
    cpu.step(&mut bus); // sev
    cpu.step(&mut bus); // wfe consumes event, no sleep
    assert!(!cpu.sleeping);
    cpu.step(&mut bus); // wfe sleeps
    assert!(cpu.sleeping && cpu.wfe, "an event may end this sleep");
    let pc = cpu.pc;
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, pc);
    cpu.sleeping = false;
    cpu.step(&mut bus);
    assert_eq!(cpu.r[0], 1);
}

#[test]
fn bkpt_records_immediate() {
    let c = run_cpu(&[0xBE00 | 0x2A]);
    assert_eq!(c.bkpt, Some(0x2A));
}

#[test]
fn svc_enters_handler_and_returns() {
    let mut bus = new_bus();
    bus.write32(4 * EXC_SVCALL as u32, 0x300 | 1);
    // main: movs r0,1 ; svc 0 ; movs r2,3
    load(&mut bus, CODE, &[movs(0, 1), svc(0), movs(2, 3)]);
    // handler: movs r1,2 ; bx lr   (lr = EXC_RETURN)
    load(&mut bus, 0x300, &[movs(4, 2), bx(14)]);
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, CODE | 1);
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(cpu.ipsr, EXC_SVCALL);
    assert_eq!(cpu.pc, 0x300);
    assert_eq!(cpu.sp(), STACK - 32);
    cpu.step(&mut bus); // movs r4
    cpu.step(&mut bus); // bx lr => exception return
    assert_eq!(cpu.ipsr, 0);
    assert_eq!(cpu.pc, CODE + 4);
    assert_eq!(cpu.sp(), STACK);
    assert_eq!(cpu.r[0], 1); // restored from the frame
    cpu.step(&mut bus);
    assert_eq!((cpu.r[4], cpu.r[2]), (2, 3));
}

#[test]
fn irq_preserves_flags_and_uses_psp_frame() {
    let mut bus = new_bus();
    bus.write32(4 * (EXC_IRQ_BASE as u32 + 5), 0x300 | 1);
    load(&mut bus, CODE, &[movs(0, 0), cmp_i(0, 0), NOP, NOP]);
    // handler clobbers flags then pop{pc} via EXC_RETURN in stacked lr
    load(&mut bus, 0x300, &[movs(1, 1), subs_i8(1, 5), bx(14)]);
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, CODE | 1);
    cpu.psp = 0x6000;
    cpu.spsel = true; // thread uses PSP
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert!(cpu.z);
    cpu.take_exception(&mut bus, EXC_IRQ_BASE + 5);
    assert_eq!(cpu.lr, 0xFFFF_FFFD);
    assert_eq!(cpu.psp, 0x6000 - 32);
    assert_eq!(cpu.sp(), STACK); // handler runs on MSP
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert!(!cpu.z && cpu.n);
    cpu.step(&mut bus); // return
    assert_eq!(cpu.ipsr, 0);
    assert!(cpu.spsel);
    assert_eq!(cpu.psp, 0x6000);
    assert!(cpu.z && !cpu.n);
    assert_eq!(cpu.pc, CODE + 4);
}

#[test]
fn stack_alignment_adjust_on_exception() {
    let mut bus = new_bus();
    bus.write32(4 * EXC_SVCALL as u32, 0x300 | 1);
    load(&mut bus, 0x300, &[bx(14)]);
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK - 4, CODE | 1); // not 8-byte aligned
    cpu.take_exception(&mut bus, EXC_SVCALL);
    assert_eq!(cpu.sp() & 7, 0);
    cpu.step(&mut bus);
    assert_eq!(cpu.sp(), STACK - 4);
}

#[test]
fn nested_exception_return_to_handler() {
    let mut bus = new_bus();
    bus.write32(4 * EXC_SVCALL as u32, 0x300 | 1);
    bus.write32(4 * EXC_PENDSV as u32, 0x400 | 1);
    load(&mut bus, 0x300, &[NOP, NOP, bx(14)]);
    load(&mut bus, 0x400, &[bx(14)]);
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, CODE | 1);
    cpu.take_exception(&mut bus, EXC_SVCALL);
    cpu.step(&mut bus);
    cpu.take_exception(&mut bus, EXC_PENDSV);
    assert_eq!(cpu.lr, 0xFFFF_FFF1);
    cpu.step(&mut bus);
    assert_eq!(cpu.ipsr, EXC_SVCALL);
    assert_eq!(cpu.pc, 0x302);
}

#[test]
fn undefined_instruction_hardfaults() {
    let mut bus = new_bus();
    bus.write32(12, 0x301);
    load(&mut bus, CODE, &[0xDE00]); // UDF
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, CODE | 1);
    cpu.step(&mut bus);
    assert_eq!(cpu.ipsr, EXC_HARDFAULT);
    // stacked pc points at the faulting instruction
    assert_eq!(bus.read32(cpu.psp.max(cpu.msp) + 24), CODE);
}

#[test]
fn reset_from_vector_table() {
    let mut bus = new_bus();
    bus.write32(0x1000, 0x7000);
    bus.write32(0x1004, 0x2001);
    let mut cpu = Cpu::new();
    cpu.reset_from_vector_table(&mut bus, 0x1000);
    assert_eq!((cpu.sp(), cpu.pc, cpu.vtor), (0x7000, 0x2000, 0x1000));
}

/// A tiny "real" program: iterative fibonacci through memory and a loop.
#[test]
fn fibonacci_program() {
    // r0=0, r1=1, r2=counter(20); loop: r3=r0+r1; r0=r1; r1=r3; r2--; bne
    let prog: Vec<u16> = alloc::vec![
        movs(0, 0),
        movs(1, 1),
        movs(2, 20),
        adds(3, 0, 1),
        mov_hi(0, 1),
        mov_hi(1, 3),
        subs_i8(2, 1),
        bcond(BNE, -6),
    ];
    let c = run_cpu(&prog);
    assert_eq!(c.r[0], 6765); // fib(20)
}

#[test]
fn sev_is_reported_for_other_cores_and_wfi_is_not_an_event_sleep() {
    let mut bus = new_bus();
    load(&mut bus, CODE, &[0xBF40, 0xBF30]); // sev ; wfi
    let mut cpu = Cpu::new();
    cpu.reset_to(STACK, CODE | 1);
    cpu.step(&mut bus);
    assert!(
        cpu.event && cpu.sev_sent,
        "SEV sets this core's event register and asks the system to forward it"
    );
    cpu.step(&mut bus);
    assert!(cpu.sleeping && !cpu.wfe, "WFI only wakes for interrupts");
}
