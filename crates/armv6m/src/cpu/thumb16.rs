//! 16-bit Thumb instruction decode and execution.

use super::*;

type R = Result<u32, Fault>;
type Handler<B> = fn(&mut Cpu, &mut B, u32, u32) -> R;

/// Handler per opcode bits 15..6 (1024 entries). Most classes depend on the
/// top five bits only; the data-processing ALU ops and the register-offset
/// loads and stores get one handler per operation, so each runs without a
/// second dispatch.
pub(super) struct Table<B: ?Sized>(core::marker::PhantomData<B>);

impl<B: Bus + ?Sized> Table<B> {
    const CLASS: [Handler<B>; 32] = [
        Cpu::op_shift_imm,
        Cpu::op_shift_imm,
        Cpu::op_shift_imm,
        Cpu::op_add_sub,
        Cpu::op_mov_imm,
        Cpu::op_cmp_imm,
        Cpu::op_add_imm,
        Cpu::op_sub_imm,
        Cpu::op_special,
        Cpu::op_ldr_literal,
        Cpu::op_undefined,
        Cpu::op_undefined,
        Cpu::op_ldst_imm,
        Cpu::op_ldst_imm,
        Cpu::op_ldst_imm,
        Cpu::op_ldst_imm,
        Cpu::op_ldst_imm,
        Cpu::op_ldst_imm,
        Cpu::op_str_sp,
        Cpu::op_ldr_sp,
        Cpu::op_adr,
        Cpu::op_add_sp_imm,
        Cpu::op_misc,
        Cpu::op_misc,
        Cpu::op_stm,
        Cpu::op_ldm,
        Cpu::op_cond_branch,
        Cpu::op_cond_branch,
        Cpu::op_branch,
        Cpu::op_undefined,
        Cpu::op_wide,
        Cpu::op_undefined,
    ];
    const ALU: [Handler<B>; 16] = [
        Cpu::op_alu::<B, 0>,
        Cpu::op_alu::<B, 1>,
        Cpu::op_alu::<B, 2>,
        Cpu::op_alu::<B, 3>,
        Cpu::op_alu::<B, 4>,
        Cpu::op_alu::<B, 5>,
        Cpu::op_alu::<B, 6>,
        Cpu::op_alu::<B, 7>,
        Cpu::op_alu::<B, 8>,
        Cpu::op_alu::<B, 9>,
        Cpu::op_alu::<B, 10>,
        Cpu::op_alu::<B, 11>,
        Cpu::op_alu::<B, 12>,
        Cpu::op_alu::<B, 13>,
        Cpu::op_alu::<B, 14>,
        Cpu::op_alu::<B, 15>,
    ];
    const LDST: [Handler<B>; 8] = [
        Cpu::op_ldst_reg::<B, 0>,
        Cpu::op_ldst_reg::<B, 1>,
        Cpu::op_ldst_reg::<B, 2>,
        Cpu::op_ldst_reg::<B, 3>,
        Cpu::op_ldst_reg::<B, 4>,
        Cpu::op_ldst_reg::<B, 5>,
        Cpu::op_ldst_reg::<B, 6>,
        Cpu::op_ldst_reg::<B, 7>,
    ];
    pub(super) const H: [Handler<B>; 1024] = {
        let mut t = [Cpu::op_undefined as Handler<B>; 1024];
        let mut i = 0;
        while i < 1024 {
            t[i] = match i >> 5 {
                8 if i & 16 == 0 => Self::ALU[i & 15],
                10 | 11 => Self::LDST[(i >> 3) & 7],
                top => Self::CLASS[top],
            };
            i += 1;
        }
        t
    };
}

impl Cpu {
    /// Fetch, decode and execute one instruction. The top five bits pick a
    /// handler from a table, so each instruction class runs in its own small
    /// function instead of one large one.
    #[inline]
    pub(super) fn exec16<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Result<u32, Fault> {
        let pc = self.pc;
        let hw = bus.fetch16(pc) as u32;
        Table::<B>::H[(hw >> 6) as usize](self, bus, hw, pc)
    }

