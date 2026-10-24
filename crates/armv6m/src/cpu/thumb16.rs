//! 16-bit Thumb instruction decode and execution.

use super::*;

impl Cpu {
    /// Fetch, decode and execute one instruction.
    pub(super) fn exec16<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Result<u32, Fault> {
        let pc = self.pc;
        let hw = bus.read16(pc) as u32;
        let mut next = pc.wrapping_add(2);
        let mut cycles = 1;

        let rd = (hw & 7) as usize;
        let rn = ((hw >> 3) & 7) as usize;
        let rm6 = ((hw >> 6) & 7) as usize;
        let rd8 = ((hw >> 8) & 7) as usize;
        let imm8 = hw & 0xFF;
        let pc_aligned = pc.wrapping_add(4) & !3;

        match hw >> 11 {
            // LSL / LSR / ASR (immediate)
            op @ 0..=2 => {
                let imm = (hw >> 6) & 0x1F;
                let val = self.r[rn];
                let (res, c) = match op {
                    0 => lsl(val, imm, self.c),
                    1 => lsr(val, if imm == 0 { 32 } else { imm }, self.c),
                    _ => asr(val, if imm == 0 { 32 } else { imm }, self.c),
                };
                self.r[rd] = res;
                self.set_nzc(res, c);
            }
            // ADD / SUB register or imm3
            3 => {
                let b = if hw & (1 << 10) != 0 { rm6 as u32 } else { self.r[rm6] };
                let (res, c, v) = if hw & (1 << 9) != 0 {
                    add_with_carry(self.r[rn], !b, true)
                } else {
                    add_with_carry(self.r[rn], b, false)
                };
                self.r[rd] = res;
                self.set_nzcv(res, c, v);
            }
            4 => {
                self.r[rd8] = imm8;
                self.set_nz(imm8);
            }
            5 => {
                let (res, c, v) = add_with_carry(self.r[rd8], !imm8, true);
                self.set_nzcv(res, c, v);
            }
            6 => {
                let (res, c, v) = add_with_carry(self.r[rd8], imm8, false);
                self.r[rd8] = res;
                self.set_nzcv(res, c, v);
            }
            7 => {
                let (res, c, v) = add_with_carry(self.r[rd8], !imm8, true);
                self.r[rd8] = res;
                self.set_nzcv(res, c, v);
            }
            8 if hw & (1 << 10) == 0 => self.exec_alu(hw),
            8 => {
                // special data / branch exchange
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
            }
            // LDR (literal)
            9 => {
                self.r[rd8] = self.ld32(bus, pc_aligned + imm8 * 4)?;
                cycles = 2;
            }
            // load/store register offset
            10 | 11 => {
                let addr = self.r[rn].wrapping_add(self.r[rm6]);
                cycles = 2;
                match (hw >> 9) & 7 {
                    0 => self.st32(bus, addr, self.r[rd])?,
                    1 => self.st16(bus, addr, self.r[rd] as u16)?,
                    2 => bus.write8(addr, self.r[rd] as u8),
                    3 => self.r[rd] = bus.read8(addr) as i8 as u32,
                    4 => self.r[rd] = self.ld32(bus, addr)?,
                    5 => self.r[rd] = self.ld16(bus, addr)? as u32,
                    6 => self.r[rd] = bus.read8(addr) as u32,
                    _ => self.r[rd] = self.ld16(bus, addr)? as i16 as u32,
                }
            }
            // load/store immediate offset
            op @ 12..=17 => {
                let imm5 = (hw >> 6) & 0x1F;
                let base = self.r[rn];
                cycles = 2;
                match op {
                    12 => self.st32(bus, base.wrapping_add(imm5 * 4), self.r[rd])?,
                    13 => self.r[rd] = self.ld32(bus, base.wrapping_add(imm5 * 4))?,
                    14 => bus.write8(base.wrapping_add(imm5), self.r[rd] as u8),
                    15 => self.r[rd] = bus.read8(base.wrapping_add(imm5)) as u32,
                    16 => self.st16(bus, base.wrapping_add(imm5 * 2), self.r[rd] as u16)?,
                    _ => self.r[rd] = self.ld16(bus, base.wrapping_add(imm5 * 2))? as u32,
                }
            }
            // STR / LDR sp-relative
            18 => {
                self.st32(bus, self.sp().wrapping_add(imm8 * 4), self.r[rd8])?;
                cycles = 2;
            }
            19 => {
                self.r[rd8] = self.ld32(bus, self.sp().wrapping_add(imm8 * 4))?;
                cycles = 2;
            }
            // ADR / ADD rd, sp, #imm
            20 => self.r[rd8] = pc_aligned + imm8 * 4,
            21 => self.r[rd8] = self.sp().wrapping_add(imm8 * 4),
            22 | 23 => cycles = self.exec_misc(bus, hw, &mut next)?,
            // STM / LDM
            24 => {
                let mut addr = self.r[rd8];
                for i in 0..8 {
                    if hw & (1 << i) != 0 {
                        self.st32(bus, addr, self.r[i])?;
                        addr += 4;
                        cycles += 1;
                    }
                }
                self.r[rd8] = addr;
            }
            25 => {
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
            }
            // conditional branch / SVC / UDF
            26 | 27 => {
                let cond = (hw >> 8) & 0xF;
                match cond {
                    0xE => return Err(Fault),
                    0xF => {
                        self.pc = next;
                        self.take_exception(bus, EXC_SVCALL);
                        return Ok(1);
                    }
                    _ => {
                        if self.cond(cond) {
                            let off = (imm8 as u8 as i8 as i32) << 1;
                            next = pc.wrapping_add(4).wrapping_add(off as u32);
                            cycles = 2;
                        }
                    }
                }
            }
            // unconditional branch
            28 => {
                let off = (((hw & 0x7FF) << 21) as i32) >> 20;
                next = pc.wrapping_add(4).wrapping_add(off as u32);
                cycles = 2;
            }
            // 32-bit encodings
            30 => {
                let hw2 = bus.read16(pc.wrapping_add(2)) as u32;
                next = pc.wrapping_add(4);
                cycles = self.exec_wide(hw, hw2, pc, &mut next)?;
            }
            _ => return Err(Fault),
        }
        self.pc = next;
        Ok(cycles)
    }

