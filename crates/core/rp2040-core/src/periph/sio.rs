//! The multi-core parts of SIO: CPUID, the inter-core FIFOs, the 32
//! hardware spinlocks and the per-core integer dividers. (The GPIO half of
//! SIO lives with the pads in [`gpio`](super::gpio).)
//!
//! Every access carries the number of the core making it, because most
//! registers show a different view to each core. Not modelled: the two
//! interpolators per core (their registers read as zero).

use alloc::collections::VecDeque;

const FIFO_DEPTH: usize = 8;

// FIFO_ST bits
const ST_VLD: u32 = 1 << 0;
const ST_RDY: u32 = 1 << 1;
const ST_WOF: u32 = 1 << 2;
const ST_ROE: u32 = 1 << 3;

/// One core's hardware divider. Results are available immediately instead
/// of after 8 cycles, so READY always reads 1.
#[derive(Clone, Copy, Default)]
struct Divider {
    dividend: u32,
    divisor: u32,
    quotient: u32,
    remainder: u32,
    signed: bool,
    /// Set by any write, cleared by reading the quotient.
    dirty: bool,
}

impl Divider {
    fn compute(&mut self) {
        (self.quotient, self.remainder) = if self.signed {
            let (n, d) = (self.dividend as i32, self.divisor as i32);
            match d {
                // Division by zero: quotient is -1 for a non-negative dividend, +1 otherwise.
                0 => (if n < 0 { 1 } else { -1 } as u32, n as u32),
                _ => (n.wrapping_div(d) as u32, n.wrapping_rem(d) as u32),
            }
        } else {
            match self.divisor {
                0 => (u32::MAX, self.dividend),
                d => (self.dividend / d, self.dividend % d),
            }
        };
    }
}

pub struct Sio {
    /// Words waiting to be read by each core (`rx[n]` is core n's RX FIFO;
    /// the other core's writes land there).
    rx: [VecDeque<u32>; 2],
    /// Sticky WOF/ROE flags per core, in FIFO_ST layout.
    sticky: [u32; 2],
    /// Spinlocks currently held (bit n = lock n).
    locks: u32,
    div: [Divider; 2],
}

impl Sio {
    pub fn new() -> Self {
        Self {
            rx: [VecDeque::new(), VecDeque::new()],
            sticky: [0; 2],
            locks: 0,
            div: [Divider::default(); 2],
        }
    }

    /// Level of core `core`'s FIFO interrupt (SIO_IRQ_PROC0 / SIO_IRQ_PROC1):
    /// data waiting, or a sticky error flag set.
    pub fn irq(&self, core: usize) -> bool {
        !self.rx[core].is_empty() || self.sticky[core] != 0
    }

    /// The spinlocks held (bit n = lock n).
    pub fn locks(&self) -> u32 {
        self.locks
    }

    /// Whether `off` (relative to the SIO base) is one of the registers here.
    pub fn handles(off: u32) -> bool {
        matches!(off, 0x000 | 0x050..=0x0FC | 0x100..=0x17C)
    }

    fn status(&self, core: usize) -> u32 {
        let other = 1 - core;
        let vld = if self.rx[core].is_empty() { 0 } else { ST_VLD };
        let rdy = if self.rx[other].len() < FIFO_DEPTH {
            ST_RDY
        } else {
            0
        };
        vld | rdy | self.sticky[core]
    }

    pub fn read(&mut self, core: usize, off: u32) -> u32 {
        match off {
            0x000 => core as u32,
            0x050 => self.status(core),
            0x058 => self.rx[core].pop_front().unwrap_or_else(|| {
                self.sticky[core] |= ST_ROE;
                0
            }),
            0x05C => self.locks,
            0x060 | 0x068 => self.div[core].dividend,
            0x064 | 0x06C => self.div[core].divisor,
            0x070 => {
                self.div[core].dirty = false;
                self.div[core].quotient
            }
            0x074 => self.div[core].remainder,
            0x078 => 1 | (self.div[core].dirty as u32) << 1,
            0x100..=0x17C => {
                // Reading takes the lock if it is free: non-zero means it is ours.
                let bit = 1 << ((off - 0x100) >> 2);
                if self.locks & bit != 0 {
                    0
                } else {
                    self.locks |= bit;
                    bit
                }
            }
            _ => 0,
        }
    }