    #[inline(always)]
    fn op_shift_imm<B: Bus + ?Sized>(&mut self, _: &mut B, hw: u32, pc: u32) -> R {
        let rd = (hw & 7) as usize;
        let rn = ((hw >> 3) & 7) as usize;
        let imm = (hw >> 6) & 0x1F;
        let val = self.r[rn];
        let (res, c) = match hw >> 11 {
            0 => lsl(val, imm, self.c),
            1 => lsr(val, if imm == 0 { 32 } else { imm }, self.c),
            _ => asr(val, if imm == 0 { 32 } else { imm }, self.c),
        };
        self.r[rd] = res;
        self.set_nzc(res, c);
        self.pc = pc.wrapping_add(2);
        Ok(1)
    }

    #[inline(always)]
    fn op_add_sub<B: Bus + ?Sized>(&mut self, _: &mut B, hw: u32, pc: u32) -> R {
        let rd = (hw & 7) as usize;
        let rn = ((hw >> 3) & 7) as usize;
        let rm6 = ((hw >> 6) & 7) as usize;
        let b = if hw & (1 << 10) != 0 {
            rm6 as u32
        } else {
            self.r[rm6]
        };
        let (res, c, v) = if hw & (1 << 9) != 0 {
            add_with_carry(self.r[rn], !b, true)
        } else {
            add_with_carry(self.r[rn], b, false)
        };
        self.r[rd] = res;
        self.set_nzcv(res, c, v);
        self.pc = pc.wrapping_add(2);
        Ok(1)
    }

    #[inline(always)]
    fn op_mov_imm<B: Bus + ?Sized>(&mut self, _: &mut B, hw: u32, pc: u32) -> R {
        let imm8 = hw & 0xFF;
        self.r[((hw >> 8) & 7) as usize] = imm8;
        self.set_nz(imm8);
        self.pc = pc.wrapping_add(2);
        Ok(1)
    }

    #[inline(always)]
    fn op_cmp_imm<B: Bus + ?Sized>(&mut self, _: &mut B, hw: u32, pc: u32) -> R {
        let (res, c, v) = add_with_carry(self.r[((hw >> 8) & 7) as usize], !(hw & 0xFF), true);
        self.set_nzcv(res, c, v);
        self.pc = pc.wrapping_add(2);
        Ok(1)
    }

    #[inline(always)]
    fn op_add_imm<B: Bus + ?Sized>(&mut self, _: &mut B, hw: u32, pc: u32) -> R {
        let rd8 = ((hw >> 8) & 7) as usize;
        let (res, c, v) = add_with_carry(self.r[rd8], hw & 0xFF, false);
        self.r[rd8] = res;
        self.set_nzcv(res, c, v);
        self.pc = pc.wrapping_add(2);
        Ok(1)
    }

    #[inline(always)]
    fn op_sub_imm<B: Bus + ?Sized>(&mut self, _: &mut B, hw: u32, pc: u32) -> R {
        let rd8 = ((hw >> 8) & 7) as usize;
        let (res, c, v) = add_with_carry(self.r[rd8], !(hw & 0xFF), true);
        self.r[rd8] = res;
        self.set_nzcv(res, c, v);
        self.pc = pc.wrapping_add(2);
        Ok(1)
    }

    #[inline(always)]
    fn op_special<B: Bus + ?Sized>(&mut self, bus: &mut B, hw: u32, pc: u32) -> R {
        // special data / branch exchange
        let mut next = pc.wrapping_add(2);
        let mut cycles = 1;
        let rm = ((hw >> 3) & 0xF) as usize;
        let rdn = ((hw >> 4) & 8 | hw & 7) as usize;
        match (hw >> 8) & 3 {
            0 => {
                let res = self.reg(rdn).wrapping_add(self.reg(rm));
                if rdn == 15 {
                    next = res & !1;
                    cycles = 2;
                } else {
                    self.write_reg(rdn, res);
                }
            }
            1 => {
                let (res, c, v) = add_with_carry(self.reg(rdn), !self.reg(rm), true);
                self.set_nzcv(res, c, v);
            }
            2 => {
                let val = self.reg(rm);
                if rdn == 15 {
                    next = val & !1;
                    cycles = 2;
                } else {
                    self.write_reg(rdn, val);
                }
            }
            _ => {
                let target = self.reg(rm);
                if hw & 0x80 != 0 {
                    self.lr = next | 1;
                }
                next = self.bx_write(bus, target)?;
                cycles = 2;
            }
        }
        self.pc = next;
        Ok(cycles)
    }

