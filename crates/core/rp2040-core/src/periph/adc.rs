//! ADC: the 12-bit SAR converter, its five-way input mux (AIN0..3 on
//! GPIO26..29, the temperature sensor on AIN4), round-robin sampling, the
//! pacing divider and the 4-entry result FIFO with its interrupt and DMA
//! request.
//!
//! A conversion takes 96 clk_adc cycles (2 µs at 48 MHz) and samples its
//! input at the start. The voltage comes from the board through the
//! sampler handed to [`Adc::update`]; the reference is a 3.3 V ADC_AVDD and
//! the converter is ideal (no noise, no offset, ERR never set). The
//! temperature sensor reads a constant 27 °C (0.706 V) while CS.TS_EN is
//! set, and 0 V otherwise.
use super::Peripheral;
use crate::system::CYCLES_PER_US;

const CS: u32 = 0x00;
const RESULT: u32 = 0x04;
const FCS: u32 = 0x08;
const FIFO: u32 = 0x0C;
const DIV: u32 = 0x10;
const INTR: u32 = 0x14;
const INTE: u32 = 0x18;
const INTF: u32 = 0x1C;
const INTS: u32 = 0x20;

const CS_EN: u32 = 1 << 0;
const CS_TS_EN: u32 = 1 << 1;
const CS_START_ONCE: u32 = 1 << 2;
const CS_START_MANY: u32 = 1 << 3;
const CS_READY: u32 = 1 << 8;
const CS_ERR_STICKY: u32 = 1 << 10;
const CS_AINSEL_SHIFT: u32 = 12;
const CS_AINSEL: u32 = 7 << CS_AINSEL_SHIFT;
const CS_RROBIN_SHIFT: u32 = 16;
const CS_RROBIN: u32 = 0x1F << CS_RROBIN_SHIFT;
/// The CS bits that are stored as written.
const CS_STORED: u32 = CS_EN | CS_TS_EN | CS_START_MANY | CS_AINSEL | CS_RROBIN;

const FCS_EN: u32 = 1 << 0;
const FCS_SHIFT: u32 = 1 << 1;
const FCS_ERR: u32 = 1 << 2;
const FCS_DREQ_EN: u32 = 1 << 3;
const FCS_EMPTY: u32 = 1 << 8;
const FCS_FULL: u32 = 1 << 9;
const FCS_UNDER: u32 = 1 << 10;
const FCS_OVER: u32 = 1 << 11;
const FCS_THRESH_SHIFT: u32 = 24;
const FCS_THRESH: u32 = 0xF << FCS_THRESH_SHIFT;
const FCS_STORED: u32 = FCS_EN | FCS_SHIFT | FCS_ERR | FCS_DREQ_EN | FCS_THRESH;

const FIFO_DEPTH: usize = 4;
/// clk_adc cycles per conversion.
const CONVERSION: u64 = 96;
/// The GPIO of AIN0.
pub const FIRST_PIN: usize = 26;
/// Full scale (ADC_AVDD).
pub const VREF: f32 = 3.3;
/// The temperature sensor's output at 27 °C (RP2040 datasheet 4.9.5).
const TEMP_SENSOR_V: f32 = 0.706;
/// clk_adc when nothing better is known: the SDK's 48 MHz from PLL_USB.
pub const DEFAULT_CLK_HZ: u64 = 48_000_000;
const CLK_SYS_HZ: u64 = CYCLES_PER_US * 1_000_000;

#[derive(Clone, Copy, Debug)]
struct Conversion {
    channel: u8,
    /// Core cycles at which it samples and at which its result is ready.
    start: u64,
    done: u64,
}

pub struct Adc {
    cs: u32,
    result: u16,
    fcs: u32,
    fifo: [u16; FIFO_DEPTH],
    level: usize,
    div: u32,
    inte: u32,
    intf: u32,
    /// The conversion under way or, while START_MANY paces them, the next
    /// one waiting for its slot.
    conv: Option<Conversion>,
    /// START_MANY with a divider: conversion `k` starts `k` periods after
    /// `origin`, so a fractional period doesn't drift.
    origin: u64,
    paced: u64,
    clk_hz: u64,
    /// Device time, set by the bus before each access.
    pub now: u64,
}

