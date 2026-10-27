//! CLOCKS: the clock generators are only registers here (the emulator runs
//! at a fixed rate), but the `*_SELECTED` status reads follow the mux
//! controls as on the chip, since start-up code waits for exact values.
use super::{Peripheral, RegStub};

const CLK_REF_CTRL: u32 = 0x30;
const CLK_REF_SELECTED: u32 = 0x38;
const CLK_SYS_CTRL: u32 = 0x3C;
const CLK_SYS_SELECTED: u32 = 0x44;
/// The other generators have no glitchless mux: SELECTED is hardwired to 1.
const FIXED_SELECTED: [u32; 4] = [0x50, 0x5C, 0x68, 0x74];

pub struct Clocks {
    regs: RegStub,
}

impl Clocks {
    pub fn new() -> Self {
        Self {
            regs: RegStub::new(&[]),
        }
    }
}

impl Default for Clocks {
    fn default() -> Self {
        Self::new()
    }
}

impl Peripheral for Clocks {
    fn read(&mut self, off: u32) -> u32 {
        match off {
            // One-hot of CTRL.SRC (2 bits for clk_ref, 1 for clk_sys).
            CLK_REF_SELECTED => 1 << (self.regs.read(CLK_REF_CTRL) & 3),
            CLK_SYS_SELECTED => 1 << (self.regs.read(CLK_SYS_CTRL) & 1),
            o if FIXED_SELECTED.contains(&o) => 1,
            o => self.regs.read(o),
        }
    }
    fn write(&mut self, off: u32, val: u32) {
        self.regs.write(off, val);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_follows_the_glitchless_muxes() {
        let mut c = Clocks::new();
        assert_eq!((c.read(CLK_REF_SELECTED), c.read(CLK_SYS_SELECTED)), (1, 1));
        c.write(CLK_REF_CTRL, 2); // XOSC
        c.write(CLK_SYS_CTRL, 1); // aux source (AUXSRC = 0)
        assert_eq!((c.read(CLK_REF_SELECTED), c.read(CLK_SYS_SELECTED)), (4, 2));
        assert_eq!(c.read(0x50), 1);
        c.write(0x48, 0x800); // CLK_PERI_CTRL.ENABLE reads back
        assert_eq!(c.read(0x48), 0x800);
    }
}
