//! 32-bit Thumb encodings (BL, MSR, MRS, barriers) and special registers.

use super::*;

impl Cpu {
    pub(super) fn exec_wide(&mut self, hw1: u32, hw2: u32, pc: u32, next: &mut u32) -> Result<u32, Fault> {
        if hw2 & 0xD000 == 0xD000 {
            // BL
            let s = (hw1 >> 10) & 1;
            let j1 = (hw2 >> 13) & 1;
            let j2 = (hw2 >> 11) & 1;
            let i1 = !(j1 ^ s) & 1;
            let i2 = !(j2 ^ s) & 1;
            let imm = s << 24 | i1 << 23 | i2 << 22 | (hw1 & 0x3FF) << 12 | (hw2 & 0x7FF) << 1;
            let off = ((imm << 7) as i32) >> 7;
            self.lr = *next | 1;
            *next = pc.wrapping_add(4).wrapping_add(off as u32);
            return Ok(4);
        }
        let result = if hw1 & 0xFFF0 == 0xF380 && hw2 & 0xFF00 == 0x8800 {
            // Rn = pc is UNPREDICTABLE; fault rather than guess
            match hw1 & 0xF {
                15 => Err(Fault),
                n => {
                    self.msr(hw2 & 0xFF, self.reg(n as usize));
                    Ok(())
                }
            }
        } else if hw1 == 0xF3EF && hw2 & 0xF000 == 0x8000 {
            match (hw2 >> 8) & 0xF {
                15 => Err(Fault),
                n => {
                    let v = self.mrs(hw2 & 0xFF);
                    self.write_reg(n as usize, v);
                    Ok(())
                }
            }
        } else if hw1 == 0xF3BF && matches!(hw2 & 0xFFF0, 0x8F40 | 0x8F50 | 0x8F60) {
            Ok(()) // DSB / DMB / ISB
        } else {
            Err(Fault)
        };
        result?;
        Ok(if hw1 == 0xF3BF { 3 } else { 4 })
    }

    fn msr(&mut self, sysm: u32, v: u32) {
        match sysm {
            0..=3 => self.set_flags(v),
            8 => self.msp = v & !3,
            9 => self.psp = v & !3,
            16 => self.primask = v & 1 != 0,
            20 => {
                if self.ipsr == 0 {
                    self.spsel = v & 2 != 0;
                }
            }
            _ => {}
        }
    }

    fn mrs(&self, sysm: u32) -> u32 {
        let apsr = self.flags();
        let ipsr = self.ipsr as u32;
        // EPSR (the Thumb bit) always reads as zero through MRS.
        match sysm {
            0 | 2 => apsr,
            1 | 3 => apsr | ipsr,
            5 | 7 => ipsr,
            6 => 0,
            8 => self.msp,
            9 => self.psp,
            16 => self.primask as u32,
            20 => (self.spsel as u32) << 1,
            _ => 0,
        }
    }
}