    #[inline(always)]
    fn op_ldr_literal<B: Bus + ?Sized>(&mut self, bus: &mut B, hw: u32, pc: u32) -> R {
        let pc_aligned = pc.wrapping_add(4) & !3;
        self.r[((hw >> 8) & 7) as usize] = self.ld32(bus, pc_aligned + (hw & 0xFF) * 4)?;
        self.pc = pc.wrapping_add(2);
        Ok(2)
    }

    #[inline(always)]
    fn op_ldst_reg<B: Bus + ?Sized, const OP: u32>(&mut self, bus: &mut B, hw: u32, pc: u32) -> R {
        let rd = (hw & 7) as usize;
        let rn = ((hw >> 3) & 7) as usize;
        let rm6 = ((hw >> 6) & 7) as usize;
        let addr = self.r[rn].wrapping_add(self.r[rm6]);
        match OP {
            0 => self.st32(bus, addr, self.r[rd])?,
            1 => self.st16(bus, addr, self.r[rd] as u16)?,
            2 => bus.write8(addr, self.r[rd] as u8),
            3 => self.r[rd] = bus.read8(addr) as i8 as u32,
            4 => self.r[rd] = self.ld32(bus, addr)?,
            5 => self.r[rd] = self.ld16(bus, addr)? as u32,
            6 => self.r[rd] = bus.read8(addr) as u32,
            _ => self.r[rd] = self.ld16(bus, addr)? as i16 as u32,
        }
        self.pc = pc.wrapping_add(2);
        Ok(2)
    }

    #[inline(always)]
    fn op_ldst_imm<B: Bus + ?Sized>(&mut self, bus: &mut B, hw: u32, pc: u32) -> R {
        let rd = (hw & 7) as usize;
        let rn = ((hw >> 3) & 7) as usize;
        let imm5 = (hw >> 6) & 0x1F;
        let base = self.r[rn];
        match hw >> 11 {
            12 => self.st32(bus, base.wrapping_add(imm5 * 4), self.r[rd])?,
            13 => self.r[rd] = self.ld32(bus, base.wrapping_add(imm5 * 4))?,
            14 => bus.write8(base.wrapping_add(imm5), self.r[rd] as u8),
            15 => self.r[rd] = bus.read8(base.wrapping_add(imm5)) as u32,
            16 => self.st16(bus, base.wrapping_add(imm5 * 2), self.r[rd] as u16)?,
            _ => self.r[rd] = self.ld16(bus, base.wrapping_add(imm5 * 2))? as u32,
        }
        self.pc = pc.wrapping_add(2);
        Ok(2)
    }

    #[inline(always)]
    fn op_str_sp<B: Bus + ?Sized>(&mut self, bus: &mut B, hw: u32, pc: u32) -> R {
        self.st32(
            bus,
            self.sp().wrapping_add((hw & 0xFF) * 4),
            self.r[((hw >> 8) & 7) as usize],
        )?;
        self.pc = pc.wrapping_add(2);
        Ok(2)
    }

    #[inline(always)]
    fn op_ldr_sp<B: Bus + ?Sized>(&mut self, bus: &mut B, hw: u32, pc: u32) -> R {
        self.r[((hw >> 8) & 7) as usize] =
            self.ld32(bus, self.sp().wrapping_add((hw & 0xFF) * 4))?;
        self.pc = pc.wrapping_add(2);
        Ok(2)
    }

    #[inline(always)]
    fn op_adr<B: Bus + ?Sized>(&mut self, _: &mut B, hw: u32, pc: u32) -> R {
        self.r[((hw >> 8) & 7) as usize] = (pc.wrapping_add(4) & !3) + (hw & 0xFF) * 4;
        self.pc = pc.wrapping_add(2);
        Ok(1)
    }

    #[inline(always)]
    fn op_add_sp_imm<B: Bus + ?Sized>(&mut self, _: &mut B, hw: u32, pc: u32) -> R {
        self.r[((hw >> 8) & 7) as usize] = self.sp().wrapping_add((hw & 0xFF) * 4);
        self.pc = pc.wrapping_add(2);
        Ok(1)
    }

    #[inline(always)]
    fn op_misc<B: Bus + ?Sized>(&mut self, bus: &mut B, hw: u32, pc: u32) -> R {
        let mut next = pc.wrapping_add(2);
        let cycles = self.exec_misc(bus, hw, &mut next)?;
        self.pc = next;
        Ok(cycles)
    }

