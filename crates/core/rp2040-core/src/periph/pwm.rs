//! PWM: eight slices, each a 16-bit counter behind a fractional clock
//! divider, with two outputs (A on even GPIOs, B on odd ones) that are high
//! while the counter is below their compare level. A slice wraps after TOP,
//! or counts back down to 0 in phase-correct mode, raising the wrap
//! interrupt and its DMA request (DREQ_PWM_WRAP0..7) each time.
//!
//! Nothing is stepped: each slice knows where its counter was at its last
//! tick, and [`Pwm::next_event`] works out the cycle of its next output
//! edge or wrap. The system brings the block up to each of those cycles
//! with [`Pwm::update`], so the edges reach the pins at their exact cycles
//! and a running slice costs a few events per period.
//!
//! As on the chip (RP2040 datasheet 4.5.2), CC and TOP are double-buffered:
//! a running slice takes the values written at its next wrap (in
//! phase-correct mode, when it gets back to 0); a stopped one at once.
//!
//! Not modelled: DIVMODE's gated and edge-counting modes (the counter stands
//! still in them; B is an input then, as on the chip), and the PH_RET and
//! PH_ADV strobes (ignored).
use super::Peripheral;

pub const SLICES: usize = 8;
const STRIDE: u32 = 0x14;
const CSR: u32 = 0x00;
const DIV: u32 = 0x04;
const CTR: u32 = 0x08;
const CC: u32 = 0x0C;
const TOP: u32 = 0x10;
const EN: u32 = 0xA0;
const INTR: u32 = 0xA4;
const INTE: u32 = 0xA8;
const INTF: u32 = 0xAC;
const INTS: u32 = 0xB0;

const CSR_EN: u32 = 1 << 0;
const CSR_PH_CORRECT: u32 = 1 << 1;
const CSR_A_INV: u32 = 1 << 2;
const CSR_B_INV: u32 = 1 << 3;
const CSR_DIVMODE: u32 = 3 << 4;
/// CSR bits kept as written (PH_RET and PH_ADV are strobes).
const CSR_STORED: u32 = 0x3F;
const DIV_MASK: u32 = 0xFFF;
const DIV_RESET: u32 = 1 << 4;
const TOP_RESET: u16 = 0xFFFF;

#[derive(Clone, Copy)]
struct Slice {
    csr: u32,
    div: u32,
    /// CC and TOP as written, and as the counter uses them.
    cc: u32,
    top: u16,
    cc_live: u32,
    top_live: u16,
    /// Where the counter is in its period: 0..period() ticks, which is the
    /// counter value itself except on the way down in phase-correct mode.
    pos: u32,
    /// The cycle of the tick that brought the counter to `pos`, and the
    /// divider's leftover sixteenths of a cycle at that tick.
    at: u64,
    frac: u32,
}

impl Slice {
    const fn new() -> Self {
        Slice {
            csr: 0,
            div: DIV_RESET,
            cc: 0,
            top: TOP_RESET,
            cc_live: 0,
            top_live: TOP_RESET,
            pos: 0,
            at: 0,
            frac: 0,
        }
    }

    fn running(&self) -> bool {
        self.csr & CSR_EN != 0 && self.csr & CSR_DIVMODE == 0
    }

    fn phase_correct(&self) -> bool {
        self.csr & CSR_PH_CORRECT != 0
    }

    /// Ticks from one wrap to the next.
    fn period(&self) -> u32 {
        (self.top_live as u32 + 1) << self.phase_correct() as u32
    }

    /// The counter's value at `pos`.
    fn counter(&self, pos: u32) -> u16 {
        let top = self.top_live as u32;
        match pos <= top {
            true => pos as u16,
            false => (2 * top + 1 - pos) as u16,
        }
    }

    /// The divider in sixteenths of a cycle; an integer part of 0 counts
    /// as 256.
    fn div16(&self) -> u64 {
        match self.div >> 4 {
            0 => 256 * 16 + (self.div & 15) as u64,
            _ => self.div as u64,
        }
    }

    /// The cycle of the `n`th tick after the one at `at`.
    fn tick_at(&self, n: u64) -> u64 {
        self.at + ((self.frac as u64 + n * self.div16()) >> 4)
    }

    /// Ticks from `at` up to and including cycle `now`.
    fn ticks_by(&self, now: u64) -> u64 {
        let span = (now.saturating_sub(self.at) + 1) * 16 - 1;
        span.saturating_sub(self.frac as u64) / self.div16()
    }

