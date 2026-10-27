//! ARM PrimeCell PL022 SSP (RP2040 SPI0/SPI1), master mode, Motorola SPI
//! frames of 4 to 16 bits.
//!
//! Like the I2C controller, transfers are instantaneous: [`Spi::pump`] (run
//! by the system after the access that queued a frame) pushes every TX FIFO
//! entry through an [`SpiTarget`] and collects the reply into the RX FIFO,
//! so BSY is never seen set. Clock polarity/phase and the baud divisors are
//! stored but have no effect, the receive-timeout interrupt never fires, and
//! slave mode, TI and Microwire frames are not modelled.
use alloc::collections::VecDeque;

use pcb::SpiTarget;

use super::Peripheral;

const FIFO_DEPTH: usize = 8;

// SSPCR1
const CR1_LBM: u32 = 1 << 0;
const CR1_SSE: u32 = 1 << 1;
const CR1_MS: u32 = 1 << 2;
// Interrupt bits (SSPRIS / SSPIMSC / SSPMIS).
const INT_ROR: u32 = 1 << 0;
const INT_RT: u32 = 1 << 1;
const INT_RX: u32 = 1 << 2;
const INT_TX: u32 = 1 << 3;
/// Bits that follow FIFO levels instead of being latched.
const INT_LEVEL: u32 = INT_RX | INT_TX;

pub struct Spi {
    cr0: u32,
    cr1: u32,
    cpsr: u32,
    imsc: u32,
    /// Latched interrupt bits (overrun, timeout).
    latched: u32,
    dmacr: u32,
    tx: VecDeque<u16>,
    rx: VecDeque<u16>,
}

impl Spi {
    pub fn new() -> Self {
        Self {
            cr0: 0,
            cr1: 0,
            cpsr: 0,
            imsc: 0,
            latched: 0,
            dmacr: 0,
            tx: VecDeque::new(),
            rx: VecDeque::new(),
        }
    }

    /// Frame size in bits (DSS + 1; the reserved values 0..=2 act as 8).
    fn frame_bits(&self) -> u8 {
        match (self.cr0 & 0xF) as u8 + 1 {
            1..=3 => 8,
            n => n,
        }
    }

    fn master_enabled(&self) -> bool {
        self.cr1 & CR1_SSE != 0 && self.cr1 & CR1_MS == 0
    }

    /// Frames are waiting to go out on the wire.
    pub fn has_work(&self) -> bool {
        !self.tx.is_empty() && self.master_enabled()
    }

    /// Send every queued frame, receiving one back for each.
    pub fn pump(&mut self, bus: &mut dyn SpiTarget) {
        let bits = self.frame_bits();
        let mask = if bits >= 16 {
            u16::MAX
        } else {
            (1u16 << bits) - 1
        };
        while self.has_work() {
            let Some(word) = self.tx.pop_front() else {
                break;
            };
            let word = word & mask;
            let reply = if self.cr1 & CR1_LBM != 0 {
                word
            } else {
                bus.transfer(word, bits) & mask
            };
            if self.rx.len() < FIFO_DEPTH {
                self.rx.push_back(reply);
            } else {
                self.latched |= INT_ROR;
            }
        }
    }

    fn ris(&self) -> u32 {
        let mut r = self.latched & !INT_LEVEL;
        if self.rx.len() >= FIFO_DEPTH / 2 {
            r |= INT_RX;
        }
        if self.tx.len() <= FIFO_DEPTH / 2 {
            r |= INT_TX;
        }
        r
    }

    /// Level of the combined interrupt line.
    pub fn irq(&self) -> bool {
        self.ris() & self.imsc != 0
    }

    /// DMA request levels (SSPDMACR enables them): TX while the FIFO has
    /// room, RX while it holds data.
    pub fn dreq_tx(&self) -> bool {
        self.dmacr & 2 != 0 && self.tx.len() < FIFO_DEPTH
    }
    pub fn dreq_rx(&self) -> bool {
        self.dmacr & 1 != 0 && !self.rx.is_empty()
    }

