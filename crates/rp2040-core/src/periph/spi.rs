//! ARM PrimeCell PL022 SSP (RP2040 SPI0/SPI1), master mode, Motorola SPI
//! frames of 4 to 16 bits.
//!
//! [`Spi::pump`] (run by the system after the access that queued a frame)
//! hands every TX FIFO entry to an [`SpiTarget`] at once, but the frames take
//! the wire's time: they go out back to back, each `bits × CPSDVSR × (1 +
//! SCR)` cycles of clk_peri ([`Spi::set_clk_peri`]). Until a frame starts it
//! still counts as in the TX FIFO, its reply reaches the RX FIFO when it
//! ends, and BSY is set until the last one is out. With no prescaler
//! programmed (CPSR = 0) transfers are instantaneous. Clock polarity and
//! phase are stored but have no effect, the receive-timeout interrupt never
//! fires, and slave mode, TI and Microwire frames are not modelled.
use alloc::collections::VecDeque;

use pcb::SpiTarget;

use super::Peripheral;

const FIFO_DEPTH: usize = 8;
/// [`Spi::outputs`] bits.
pub const OUT_IRQ: u8 = 1;
pub const OUT_DREQ_TX: u8 = 2;
pub const OUT_DREQ_RX: u8 = 4;
/// The emulator's clock, which the wire's times are counted in.
const CLK_SYS_HZ: u64 = crate::system::CYCLES_PER_US * 1_000_000;

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
    /// clk_peri, which the prescaler divides.
    clk_peri_hz: u64,
    /// The cycle up to which the wire has been run.
    cycles: u64,
    /// Frames handed to the target but not finished on the wire: when each
    /// starts and ends, and the reply that lands when it does.
    wire: VecDeque<(u64, u64, u16)>,
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
            clk_peri_hz: CLK_SYS_HZ,
            cycles: 0,
            wire: VecDeque::new(),
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

    /// Set clk_peri's frequency (from the clock generators). Frames already
    /// handed out keep their times.
    pub fn set_clk_peri(&mut self, hz: u64) {
        self.clk_peri_hz = hz.max(1);
    }

    /// The cycles a frame takes on the wire, or None while no prescaler is
    /// programmed (transfers are then instantaneous).
    fn frame_cycles(&self) -> Option<u64> {
        let cpsdvsr = (self.cpsr & 0xFE) as u64;
        let scr = (self.cr0 >> 8 & 0xFF) as u64;
        let clocks = self.frame_bits() as u64 * cpsdvsr * (1 + scr);
        let per = self.clk_peri_hz;
        (cpsdvsr != 0).then(|| (clocks * CLK_SYS_HZ + per / 2) / per)
    }

    /// Entries in the TX FIFO: those not handed out yet, and those handed
    /// out that haven't started on the wire.
    fn tx_level(&self) -> usize {
        self.tx.len() + self.wire.iter().filter(|f| f.0 > self.cycles).count()
    }

    /// Frames are waiting to go out on the wire.
    pub fn has_work(&self) -> bool {
        !self.tx.is_empty() && self.master_enabled()
    }

    /// Run the wire up to `to`: replies of the frames that ended land in the
    /// RX FIFO. True if that moved one of the [`Spi::outputs`] in `watched`.
    /// The system runs this whenever its clock moves (register accesses
    /// included, so the firmware always reads the wire as it is), so an idle
    /// wire returns at once (and leaves `cycles` behind: [`Spi::pump`]
    /// catches it up).
    #[inline]
    pub fn update(&mut self, to: u64, watched: u8) -> bool {
        if self.wire.is_empty() {
            return false;
        }
        if watched & self.enabled_outputs() == 0 {
            self.run_wire(to);
            return false;
        }
        let before = self.outputs() & watched;
        self.run_wire(to) && self.outputs() & watched != before
    }

    fn run_wire(&mut self, to: u64) -> bool {
        let mut changed = false;
        while let Some(&(_, end, reply)) = self.wire.front().filter(|f| f.1 <= to) {
            self.wire.pop_front();
            self.cycles = end;
            self.receive(reply);
            changed = true;
        }
        self.cycles = self.cycles.max(to);
        changed
    }

    /// The cycle at which the wire next changes on its own: the next frame
    /// end (frames go back to back, so it is also when the next one starts).
    /// None if none of the outputs in `watched` is enabled: then nothing
    /// sees a frame end until the firmware reads a register, which runs the
    /// wire first, and an event per frame would only keep the cores from
    /// running in batches for as long as the wire is busy.
    #[inline]
    pub fn next_event(&self, watched: u8) -> Option<u64> {
        self.wire
            .front()
            .filter(|_| watched & self.enabled_outputs() != 0)
            .map(|f| f.1)
    }

    /// The end of the frame on the wire, whoever watches it.
    #[inline]
    pub fn next_frame_end(&self) -> Option<u64> {
        self.wire.front().map(|f| f.1)
    }

    /// When the `i`th frame end from now (1-based) comes, if the wire stays
    /// busy that long: the frames on it, then more of the same length back
    /// to back (as a DMA keeping the FIFO full gives).
    pub fn frame_end(&self, i: usize) -> Option<u64> {
        let last = self.wire.back()?.1;
        match i.checked_sub(1).and_then(|k| self.wire.get(k)) {
            Some(f) => Some(f.1),
            None => Some(last + (i - self.wire.len()) as u64 * self.frame_cycles()?),
        }
    }

    /// Some interrupt is enabled.
    pub fn interrupts_enabled(&self) -> bool {
        self.imsc != 0
    }

    /// What the rest of the chip sees of the controller without reading a
    /// register, as bits: the interrupt line ([`OUT_IRQ`]) and the TX
    /// ([`OUT_DREQ_TX`]) and RX ([`OUT_DREQ_RX`]) DMA requests.
    pub fn outputs(&self) -> u8 {
        self.irq() as u8 | (self.dreq_tx() as u8) << 1 | (self.dreq_rx() as u8) << 2
    }

    /// The outputs that can be set at all.
    #[inline]
    fn enabled_outputs(&self) -> u8 {
        (self.imsc != 0) as u8
            | ((self.dmacr & 2 != 0) as u8) << 1
            | ((self.dmacr & 1 != 0) as u8) << 2
    }

    fn receive(&mut self, reply: u16) {
        if self.rx.len() < FIFO_DEPTH {
            self.rx.push_back(reply);
        } else {
            self.latched |= INT_ROR;
        }
    }

    /// Hand every queued frame to the target at cycle `now`, receiving one
    /// back for each; on a timed wire the replies land as the frames end.
    pub fn pump(&mut self, bus: &mut dyn SpiTarget, now: u64) {
        if !self.wire.is_empty() {
            self.run_wire(now);
        }
        self.cycles = self.cycles.max(now);
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
            match self.frame_cycles() {
                Some(f) => {
                    let start = self
                        .wire
                        .back()
                        .map_or(self.cycles, |w| w.1.max(self.cycles));
                    self.wire.push_back((start, start + f, reply));
                }
                None => self.receive(reply),
            }
        }
    }

    fn ris(&self) -> u32 {
        let mut r = self.latched & !INT_LEVEL;
        if self.rx.len() >= FIFO_DEPTH / 2 {
            r |= INT_RX;
        }
        if self.tx_level() <= FIFO_DEPTH / 2 {
            r |= INT_TX;
        }
        r
    }

    /// Level of the combined interrupt line.
    pub fn irq(&self) -> bool {
        self.imsc != 0 && self.ris() & self.imsc != 0
    }

    /// DMA request levels (SSPDMACR enables them): TX while the FIFO has
    /// room, RX while it holds data.
    pub fn dreq_tx(&self) -> bool {
        self.dmacr & 2 != 0 && self.tx_level() < FIFO_DEPTH
    }
    pub fn dreq_rx(&self) -> bool {
        self.dmacr & 1 != 0 && !self.rx.is_empty()
    }

    fn status(&self) -> u32 {
        let tx = self.tx_level();
        let busy = !self.wire.is_empty() || self.has_work();
        ((tx == 0) as u32)
            | ((tx < FIFO_DEPTH) as u32) << 1
            | (!self.rx.is_empty() as u32) << 2
            | ((self.rx.len() == FIFO_DEPTH) as u32) << 3
            | (busy as u32) << 4
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
                if self.tx_level() < FIFO_DEPTH {
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

    const OUT_ALL: u8 = OUT_IRQ | OUT_DREQ_TX | OUT_DREQ_RX;

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
        s.pump(&mut w, 0);
        assert_eq!(w.seen, [(0xA5, 8), (0x01, 8)]);
        assert_eq!(s.read(0x00C), 0b0111);
        assert_eq!((s.read(0x008), s.read(0x008)), (0x5A, 0xFE));
        assert_eq!(s.read(0x00C), 0b0011);
        assert_eq!(s.read(0x008), 0, "RX underrun reads zero");
    }

    #[test]
    fn frames_take_the_wire_time() {
        // CPSDVSR = 2, SCR = 0, 8 bits at clk_peri = clk_sys: 16 cycles each.
        let mut s = enabled(7);
        s.write(0x010, 2);
        s.write(0x024, 1); // RX DMA request: frame ends are seen
        let mut w = Wire::default();
        for b in 1..=3 {
            s.write(0x008, b);
        }
        s.pump(&mut w, 100);
        assert_eq!(w.seen.len(), 3, "the target sees every frame at once");
        // Two frames wait behind the one on the wire; nothing has come back.
        assert_eq!(s.read(0x00C), 0b1_0010, "BSY, TX room, TX not empty");
        assert_eq!(s.next_event(OUT_ALL), Some(116));
        assert!(!s.update(115, OUT_ALL));
        assert!(s.update(116, OUT_ALL));
        assert_eq!(s.read(0x00C) & 0b0100, 0b0100, "first reply in");
        assert_eq!(s.read(0x008), !1 & 0xFF);
        s.update(148, OUT_ALL);
        assert_eq!(s.read(0x00C), 0b0111, "idle: TX empty, both replies in");
        assert_eq!(s.next_event(OUT_ALL), None);
        // A pump later starts on an idle wire from its own time.
        s.write(0x008, 4);
        s.pump(&mut w, 1000);
        assert_eq!(s.next_event(OUT_ALL), Some(1016));
    }

    #[test]
    fn unwatched_frames_end_unannounced() {
        // No interrupt or DMA request enabled: nothing sees a frame end but
        // a register read, so it isn't an event, yet reads find it ended.
        let mut s = enabled(7);
        s.write(0x010, 2);
        let mut w = Wire::default();
        s.write(0x008, 1);
        s.write(0x008, 2);
        s.pump(&mut w, 100);
        assert_eq!(s.next_event(OUT_ALL), None);
        assert!(!s.update(116, OUT_ALL), "a frame ended, unseen");
        assert_eq!(s.read(0x00C), 0b1_0111, "BSY, TX empty, first reply in");
        assert!(!s.update(132, OUT_ALL));
        assert_eq!(s.read(0x00C), 0b0111, "idle");
        // Enabling the RX interrupt makes the next frame end an event.
        s.write(0x014, INT_RX);
        s.write(0x008, 3);
        s.pump(&mut w, 200);
        assert_eq!(s.next_event(OUT_ALL), Some(216));
        // An enabled DMA request no channel waits on isn't watched.
        s.write(0x014, 0);
        s.write(0x024, 3);
        assert_eq!(s.next_event(OUT_IRQ), None);
        assert_eq!(s.next_event(OUT_DREQ_RX), Some(216));
        assert!(!s.update(216, OUT_IRQ));
    }

    #[test]
    fn the_tx_fifo_counts_frames_not_yet_on_the_wire() {
        let mut s = enabled(7);
        s.write(0x010, 254); // slow: 2032 cycles a frame
        let mut w = Wire::default();
        for b in 0..12 {
            s.write(0x008, b);
            s.pump(&mut w, 0);
        }
        // One shifting, eight in the FIFO: the rest were dropped.
        assert_eq!(w.seen.len(), 9);
        assert_eq!(s.read(0x00C) & 0b0011, 0, "TX full and not empty");
        s.write(0x024, 2);
        assert!(!s.dreq_tx(), "no DMA request while full");
        s.update(2032, OUT_ALL);
        assert!(s.dreq_tx(), "a frame started, so there is room");
    }

    #[test]
    fn the_divisors_divide_clk_peri() {
        let mut s = enabled(7 | 1 << 8); // SCR = 1
        s.write(0x010, 2);
        s.set_clk_peri(48_000_000);
        s.write(0x024, 2);
        s.write(0x008, 0);
        s.pump(&mut Wire::default(), 0);
        // 8 bits x 2 x 2 = 32 clocks of 48 MHz: 83.3 cycles of 125 MHz.
        assert_eq!(s.next_event(OUT_ALL), Some(83));
    }

    #[test]
    fn frame_sizes() {
        let mut s = enabled(15); // 16-bit
        let mut w = Wire::default();
        s.write(0x008, 0xBEEF);
        s.pump(&mut w, 0);
        assert_eq!((w.seen[0], s.read(0x008)), ((0xBEEF, 16), 0x4110));
        let mut s = enabled(3); // 4-bit
        s.write(0x008, 0xAB);
        s.pump(&mut w, 0);
        assert_eq!((w.seen[1], s.read(0x008)), ((0xB, 4), 0x4));
        let mut s = enabled(1); // reserved size acts as 8 bits
        s.write(0x008, 0x77);
        s.pump(&mut w, 0);
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
        s.pump(&mut w, 0);
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
        s.pump(&mut w, 0);
        assert_eq!(
            w.seen.len(),
            FIFO_DEPTH,
            "frames beyond the depth were dropped"
        );
        assert_eq!(s.read(0x00C) & 0b1000, 0b1000, "RX full");
        assert_eq!(s.read(0x018) & INT_ROR, 0);
        // One more frame with a full RX FIFO: overrun, data lost.
        s.write(0x008, 0x55);
        s.pump(&mut w, 0);
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
        s.pump(&mut w, 0);
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
        s.pump(&mut w, 0);
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
        s.pump(&mut w, 0);
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
