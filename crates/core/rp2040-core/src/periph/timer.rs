//! TIMER: free-running 64-bit microsecond counter with four alarms.
//!
//! Time is derived from the core's cycle count (125 cycles = 1 µs), so it is
//! deterministic. The 1 MHz tick is assumed to be running (the SDK starts it
//! from the watchdog TICK block during init; that block is not modelled).
//! Alarm IRQs are TIMER_IRQ_0..3 = NVIC lines 0..3.
use super::Peripheral;
use crate::system::CYCLES_PER_US;

#[derive(Debug)]
pub struct Timer {
    /// Core cycle count as of the last [`Timer::update`].
    cycles: u64,
    /// Counter value at `base_cycle`.
    base_us: u64,
    base_cycle: u64,
    paused: bool,
    /// Low 32 bits of the counter at the last update (for alarm matching).
    last_lo: u32,
    /// TIMEHR latch (filled by a TIMELR read) and TIMEHW latch (for TIMELW).
    read_hi: u32,
    write_hi: u32,
    alarm: [u32; 4],
    armed: u32,
    raw: u32,
    inte: u32,
    intf: u32,
    dbgpause: u32,
}

impl Timer {
    pub fn new() -> Self {
        Self {
            cycles: 0,
            base_us: 0,
            base_cycle: 0,
            paused: false,
            last_lo: 0,
            read_hi: 0,
            write_hi: 0,
            alarm: [0; 4],
            armed: 0,
            raw: 0,
            inte: 0,
            intf: 0,
            dbgpause: 3,
        }
    }

    /// Current counter value in microseconds.
    pub fn now_us(&self) -> u64 {
        if self.paused {
            self.base_us
        } else {
            self.base_us + (self.cycles - self.base_cycle) / CYCLES_PER_US
        }
    }

    /// Move time forward to core cycle count `cycles` and fire due alarms.
    pub fn update(&mut self, cycles: u64) {
        self.cycles = cycles;
        let now = self.now_us() as u32;
        let elapsed = now.wrapping_sub(self.last_lo);
        for i in 0..4 {
            if self.armed >> i & 1 == 0 {
                continue;
            }
            // Hardware matches on equality; compare against the window of
            // values passed since the last update so none can be skipped.
            let due = self.alarm[i].wrapping_sub(self.last_lo);
            if due != 0 && due <= elapsed {
                self.armed &= !(1 << i);
                self.raw |= 1 << i;
            }
        }
        self.last_lo = now;
    }

    /// Asserted NVIC lines (bit n = TIMER_IRQ_n).
    pub fn irq_lines(&self) -> u32 {
        (self.raw | self.intf) & self.inte & 0xF
    }

    /// Core cycles until the next armed alarm fires.
    pub fn cycles_to_event(&self) -> Option<u64> {
        if self.paused {
            return None;
        }
        let now = self.now_us() as u32;
        let phase = (self.cycles - self.base_cycle) % CYCLES_PER_US;
        (0..4)
            .filter(|i| self.armed >> i & 1 != 0)
            .map(|i| {
                // An alarm equal to "now" has been missed: it waits for a wrap.
                let d = match self.alarm[i].wrapping_sub(now) {
                    0 => 1 << 32,
                    d => d as u64,
                };
                (d * CYCLES_PER_US).saturating_sub(phase)
            })
            .min()
    }

    fn set_time(&mut self, us: u64) {
        self.base_us = us;
        self.base_cycle = self.cycles;
        self.last_lo = us as u32;
    }