    fn status(&self) -> u32 {
        (self.tx.is_empty() as u32)
            | ((self.tx.len() < FIFO_DEPTH) as u32) << 1
            | (!self.rx.is_empty() as u32) << 2
            | ((self.rx.len() == FIFO_DEPTH) as u32) << 3
        // BSY (bit 4) is always 0: transfers complete instantly.
    }
}

impl Default for Spi {
    fn default() -> Self {
        Self::new()
    }
}

impl Peripheral for Spi {
    fn read(&mut self, off: u32) -> u32 {
        match off {
            0x000 => self.cr0,
            0x004 => self.cr1,
            0x008 => self.rx.pop_front().unwrap_or(0) as u32,
            0x00C => self.status(),
            0x010 => self.cpsr,
            0x014 => self.imsc,
            0x018 => self.ris(),
            0x01C => self.ris() & self.imsc,
            0x024 => self.dmacr,
            // PrimeCell identification: SSP, rev 3 (PeriphID 0x00341022).
            0xFE0 => 0x22,
            0xFE4 => 0x10,
            0xFE8 => 0x34,
            0xFEC => 0x00,
            0xFF0 => 0x0D,
            0xFF4 => 0xF0,
            0xFF8 => 0x05,
            0xFFC => 0xB1,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, val: u32) {
        match off {
            0x000 => self.cr0 = val & 0xFFFF,
            0x004 => self.cr1 = val & 0xF,
            0x008 => {
                // A full TX FIFO silently drops the write.
                if self.tx.len() < FIFO_DEPTH {
                    self.tx.push_back(val as u16);
                }
            }
            0x010 => self.cpsr = val & 0xFF,
            0x014 => self.imsc = val & 0xF,
            0x020 => self.latched &= !(val & (INT_ROR | INT_RT)),
            0x024 => self.dmacr = val & 3,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    /// Answers each frame with `frame ^ 0xFF`, recording what it saw.
    #[derive(Default)]
    struct Wire {
        seen: Vec<(u16, u8)>,
    }

    impl SpiTarget for Wire {
        fn transfer(&mut self, mosi: u16, bits: u8) -> u16 {
            self.seen.push((mosi, bits));
            !mosi
        }
    }

    fn enabled(dss: u32) -> Spi {
        let mut s = Spi::new();
        s.write(0x000, dss);
        s.write(0x004, CR1_SSE);
        s
    }

    #[test]
    fn full_duplex_frames_and_status_flags() {
        let mut s = enabled(7);
        let mut w = Wire::default();
        assert_eq!(
            s.read(0x00C),
            0b0011,
            "empty TX, room in TX, nothing received"
        );
        s.write(0x008, 0x1A5); // upper bits fall outside the 8-bit frame
        s.write(0x008, 0x01);
        assert_eq!(s.read(0x00C) & 1, 0, "TX not empty until pumped");
        assert!(s.has_work());
        s.pump(&mut w);
        assert_eq!(w.seen, [(0xA5, 8), (0x01, 8)]);
        assert_eq!(s.read(0x00C), 0b0111);
        assert_eq!((s.read(0x008), s.read(0x008)), (0x5A, 0xFE));
        assert_eq!(s.read(0x00C), 0b0011);
        assert_eq!(s.read(0x008), 0, "RX underrun reads zero");
    }

    #[test]
    fn frame_sizes() {
        let mut s = enabled(15); // 16-bit
        let mut w = Wire::default();
        s.write(0x008, 0xBEEF);
        s.pump(&mut w);
        assert_eq!((w.seen[0], s.read(0x008)), ((0xBEEF, 16), 0x4110));
        let mut s = enabled(3); // 4-bit
        s.write(0x008, 0xAB);
        s.pump(&mut w);
        assert_eq!((w.seen[1], s.read(0x008)), ((0xB, 4), 0x4));
        let mut s = enabled(1); // reserved size acts as 8 bits
        s.write(0x008, 0x77);
        s.pump(&mut w);
        assert_eq!(w.seen[2], (0x77, 8));
    }

    #[test]
    fn nothing_moves_while_disabled_or_in_slave_mode() {
        let mut s = Spi::new();
        let mut w = Wire::default();
        s.write(0x000, 7);
        s.write(0x008, 1);
        assert!(!s.has_work(), "SSE=0: the FIFO just fills");
        s.write(0x004, CR1_SSE | CR1_MS);
        assert!(!s.has_work(), "slave mode is not modelled");
        s.write(0x004, CR1_SSE);
        s.pump(&mut w);
        assert_eq!(w.seen.len(), 1, "queued frame goes out once enabled");
    }

    #[test]
    fn tx_fifo_depth_and_rx_overrun() {
        let mut s = Spi::new();
        s.write(0x000, 7);
        for n in 0..FIFO_DEPTH as u32 + 3 {
            s.write(0x008, n);
        }
        assert_eq!(s.read(0x00C) & 0b11, 0b00, "TX full");
        s.write(0x004, CR1_SSE);
        let mut w = Wire::default();
        s.pump(&mut w);
        assert_eq!(
            w.seen.len(),
            FIFO_DEPTH,
            "frames beyond the depth were dropped"
        );
        assert_eq!(s.read(0x00C) & 0b1000, 0b1000, "RX full");
        assert_eq!(s.read(0x018) & INT_ROR, 0);
        // One more frame with a full RX FIFO: overrun, data lost.
        s.write(0x008, 0x55);
        s.pump(&mut w);
        assert_eq!(s.read(0x018) & INT_ROR, INT_ROR);
        assert_eq!(s.read(0x008), 0xFF, "oldest data kept: frame 0 -> 0xFF");
        s.write(0x020, INT_ROR);
        assert_eq!(s.read(0x018) & INT_ROR, 0);
    }

    #[test]
    fn loopback_bypasses_the_bus() {
        let mut s = Spi::new();
        s.write(0x000, 7);
        s.write(0x004, CR1_SSE | CR1_LBM);
        s.write(0x008, 0xC3);
        let mut w = Wire::default();
        s.pump(&mut w);
        assert!(w.seen.is_empty());
        assert_eq!(s.read(0x008), 0xC3);
    }

    #[test]
    fn interrupts_follow_fifo_levels() {
        let mut s = enabled(7);
        let mut w = Wire::default();
        assert!(!s.irq());
        s.write(0x014, INT_TX | INT_RX);
        assert!(s.irq(), "TX FIFO at or below half: asserted");
        assert_eq!(s.read(0x01C), INT_TX);
        for n in 0..4 {
            s.write(0x008, n);
        }
        s.pump(&mut w);
        assert_eq!(s.read(0x01C), INT_TX | INT_RX, "RX half full");
        s.write(0x014, INT_RX);
        assert_eq!(s.read(0x01C), INT_RX);
        for _ in 0..4 {
            s.read(0x008);
        }
        assert!(!s.irq());
        // Level interrupts can't be cleared through ICR.
        s.write(0x014, INT_TX);
        s.write(0x020, 0xF);
        assert!(s.irq());
    }

    #[test]
    fn dma_requests_need_enable_bits_and_levels() {
        let mut s = enabled(7);
        let mut w = Wire::default();
        assert!(!s.dreq_tx() && !s.dreq_rx());
        s.write(0x024, 3);
        assert!(s.dreq_tx() && !s.dreq_rx());
        s.write(0x008, 1);
        s.pump(&mut w);
        assert!(s.dreq_rx());
        s.read(0x008);
        assert!(!s.dreq_rx());
        // Full TX FIFO withdraws the request (SSE=0 so nothing drains it).
        let mut t = Spi::new();
        t.write(0x024, 2);
        for _ in 0..FIFO_DEPTH {
            t.write(0x008, 0);
        }
        assert!(!t.dreq_tx());
    }

    #[test]
    fn registers_and_identification() {
        let mut s = Spi::new();
        s.write(0x000, 0x1_07C7);
        s.write(0x004, 0xFF);
        s.write(0x010, 0x1FF);
        assert_eq!(
            (s.read(0x000), s.read(0x004), s.read(0x010)),
            (0x07C7, 0xF, 0xFF)
        );
        let id: Vec<u32> = (0..4).map(|i| s.read(0xFE0 + 4 * i)).collect();
        assert_eq!(id, [0x22, 0x10, 0x34, 0x00]);
        assert_eq!(s.read(0xFFC), 0xB1);
    }
}
