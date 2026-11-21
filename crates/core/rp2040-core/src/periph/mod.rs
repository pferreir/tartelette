//! Memory-mapped peripherals. Each one sees only its own register offsets;
//! address decoding, reset gating and the atomic set/clear/xor aliases are
//! handled by the system bus.
use alloc::vec::Vec;

pub mod clocks;
pub mod dma;
pub mod gpio;
pub mod i2c;
pub mod pio;
pub mod psm;
pub mod resets;
pub mod rtc;
pub mod sio;
pub mod spi;
pub mod ssi;
pub mod timer;
pub mod uart;
pub mod usb;

pub trait Peripheral {
    /// Read the 32-bit register at byte offset `off` (0..0x1000).
    fn read(&mut self, off: u32) -> u32;
    fn write(&mut self, off: u32, val: u32);
}

/// Placeholder for peripherals that are not modelled yet: writes are stored
/// and read back, except for bits in `forced` which always read as 1. The
/// forced bits are the "ready/locked/stable" status flags that SDK start-up
/// code would otherwise spin on forever. [`RegStub::with_reset`] sets the
/// registers whose reset value firmware looks at.
pub struct RegStub {
    regs: Vec<u32>,
    forced: &'static [(u32, u32)],
}

impl RegStub {
    pub fn new(forced: &'static [(u32, u32)]) -> Self {
        Self {
            regs: alloc::vec![0; 0x400],
            forced,
        }
    }
}

impl RegStub {
    /// The same, with `(offset, value)` reset values.
    pub fn with_reset(forced: &'static [(u32, u32)], reset: &[(u32, u32)]) -> Self {
        let mut s = Self::new(forced);
        for &(off, val) in reset {
            s.write(off, val);
        }
        s
    }
}

impl Peripheral for RegStub {
    fn read(&mut self, off: u32) -> u32 {
        let v = self.regs.get((off >> 2) as usize).copied().unwrap_or(0);
        let force = self
            .forced
            .iter()
            .filter(|(o, _)| *o == off)
            .fold(0, |a, (_, m)| a | m);
        v | force
    }
    fn write(&mut self, off: u32, val: u32) {
        if let Some(r) = self.regs.get_mut((off >> 2) as usize) {
            *r = val;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regstub_stores_and_forces_status_bits() {
        static FORCED: &[(u32, u32)] = &[(0x10, 1 << 31), (0x10, 1)];
        let mut s = RegStub::new(FORCED);
        assert_eq!(s.read(0x10), 1 << 31 | 1); // forced bits read as 1 even when unwritten
        s.write(0x10, 0x10);
        assert_eq!(s.read(0x10), 1 << 31 | 0x11);
        s.write(0x14, 0xDEAD_BEEF);
        assert_eq!(s.read(0x14), 0xDEAD_BEEF);
        assert_eq!(s.read(0x18), 0);
        s.write(0xFFC, 5); // last register in the 4 KiB window
        assert_eq!(s.read(0xFFC), 5);
    }

    #[test]
    fn regstub_reset_values_read_back_until_written() {
        let mut s = RegStub::with_reset(&[], &[(0x0C, 0x1F)]);
        assert_eq!(s.read(0x0C), 0x1F);
        s.write(0x0C, 3);
        assert_eq!(s.read(0x0C), 3);
    }
}