    /// Move the anchor `n` ticks on (none of them an event).
    fn skip(&mut self, n: u64) {
        let f = self.frac as u64 + n * self.div16();
        self.at += f >> 4;
        self.frac = (f & 15) as u32;
        self.pos = ((self.pos as u64 + n) % self.period() as u64) as u32;
    }

    /// The positions an output changes at: (fall, rise) for compare level
    /// `cc` (none when it is always high or always low).
    fn edges(&self, cc: u16) -> Option<(u32, u32)> {
        let top = self.top_live as u32;
        let cc = cc as u32;
        (cc != 0 && cc <= top).then(|| match self.phase_correct() {
            true => (cc, 2 * top + 2 - cc),
            false => (cc, 0),
        })
    }

    /// Ticks until the counter next reaches a position where an output
    /// changes or the slice wraps.
    fn ticks_to_event(&self) -> u32 {
        let period = self.period();
        let until = |p: u32| match (p + period - self.pos) % period {
            0 => period,
            d => d,
        };
        let mut d = until(0);
        for cc in [self.cc_live as u16, (self.cc_live >> 16) as u16] {
            if let Some((fall, rise)) = self.edges(cc) {
                d = d.min(until(fall)).min(until(rise));
            }
        }
        d
    }

    fn next_event(&self) -> Option<u64> {
        self.running()
            .then(|| self.tick_at(self.ticks_to_event() as u64))
    }

    /// Outputs A and B (bits 0 and 1), inversions applied.
    fn outputs(&self) -> u32 {
        let c = self.counter(self.pos);
        let a = (c < self.cc_live as u16) ^ (self.csr & CSR_A_INV != 0);
        let b = (c < (self.cc_live >> 16) as u16) ^ (self.csr & CSR_B_INV != 0);
        a as u32 | (b as u32) << 1
    }

    /// Take the buffered CC and TOP.
    fn latch(&mut self) {
        self.cc_live = self.cc;
        self.top_live = self.top;
    }
}

pub struct Pwm {
    slices: [Slice; SLICES],
    intr: u8,
    inte: u8,
    intf: u8,
    /// Wrap DMA requests not yet served, a bit per slice.
    dreq: u8,
    /// Each slice's A and B outputs (bit 2n is slice n's A).
    out: u16,
    /// The earliest cycle any slice has an event at.
    next: Option<u64>,
    /// Device time, set by the system before a register access.
    pub now: u64,
}

impl Default for Pwm {
    fn default() -> Self {
        Self::new()
    }
}

