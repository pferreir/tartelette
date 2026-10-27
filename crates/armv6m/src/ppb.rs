//! Cortex-M0+ private peripheral bus: NVIC, SysTick and the SCB bits that
//! matter for interrupt handling. VTOR lives in the chip's system bus,
//! which copies it into the CPU.
//!
//! The core's view is mirrored in: `ipsr` (for ICSR.VECTACTIVE) and `lines`
//! (the level-sensitive device IRQ inputs), refreshed by the chip's system on every step.
//! Not modelled: NMI, VECTPENDING/ISRPENDING, SHCSR, SVCall pending.
const SYST_ENABLE: u32 = 1;
const SYST_TICKINT: u32 = 1 << 1;
const SYST_CLKSOURCE: u32 = 1 << 2; // 1 = processor clock, 0 = 1 MHz reference tick
const SYST_COUNTFLAG: u32 = 1 << 16;

const ICSR_PENDSVSET: u32 = 1 << 28;
const ICSR_PENDSVCLR: u32 = 1 << 27;
const ICSR_PENDSTSET: u32 = 1 << 26;
const ICSR_PENDSTCLR: u32 = 1 << 25;

use crate::cpu::{EXC_IRQ_BASE, EXC_PENDSV, EXC_SVCALL, EXC_SYSTICK};

#[derive(Debug)]
pub struct Ppb {
    /// Core clock cycles per 1 MHz SysTick reference tick.
    cycles_per_us: u64,
    /// Mirror of the core's IPSR.
    pub ipsr: u8,
    /// Mirror of the device IRQ input lines (bit n = IRQ n asserted).
    pub lines: u32,

    enabled: u32,
    /// IRQs pended by software (ISPR); device lines are OR-ed in on top.
    pending_sw: u32,
    /// 2-bit priority per IRQ (0 = most urgent).
    prio: [u8; 32],
    shpr2: u32,
    shpr3: u32,
    pendsv: bool,
    systick_pending: bool,
    scr: u32,

    syst_csr: u32,
    syst_rvr: u32,
    syst_cvr: u32,
    /// Core cycles not yet converted into 1 MHz reference ticks.
    ref_acc: u64,
}

impl Ppb {
    pub fn new(cycles_per_us: u64) -> Self {
        Self {
            cycles_per_us,
            ipsr: 0,
            lines: 0,
            enabled: 0,
            pending_sw: 0,
            prio: [0; 32],
            shpr2: 0,
            shpr3: 0,
            pendsv: false,
            systick_pending: false,
            scr: 0,
            syst_csr: 0,
            syst_rvr: 0,
            syst_cvr: 0,
            ref_acc: 0,
        }
    }

    // ---- interrupt selection -------------------------------------------

    fn irq_pending(&self) -> u32 {
        (self.pending_sw | self.lines) & self.enabled
    }

    fn priority(&self, exc: u8) -> u8 {
        match exc {
            EXC_SVCALL => (self.shpr2 >> 30) as u8,
            EXC_PENDSV => (self.shpr3 >> 22 & 3) as u8,
            EXC_SYSTICK => (self.shpr3 >> 30) as u8,
            e if e >= EXC_IRQ_BASE => self.prio[(e - EXC_IRQ_BASE) as usize],
            _ => 0,
        }
    }

    /// The exception to take now, if one pending exception is urgent enough
    /// to preempt the current execution level.
    pub fn pick(&self, ipsr: u8, primask: bool) -> Option<u8> {
        if primask {
            return None;
        }
        let irqs = self.irq_pending();
        if irqs == 0 && !self.pendsv && !self.systick_pending {
            return None;
        }
        // Thread mode runs below every configurable priority (0..=3).
        let current = if ipsr == 0 {
            4
        } else {
            self.priority_of_active(ipsr)
        };
        let mut best: Option<(u8, u8)> = None;
        let mut consider = |exc: u8| {
            let p = self.priority(exc);
            if p < current && best.is_none_or(|(bp, _)| p < bp) {
                best = Some((p, exc));
            }
        };
        if self.pendsv {
            consider(EXC_PENDSV);
        }
        if self.systick_pending {
            consider(EXC_SYSTICK);
        }
        for n in (0..32).filter(|n| irqs >> n & 1 != 0) {
            consider(EXC_IRQ_BASE + n as u8);
        }
        best.map(|(_, e)| e)
    }

