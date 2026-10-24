//! RESETS block: tracks which peripherals are held in reset.
use super::Peripheral;

pub const ALL: u32 = 0x01FF_FFFF;

pub const IO_QSPI: u32 = 6;
pub const PADS_QSPI: u32 = 9;

pub struct Resets {
    /// Bit set = peripheral is held in reset.
    pub reset: u32,
    wdsel: u32,
}

impl Resets {
    /// Power-on state, minus the QSPI IO/pads that the bootrom releases
    /// before handing over to the application (we have no bootrom).
    pub fn new() -> Self {
        Self {
            reset: ALL & !(1 << IO_QSPI | 1 << PADS_QSPI),
            wdsel: 0,
        }
    }

    pub fn in_reset(&self, bit: u32) -> bool {
        self.reset >> bit & 1 != 0
    }
}

impl Default for Resets {
    fn default() -> Self {
        Self::new()
    }
}

impl Peripheral for Resets {
    fn read(&mut self, off: u32) -> u32 {
        match off {
            0x0 => self.reset,
            0x4 => self.wdsel,
            // Reset release is instantaneous in the model.
            0x8 => !self.reset & ALL,
            _ => 0,
        }
    }
    fn write(&mut self, off: u32, val: u32) {
        match off {
            0x0 => self.reset = val & ALL,
            0x4 => self.wdsel = val & ALL,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_on_state_has_qspi_released() {
        let r = Resets::new();
        assert!(!r.in_reset(IO_QSPI) && !r.in_reset(PADS_QSPI));
        assert!(r.in_reset(0) && r.in_reset(24));
    }

    #[test]
    fn registers_are_masked_and_reset_done_is_the_complement() {
        let mut r = Resets::new();
        r.write(0x0, 0xFFFF_FFFF);
        assert_eq!(r.read(0x0), ALL);
        assert_eq!(r.read(0x8), 0);
        r.write(0x0, 1 << 5);
        assert_eq!(r.read(0x8), ALL & !(1 << 5));
        r.write(0x4, 0xFFFF_FFFF);
        assert_eq!(r.read(0x4), ALL);
        assert_eq!(r.read(0xC), 0); // unknown offset
    }

    #[test]
    fn writes_to_unknown_offsets_are_ignored_and_default_matches_new() {
        let mut r = Resets::default();
        let before = (r.read(0x0), r.read(0x4));
        r.write(0xC, 0xFFFF_FFFF);
        r.write(0x100, 0xFFFF_FFFF);
        assert_eq!((r.read(0x0), r.read(0x4)), before);
        assert_eq!(before, (Resets::new().reset, 0));
    }
}
