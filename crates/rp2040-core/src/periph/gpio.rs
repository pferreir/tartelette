//! GPIO: the pin matrix shared by SIO (GPIO_OUT/OE/IN), IO_BANK0 (function
//! select, overrides, interrupts) and PADS_BANK0 (pulls, input enable).
//!
//! The three register blocks live at different addresses but describe the
//! same 30 pins, so they are views over one [`Gpio`]. Everything is
//! evaluated synchronously: any register write or external change calls
//! [`Gpio::refresh`], which re-resolves every pin, latches edge events and
//! records which pins changed for the board layer.
//!
//! SIO (funcsel 5) and the two PIO blocks (funcsel 6 and 7) drive pins. Other
//! functions leave the pin undriven until the UART/SPI/I2C/PWM models feed
//! their outputs in, though the IO_BANK0 overrides can still force a pin.
use super::Peripheral;

pub const NUM_PINS: usize = 30;
pub const PIN_MASK: u32 = (1 << NUM_PINS) - 1;

pub const FUNCSEL_SIO: u32 = 5;
/// FUNCSEL of PIO0 and PIO1 (`FUNCSEL_PIO0 + block`).
pub const FUNCSEL_PIO0: u32 = 6;
pub const FUNCSEL_NULL: u32 = 31;

const PAD_PDE: u32 = 1 << 2;
const PAD_PUE: u32 = 1 << 3;
const PAD_IE: u32 = 1 << 6;
const PAD_OD: u32 = 1 << 7;
const PAD_RESET: u32 = 0x56; // schmitt, pull-down, 4 mA, input enabled
const PAD_MASK: u32 = 0xFF;

// FUNCSEL, OUTOVER, OEOVER, INOVER, IRQOVER
const CTRL_MASK: u32 = 0x1F | 3 << 8 | 3 << 12 | 3 << 16 | 3 << 28;
const CTRL_RESET: u32 = FUNCSEL_NULL;

/// Per-pin interrupt sources, 4 bits each: LEVEL_LOW, LEVEL_HIGH, EDGE_LOW, EDGE_HIGH.
const EDGE_BITS: u32 = 0xCCCC_CCCC;
const NIBBLES: u32 = 8; // pins per INTR/INTE/INTF/INTS register
/// Valid bits of each of the four interrupt registers (the last holds pins 24..29).
const INT_MASK: [u32; 4] = [!0, !0, !0, 0x00FF_FFFF];

/// Which processor an interrupt register set belongs to.
const PROC0: usize = 0;
const SETS: usize = 3; // proc0, proc1, dormant wake

pub struct Gpio {
    sio_out: u32,
    sio_oe: u32,
    /// What each PIO block drives onto the pads it owns (funcsel 6 / 7).
    pio_out: [u32; 2],
    pio_oe: [u32; 2],
    ctrl: [u32; NUM_PINS],
    pads: [u32; NUM_PINS],
    voltage_select: u32,
    /// Externally driven pins (bit set = something on the board drives it)
    /// and the level they are driven to.
    ext_drive: u32,
    ext_level: u32,
    /// Voltage on each pad, and the value the CPU/peripherals see (after
    /// input enable and INOVER).
    pad_level: u32,
    in_level: u32,
    /// Latched EDGE_LOW/EDGE_HIGH events, in INTR layout.
    edges: [u32; 4],
    inte: [[u32; 4]; SETS],
    intf: [[u32; 4]; SETS],
    /// Pads whose voltage changed since the board last looked.
    changed: u32,
}

/// What a pin's output stage is doing.
struct Drive {
    out_from_func: bool,
    oe_from_func: bool,
    out: bool,
    oe: bool,
}

impl Gpio {
    pub fn new() -> Self {
        let mut g = Self {
            sio_out: 0,
            sio_oe: 0,
            pio_out: [0; 2],
            pio_oe: [0; 2],
            ctrl: [CTRL_RESET; NUM_PINS],
            pads: [PAD_RESET; NUM_PINS],
            voltage_select: 0,
            ext_drive: 0,
            ext_level: 0,
            pad_level: 0,
            in_level: 0,
            edges: [0; 4],
            inte: [[0; 4]; SETS],
            intf: [[0; 4]; SETS],
            changed: 0,
        };
        g.refresh();
        g.edges = [0; 4];
        g.changed = 0;
        g
    }