    fn set_paused(&mut self, paused: bool) {
        if paused != self.paused {
            self.base_us = self.now_us();
            self.base_cycle = self.cycles;
            self.paused = paused;
        }
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}

impl Peripheral for Timer {
    fn read(&mut self, off: u32) -> u32 {
        match off {
            0x08 => self.read_hi,
            0x0C => {
                let t = self.now_us();
                self.read_hi = (t >> 32) as u32;
                t as u32
            }
            0x10..=0x1C => self.alarm[((off - 0x10) >> 2) as usize],
            0x20 => self.armed,
            0x24 => (self.now_us() >> 32) as u32,
            0x28 => self.now_us() as u32,
            0x2C => self.dbgpause,
            0x30 => self.paused as u32,
            0x34 => self.raw,
            0x38 => self.inte,
            0x3C => self.intf,
            0x40 => (self.raw | self.intf) & self.inte,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, val: u32) {
        match off {
            0x00 => self.write_hi = val,
            0x04 => self.set_time((self.write_hi as u64) << 32 | val as u64),
            0x10..=0x1C => {
                let i = ((off - 0x10) >> 2) as usize;
                self.alarm[i] = val;
                self.armed |= 1 << i;
            }
            0x20 => self.armed &= !val,
            0x2C => self.dbgpause = val & 7,
            0x30 => self.set_paused(val & 1 != 0),
            0x34 => self.raw &= !val,
            0x38 => self.inte = val & 0xF,
            0x3C => self.intf = val & 0xF,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_microseconds_and_latches_high_word() {
        let mut t = Timer::new();
        t.update(125 * 1000 + 124);
        assert_eq!(t.read(0x0C), 1000);
        t.write(0x00, 1);
        t.write(0x04, 0xFFFF_FFFF); // 0x1_FFFF_FFFF µs
        t.update(t.cycles + 125);
        assert_eq!(t.read(0x0C), 0); // wrapped low word...
        assert_eq!(t.read(0x08), 2); // ...latched high word
    }

    #[test]
    fn alarm_fires_once_and_can_be_cleared() {
        let mut t = Timer::new();
        t.write(0x38, 1);
        t.write(0x10, 100);
        assert_eq!(t.cycles_to_event(), Some(100 * 125));
        t.update(99 * 125);
        assert_eq!(t.irq_lines(), 0);
        t.update(7000 * 125); // jumped well past: still fires
        assert_eq!(t.irq_lines(), 1);
        assert_eq!(t.read(0x20), 0); // disarmed
        assert_eq!(t.cycles_to_event(), None);
        t.write(0x34, 1);
        assert_eq!(t.irq_lines(), 0);
    }

    #[test]
    fn past_alarm_waits_for_wrap_and_pause_freezes() {
        let mut t = Timer::new();
        t.update(1000 * 125);
        t.write(0x14, 10); // already in the past
        t.update(2000 * 125);
        assert_eq!(t.read(0x34), 0);
        t.write(0x30, 1);
        t.update(9000 * 125);
        assert_eq!(t.read(0x28), 2000);
        assert_eq!(t.cycles_to_event(), None);
        t.write(0x30, 0);
        t.update(9000 * 125 + 125 * 5);
        assert_eq!(t.read(0x28), 2005);
    }

    #[test]
    fn register_readback_masks_and_write_one_to_clear() {
        let mut t = Timer::new();
        t.write(0x10, 111);
        t.write(0x1C, 444);
        assert_eq!((t.read(0x10), t.read(0x1C), t.read(0x14)), (111, 444, 0));
        assert_eq!(t.read(0x20), 0b1001); // ARMED
        t.write(0x20, 0b0001); // W1C
        assert_eq!(t.read(0x20), 0b1000);
        assert_eq!(t.read(0x2C), 3); // DBGPAUSE reset value
        t.write(0x2C, 0xFF);
        assert_eq!(t.read(0x2C), 7);
        t.write(0x38, 0xFF);
        t.write(0x3C, 0xFF);
        assert_eq!((t.read(0x38), t.read(0x3C)), (0xF, 0xF));
        assert_eq!(t.read(0x40), 0xF); // INTS = (raw | force) & enable
        assert_eq!(t.irq_lines(), 0xF);
        t.write(0x3C, 0b0010);
        t.write(0x38, 0b0110);
        assert_eq!(
            (t.read(0x34), t.read(0x40), t.irq_lines()),
            (0, 0b0010, 0b0010)
        );
        assert_eq!(t.read(0x44), 0); // unknown offset
        t.write(0x44, 1);
    }

    #[test]
    fn raw_high_word_and_pause_status() {
        let mut t = Timer::new();
        t.write(0x00, 7);
        t.write(0x04, 5);
        assert_eq!((t.read(0x24), t.read(0x28)), (7, 5));
        assert_eq!(t.read(0x30), 0);
        t.write(0x30, 1);
        assert_eq!(t.read(0x30), 1);
        t.write(0x30, 1); // pausing twice changes nothing
        assert_eq!(t.read(0x28), 5);
    }

    #[test]
    fn alarm_equal_to_now_waits_a_full_wrap() {
        let mut t = Timer::new();
        t.update(1000 * 125);
        t.write(0x10, 1000);
        assert_eq!(t.cycles_to_event(), Some((1u64 << 32) * 125));
        t.update(1000 * 125 + 125 * 50);
        assert_eq!(t.read(0x34), 0);
    }

    #[test]
    fn default_is_a_fresh_timer() {
        let t = Timer::default();
        assert_eq!(
            (t.now_us(), t.irq_lines(), t.cycles_to_event()),
            (0, 0, None)
        );
    }
}