    fn exec_alu(&mut self, hw: u32) {
        let rd = (hw & 7) as usize;
        let rm = ((hw >> 3) & 7) as usize;
        let a = self.r[rd];
        let b = self.r[rm];
        let c = self.c;
        match (hw >> 6) & 0xF {
            0x0 => { let r = a & b; self.r[rd] = r; self.set_nz(r) }
            0x1 => { let r = a ^ b; self.r[rd] = r; self.set_nz(r) }
            0x2 => { let (r, c) = lsl(a, b & 0xFF, c); self.r[rd] = r; self.set_nzc(r, c) }
            0x3 => { let (r, c) = lsr(a, b & 0xFF, c); self.r[rd] = r; self.set_nzc(r, c) }
            0x4 => { let (r, c) = asr(a, b & 0xFF, c); self.r[rd] = r; self.set_nzc(r, c) }
            0x5 => { let (r, c, v) = add_with_carry(a, b, c); self.r[rd] = r; self.set_nzcv(r, c, v) }
            0x6 => { let (r, c, v) = add_with_carry(a, !b, c); self.r[rd] = r; self.set_nzcv(r, c, v) }
            0x7 => { let (r, c) = ror(a, b & 0xFF, c); self.r[rd] = r; self.set_nzc(r, c) }
            0x8 => self.set_nz(a & b),
            0x9 => { let (r, c, v) = add_with_carry(!b, 0, true); self.r[rd] = r; self.set_nzcv(r, c, v) }
            0xA => { let (r, c, v) = add_with_carry(a, !b, true); self.set_nzcv(r, c, v) }
            0xB => { let (r, c, v) = add_with_carry(a, b, false); self.set_nzcv(r, c, v) }
            0xC => { let r = a | b; self.r[rd] = r; self.set_nz(r) }
            0xD => { let r = a.wrapping_mul(b); self.r[rd] = r; self.set_nz(r) }
            0xE => { let r = a & !b; self.r[rd] = r; self.set_nz(r) }
            _ => { let r = !b; self.r[rd] = r; self.set_nz(r) }
        }
    }

    fn exec_misc<B: Bus + ?Sized>(&mut self, bus: &mut B, hw: u32, next: &mut u32) -> Result<u32, Fault> {
        let rd = (hw & 7) as usize;
        let rm = ((hw >> 3) & 7) as usize;
        let mut cycles = 1;
        match hw & 0xFF00 {
            0xB000 => {
                let imm = (hw & 0x7F) * 4;
                let sp = self.sp();
                self.set_sp(if hw & 0x80 != 0 { sp.wrapping_sub(imm) } else { sp.wrapping_add(imm) });
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