    // ---- host / board side ----

    /// Drive `pin` from outside the chip (`Some(level)`), or release it
    /// (`None`, high impedance: pulls take over).
    pub fn set_external(&mut self, pin: usize, level: Option<bool>) {
        if pin >= NUM_PINS {
            return;
        }
        let bit = 1 << pin;
        match level {
            Some(l) => {
                self.ext_drive |= bit;
                self.ext_level = self.ext_level & !bit | (l as u32) << pin;
            }
            None => self.ext_drive &= !bit,
        }
        self.refresh();
    }

    /// Set what PIO block `block` drives (bit n = GPIO n); it only reaches
    /// pads whose function select is that PIO.
    pub fn set_pio_outputs(&mut self, block: usize, out: u32, oe: u32) {
        if self.pio_out[block] != out || self.pio_oe[block] != oe {
            (self.pio_out[block], self.pio_oe[block]) = (out, oe);
            self.refresh();
        }
    }

    /// The pad values as peripherals (PIO, ...) see them: after the input
    /// enable and the input override.
    pub fn pad_inputs(&self) -> u32 {
        self.in_level
    }

    /// Voltage on the pad (what a LED or probe on the board would see).
    pub fn pin_level(&self, pin: usize) -> bool {
        pin < NUM_PINS && self.pad_level >> pin & 1 != 0
    }

    /// Whether the chip itself is driving the pin.
    pub fn is_driven_by_chip(&self, pin: usize) -> bool {
        pin < NUM_PINS && self.drive(pin).oe && self.pads[pin] & PAD_OD == 0
    }

    /// Pads that changed voltage since the last call.
    pub fn take_changes(&mut self) -> u32 {
        core::mem::take(&mut self.changed)
    }

    pub fn has_changes(&self) -> bool {
        self.changed != 0
    }

    /// Level of the IO_IRQ_BANK0 line towards core 0.
    pub fn irq(&self) -> bool {
        self.irq_for(PROC0)
    }

    // ---- pin resolution ----

    fn drive(&self, pin: usize) -> Drive {
        let c = self.ctrl[pin];
        let bit = 1 << pin;
        let func = c & 0x1F;
        let (mut out_from_func, mut oe_from_func) = (false, false);
        if func == FUNCSEL_SIO {
            (out_from_func, oe_from_func) = (self.sio_out & bit != 0, self.sio_oe & bit != 0);
        } else if (FUNCSEL_PIO0..FUNCSEL_PIO0 + 2).contains(&func) {
            let block = (func - FUNCSEL_PIO0) as usize;
            (out_from_func, oe_from_func) = (
                self.pio_out[block] & bit != 0,
                self.pio_oe[block] & bit != 0,
            );
        }
        let out = match c >> 8 & 3 {
            0 => out_from_func,
            1 => !out_from_func,
            2 => false,
            _ => true,
        };
        let oe = match c >> 12 & 3 {
            0 => oe_from_func,
            1 => !oe_from_func,
            2 => false,
            _ => true,
        };
        Drive {
            out_from_func,
            oe_from_func,
            out,
            oe,
        }
    }

    /// Re-resolve every pin after any input changed.
    pub fn refresh(&mut self) {
        let (old_pad, old_in) = (self.pad_level, self.in_level);
        let (mut pad, mut inp) = (0u32, 0u32);
        for pin in 0..NUM_PINS {
            let bit = 1 << pin;
            let p = self.pads[pin];
            let d = self.drive(pin);
            let level = if d.oe && p & PAD_OD == 0 {
                d.out
            } else if self.ext_drive & bit != 0 {
                self.ext_level & bit != 0
            } else {
                match (p & PAD_PUE != 0, p & PAD_PDE != 0) {
                    (true, false) => true,
                    (false, true) => false,
                    // No pull, or both (bus keeper): the pin holds its last level.
                    _ => old_pad & bit != 0,
                }
            };
            pad |= (level as u32) << pin;
            let seen = p & PAD_IE != 0 && level;
            let seen = match self.ctrl[pin] >> 16 & 3 {
                0 => seen,
                1 => !seen,
                2 => false,
                _ => true,
            };
            inp |= (seen as u32) << pin;
        }
        self.pad_level = pad;
        self.in_level = inp;
        self.changed |= pad ^ old_pad;

        let flips = inp ^ old_in;
        if flips != 0 {
            for pin in 0..NUM_PINS {
                if flips >> pin & 1 != 0 {
                    let rising = inp >> pin & 1 != 0;
                    self.edges[pin / 8] |=
                        1 << ((pin as u32 % NIBBLES) * 4 + if rising { 3 } else { 2 });
                }
            }
        }
    }