    pub fn write(&mut self, core: usize, off: u32, val: u32) {
        let d = &mut self.div[core];
        match off {
            // Any write clears both sticky flags (datasheet 2.3.1.3; the SVD's
            // "oneToClear" is wrong: embassy clears the IRQ by writing 0).
            0x050 => self.sticky[core] = 0,
            0x054 => {
                let other = 1 - core;
                if self.rx[other].len() < FIFO_DEPTH {
                    self.rx[other].push_back(val);
                } else {
                    self.sticky[core] |= ST_WOF;
                }
            }
            0x060 | 0x068 | 0x064 | 0x06C => {
                match off {
                    0x060 | 0x068 => d.dividend = val,
                    _ => d.divisor = val,
                }
                d.signed = off >= 0x068;
                d.dirty = true;
                d.compute();
            }
            0x070 => {
                d.quotient = val;
                d.dirty = true;
            }
            0x074 => {
                d.remainder = val;
                d.dirty = true;
            }
            0x100..=0x17C => self.locks &= !(1 << ((off - 0x100) >> 2)),
            _ => {}
        }
    }
}

impl Default for Sio {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpuid_is_the_core_number() {
        let mut s = Sio::new();
        assert_eq!((s.read(0, 0), s.read(1, 0)), (0, 1));
    }

    #[test]
    fn fifos_connect_the_cores_crosswise() {
        let mut s = Sio::new();
        assert_eq!(s.read(0, 0x050), ST_RDY, "nothing to read, room to write");
        s.write(0, 0x054, 0xAAAA);
        s.write(0, 0x054, 0xBBBB);
        s.write(1, 0x054, 0xCCCC);
        assert_eq!(s.read(0, 0x050) & ST_VLD, ST_VLD);
        assert_eq!(s.read(1, 0x050) & ST_VLD, ST_VLD);
        assert_eq!(
            (s.read(1, 0x058), s.read(1, 0x058)),
            (0xAAAA, 0xBBBB),
            "core 1 reads in order"
        );
        assert_eq!(s.read(1, 0x050) & ST_VLD, 0);
        assert_eq!(s.read(0, 0x058), 0xCCCC);
        assert_eq!(s.read(0, 0x050) & ST_VLD, 0);
    }

    #[test]
    fn full_fifo_drops_writes_and_flags_overflow_until_cleared() {
        let mut s = Sio::new();
        for n in 0..FIFO_DEPTH as u32 {
            assert_eq!(s.read(0, 0x050) & ST_RDY, ST_RDY);
            s.write(0, 0x054, n);
        }
        assert_eq!(s.read(0, 0x050) & (ST_RDY | ST_WOF), 0);
        s.write(0, 0x054, 99);
        assert_eq!(s.read(0, 0x050) & ST_WOF, ST_WOF);
        assert!(s.irq(0), "sticky flag raises the interrupt");
        let got: alloc::vec::Vec<u32> = (0..FIFO_DEPTH).map(|_| s.read(1, 0x058)).collect();
        assert_eq!(got, [0, 1, 2, 3, 4, 5, 6, 7], "the extra word was dropped");
        s.write(0, 0x050, 0xFF);
        assert_eq!(s.read(0, 0x050) & ST_WOF, 0);
        assert!(!s.irq(0));
        assert_eq!(s.read(0, 0x050) & ST_RDY, ST_RDY, "room again");
    }

    #[test]
    fn reading_an_empty_fifo_returns_zero_and_sets_roe() {
        let mut s = Sio::new();
        assert_eq!(s.read(1, 0x058), 0);
        assert_eq!(s.read(1, 0x050) & ST_ROE, ST_ROE);
        assert!(s.irq(1) && !s.irq(0));
        s.write(1, 0x050, ST_ROE);
        assert!(!s.irq(1));
        // Writing any value clears them, as embassy's SIO_IRQ_PROC1 handler relies on.
        s.read(1, 0x058);
        s.write(1, 0x050, 0);
        assert!(!s.irq(1));
    }

    #[test]
    fn fifo_interrupt_is_level_sensitive_on_data() {
        let mut s = Sio::new();
        s.write(0, 0x054, 1);
        assert!(s.irq(1) && !s.irq(0));
        s.read(1, 0x058);
        assert!(!s.irq(1));
    }

