//! PSM (power-on state machine): the registers firmware uses to force
//! processors and other blocks off and on again. Only core 1's bit (PROC1)
//! does anything: turning it off holds the core in reset, and turning it back
//! on restarts it in the bootrom, which is how `multicore_reset_core1` and
//! embassy's `spawn_core1` get a clean core before the launch handshake.
//! The rest of the register file just stores what is written.
use super::Peripheral;

pub const PROC1: u32 = 1 << 16;
/// Every block the PSM controls (bits 0..=16).
const ALL: u32 = 0x1_FFFF;

#[derive(Default)]
pub struct Psm {
    frce_on: u32,
    frce_off: u32,
    wdsel: u32,
    /// New state of PROC1's force-off bit when it changed since the system
    /// last looked (`true` = hold core 1 in reset).
    proc1_change: Option<bool>,
}

impl Psm {
    pub fn new() -> Self {
        Self::default()
    }

    /// Core 1's reset line changed: `Some(true)` = now held in reset,
    /// `Some(false)` = released (restart it). Reading clears the notice.
    pub fn take_proc1_change(&mut self) -> Option<bool> {
        self.proc1_change.take()
    }

    pub fn has_change(&self) -> bool {
        self.proc1_change.is_some()
    }
}

impl Peripheral for Psm {
    fn read(&mut self, off: u32) -> u32 {
        match off {
            0x0 => self.frce_on,
            0x4 => self.frce_off,
            0x8 => self.wdsel,
            // Everything comes out of reset instantly.
            0xC => ALL,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, val: u32) {
        match off {
            0x0 => self.frce_on = val & ALL,
            0x4 => {
                let was = self.frce_off & PROC1 != 0;
                self.frce_off = val & ALL;
                let now = self.frce_off & PROC1 != 0;
                if was != now {
                    self.proc1_change = Some(now);
                }
            }
            0x8 => self.wdsel = val & ALL,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc1_transitions_are_reported_once() {
        let mut p = Psm::new();
        assert!(!p.has_change());
        p.write(0x4, PROC1);
        assert_eq!(
            p.read(0x4),
            PROC1,
            "reads back, so firmware can wait for it"
        );
        assert_eq!(p.take_proc1_change(), Some(true));
        assert_eq!(p.take_proc1_change(), None);
        p.write(0x4, PROC1 | 1); // other bits don't matter, no transition
        assert_eq!(p.take_proc1_change(), None);
        p.write(0x4, 1);
        assert!(p.has_change());
        assert_eq!(p.take_proc1_change(), Some(false));
    }

    #[test]
    fn other_registers_store_and_done_is_always_complete() {
        let mut p = Psm::new();
        p.write(0x0, 0xFFFF_FFFF);
        p.write(0x8, 0x1234);
        assert_eq!((p.read(0x0), p.read(0x8), p.read(0xC)), (ALL, 0x1234, ALL));
        assert_eq!(p.take_proc1_change(), None);
    }
}