    #[inline(always)]
    fn op_stm<B: Bus + ?Sized>(&mut self, bus: &mut B, hw: u32, pc: u32) -> R {
        let rd8 = ((hw >> 8) & 7) as usize;
        let mut cycles = 1;
        let mut addr = self.r[rd8];
        for i in 0..8 {
            if hw & (1 << i) != 0 {
                self.st32(bus, addr, self.r[i])?;
                addr += 4;
                cycles += 1;
            }
        }
        self.r[rd8] = addr;
        self.pc = pc.wrapping_add(2);
        Ok(cycles)
    }

    #[inline(always)]
    fn op_ldm<B: Bus + ?Sized>(&mut self, bus: &mut B, hw: u32, pc: u32) -> R {
        let rd8 = ((hw >> 8) & 7) as usize;
        let mut cycles = 1;
        let mut addr = self.r[rd8];
        for i in 0..8 {
            if hw & (1 << i) != 0 {
                self.r[i] = self.ld32(bus, addr)?;
                addr += 4;
                cycles += 1;
            }
        }
        if hw & (1 << rd8) == 0 {
            self.r[rd8] = addr;
        }
        self.pc = pc.wrapping_add(2);
        Ok(cycles)
    }

    #[inline(always)]
    fn op_cond_branch<B: Bus + ?Sized>(&mut self, bus: &mut B, hw: u32, pc: u32) -> R {
        let next = pc.wrapping_add(2);
        let cond = (hw >> 8) & 0xF;
        match cond {
            0xE => Err(Fault),
            0xF => {
                self.pc = next;
                self.take_exception(bus, EXC_SVCALL);
                Ok(1)
            }
            _ if self.cond(cond) => {
                let off = ((hw & 0xFF) as u8 as i8 as i32) << 1;
                self.pc = pc.wrapping_add(4).wrapping_add(off as u32);
                Ok(2)
            }
            _ => {
                self.pc = next;
                Ok(1)
            }
        }
    }

    #[inline(always)]
    fn op_branch<B: Bus + ?Sized>(&mut self, _: &mut B, hw: u32, pc: u32) -> R {
        let off = (((hw & 0x7FF) << 21) as i32) >> 20;
        self.pc = pc.wrapping_add(4).wrapping_add(off as u32);
        Ok(2)
    }

    #[inline(always)]
    fn op_wide<B: Bus + ?Sized>(&mut self, bus: &mut B, hw: u32, pc: u32) -> R {
        let hw2 = bus.fetch16(pc.wrapping_add(2)) as u32;
        let mut next = pc.wrapping_add(4);
        let cycles = self.exec_wide(hw, hw2, pc, &mut next)?;
        self.pc = next;
        Ok(cycles)
    }

    #[inline(always)]
    fn op_undefined<B: Bus + ?Sized>(&mut self, _: &mut B, _: u32, _: u32) -> R {
        Err(Fault)
    }

    #[inline(always)]
    fn op_alu<B: Bus + ?Sized, const OP: u32>(&mut self, _: &mut B, hw: u32, pc: u32) -> R {
        let rd = (hw & 7) as usize;
        let rm = ((hw >> 3) & 7) as usize;
        let a = self.r[rd];
        let b = self.r[rm];
        let c = self.c;
        match OP {
            0x0 => {
                let r = a & b;
                self.r[rd] = r;
                self.set_nz(r)
            }
            0x1 => {
                let r = a ^ b;
                self.r[rd] = r;
                self.set_nz(r)
            }
            0x2 => {
                let (r, c) = lsl(a, b & 0xFF, c);
                self.r[rd] = r;
                self.set_nzc(r, c)
            }
            0x3 => {
                let (r, c) = lsr(a, b & 0xFF, c);
                self.r[rd] = r;
                self.set_nzc(r, c)
            }
            0x4 => {
                let (r, c) = asr(a, b & 0xFF, c);
                self.r[rd] = r;
                self.set_nzc(r, c)
            }
            0x5 => {
                let (r, c, v) = add_with_carry(a, b, c);
                self.r[rd] = r;
                self.set_nzcv(r, c, v)
            }
            0x6 => {
                let (r, c, v) = add_with_carry(a, !b, c);
                self.r[rd] = r;
                self.set_nzcv(r, c, v)
            }
            0x7 => {
                let (r, c) = ror(a, b & 0xFF, c);
                self.r[rd] = r;
                self.set_nzc(r, c)
            }
            0x8 => self.set_nz(a & b),
            0x9 => {
                let (r, c, v) = add_with_carry(!b, 0, true);
                self.r[rd] = r;
                self.set_nzcv(r, c, v)
            }
            0xA => {
                let (r, c, v) = add_with_carry(a, !b, true);
                self.set_nzcv(r, c, v)
            }
            0xB => {
                let (r, c, v) = add_with_carry(a, b, false);
                self.set_nzcv(r, c, v)
            }
            0xC => {
                let r = a | b;
                self.r[rd] = r;
                self.set_nz(r)
            }
            0xD => {
                let r = a.wrapping_mul(b);
                self.r[rd] = r;
                self.set_nz(r)
            }
            0xE => {
                let r = a & !b;
                self.r[rd] = r;
                self.set_nz(r)
            }
            _ => {
                let r = !b;
                self.r[rd] = r;
                self.set_nz(r)
            }
        }
        self.pc = pc.wrapping_add(2);
        Ok(1)
    }