    // ---- interrupt registers ----

    /// Raw interrupt state: live level bits plus latched edges.
    fn intr(&self, n: usize) -> u32 {
        let mut v = self.edges[n];
        for i in 0..NIBBLES as usize {
            let pin = n * 8 + i;
            if pin >= NUM_PINS {
                break;
            }
            v |= if self.in_level >> pin & 1 != 0 { 2 } else { 1 } << (i * 4);
        }
        v
    }

    /// Level of the IO_IRQ_BANK0 line as seen by core `core`: each core has
    /// its own interrupt enable/force/status set.
    pub fn irq_for(&self, core: usize) -> bool {
        (0..4).any(|n| self.ints(core, n) != 0)
    }

    fn ints(&self, set: usize, n: usize) -> u32 {
        if self.inte[set][n] == 0 {
            return 0;
        }
        (self.intr(n) | self.intf[set][n]) & self.inte[set][n]
    }

    // ---- IO_BANK0 ----

    pub fn io_read(&self, off: u32) -> u32 {
        match off {
            0..0xF0 => {
                let pin = (off / 8) as usize;
                if off % 8 == 4 {
                    return self.ctrl[pin];
                }
                let d = self.drive(pin);
                (d.out_from_func as u32) << 8
                    | (d.out as u32) << 9
                    | (d.oe_from_func as u32) << 12
                    | (d.oe as u32) << 13
                    | (self.pad_level >> pin & 1) << 17
                    | (self.in_level >> pin & 1) << 19
            }
            0xF0..0x100 => self.intr((off as usize - 0xF0) / 4),
            0x100..0x190 => {
                let rel = off as usize - 0x100;
                let (set, kind, n) = (rel / 0x30, rel % 0x30 / 0x10, rel % 0x10 / 4);
                match kind {
                    0 => self.inte[set][n],
                    1 => self.intf[set][n],
                    _ => self.ints(set, n),
                }
            }
            _ => 0,
        }
    }

    pub fn io_write(&mut self, off: u32, val: u32) {
        match off {
            0..0xF0 if off % 8 == 4 => self.ctrl[(off / 8) as usize] = val & CTRL_MASK,
            // Edge events are write-1-to-clear; level bits are read-only.
            0xF0..0x100 => self.edges[(off as usize - 0xF0) / 4] &= !(val & EDGE_BITS),
            0x100..0x190 => {
                let rel = off as usize - 0x100;
                let (set, kind, n) = (rel / 0x30, rel % 0x30 / 0x10, rel % 0x10 / 4);
                match kind {
                    0 => self.inte[set][n] = val & INT_MASK[n],
                    1 => self.intf[set][n] = val & INT_MASK[n],
                    _ => {}
                }
            }
            _ => {}
        }
        self.refresh();
    }

    // ---- PADS_BANK0 ----

    pub fn pads_read(&self, off: u32) -> u32 {
        match off {
            0 => self.voltage_select,
            4..=0x78 if (off / 4 - 1) < NUM_PINS as u32 => self.pads[(off / 4 - 1) as usize],
            _ => 0,
        }
    }

    pub fn pads_write(&mut self, off: u32, val: u32) {
        match off {
            0 => self.voltage_select = val & 1,
            4..=0x78 if (off / 4 - 1) < NUM_PINS as u32 => {
                self.pads[(off / 4 - 1) as usize] = val & PAD_MASK
            }
            _ => {}
        }
        self.refresh();
    }

    // ---- SIO GPIO registers ----

    /// Whether `off` (relative to SIO_BASE) is one of the GPIO registers.
    pub fn is_sio_gpio(off: u32) -> bool {
        (0x04..=0x2C).contains(&off)
    }

    pub fn sio_read(&self, off: u32) -> u32 {
        match off {
            0x04 => self.in_level,
            0x10 => self.sio_out,
            0x20 => self.sio_oe,
            _ => 0,
        }
    }

