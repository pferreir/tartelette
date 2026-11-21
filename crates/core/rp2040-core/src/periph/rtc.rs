//! RTC: a calendar clock. The firmware writes a date and time to SETUP_0/1,
//! loads it with CTRL.LOAD and starts it with CTRL.RTC_ENABLE; RTC_ACTIVE
//! follows enable at once (on the chip it takes a few clk_rtc cycles), and
//! RTC_1/RTC_0 read the date and time it has counted to since, from the
//! emulated clock. Alarms (RTC_IRQ) are not modelled: their registers are
//! stored and read back.
use super::Peripheral;
use crate::system::CYCLES_PER_US;

const CYCLES_PER_SEC: u64 = CYCLES_PER_US * 1_000_000;

const SETUP_0: u32 = 0x04;
const SETUP_1: u32 = 0x08;
const CTRL: u32 = 0x0C;
const RTC_1: u32 = 0x18;
const RTC_0: u32 = 0x1C;

const CTRL_ENABLE: u32 = 1 << 0;
const CTRL_ACTIVE: u32 = 1 << 1;
const CTRL_LOAD: u32 = 1 << 4;
const CTRL_FORCE_NOTLEAPYEAR: u32 = 1 << 8;

pub struct Rtc {
    regs: [u32; 0x30 / 4],
    /// Seconds since 1970-01-01 at `base_cycle` (the last load, or the
    /// last stop).
    base: i64,
    base_cycle: u64,
    running: bool,
    /// Device time, set by the bus before each access.
    pub now: u64,
}

impl Rtc {
    pub fn new() -> Self {
        Self {
            regs: [0; 0x30 / 4],
            base: 0,
            base_cycle: 0,
            running: false,
            now: 0,
        }
    }

    fn seconds(&self) -> i64 {
        if self.running {
            self.base + (self.now.saturating_sub(self.base_cycle) / CYCLES_PER_SEC) as i64
        } else {
            self.base
        }
    }

    fn load(&mut self) {
        let (s0, s1) = (
            self.regs[(SETUP_0 / 4) as usize],
            self.regs[(SETUP_1 / 4) as usize],
        );
        let (year, month, day) = (s0 >> 12 & 0xFFF, s0 >> 8 & 0xF, s0 & 0x1F);
        let (hour, min, sec) = (s1 >> 16 & 0x1F, s1 >> 8 & 0x3F, s1 & 0x3F);
        let days = days_from_civil(year as i64, month.max(1), day.max(1));
        self.base = days * 86400 + (hour * 3600 + min * 60 + sec) as i64;
        self.base_cycle = self.now;
    }
}

impl Default for Rtc {
    fn default() -> Self {
        Self::new()
    }
}

impl Peripheral for Rtc {
    fn read(&mut self, off: u32) -> u32 {
        match off {
            CTRL => {
                let c = self.regs[3] & (CTRL_ENABLE | CTRL_FORCE_NOTLEAPYEAR);
                c | if self.running { CTRL_ACTIVE } else { 0 }
            }
            RTC_1 | RTC_0 => {
                let t = self.seconds();
                let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400) as u32);
                if off == RTC_1 {
                    let (y, m, d) = civil_from_days(days);
                    (y as u32 & 0xFFF) << 12 | m << 8 | d
                } else {
                    // 1970-01-01 was a Thursday; Sunday is 0.
                    let dotw = (days + 4).rem_euclid(7) as u32;
                    dotw << 24 | (secs / 3600) << 16 | (secs / 60 % 60) << 8 | (secs % 60)
                }
            }
            o if (o as usize) < self.regs.len() * 4 => self.regs[(o / 4) as usize],
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, val: u32) {
        match off {
            CTRL => {
                if val & CTRL_LOAD != 0 {
                    self.load();
                }
                let enable = val & CTRL_ENABLE != 0;
                if enable != self.running {
                    self.base = self.seconds();
                    self.base_cycle = self.now;
                    self.running = enable;
                }
                self.regs[3] = val & !CTRL_LOAD;
            }
            RTC_1 | RTC_0 => {}
            o if (o as usize) < self.regs.len() * 4 => self.regs[(o / 4) as usize] = val,
            _ => {}
        }
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_load_enable_and_count() {
        let mut r = Rtc::new();
        // 2021-02-28 23:59:58, as pico-sdk's rtc_set_datetime writes it.
        r.write(SETUP_0, 2021 << 12 | 2 << 8 | 28);
        r.write(SETUP_1, 23 << 16 | 59 << 8 | 58);
        r.write(CTRL, CTRL_LOAD);
        assert_eq!(r.read(CTRL) & CTRL_ACTIVE, 0);
        r.write(CTRL, CTRL_ENABLE);
        assert_eq!(r.read(CTRL) & CTRL_ACTIVE, CTRL_ACTIVE);
        r.now = 3 * CYCLES_PER_SEC;
        assert_eq!(r.read(RTC_1), 2021 << 12 | 3 << 8 | 1);
        // 2021-03-01 was a Monday.
        assert_eq!(r.read(RTC_0), 1 << 24 | 1);
        // Stopped, it holds.
        r.write(CTRL, 0);
        r.now = 10 * CYCLES_PER_SEC;
        assert_eq!(r.read(RTC_0), 1 << 24 | 1);
    }

    #[test]
    fn leap_years_are_counted() {
        for (y, m, d) in [(2024, 2, 29), (2000, 2, 29), (1970, 1, 1), (2100, 3, 1)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
        assert_eq!(
            days_from_civil(2100, 3, 1) - days_from_civil(2100, 2, 28),
            1,
            "2100 is not a leap year"
        );
    }
}