    /// Priority of the running handler. HardFault (and anything else not
    /// configurable) is above all configurable exceptions.
    fn priority_of_active(&self, ipsr: u8) -> u8 {
        match ipsr {
            1..=3 => 0, // nothing configurable can preempt these; 0 is not < 0
            e => self.priority(e),
        }
    }

    /// Any pending exception that should end a WFI sleep. Wakeup ignores
    /// PRIMASK and priority, as on hardware.
    pub fn wakes_core(&self) -> bool {
        self.pendsv || self.systick_pending || self.irq_pending() != 0
    }

    /// The core is entering `exc`: clear its pending state.
    pub fn acknowledge(&mut self, exc: u8) {
        match exc {
            EXC_PENDSV => self.pendsv = false,
            EXC_SYSTICK => self.systick_pending = false,
            e if e >= EXC_IRQ_BASE => self.pending_sw &= !(1 << (e - EXC_IRQ_BASE)),
            _ => {}
        }
    }

    // ---- time ----------------------------------------------------------

    /// Advance SysTick by `cycles` core cycles.
    pub fn advance(&mut self, cycles: u64) {
        if self.syst_csr & SYST_ENABLE == 0 {
            return;
        }
        let ticks = if self.syst_csr & SYST_CLKSOURCE != 0 {
            cycles
        } else {
            self.ref_acc += cycles;
            let t = self.ref_acc / self.cycles_per_us;
            self.ref_acc %= self.cycles_per_us;
            t
        };
        self.tick(ticks);
    }

    fn tick(&mut self, mut n: u64) {
        loop {
            if n == 0 {
                return;
            }
            if self.syst_cvr == 0 {
                // The tick after reaching zero reloads the counter.
                if self.syst_rvr == 0 {
                    return;
                }
                self.syst_cvr = self.syst_rvr;
                n -= 1;
                continue;
            }
            if n < self.syst_cvr as u64 {
                self.syst_cvr -= n as u32;
                return;
            }
            n -= self.syst_cvr as u64;
            self.syst_cvr = 0;
            self.syst_csr |= SYST_COUNTFLAG;
            if self.syst_csr & SYST_TICKINT != 0 {
                self.systick_pending = true;
            }
            // Whole periods only repeat the (idempotent) wrap event.
            n %= self.syst_rvr as u64 + 1;
        }
    }

    /// Core cycles until SysTick next wraps, if it can ever raise an interrupt.
    pub fn cycles_to_event(&self) -> Option<u64> {
        if self.syst_csr & (SYST_ENABLE | SYST_TICKINT) != (SYST_ENABLE | SYST_TICKINT)
            || self.syst_rvr == 0
        {
            return None;
        }
        let ticks = if self.syst_cvr == 0 {
            self.syst_rvr as u64 + 1
        } else {
            self.syst_cvr as u64
        };
        Some(if self.syst_csr & SYST_CLKSOURCE != 0 {
            ticks
        } else {
            (ticks * self.cycles_per_us).saturating_sub(self.ref_acc)
        })
    }

    // ---- registers (offset from PPB_BASE) --------------------------------

