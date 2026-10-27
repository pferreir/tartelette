//! Texas Instruments DAC8165, a quad 14-bit voltage DAC, on a bit-banged serial
//! link (the board drives it from a PIO machine, so the pins are watched
//! directly instead of sitting on an SPI controller).
//!
//! The 24-bit frame is shifted in MSB first on each falling clock edge while
//! the (active-low) sync line is low and acted on when sync rises:
//! `DB21:DB20` is the load mode, `DB18:DB17` the channel, `DB15:DB2` the
//! 14-bit code (`DB1:DB0` are ignored). Modes: `00` store in the channel's
//! input register, `01` store and update that channel's output, `10` store
//! and update all outputs from their input registers, `11` store the code in
//! every channel and update all. Frames with other bit patterns
//! (power-down, broadcast commands, reference control) are counted as
//! `unsupported` and otherwise ignored. A sync rising together with the last
//! clock edge counts that edge, as the firmware's PIO program produces it.
//!
//! Events: `dac` with value `channel << 16 | code` whenever an output
//! changes. [`Dac8165::millivolts`] gives `vref * code / 16384`.
use alloc::boxed::Box;
use pcb::{Device, Io, Kind, Param};

pub struct Dac8165 {
    sdi: usize,
    cs: usize,
    clk: usize,
    rst: Option<usize>,
    vref_mv: u32,
    /// Shift register and how many bits it holds.
    shift: u32,
    count: u32,
    /// Level of sync / clock before the change being processed.
    cs_high: bool,
    clk_high: bool,
    /// Per-channel input registers and output (DAC) registers, 14-bit codes.
    input: [u16; 4],
    output: [u16; 4],
    /// Frames received, and frames that were malformed or unsupported.
    pub frames: u32,
    pub unsupported: u32,
}

pub const KIND: Kind = Kind {
    name: "dac8165",
    summary: "quad 14-bit DAC on a bit-banged serial link",
    bus: None,
    params: &[
        Param::pin("sdi", "serial data in"),
        Param::pin("cs", "sync (chip select), active low"),
        Param::pin("clk", "serial clock"),
        Param::pin("rst", "reset, active low").optional(),
        Param::int("vref_mv", 1, 10_000, "reference voltage in millivolts: the full-scale output").default_int(2500),
    ],
    build: build_dac,
};

fn build_dac(p: &mut pcb::Params) -> Result<Box<dyn Device>, alloc::string::String> {
    Ok(Box::new(Dac8165 {
        sdi: p.pin("sdi"),
        cs: p.pin("cs"),
        clk: p.pin("clk"),
        rst: p.opt_pin("rst"),
        vref_mv: p.int("vref_mv") as u32,
        shift: 0,
        count: 0,
        cs_high: true,
        clk_high: false,
        input: [0; 4],
        output: [0; 4],
        frames: 0,
        unsupported: 0,
    }))
}

impl Dac8165 {
    /// 14-bit output code of channel `ch` (0 = A .. 3 = D).
    pub fn code(&self, ch: usize) -> u16 {
        self.output[ch]
    }

    /// Output voltage of channel `ch` for the configured reference.
    pub fn millivolts(&self, ch: usize) -> u32 {
        self.output[ch] as u32 * self.vref_mv / 16384
    }

    fn frame(&mut self, io: &mut Io) {
        let f = self.shift & 0xFF_FFFF;
        self.frames += 1;
        let (ld, sel, flags) = (f >> 20 & 3, (f >> 17 & 3) as usize, f >> 22 | (f >> 19 & 1) | (f >> 16 & 1));
        if flags != 0 {
            // Reserved bits set, broadcast (DB19) or power-down (DB16): not modelled.
            self.unsupported += 1;
            return;
        }
        let code = (f >> 2 & 0x3FFF) as u16;
        let before = self.output;
        match ld {
            0 => self.input[sel] = code,
            1 => {
                self.input[sel] = code;
                self.output[sel] = code;
            }
            2 => {
                self.input[sel] = code;
                self.output = self.input;
            }
            _ => {
                self.input = [code; 4];
                self.output = self.input;
            }
        }
        for ch in 0..4 {
            if self.output[ch] != before[ch] {
                io.emit("dac", (ch as i64) << 16 | self.output[ch] as i64);
            }
        }
    }
}

impl Device for Dac8165 {
    fn watched_pins(&self) -> u32 {
        1 << self.cs | 1 << self.clk | self.rst.map_or(0, |p| 1 << p)
    }

    fn attach(&mut self, io: &mut Io) {
        self.cs_high = io.level(self.cs);
        self.clk_high = io.level(self.clk);
    }