    fn exec_misc<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        hw: u32,
        next: &mut u32,
    ) -> Result<u32, Fault> {
        let rd = (hw & 7) as usize;
        let rm = ((hw >> 3) & 7) as usize;
        let mut cycles = 1;
        match hw & 0xFF00 {
            0xB000 => {
                let imm = (hw & 0x7F) * 4;
                let sp = self.sp();
                self.set_sp(if hw & 0x80 != 0 {
                    sp.wrapping_sub(imm)
                } else {
                    sp.wrapping_add(imm)
                });
            }
            0xB200 => {
                let v = self.r[rm];
                self.r[rd] = match (hw >> 6) & 3 {
                    0 => v as u16 as i16 as u32,
                    1 => v as u8 as i8 as u32,
                    2 => v as u16 as u32,
                    _ => v as u8 as u32,
                };
            }
            0xB400 | 0xB500 => {
                let lr = hw & 0x100 != 0;
                let count = (hw & 0xFF).count_ones() + lr as u32;
                let mut addr = self.sp().wrapping_sub(4 * count);
                self.set_sp(addr);
                for i in 0..8 {
                    if hw & (1 << i) != 0 {
                        self.st32(bus, addr, self.r[i])?;
                        addr += 4;
                    }
                }
                if lr {
                    self.st32(bus, addr, self.lr)?;
                }
                cycles += count;
            }
            0xB600 => {
                // CPSID i / CPSIE i
                if hw & 0xFFEF == 0xB662 {
                    self.primask = hw & 0x10 != 0;
                    self.run.resync = true;
                } else {
                    return Err(Fault);
                }
            }
            0xBA00 => {
                let v = self.r[rm];
                self.r[rd] = match (hw >> 6) & 3 {
                    0 => v.swap_bytes(),
                    1 => ((v & 0xFF00_FF00) >> 8) | ((v & 0x00FF_00FF) << 8),
                    3 => (v.swap_bytes() >> 16) as u16 as i16 as u32,
                    _ => return Err(Fault),
                };
            }
            0xBC00 | 0xBD00 => {
                let pc_bit = hw & 0x100 != 0;
                let mut addr = self.sp();
                for i in 0..8 {
                    if hw & (1 << i) != 0 {
                        self.r[i] = self.ld32(bus, addr)?;
                        addr += 4;
                        cycles += 1;
                    }
                }
                if pc_bit {
                    let target = self.ld32(bus, addr)?;
                    addr += 4;
                    self.set_sp(addr);
                    *next = self.bx_write(bus, target)?;
                    cycles += 3;
                } else {
                    self.set_sp(addr);
                }
            }
            0xBE00 => self.bkpt = Some(hw as u8),
            0xBF00 => match hw & 0xFF {
                0x00 | 0x10 => {}
                0x20 => {
                    // WFE: consume a pending event, otherwise sleep
                    if self.event {
                        self.event = false;
                    } else {
                        self.sleeping = true;
                        self.wfe = true;
                    }
                }
                0x30 => self.sleeping = true,
                0x40 => {
                    self.event = true;
                    self.sev_sent = true;
                }
                // unallocated hints execute as NOP; a non-zero low nibble is IT
                x if x & 0xF == 0 => {}
                _ => return Err(Fault),
            },
            _ => return Err(Fault),
        }
        Ok(cycles)
    }
}