    pub fn sio_write(&mut self, off: u32, val: u32) {
        let val = val & PIN_MASK;
        let (reg, op) = match off {
            0x10..=0x1C => (&mut self.sio_out, off - 0x10),
            0x20..=0x2C => (&mut self.sio_oe, off - 0x20),
            _ => return,
        };
        match op {
            0x0 => *reg = val,
            0x4 => *reg |= val,
            0x8 => *reg &= !val,
            _ => *reg ^= val,
        }
        self.refresh();
    }
}

impl pcb::Pins for Gpio {
    fn level(&self, pin: usize) -> bool {
        self.pin_level(pin)
    }
    fn drive(&mut self, pin: usize, level: Option<bool>) {
        self.set_external(pin, level);
    }
}

const _: () = assert!(NUM_PINS == pcb::NUM_PINS);

impl Default for Gpio {
    fn default() -> Self {
        Self::new()
    }
}

/// IO_BANK0 register window.
pub struct IoBank0<'a>(pub &'a mut Gpio);
/// PADS_BANK0 register window.
pub struct PadsBank0<'a>(pub &'a mut Gpio);

impl Peripheral for IoBank0<'_> {
    fn read(&mut self, off: u32) -> u32 {
        self.0.io_read(off)
    }
    fn write(&mut self, off: u32, val: u32) {
        self.0.io_write(off, val)
    }
}