    fn pin_changed(&mut self, io: &mut Io, changed: u32) {
        let cs = io.level(self.cs);
        let clk = io.level(self.clk);
        if self.rst.is_some_and(|p| !io.level(p)) {
            self.input = [0; 4];
            self.output = [0; 4];
            self.count = 0;
        }
        // A falling clock edge shifts the data in while sync was low.
        if changed >> self.clk & 1 != 0 && self.clk_high && !clk && !self.cs_high {
            self.shift = self.shift << 1 | io.level(self.sdi) as u32;
            self.count += 1;
        }
        if changed >> self.cs & 1 != 0 {
            if cs && !self.cs_high {
                // Sync rising ends the frame.
                if self.count == 24 {
                    self.frame(io);
                } else if self.count != 0 {
                    self.frames += 1;
                    self.unsupported += 1;
                }
                self.count = 0;
            } else if !cs && self.cs_high {
                self.count = 0;
                self.shift = 0;
            }
        }
        self.cs_high = cs;
        self.clk_high = clk;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use crate::testutil::FakePins;
    use pcb::Board;

    const SDI: usize = 4;
    const CS: usize = 5;
    const CLK: usize = 6;
    const RST: usize = 7;

    struct Rig {
        board: Board,
        pins: FakePins,
    }

    impl Rig {
        fn new(extra: &str) -> Self {
            let sep = if extra.is_empty() { "" } else { ", " };
            let text = alloc::format!("parts.dac = {{ kind = \"dac8165\", sdi = {SDI}, cs = {CS}, clk = {CLK}, rst = {RST}{sep}{extra} }}");
            let mut board = Board::parse_with(&text, &crate::registry()).unwrap();
            let mut pins = FakePins::default();
            pins.set_chip(CS, true);
            pins.set_chip(RST, true);
            board.attach(&mut pins);
            Rig { board, pins }
        }
        fn set(&mut self, levels: &[(usize, bool)]) {
            for &(p, l) in levels {
                self.pins.set_chip(p, l);
            }
            let changed = self.pins.take_changes();
            self.board.service(&mut self.pins, changed, 0);
        }
        /// Clock out `frame` the way the firmware's PIO program does: CS low
        /// with the first data bit, then per bit CLK high, CLK low, next
        /// bit; the last CLK fall and the CS rise happen together.
        fn send(&mut self, frame: u32, bits: u32) {
            self.set(&[(CS, false), (SDI, frame >> (bits - 1) & 1 != 0)]);
            for i in (0..bits).rev() {
                self.set(&[(CLK, true)]);
                if i == 0 {
                    self.set(&[(CLK, false), (CS, true)]);
                } else {
                    self.set(&[(CLK, false)]);
                    self.set(&[(SDI, frame >> (i - 1) & 1 != 0)]);
                }
            }
        }
        fn dac(&self) -> &Dac8165 {
            self.board.device::<Dac8165>("dac").unwrap()
        }
    }

    /// The firmware's `to_dac_register`: single-channel update.
    fn word(ch: u32, volts: u16) -> u32 {
        0x10_0000 | ch << 17 | (volts & 0xFFFC) as u32
    }

    #[test]
    fn single_channel_updates_land_on_the_right_output() {
        let mut r = Rig::new("");
        r.send(word(0, 0x8000), 24);
        assert_eq!((r.dac().code(0), r.dac().code(1)), (0x2000, 0));
        r.send(word(1, 0xFFFC), 24);
        r.send(word(2, 0x1234), 24);
        r.send(word(3, 0x0004), 24);
        let codes: Vec<u16> = (0..4).map(|c| r.dac().code(c)).collect();
        assert_eq!(codes, [0x2000, 0x3FFF, 0x048D, 1]);
        assert_eq!(r.dac().frames, 5 - 1);
        let ev: Vec<(&str, i64)> = r.board.events.iter().map(|e| (e.label, e.value)).collect();
        assert_eq!(ev, [("dac", 0x2000), ("dac", 1 << 16 | 0x3FFF), ("dac", 2 << 16 | 0x048D), ("dac", 3 << 16 | 1)]);
        // An update that changes nothing is silent.
        r.send(word(3, 0x0004), 24);
        assert_eq!(r.board.events.len(), 4);
    }

    #[test]
    fn load_modes() {
        let mut r = Rig::new("");
        // 00: store only.
        r.send(1 << 17 | 0x4000, 24);
        assert_eq!(r.dac().code(1), 0);
        // 10: store and update everything from the input registers.
        r.send(0x20_0000 | 0x8000, 24);
        assert_eq!((r.dac().code(0), r.dac().code(1)), (0x2000, 0x1000));
        // 11: store the code in every channel and update all.
        r.send(0x30_0000 | 0xC000, 24);
        assert_eq!((0..4).map(|c| r.dac().code(c)).collect::<Vec<_>>(), [0x3000; 4]);
    }

    #[test]
    fn millivolts_follow_the_reference() {
        let mut r = Rig::new("vref_mv = 1250");
        r.send(word(0, 0x8000), 24);
        assert_eq!(r.dac().millivolts(0), 625);
        r.send(word(0, 0xFFFC), 24);
        assert_eq!(r.dac().millivolts(0), 1249);
        assert_eq!(r.dac().millivolts(1), 0);
    }

    #[test]
    fn malformed_and_unsupported_frames_are_ignored() {
        let mut r = Rig::new("");
        r.send(word(0, 0x8000), 23); // one clock short
        assert_eq!((r.dac().code(0), r.dac().unsupported), (0, 1));
        r.send(0x10_0000 | 1 << 19 | 0x8000, 24); // broadcast bit
        r.send(0x10_0000 | 1 << 16 | 0x8000, 24); // power-down bit
        r.send(0x40_0000 | 0x10_0000 | 0x8000, 24); // reserved bit
        assert_eq!((r.dac().code(0), r.dac().unsupported), (0, 4));
        // The next good frame still works.
        r.send(word(0, 0x8000), 24);
        assert_eq!(r.dac().code(0), 0x2000);
    }

    #[test]
    fn bits_only_count_with_sync_low_and_reset_clears_registers() {
        let mut r = Rig::new("");
        // Clock edges while sync is high are ignored.
        r.set(&[(CLK, true)]);
        r.set(&[(CLK, false)]);
        r.send(word(0, 0x8000), 24);
        assert_eq!(r.dac().code(0), 0x2000);
        r.set(&[(RST, false)]);
        assert_eq!(r.dac().code(0), 0);
    }

    #[test]
    fn factory_requires_the_serial_pins() {
        let reg = crate::registry();
        assert!(Board::parse_with("parts.d = { kind = \"dac8165\", sdi = 1, cs = 2 }", &reg).is_err());
        assert!(Board::parse_with("parts.d = { kind = \"dac8165\", sdi = 1, cs = 2, clk = 3 }", &reg).is_ok());
    }
}