impl Adc {
    pub fn new() -> Self {
        Self {
            cs: 0,
            result: 0,
            fcs: 0,
            fifo: [0; FIFO_DEPTH],
            level: 0,
            div: 0,
            inte: 0,
            intf: 0,
            conv: None,
            origin: 0,
            paced: 0,
            clk_hz: DEFAULT_CLK_HZ,
            now: 0,
        }
    }

    /// Back to the reset state (the RESETS bit was asserted); clk_adc stays.
    pub fn reset(&mut self) {
        *self = Self {
            clk_hz: self.clk_hz,
            now: self.now,
            ..Self::new()
        };
    }

    /// clk_adc's frequency, from the CLOCKS block.
    pub fn set_clk_adc(&mut self, hz: u64) {
        self.clk_hz = hz.max(1);
    }

    /// Core cycles for `ticks` clk_adc cycles in units of 1/256.
    fn cycles(&self, ticks: u64) -> u64 {
        (ticks as u128 * CLK_SYS_HZ as u128 / (self.clk_hz as u128 * 256)) as u64
    }

    /// The pacing period in 1/256 clk_adc cycles: DIV's 1 + INT + FRAC/256,
    /// or back to back when DIV is 0 or shorter than a conversion.
    fn period(&self) -> u64 {
        let p = match self.div {
            0 => 0,
            d => 256 + d as u64,
        };
        p.max(CONVERSION * 256)
    }

    fn start(&mut self, at: u64) {
        let channel = (self.cs >> CS_AINSEL_SHIFT & 7) as u8;
        self.conv = Some(Conversion {
            channel,
            start: at,
            done: at + self.cycles(CONVERSION * 256),
        });
    }

    /// The core cycle at which the next result is ready.
    pub fn next_event(&self) -> Option<u64> {
        self.conv.map(|c| c.done)
    }

    /// Finish the conversions due by core cycle `to`. `sample(gpio, cycle)`
    /// is the voltage on an analog pin at the cycle its conversion samples
    /// it. Returns whether any finished.
    pub fn update(&mut self, to: u64, mut sample: impl FnMut(usize, u64) -> f32) -> bool {
        let mut any = false;
        while let Some(c) = self.conv.filter(|c| c.done <= to) {
            any = true;
            let v = match c.channel {
                0..=3 => sample(FIRST_PIN + c.channel as usize, c.start),
                4 if self.cs & CS_TS_EN != 0 => TEMP_SENSOR_V,
                _ => 0.0,
            };
            self.finish(code(v));
            self.conv = None;
            if self.cs & CS_START_MANY != 0 {
                self.paced += 1;
                let next = self.origin + self.cycles(self.paced * self.period());
                if next <= c.done {
                    // Back to back: restart the pacing from here.
                    (self.origin, self.paced) = (c.done, 0);
                    self.start(c.done);
                } else {
                    self.start(next);
                }
            }
        }
        any
    }

    fn finish(&mut self, value: u16) {
        self.result = value;
        if self.fcs & FCS_EN != 0 {
            let v = match self.fcs & FCS_SHIFT {
                0 => value,
                _ => value >> 4,
            };
            if self.level < FIFO_DEPTH {
                self.fifo[self.level] = v;
                self.level += 1;
            } else {
                self.fcs |= FCS_OVER;
            }
        }
        let rrobin = self.cs >> CS_RROBIN_SHIFT & 0x1F;
        if rrobin != 0 {
            let cur = self.cs >> CS_AINSEL_SHIFT & 7;
            let next = (1..=5)
                .map(|i| (cur + i) % 5)
                .find(|&ch| rrobin >> ch & 1 != 0)
                .unwrap_or(cur);
            self.cs = self.cs & !CS_AINSEL | next << CS_AINSEL_SHIFT;
        }
    }

    /// FCS.THRESH. At its reset value of 0 the interrupt and the DMA
    /// request are always asserted: embassy-rp's one-shot `read` enables
    /// the interrupt with the FIFO off and relies on it firing at once.
    fn thresh(&self) -> usize {
        (self.fcs >> FCS_THRESH_SHIFT & 0xF) as usize
    }

    fn intr(&self) -> u32 {
        (self.level >= self.thresh()) as u32
    }

    /// ADC_IRQ_FIFO.
    pub fn irq(&self) -> bool {
        (self.intr() | self.intf) & self.inte & 1 != 0
    }

    /// DREQ_ADC.
    pub fn dreq(&self) -> bool {
        self.fcs & FCS_DREQ_EN != 0 && self.level >= self.thresh()
    }