    pub fn read(&mut self, off: u32) -> u32 {
        match off {
            0xE010 => {
                let v = self.syst_csr;
                self.syst_csr &= !SYST_COUNTFLAG;
                v
            }
            0xE014 => self.syst_rvr,
            0xE018 => self.syst_cvr,
            0xE01C => 1 << 30, // CALIB: SKEW, no TENMS value provided
            0xE100 | 0xE180 => self.enabled,
            0xE200 | 0xE280 => self.pending_sw | self.lines,
            0xE400..=0xE41C => {
                let base = (off - 0xE400) as usize;
                (0..4).fold(0, |a, i| a | (self.prio[base + i] as u32) << (8 * i + 6))
            }
            0xED00 => 0x410C_C601, // CPUID: Cortex-M0+ r0p1
            0xED04 => {
                (self.pendsv as u32) << 28 | (self.systick_pending as u32) << 26 | self.ipsr as u32
            }
            0xED0C => 0xFA05_0000,
            0xED10 => self.scr,
            0xED14 => 0x204, // CCR: STKALIGN | UNALIGN_TRP
            0xED1C => self.shpr2,
            0xED20 => self.shpr3,
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u32, val: u32) {
        match off {
            0xE010 => {
                // Enabling resets the sub-tick phase so the first tick is a full µs.
                if val & SYST_ENABLE != 0 && self.syst_csr & SYST_ENABLE == 0 {
                    self.ref_acc = 0;
                }
                self.syst_csr = (self.syst_csr & SYST_COUNTFLAG) | (val & 7);
            }
            0xE014 => self.syst_rvr = val & 0xFF_FFFF,
            0xE018 => {
                self.syst_cvr = 0;
                self.syst_csr &= !SYST_COUNTFLAG;
            }
            0xE100 => self.enabled |= val,
            0xE180 => self.enabled &= !val,
            0xE200 => self.pending_sw |= val,
            0xE280 => self.pending_sw &= !val,
            0xE400..=0xE41C => {
                let base = (off - 0xE400) as usize;
                for i in 0..4 {
                    self.prio[base + i] = (val >> (8 * i + 6) & 3) as u8;
                }
            }
            0xED04 => {
                if val & ICSR_PENDSVSET != 0 {
                    self.pendsv = true;
                } else if val & ICSR_PENDSVCLR != 0 {
                    self.pendsv = false;
                }
                if val & ICSR_PENDSTSET != 0 {
                    self.systick_pending = true;
                } else if val & ICSR_PENDSTCLR != 0 {
                    self.systick_pending = false;
                }
            }
            0xED10 => self.scr = val & 0x16,
            0xED1C => self.shpr2 = val & 0xC000_0000,
            0xED20 => self.shpr3 = val & 0xC0C0_0000,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CYCLES_PER_US: u64 = 125;

    #[test]
    fn nvic_priorities_and_masking() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.write(0xE100, 0b11); // enable IRQ 0, 1
        p.write(0xE400, 2 << 6); // IRQ0 prio 2, IRQ1 prio 0
        p.write(0xE200, 0b11); // pend both
        assert_eq!(p.pick(0, true), None); // PRIMASK
        assert_eq!(p.pick(0, false), Some(EXC_IRQ_BASE + 1));
        assert_eq!(p.pick(EXC_IRQ_BASE + 1, false), None); // same prio can't preempt
        assert_eq!(p.pick(EXC_IRQ_BASE, false), Some(EXC_IRQ_BASE + 1)); // higher can
        p.acknowledge(EXC_IRQ_BASE + 1);
        assert_eq!(p.pick(0, false), Some(EXC_IRQ_BASE));
        p.write(0xE180, 1); // disable IRQ0
        assert_eq!(p.pick(0, false), None);
        assert!(!p.wakes_core());
    }

    #[test]
    fn level_lines_repend_until_cleared() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.write(0xE100, 1);
        p.lines = 1;
        assert_eq!(p.pick(0, false), Some(EXC_IRQ_BASE));
        p.acknowledge(EXC_IRQ_BASE);
        assert_eq!(p.pick(0, false), Some(EXC_IRQ_BASE)); // still asserted
        p.lines = 0;
        assert_eq!(p.pick(0, false), None);
    }

    #[test]
    fn pendsv_via_icsr() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.write(0xED04, ICSR_PENDSVSET);
        assert_eq!(p.read(0xED04) >> 28 & 1, 1);
        assert_eq!(p.pick(0, false), Some(EXC_PENDSV));
        p.write(0xED04, ICSR_PENDSVCLR);
        assert_eq!(p.pick(0, false), None);
    }