impl Peripheral for PadsBank0<'_> {
    fn read(&mut self, off: u32) -> u32 {
        self.0.pads_read(off)
    }
    fn write(&mut self, off: u32, val: u32) {
        self.0.pads_write(off, val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIO_IN: u32 = 0x04;
    const SIO_OUT: u32 = 0x10;
    const SIO_OE: u32 = 0x20;

    fn ctrl(pin: u32) -> u32 {
        8 * pin + 4
    }

    /// Route `pin` to SIO.
    fn sio_pin(g: &mut Gpio, pin: u32) {
        g.io_write(ctrl(pin), FUNCSEL_SIO);
    }

    #[test]
    fn reset_state_is_null_function_with_pull_down() {
        let mut g = Gpio::new();
        assert_eq!(g.io_read(ctrl(3)), FUNCSEL_NULL);
        assert_eq!(g.pads_read(4 + 4 * 3), 0x56);
        assert_eq!(g.sio_read(SIO_IN), 0);
        assert!(!g.has_changes() && !g.irq());
        assert_eq!(g.io_read(0xFC), 0x0011_1111); // every pin reads low: LEVEL_LOW
        g.sio_write(SIO_OUT, 1 << 3);
        g.sio_write(SIO_OE, 1 << 3);
        assert!(!g.pin_level(3)); // funcsel NULL: SIO cannot drive
    }

    #[test]
    fn sio_output_needs_funcsel_and_oe() {
        let mut g = Gpio::new();
        g.sio_write(SIO_OUT + 4, 1 << 7); // OUT_SET
        g.sio_write(SIO_OE + 4, 1 << 7);
        assert!(!g.pin_level(7));
        sio_pin(&mut g, 7);
        assert!(g.pin_level(7) && g.is_driven_by_chip(7));
        assert_eq!(g.sio_read(SIO_IN), 1 << 7);
        g.sio_write(SIO_OUT + 0xC, 1 << 7); // XOR
        assert!(!g.pin_level(7));
        g.sio_write(SIO_OUT + 8, 1 << 7); // CLR
        g.sio_write(SIO_OUT + 4, 1 << 7);
        g.sio_write(SIO_OE + 8, 1 << 7);
        assert!(!g.is_driven_by_chip(7));
        assert!(!g.pin_level(7)); // released: pull-down wins
        g.sio_write(SIO_OUT, !0);
        assert_eq!(g.sio_read(SIO_OUT), PIN_MASK);
    }

    #[test]
    fn pulls_external_drive_and_keeper() {
        let mut g = Gpio::new();
        g.pads_write(4 + 4 * 2, PAD_IE | PAD_PUE);
        assert!(g.pin_level(2));
        g.set_external(2, Some(false));
        assert!(!g.pin_level(2)); // a driver beats the pull-up
        g.set_external(2, None);
        assert!(g.pin_level(2));
        g.pads_write(4 + 4 * 2, PAD_IE); // floating: keeps its level
        assert!(g.pin_level(2));
        g.set_external(2, Some(false));
        g.set_external(2, None);
        assert!(!g.pin_level(2));
        g.pads_write(4 + 4 * 2, PAD_IE | PAD_PUE | PAD_PDE);
        assert!(!g.pin_level(2)); // both pulls: keeper
        g.set_external(99, Some(true)); // out of range: ignored
    }

    #[test]
    fn input_enable_and_inover() {
        let mut g = Gpio::new();
        g.pads_write(4 + 4 * 4, PAD_PUE | PAD_IE);
        assert_eq!(g.sio_read(SIO_IN) >> 4 & 1, 1);
        g.pads_write(4 + 4 * 4, PAD_PUE); // IE off: CPU sees 0, pad still high
        assert_eq!(g.sio_read(SIO_IN) >> 4 & 1, 0);
        assert!(g.pin_level(4));
        g.pads_write(4 + 4 * 4, PAD_PUE | PAD_IE);
        for (over, expect) in [(1, 0), (2, 0), (3, 1), (0, 1)] {
            g.io_write(ctrl(4), FUNCSEL_NULL | over << 16);
            assert_eq!(g.sio_read(SIO_IN) >> 4 & 1, expect, "INOVER {over}");
        }
    }

    #[test]
    fn output_and_oe_overrides_and_pad_od() {
        let mut g = Gpio::new();
        sio_pin(&mut g, 9);
        g.io_write(ctrl(9), FUNCSEL_SIO | 3 << 12 | 3 << 8); // OEOVER=enable, OUTOVER=high
        assert!(g.pin_level(9));
        g.io_write(ctrl(9), FUNCSEL_SIO | 3 << 12 | 2 << 8);
        assert!(!g.pin_level(9));
        g.io_write(ctrl(9), FUNCSEL_NULL | 3 << 12 | 3 << 8); // overrides work without SIO
        assert!(g.pin_level(9));
        g.pads_write(4 + 4 * 9, 0x56 | PAD_OD); // output disable
        assert!(!g.pin_level(9) && !g.is_driven_by_chip(9));
        g.pads_write(4 + 4 * 9, 0x56);
        g.io_write(ctrl(9), FUNCSEL_SIO | 1 << 8); // OUTOVER=invert, OEOVER normal (off)
        assert!(!g.is_driven_by_chip(9));
        g.sio_write(SIO_OE + 4, 1 << 9);
        assert!(g.pin_level(9)); // sio_out is 0, inverted
    }

    #[test]
    fn status_register_reports_the_output_stage() {
        let mut g = Gpio::new();
        sio_pin(&mut g, 1);
        g.sio_write(SIO_OUT + 4, 1 << 1);
        g.sio_write(SIO_OE + 4, 1 << 1);
        let s = g.io_read(8);
        assert_eq!(
            s & (1 << 8 | 1 << 9 | 1 << 12 | 1 << 13),
            1 << 8 | 1 << 9 | 1 << 12 | 1 << 13
        );
        assert_eq!(s >> 17 & 1, 1);
        assert_eq!(s >> 19 & 1, 1);
        g.io_write(ctrl(1), FUNCSEL_SIO | 2 << 8); // force output low
        let s = g.io_read(8);
        assert_eq!((s >> 8 & 1, s >> 9 & 1), (1, 0));
        g.io_write(8, 0xFFFF_FFFF); // STATUS is read-only
        assert_eq!(g.io_read(8), s);
    }

    #[test]
    fn edge_events_latch_and_clear() {
        let mut g = Gpio::new();
        sio_pin(&mut g, 10); // INTR1, nibble 2
        g.sio_write(SIO_OE + 4, 1 << 10);
        g.io_write(0x100 + 4, 0b1100 << 8); // INTE0.1: both edges of pin 10
        g.sio_write(SIO_OUT + 4, 1 << 10);
        assert_eq!(g.io_read(0xF4) >> 8 & 0xF, 0b1010); // EDGE_HIGH | LEVEL_HIGH
        assert!(g.irq());
        assert_eq!(g.io_read(0x120 + 4), 0b1000 << 8);
        g.io_write(0xF4, 0xFFFF_FFFF);
        assert_eq!(g.io_read(0xF4) >> 8 & 0xF, 0b0010); // level stays, edge gone
        assert!(!g.irq());
        g.sio_write(SIO_OUT + 8, 1 << 10);
        assert_eq!(g.io_read(0x120 + 4), 0b0100 << 8);
        assert!(g.irq());
    }

    #[test]
    fn level_interrupts_follow_the_pin_and_force_register_works() {
        let mut g = Gpio::new();
        g.pads_write(4 + 4 * 26, PAD_IE); // INTR3, nibble 2; floating, low
        g.io_write(0x100 + 12, 0b0010 << 8); // INTE3: LEVEL_HIGH
        assert!(!g.irq());
        g.set_external(26, Some(true));
        assert!(g.irq());
        g.set_external(26, Some(false));
        assert!(!g.irq());
        g.io_write(0x110 + 12, 0b0010 << 8); // INTF3 forces it
        assert!(g.irq());
        assert_eq!(g.io_read(0x120 + 12), 0b0010 << 8);
        g.io_write(0x110 + 12, 0xFFFF_FFFF);
        assert_eq!(g.io_read(0x110 + 12), 0x00FF_FFFF); // only pins 24..29 exist
        // proc1 and dormant sets are independent registers and never raise core 0's line.
        g.io_write(0x130, 0b0001); // proc1 INTE0: LEVEL_LOW on pin 0 (low)
        assert_eq!(g.io_read(0x150), 0b0001);
        g.io_write(0x160, 0b0001);
        assert_eq!(g.io_read(0x180), 0b0001);
        g.io_write(0x110 + 12, 0);
        assert!(!g.irq());
    }

    #[test]
    fn changes_are_reported_once() {
        let mut g = Gpio::new();
        sio_pin(&mut g, 25);
        g.sio_write(SIO_OE + 4, 1 << 25);
        assert!(!g.has_changes());
        g.sio_write(SIO_OUT + 4, 1 << 25);
        assert_eq!(g.take_changes(), 1 << 25);
        assert_eq!(g.take_changes(), 0);
        g.sio_write(SIO_OUT + 4, 1 << 25); // no change in level
        assert!(!g.has_changes());
    }

    #[test]
    fn pad_bank_registers_and_bounds() {
        let mut g = Gpio::new();
        g.pads_write(0, 0xFF);
        assert_eq!(g.pads_read(0), 1);
        g.pads_write(4 + 4 * 29, 0x1FF);
        assert_eq!(g.pads_read(4 + 4 * 29), 0xFF);
        assert_eq!(g.pads_read(4 + 4 * 30), 0); // not a GPIO pad
        g.pads_write(4 + 4 * 30, 0xFF);
        assert_eq!(g.pads_read(4 + 4 * 30), 0);
        // IO_BANK0: unknown offsets are inert.
        g.io_write(0x200, 1);
        assert_eq!(g.io_read(0x200), 0);
        assert!(
            Gpio::is_sio_gpio(0x04)
                && Gpio::is_sio_gpio(0x2C)
                && !Gpio::is_sio_gpio(0x00)
                && !Gpio::is_sio_gpio(0x30)
        );
    }

    #[test]
    fn pio_drives_only_pads_selected_to_it() {
        let mut g = Gpio::new();
        g.io_write(8 * 4 + 4, FUNCSEL_PIO0); // pin 4 -> PIO0
        g.io_write(8 * 5 + 4, FUNCSEL_PIO0 + 1); // pin 5 -> PIO1
        g.io_write(8 * 6 + 4, FUNCSEL_SIO); // pin 6 -> SIO, not PIO
        g.set_pio_outputs(0, 1 << 4 | 1 << 5 | 1 << 6, 1 << 4 | 1 << 5 | 1 << 6);
        assert!(g.pin_level(4), "PIO0 owns pin 4");
        assert!(
            !g.pin_level(5) && !g.pin_level(6),
            "wrong function: not driven"
        );
        g.set_pio_outputs(1, 1 << 4 | 1 << 5, 1 << 5);
        assert!(g.pin_level(5), "PIO1 owns pin 5");
        assert!(g.is_driven_by_chip(5) && g.is_driven_by_chip(4));
        // Output enable gates the output; pad_inputs sees the pad (input enabled by default).
        g.set_pio_outputs(0, 1 << 4, 0);
        assert!(!g.pin_level(4), "output disabled: the pull-down wins");
        assert_eq!(g.pad_inputs() >> 5 & 1, 1);
        // Pad changes are reported to the board layer.
        g.take_changes();
        g.set_pio_outputs(0, 1 << 4, 1 << 4);
        assert_eq!(g.take_changes(), 1 << 4);
    }
}