impl Pwm {
    pub const fn new() -> Self {
        Pwm {
            slices: [Slice::new(); SLICES],
            intr: 0,
            inte: 0,
            intf: 0,
            dreq: 0,
            out: 0,
            next: None,
            now: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// The cycle of the next output edge or wrap, if a slice is running.
    pub fn next_event(&self) -> Option<u64> {
        self.next
    }

    /// Level of the PWM_IRQ_WRAP line.
    pub fn irq(&self) -> bool {
        (self.intr | self.intf) & self.inte != 0
    }

    /// DREQ_PWM_WRAPn: raised by a wrap, until the DMA serves it.
    pub fn dreq(&self, slice: usize) -> bool {
        self.dreq >> slice & 1 != 0
    }

    /// The DMA moved an item for DREQ_PWM_WRAPn.
    pub fn served(&mut self, slice: usize) {
        self.dreq &= !(1 << slice);
    }

    /// What the outputs drive, a bit per GPIO: GPIO n is output (n & 1) of
    /// slice (n >> 1) & 7, so GPIO16 and up repeat GPIO0's slices.
    pub fn pin_levels(&self) -> u32 {
        let o = self.out as u32;
        o | (o & 0x3FFF) << 16
    }

    /// Which GPIOs the PWM drives when they select it: all but the B pins
    /// of slices that count on B (DIVMODE other than free-running).
    pub fn pin_enables(&self) -> u32 {
        let mut inputs = 0u32;
        for (n, s) in self.slices.iter().enumerate() {
            if s.csr & CSR_DIVMODE != 0 {
                inputs |= 2 << (2 * n);
            }
        }
        let oe = !inputs & 0xFFFF;
        oe | (oe & 0x3FFF) << 16
    }

    /// Run every slice up to cycle `to`, calling `edge` after each event
    /// with its cycle. Returns whether a slice wrapped (the interrupt or a
    /// DMA request may have risen).
    pub fn update(&mut self, to: u64, mut edge: impl FnMut(&Self, u64)) -> bool {
        let mut wrapped = false;
        while let Some(t) = self.next.filter(|&t| t <= to) {
            for n in 0..SLICES {
                let s = &mut self.slices[n];
                if s.next_event() != Some(t) {
                    continue;
                }
                let d = s.ticks_to_event();
                s.skip(d as u64);
                if s.pos == 0 {
                    s.latch();
                    self.intr |= 1 << n;
                    self.dreq |= 1 << n;
                    wrapped = true;
                }
                self.set_outputs(n);
            }
            self.find_next();
            edge(self, t);
        }
        wrapped
    }

    fn set_outputs(&mut self, n: usize) {
        let o = self.slices[n].outputs() as u16;
        self.out = self.out & !(3 << (2 * n)) | o << (2 * n);
    }

    fn find_next(&mut self) {
        self.next = self.slices.iter().filter_map(Slice::next_event).min();
    }

    /// Bring slice `n`'s counter up to `now` before a register changes how
    /// it counts.
    fn catch_up(&mut self, n: usize) {
        let now = self.now;
        let s = &mut self.slices[n];
        if s.running() {
            s.skip(s.ticks_by(now));
        }
    }

    fn write_slice(&mut self, n: usize, reg: u32, val: u32) {
        self.catch_up(n);
        let now = self.now;
        let s = &mut self.slices[n];
        let was_running = s.running();
        match reg {
            CSR => {
                s.csr = val & CSR_STORED;
                if s.pos >= s.period() {
                    s.pos = 0;
                }
            }
            DIV => s.div = val & DIV_MASK,
            CTR => {
                s.pos = val & 0xFFFF;
                s.at = now;
                s.frac = 0;
            }
            CC => s.cc = val,
            TOP => s.top = val as u16,
            _ => {}
        }
        if !s.running() {
            s.latch();
            if s.pos >= s.period() {
                s.pos = 0;
            }
        } else if !was_running {
            // Counting starts from here.
            (s.at, s.frac) = (now, 0);
        }
        self.set_outputs(n);
        self.find_next();
    }

    fn set_enables(&mut self, val: u32) {
        for n in 0..SLICES {
            let csr = self.slices[n].csr & !CSR_EN | (val >> n & 1);
            if csr != self.slices[n].csr {
                self.write_slice(n, CSR, csr);
            }
        }
    }
}

impl Peripheral for Pwm {
    fn read(&mut self, off: u32) -> u32 {
        match off {
            o if o < EN => {
                let (n, reg) = ((o / STRIDE) as usize, o % STRIDE);
                self.catch_up(n);
                let s = &self.slices[n];
                match reg {
                    CSR => s.csr,
                    DIV => s.div,
                    CTR => s.counter(s.pos) as u32,
                    CC => s.cc,
                    TOP => s.top as u32,
                    _ => 0,
                }
            }
            EN => self
                .slices
                .iter()
                .enumerate()
                .fold(0, |m, (n, s)| m | (s.csr & CSR_EN) << n),
            INTR => self.intr as u32,
            INTE => self.inte as u32,
            INTF => self.intf as u32,
            INTS => ((self.intr | self.intf) & self.inte) as u32,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, val: u32) {
        match off {
            o if o < EN => self.write_slice((o / STRIDE) as usize, o % STRIDE, val),
            EN => self.set_enables(val & 0xFF),
            INTR => self.intr &= !(val as u8),
            INTE => self.inte = val as u8,
            INTF => self.intf = val as u8,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    fn slice_reg(n: u32, reg: u32) -> u32 {
        n * STRIDE + reg
    }

    /// Run to `to`, collecting (cycle, slice 0's outputs) at each event.
    fn run(p: &mut Pwm, to: u64) -> Vec<(u64, u32)> {
        let mut v = Vec::new();
        p.update(to, |p, t| v.push((t, (p.out & 3) as u32)));
        v
    }

    #[test]
    fn free_running_edges_and_wraps() {
        let mut p = Pwm::new();
        p.write(slice_reg(0, TOP), 9);
        p.write(slice_reg(0, CC), 3 | 7 << 16);
        p.write(slice_reg(0, DIV), 2 << 4);
        assert_eq!(p.pin_levels() & 3, 0b11, "set at once while stopped");
        p.now = 100;
        p.write(slice_reg(0, CSR), CSR_EN);
        // A tick every 2 cycles: A falls at 3, B at 7, both rise at the wrap
        // (10 ticks = 20 cycles).
        let ev = run(&mut p, 160);
        assert_eq!(
            ev[..4],
            [(106, 0b10), (114, 0b00), (120, 0b11), (126, 0b10)]
        );
        assert_eq!(p.read(INTR), 1);
        assert!(p.dreq(0));
        p.served(0);
        assert!(!p.dreq(0));
        p.now = 161;
        assert_eq!(p.read(slice_reg(0, CTR)), 0, "160 is a wrap");
        p.now = 163;
        assert_eq!(p.read(slice_reg(0, CTR)), 1);
        // GPIO16 and 17 are slice 0 too.
        assert_eq!(p.pin_levels() >> 16 & 3, p.pin_levels() & 3);
    }

    #[test]
    fn compare_and_top_wait_for_the_wrap_while_running() {
        let mut p = Pwm::new();
        p.write(slice_reg(0, TOP), 9);
        p.write(slice_reg(0, CC), 5);
        p.write(slice_reg(0, CSR), CSR_EN);
        run(&mut p, 2);
        p.now = 2;
        p.write(slice_reg(0, CC), 2);
        p.write(slice_reg(0, TOP), 3);
        assert_eq!(p.read(slice_reg(0, CC)), 2);
        // This period ends as set: A falls at 5, the wrap at 10. Then the new
        // values: A falls at 12, wraps at 14.
        let ev: Vec<_> = run(&mut p, 20).into_iter().map(|e| e.0).collect();
        assert_eq!(ev, [5, 10, 12, 14, 16, 18, 20]);
    }

    #[test]
    fn phase_correct_counts_up_and_down() {
        let mut p = Pwm::new();
        p.write(slice_reg(0, TOP), 4);
        p.write(slice_reg(0, CC), 2);
        p.write(slice_reg(0, CSR), CSR_EN | CSR_PH_CORRECT);
        // 0 1 2 3 4 4 3 2 1 0 | 0 ...: high below 2, centred on the wrap.
        let ev = run(&mut p, 10);
        assert_eq!(ev, [(2, 0b00), (8, 0b01), (10, 0b01)]);
        let mut q = Pwm::new();
        q.write(slice_reg(0, TOP), 4);
        q.write(slice_reg(0, CSR), CSR_EN | CSR_PH_CORRECT);
        q.now = 6;
        assert_eq!(q.read(slice_reg(0, CTR)), 3, "on the way down");
    }

    #[test]
    fn fractional_divider_and_extremes() {
        let mut p = Pwm::new();
        // 2.5 cycles a tick: ticks at 2, 5, 7, 10, ...
        p.write(slice_reg(1, DIV), 2 << 4 | 8);
        p.write(slice_reg(1, TOP), 1);
        p.write(slice_reg(1, CC), 1 | 2 << 16);
        p.write(slice_reg(1, CSR), CSR_EN | CSR_B_INV);
        let mut cycles = Vec::new();
        p.update(10, |_, t| cycles.push(t));
        assert_eq!(cycles, [2, 5, 7, 10]);
        // B's level is above TOP: always high, inverted to always low.
        assert_eq!(p.out >> 2 & 3, 0b01);
        // A stopped slice has no events; enabling through EN starts it.
        let mut q = Pwm::new();
        assert_eq!(q.next_event(), None);
        q.write(EN, 1 << 3);
        assert_eq!(q.read(slice_reg(3, CSR)) & CSR_EN, CSR_EN);
        assert_eq!(
            q.next_event(),
            Some(65536),
            "the wrap, TOP 0xFFFF, CC 0: no edges"
        );
    }

    #[test]
    fn interrupt_and_counting_on_b() {
        let mut p = Pwm::new();
        p.write(slice_reg(2, TOP), 1);
        p.write(INTE, 1 << 2);
        p.write(slice_reg(2, CSR), CSR_EN);
        assert!(p.update(2, |_, _| {}));
        assert!(p.irq());
        p.write(INTR, 1 << 2);
        assert!(!p.irq());
        // Counting on B's input: no ticks, and B isn't driven.
        p.write(slice_reg(2, CSR), CSR_EN | 1 << 4);
        assert_eq!(p.next_event(), None);
        assert_eq!(p.pin_enables() >> 5 & 1, 0);
        assert_eq!(p.pin_enables() >> 4 & 1, 1);
    }
}