    fn ready(&self) -> bool {
        self.cs & CS_EN != 0 && self.conv.is_none_or(|c| self.now < c.start)
    }

    fn pop(&mut self) -> u16 {
        if self.level == 0 {
            self.fcs |= FCS_UNDER;
            return 0;
        }
        let v = self.fifo[0];
        self.fifo.copy_within(1.., 0);
        self.level -= 1;
        v
    }
}

/// The 12-bit code for `v` volts.
pub fn code(v: f32) -> u16 {
    // `as` saturates (and turns NaN into 0); round half up.
    ((v / VREF * 4096.0 + 0.5) as u32).min(4095) as u16
}

impl Default for Adc {
    fn default() -> Self {
        Self::new()
    }
}

impl Peripheral for Adc {
    fn read(&mut self, off: u32) -> u32 {
        match off {
            CS => self.cs | if self.ready() { CS_READY } else { 0 },
            RESULT => self.result as u32,
            FCS => {
                let level = self.level as u32;
                self.fcs
                    | level << 16
                    | if level == 0 { FCS_EMPTY } else { 0 }
                    | if self.level == FIFO_DEPTH {
                        FCS_FULL
                    } else {
                        0
                    }
            }
            FIFO => self.pop() as u32,
            DIV => self.div,
            INTR => self.intr(),
            INTE => self.inte,
            INTF => self.intf,
            INTS => (self.intr() | self.intf) & self.inte,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, val: u32) {
        match off {
            CS => {
                let was_many = self.cs & CS_START_MANY != 0;
                let sticky = self.cs & CS_ERR_STICKY & !val;
                self.cs = val & CS_STORED | sticky;
                if self.cs & CS_EN == 0 {
                    self.conv = None;
                    return;
                }
                let many = self.cs & CS_START_MANY != 0;
                if many && !was_many {
                    match self.conv {
                        Some(c) => (self.origin, self.paced) = (c.start, 0),
                        None => {
                            (self.origin, self.paced) = (self.now, 0);
                            self.start(self.now);
                        }
                    }
                } else if val & CS_START_ONCE != 0 && self.conv.is_none() {
                    self.start(self.now);
                }
            }
            FCS => {
                let cleared = val & (FCS_UNDER | FCS_OVER);
                self.fcs = val & FCS_STORED | self.fcs & (FCS_UNDER | FCS_OVER) & !cleared;
            }
            DIV => {
                self.div = val & 0xFF_FFFF;
                // Writing the divider restarts its count.
                if let Some(c) = self.conv {
                    (self.origin, self.paced) = (c.start, 0);
                }
            }
            INTE => self.inte = val & 1,
            INTF => self.intf = val & 1,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 96 clk_adc cycles at 48 MHz in core cycles.
    const CONV_CYCLES: u64 = 250;

    fn ain(ch: u32) -> u32 {
        CS_EN | ch << CS_AINSEL_SHIFT
    }

    fn run(adc: &mut Adc, to: u64, v: &[f32; 4]) -> bool {
        adc.update(to, |pin, _| v[pin - FIRST_PIN])
    }

    #[test]
    fn codes_follow_the_reference() {
        assert_eq!(code(0.0), 0);
        assert_eq!(code(-1.0), 0);
        assert_eq!(code(1.65), 2048);
        assert_eq!(code(3.3), 4095);
        assert_eq!(code(5.0), 4095);
        assert_eq!(code(f32::NAN), 0);
    }

    #[test]
    fn one_shot_takes_two_microseconds() {
        let mut a = Adc::new();
        a.write(CS, CS_EN);
        assert_ne!(a.read(CS) & CS_READY, 0);
        a.write(CS, ain(1) | CS_START_ONCE);
        assert_eq!(a.read(CS) & (CS_READY | CS_START_ONCE), 0);
        assert_eq!(a.next_event(), Some(CONV_CYCLES));
        assert!(!run(&mut a, CONV_CYCLES - 1, &[0.0, 1.0, 0.0, 0.0]));
        assert!(run(&mut a, CONV_CYCLES, &[0.0, 1.0, 0.0, 0.0]));
        a.now = CONV_CYCLES;
        assert_ne!(a.read(CS) & CS_READY, 0);
        assert_eq!(a.read(RESULT), code(1.0) as u32);
        assert_eq!(a.next_event(), None);
    }

    #[test]
    fn samples_at_the_start_of_the_conversion() {
        let mut a = Adc::new();
        a.now = 1000;
        a.write(CS, ain(0) | CS_START_ONCE);
        let mut at = 0;
        a.update(u64::MAX, |_, t| {
            at = t;
            0.0
        });
        assert_eq!(at, 1000);
    }

    #[test]
    fn temperature_sensor_needs_ts_en() {
        let mut a = Adc::new();
        a.write(CS, ain(4) | CS_START_ONCE);
        run(&mut a, u64::MAX, &[3.3; 4]);
        assert_eq!(a.read(RESULT), 0);
        a.write(CS, ain(4) | CS_TS_EN | CS_START_ONCE);
        run(&mut a, u64::MAX, &[3.3; 4]);
        assert_eq!(a.read(RESULT), 876);
    }

    #[test]
    fn round_robin_fills_the_fifo_and_raises_the_irq() {
        let mut a = Adc::new();
        a.write(FCS, FCS_EN | FCS_DREQ_EN | 4 << FCS_THRESH_SHIFT);
        a.write(INTE, 1);
        // AIN0, 2 and 3, starting from AIN2.
        a.write(CS, ain(2) | 0b1101 << CS_RROBIN_SHIFT | CS_START_MANY);
        let v = [0.5, 1.0, 1.5, 2.0];
        run(&mut a, 3 * CONV_CYCLES, &v);
        assert_eq!(a.read(FCS) >> 16 & 0xF, 3);
        assert!(!a.irq() && !a.dreq());
        run(&mut a, 4 * CONV_CYCLES, &v);
        assert!(a.irq() && a.dreq());
        assert_ne!(a.read(FCS) & FCS_FULL, 0);
        let got: [u32; 4] = core::array::from_fn(|_| a.read(FIFO));
        assert_eq!(got, [1.5, 2.0, 0.5, 1.5].map(|v| code(v) as u32));
        assert!(!a.irq());
        // A fifth result has nowhere to go once the FIFO is full again.
        run(&mut a, 9 * CONV_CYCLES, &v);
        assert_ne!(a.read(FCS) & FCS_OVER, 0);
        a.write(FCS, FCS_OVER);
        assert_eq!(a.read(FCS) & FCS_OVER, 0);
    }

    #[test]
    fn threshold_zero_raises_the_irq_on_an_empty_fifo() {
        let mut a = Adc::new();
        a.write(INTE, 1);
        assert!(a.irq());
        a.write(FCS, 1 << FCS_THRESH_SHIFT);
        assert!(!a.irq());
    }

    #[test]
    fn empty_fifo_reads_underflow() {
        let mut a = Adc::new();
        assert_eq!(a.read(FIFO), 0);
        assert_ne!(a.read(FCS) & (FCS_UNDER | FCS_EMPTY), 0);
    }

    #[test]
    fn shift_keeps_the_top_eight_bits() {
        let mut a = Adc::new();
        a.write(FCS, FCS_EN | FCS_SHIFT);
        a.write(CS, ain(0) | CS_START_ONCE);
        run(&mut a, u64::MAX, &[3.3, 0.0, 0.0, 0.0]);
        assert_eq!(a.read(FIFO), 0xFF);
        assert_eq!(a.read(RESULT), 0xFFF);
    }

    #[test]
    fn divider_paces_conversions_without_drift() {
        let mut a = Adc::new();
        a.write(FCS, FCS_EN);
        // 48 MHz / 1000 = 48 kS/s: a period of 999 + 1 clk_adc cycles,
        // 2604.1666 core cycles.
        a.write(DIV, 999 << 8);
        a.write(CS, ain(0) | CS_START_MANY);
        let mut starts = alloc::vec::Vec::new();
        a.update(1_000_000 + CONV_CYCLES, |_, t| {
            starts.push(t);
            0.0
        });
        assert_eq!(&starts[..4], &[0, 2604, 5208, 7812]);
        assert_eq!(starts[384], 1_000_000);
        // Stopping lets the conversion under way finish.
        a.write(CS, ain(0));
        assert!(a.next_event().is_some());
        a.update(u64::MAX, |_, _| 0.0);
        assert_eq!(a.next_event(), None);
    }

    #[test]
    fn disabling_aborts_the_conversion() {
        let mut a = Adc::new();
        a.write(CS, ain(0) | CS_START_ONCE);
        a.write(CS, 0);
        assert_eq!(a.next_event(), None);
        assert_eq!(a.read(CS) & CS_READY, 0);
    }
}