    #[test]
    fn systick_counts_and_wraps() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.write(0xE014, 99);
        p.write(0xE010, SYST_ENABLE | SYST_TICKINT | SYST_CLKSOURCE);
        p.advance(99);
        assert_eq!(p.read(0xE018), 1);
        assert_eq!(p.pick(0, false), None);
        p.advance(1);
        assert_eq!(p.pick(0, false), Some(EXC_SYSTICK));
        assert_eq!(p.read(0xE010) & SYST_COUNTFLAG, SYST_COUNTFLAG);
        assert_eq!(p.read(0xE010) & SYST_COUNTFLAG, 0); // read clears
        p.advance(1); // reload tick
        assert_eq!(p.read(0xE018), 99);
        assert_eq!(p.cycles_to_event(), Some(99));
    }

    #[test]
    fn systick_reference_clock_is_one_mhz() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.write(0xE014, 9);
        p.write(0xE010, SYST_ENABLE | SYST_TICKINT);
        p.advance(CYCLES_PER_US * 3 + 5);
        assert_eq!(p.read(0xE018), 7);
        assert_eq!(p.cycles_to_event(), Some(7 * CYCLES_PER_US - 5));
    }

    #[test]
    fn systick_reload_zero_never_fires_and_cvr_write_clears() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.write(0xE010, SYST_ENABLE | SYST_TICKINT | SYST_CLKSOURCE);
        p.advance(1000);
        assert_eq!(p.pick(0, false), None);
        assert_eq!(p.cycles_to_event(), None);
        p.write(0xE014, 5);
        p.advance(3); // reload, then 5 -> 4 -> 3
        assert_eq!(p.read(0xE018), 3);
        p.advance(3);
        assert_eq!(p.read(0xE010) & SYST_COUNTFLAG, SYST_COUNTFLAG);
        p.advance(1);
        p.write(0xE018, 0xDEAD); // any write zeroes the counter and COUNTFLAG
        assert_eq!(p.read(0xE018), 0);
        assert_eq!(p.read(0xE010) & SYST_COUNTFLAG, 0);
    }

    #[test]
    fn systick_without_tickint_sets_flag_but_not_pending() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.write(0xE014, 2);
        p.write(0xE010, SYST_ENABLE | SYST_CLKSOURCE);
        p.advance(10);
        assert_eq!(p.pick(0, false), None);
        assert_eq!(p.read(0xE010) & SYST_COUNTFLAG, SYST_COUNTFLAG);
        assert_eq!(p.cycles_to_event(), None);
    }

    #[test]
    fn systick_many_periods_in_one_advance() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.write(0xE014, 9);
        p.write(0xE010, SYST_ENABLE | SYST_TICKINT | SYST_CLKSOURCE);
        p.advance(10 * 1000 + 4); // 1000 whole periods plus 4 ticks
        assert_eq!(p.pick(0, false), Some(EXC_SYSTICK));
        assert_eq!(p.read(0xE018), 6);
    }

    #[test]
    fn systick_holds_when_disabled() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.write(0xE014, 50);
        p.advance(100);
        assert_eq!(p.read(0xE018), 0);
        p.write(0xE010, SYST_ENABLE | SYST_CLKSOURCE);
        p.write(0xE010, SYST_CLKSOURCE); // disable again; COUNTFLAG preserved, bits masked
        assert_eq!(p.read(0xE010), SYST_CLKSOURCE);
    }

    #[test]
    fn register_readback_and_masks() {
        let mut p = Ppb::new(CYCLES_PER_US);
        assert_eq!(p.read(0xED00), 0x410C_C601);
        assert_eq!(p.read(0xE01C), 1 << 30);
        p.write(0xE100, 0xF0);
        p.write(0xE100, 0x0F); // ISER is write-1-to-set
        assert_eq!(p.read(0xE100), 0xFF);
        assert_eq!(p.read(0xE180), 0xFF);
        p.write(0xE180, 0x0F);
        assert_eq!(p.read(0xE100), 0xF0);
        p.write(0xE200, 0b101);
        p.lines = 0b010;
        assert_eq!(p.read(0xE200), 0b111); // software pending OR device lines
        p.write(0xE280, 0b001);
        assert_eq!(p.read(0xE280), 0b110);
        p.write(0xE404, 0xFFFF_FFFF); // IPR1 -> IRQ 4..7, only bits 7:6 stick
        assert_eq!(p.read(0xE404), 0xC0C0_C0C0);
        p.write(0xE404, 0x4080_C000);
        assert_eq!(p.read(0xE404), 0x4080_C000);
        p.write(0xED1C, 0xFFFF_FFFF);
        p.write(0xED20, 0xFFFF_FFFF);
        assert_eq!((p.read(0xED1C), p.read(0xED20)), (0xC000_0000, 0xC0C0_0000));
        p.write(0xED10, 0xFFFF_FFFF);
        assert_eq!(p.read(0xED10), 0x16);
        assert_eq!(p.read(0xED0C), 0xFA05_0000);
        assert_eq!(p.read(0xED14), 0x204);
        assert_eq!(p.read(0xE000), 0); // unmapped
    }

    #[test]
    fn icsr_reports_pending_and_active() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.ipsr = 17;
        p.write(0xED04, ICSR_PENDSVSET | ICSR_PENDSTSET);
        assert_eq!(p.read(0xED04), 1 << 28 | 1 << 26 | 17);
        p.write(0xED04, ICSR_PENDSTCLR);
        assert_eq!(p.read(0xED04), 1 << 28 | 17);
    }

    #[test]
    fn priorities_of_system_handlers_decide_preemption() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.write(0xED20, 1 << 30 | 2 << 22); // SysTick prio 1, PendSV prio 2
        p.write(0xED04, ICSR_PENDSVSET | ICSR_PENDSTSET);
        assert_eq!(p.pick(0, false), Some(EXC_SYSTICK));
        assert_eq!(p.pick(EXC_PENDSV, false), Some(EXC_SYSTICK)); // SysTick (1) beats PendSV (2)
        assert_eq!(p.pick(EXC_SYSTICK, false), None); // PendSV can't preempt SysTick
        assert_eq!(p.pick(3, false), None); // nothing preempts HardFault
        assert!(p.wakes_core());
    }

    #[test]
    fn lower_exception_number_wins_a_priority_tie() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.write(0xE100, 0b110);
        p.write(0xE200, 0b110);
        assert_eq!(p.pick(0, false), Some(EXC_IRQ_BASE + 1));
        p.write(0xED04, ICSR_PENDSVSET);
        assert_eq!(p.pick(0, false), Some(EXC_PENDSV)); // 14 < 17
    }

    #[test]
    fn acknowledge_clears_pendsv_and_systick_and_ignores_fixed_exceptions() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.write(0xED04, ICSR_PENDSVSET | ICSR_PENDSTSET);
        p.acknowledge(EXC_PENDSV);
        assert_eq!(p.pick(0, false), Some(EXC_SYSTICK));
        p.acknowledge(EXC_SYSTICK);
        assert_eq!(p.pick(0, false), None);
        p.acknowledge(3); // HardFault has no pending state to clear
        p.acknowledge(EXC_SVCALL);
    }

    #[test]
    fn svcall_priority_comes_from_shpr2_and_reload_reads_back() {
        let mut p = Ppb::new(CYCLES_PER_US);
        p.write(0xED1C, 3 << 30);
        assert_eq!(p.priority(EXC_SVCALL), 3);
        p.write(0xE014, 0xFFFF_FFFF);
        assert_eq!(p.read(0xE014), 0xFF_FFFF); // 24-bit reload
        // Writes to unmodelled registers (and the VTOR slot, owned by the bus) are ignored.
        p.write(0xED08, 0xFFFF_FFFF);
        p.write(0xE000, 0xFFFF_FFFF);
        assert_eq!(p.priority(0), 0);
        assert_eq!(p.read(0xED08), 0);
    }
}