    #[test]
    fn spinlocks_are_taken_by_reading_and_released_by_writing() {
        let mut s = Sio::new();
        assert_eq!(s.read(0, 0x100 + 4 * 5), 1 << 5, "free: acquired");
        assert_eq!(s.read(1, 0x100 + 4 * 5), 0, "held: the other core fails");
        assert_eq!(s.read(0, 0x100 + 4 * 5), 0, "not re-entrant");
        assert_eq!(s.read(0, 0x5C), 1 << 5);
        assert_ne!(s.read(1, 0x100 + 4 * 6), 0, "locks are independent");
        s.write(1, 0x100 + 4 * 5, 0); // any core may release
        assert_ne!(s.read(1, 0x100 + 4 * 5), 0);
        assert_eq!(s.read(0, 0x5C), 1 << 5 | 1 << 6);
        s.write(0, 0x100 + 4 * 31, 0);
        assert_ne!(s.read(0, 0x100 + 4 * 31), 0);
    }

    fn divide(s: &mut Sio, core: usize, signed: bool, n: u32, d: u32) -> (u32, u32) {
        let (nr, dr) = if signed { (0x68, 0x6C) } else { (0x60, 0x64) };
        s.write(core, nr, n);
        s.write(core, dr, d);
        (s.read(core, 0x70), s.read(core, 0x74))
    }

    #[test]
    fn unsigned_division() {
        let mut s = Sio::new();
        assert_eq!(divide(&mut s, 0, false, 100, 7), (14, 2));
        assert_eq!(divide(&mut s, 0, false, 0xFFFF_FFFF, 2), (0x7FFF_FFFF, 1));
        assert_eq!(
            divide(&mut s, 0, false, 5, 0),
            (u32::MAX, 5),
            "divide by zero"
        );
        assert_eq!(divide(&mut s, 0, false, 0, 9), (0, 0));
    }

    #[test]
    fn signed_division_truncates_toward_zero() {
        let mut s = Sio::new();
        let r = |(q, m): (u32, u32)| (q as i32, m as i32);
        assert_eq!(r(divide(&mut s, 0, true, (-7i32) as u32, 2)), (-3, -1));
        assert_eq!(r(divide(&mut s, 0, true, 7, (-2i32) as u32)), (-3, 1));
        assert_eq!(
            r(divide(&mut s, 0, true, (-7i32) as u32, (-2i32) as u32)),
            (3, -1)
        );
        assert_eq!(
            r(divide(&mut s, 0, true, 9, 0)),
            (-1, 9),
            "divide by zero, non-negative dividend"
        );
        assert_eq!(r(divide(&mut s, 0, true, (-9i32) as u32, 0)), (1, -9));
        assert_eq!(
            r(divide(&mut s, 0, true, i32::MIN as u32, (-1i32) as u32)),
            (i32::MIN, 0),
            "overflow wraps"
        );
    }

    #[test]
    fn divider_csr_each_core_has_its_own_state() {
        let mut s = Sio::new();
        assert_eq!(s.read(0, 0x78), 1, "READY, clean");
        s.write(0, 0x60, 10);
        assert_eq!(s.read(0, 0x78), 3, "dirty after a write");
        s.write(0, 0x64, 3);
        assert_eq!(s.read(0, 0x74), 1, "remainder read leaves it dirty");
        assert_eq!(s.read(0, 0x78) >> 1, 1);
        assert_eq!(s.read(0, 0x70), 3);
        assert_eq!(s.read(0, 0x78), 1, "reading the quotient cleans it");
        // Core 1's divider is untouched, and the operands read back.
        assert_eq!((s.read(1, 0x78), s.read(1, 0x60)), (1, 0));
        assert_eq!((s.read(0, 0x60), s.read(0, 0x64)), (10, 3));
        // Results can be written back (context switches save and restore them).
        s.write(0, 0x70, 77);
        s.write(0, 0x74, 5);
        assert_eq!((s.read(0, 0x70), s.read(0, 0x74)), (77, 5));
    }

    #[test]
    fn register_ownership() {
        assert!(
            Sio::handles(0x000)
                && Sio::handles(0x050)
                && Sio::handles(0x17C)
                && Sio::handles(0x100)
        );
        assert!(!Sio::handles(0x004) && !Sio::handles(0x2C) && !Sio::handles(0x180));
    }
}
