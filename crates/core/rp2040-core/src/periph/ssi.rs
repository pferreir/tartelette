//! The XIP SSI (Synopsys DW_apb_ssi at 0x1800_0000) and the W25Q-style
//! flash chip behind it, as the bootrom's and pico-sdk's flash routines use
//! them: serial commands to read the unique ID or JEDEC ID, poll the status
//! register, program pages and erase sectors.
//!
//! XIP reads never come through here: the bus maps the flash array directly,
//! so the quad/continuous-read setup boot2 does is accepted and ignored
//! (those frames receive zeros).
//!
//! Frames written to DR0 wait in the TX FIFO until the next register read,
//! then go out together, instantly (BUSY is never seen set). That is what
//! keeps the chip selected across back-to-back writes when the SSI drives
//! chip select itself (boot2 queues a command and a dummy byte, then reads
//! both). Chip select can also be forced through the IO_QSPI SS override,
//! as the bootrom does; [`Ssi::set_cs_override`] follows it. DMA requests,
//! interrupts and erase/program timing are not modelled.
use alloc::{collections::VecDeque, vec::Vec};

const FIFO_DEPTH: usize = 16;

const CTRLR0: u32 = 0x00;
const CTRLR1: u32 = 0x04;
const SSIENR: u32 = 0x08;
const SER: u32 = 0x10;
const TXFLR: u32 = 0x20;
const RXFLR: u32 = 0x24;
const SR: u32 = 0x28;
const IDR: u32 = 0x58;
const SSI_VERSION_ID: u32 = 0x5C;
const DR0: u32 = 0x60;

// SR
const SR_TFNF: u32 = 1 << 1;
const SR_TFE: u32 = 1 << 2;
const SR_RFNE: u32 = 1 << 3;
const SR_RFF: u32 = 1 << 4;

// IO_QSPI GPIO_QSPI_SS_CTRL.OUTOVER
const OUTOVER_LOW: u32 = 2;
const OUTOVER_HIGH: u32 = 3;

/// TMOD: transmit and receive, transmit only, receive only, EEPROM read.
const TMOD_TX: u32 = 1;
const TMOD_RX: u32 = 2;
const TMOD_EEPROM: u32 = 3;

pub struct Ssi {
    regs: [u32; 0x40],
    tx: VecDeque<u32>,
    rx: VecDeque<u32>,
    /// Chip select forced by the IO_QSPI override: Some(true) low (selected),
    /// Some(false) high, None left to the SSI.
    forced: Option<bool>,
    pub flash: FlashChip,
}

impl Ssi {
    pub fn new() -> Self {
        Self {
            regs: [0; 0x40],
            tx: VecDeque::new(),
            rx: VecDeque::new(),
            forced: None,
            flash: FlashChip::new(),
        }
    }

    fn ctrlr0(&self) -> u32 {
        self.regs[(CTRLR0 >> 2) as usize]
    }

    fn enabled(&self) -> bool {
        self.regs[(SSIENR >> 2) as usize] & 1 != 0
    }

    /// Bytes per frame on the wire (DFS_32 + 1 bits, rounded up).
    fn frame_bytes(&self) -> u32 {
        match (self.ctrlr0() >> 16 & 0x1F) + 1 {
            1..=8 => 1,
            n => n.div_ceil(8),
        }
    }

    /// The IO_QSPI SS override changed (OUTOVER field value).
    pub fn set_cs_override(&mut self, outover: u32, mem: &mut Vec<u8>) {
        self.flush(mem);
        self.forced = match outover {
            OUTOVER_LOW => Some(true),
            OUTOVER_HIGH => Some(false),
            _ => None,
        };
        match self.forced {
            Some(true) => self.flash.select(),
            _ => self.flash.deselect(mem),
        }
    }

    pub fn read(&mut self, off: u32, mem: &mut Vec<u8>) -> u32 {
        self.flush(mem);
        match off {
            TXFLR => 0,
            RXFLR => self.rx.len() as u32,
            SR => {
                let mut sr = SR_TFNF | SR_TFE;
                if !self.rx.is_empty() {
                    sr |= SR_RFNE;
                }
                if self.rx.len() >= FIFO_DEPTH {
                    sr |= SR_RFF;
                }
                sr
            }
            IDR => 0x5153_5049,
            SSI_VERSION_ID => 0x3430_312A,
            // All 36 DR aliases read the RX FIFO.
            DR0..0xF0 => self.rx.pop_front().unwrap_or(0),
            _ => self.regs.get((off >> 2) as usize).copied().unwrap_or(0),
        }
    }

    pub fn write(&mut self, off: u32, val: u32, mem: &mut Vec<u8>) {
        match off {
            DR0..0xF0 => {
                if self.enabled() && self.tx.len() < FIFO_DEPTH {
                    self.tx.push_back(val);
                }
            }
            SSIENR => {
                self.flush(mem);
                if val & 1 == 0 {
                    self.tx.clear();
                    self.rx.clear();
                }
                self.regs[(off >> 2) as usize] = val;
            }
            _ => {
                if let Some(r) = self.regs.get_mut((off >> 2) as usize) {
                    *r = val;
                }
            }
        }
    }

    /// Send everything in the TX FIFO.
    fn flush(&mut self, mem: &mut Vec<u8>) {
        if self.tx.is_empty() {
            return;
        }
        let ctrlr0 = self.ctrlr0();
        let tmod = ctrlr0 >> 8 & 3;
        let standard = ctrlr0 >> 21 & 3 == 0;
        // With the override released, the SSI asserts the chip selects in
        // SER for as long as it has frames to send.
        let ssi_cs = self.forced.is_none() && self.regs[(SER >> 2) as usize] & 1 != 0;
        if ssi_cs && standard {
            self.flash.select();
        }
        let selected = standard && (self.forced == Some(true) || ssi_cs);
        let bytes = self.frame_bytes();
        while let Some(frame) = self.tx.pop_front() {
            if tmod == TMOD_RX {
                continue; // the write only starts the reception
            }
            let reply = if selected {
                self.flash.frame(frame, bytes, mem)
            } else {
                0
            };
            if tmod != TMOD_TX && tmod != TMOD_EEPROM {
                self.push_rx(reply);
            }
        }
        if tmod == TMOD_RX || tmod == TMOD_EEPROM {
            let frames = (self.regs[(CTRLR1 >> 2) as usize] & 0xFFFF) + 1;
            for _ in 0..frames {
                let reply = if selected {
                    self.flash.frame(0, bytes, mem)
                } else {
                    0
                };
                self.push_rx(reply);
            }
        }
        if ssi_cs && standard {
            self.flash.deselect(mem);
        }
    }

    fn push_rx(&mut self, v: u32) {
        if self.rx.len() < FIFO_DEPTH {
            self.rx.push_back(v);
        }
    }
}

impl Default for Ssi {
    fn default() -> Self {
        Self::new()
    }
}

/// A Winbond W25Q-style SPI NOR flash, standard (single-bit) commands only.
/// It works on the bus's flash array, which grows (erased) up to the
/// reported capacity when written.
pub struct FlashChip {
    /// What command 0x4B returns.
    pub unique_id: [u8; 8],
    selected: bool,
    cmd: Option<u8>,
    /// Bytes clocked since the command byte.
    pos: u32,
    addr: u32,
    /// Write enable latch.
    wel: bool,
}

impl FlashChip {
    pub fn new() -> Self {
        Self {
            unique_id: [0xE6, 0x61, 0x41, 0x04, 0x03, 0x2E, 0x45, 0x2B],
            selected: false,
            cmd: None,
            pos: 0,
            addr: 0,
            wel: false,
        }
    }

    /// log2 of the capacity: 2 MiB (a Pico's W25Q16JV), or more if the
    /// image loaded is bigger.
    fn capacity_log2(mem: &[u8]) -> u32 {
        mem.len().next_power_of_two().trailing_zeros().max(21)
    }

    fn select(&mut self) {
        if !self.selected {
            self.selected = true;
            self.cmd = None;
            self.pos = 0;
            self.addr = 0;
        }
    }

    /// Chip select went high: erases and status writes take effect.
    fn deselect(&mut self, mem: &mut Vec<u8>) {
        if !self.selected {
            return;
        }
        self.selected = false;
        let Some(cmd) = self.cmd.take() else { return };
        let size = match cmd {
            0x20 if self.pos >= 3 => 4 << 10,
            0x52 if self.pos >= 3 => 32 << 10,
            0xD8 if self.pos >= 3 => 64 << 10,
            0x60 | 0xC7 => 1 << Self::capacity_log2(mem),
            0x01 | 0x31 | 0x11 | 0x02 => 0,
            _ => return,
        };
        if self.wel && size != 0 {
            let start = (self.addr & ((1 << Self::capacity_log2(mem)) - 1)) as usize & !(size - 1);
            Self::grow(mem, start + size);
            mem[start..start + size].fill(0xFF);
        }
        self.wel = false;
    }

    fn grow(mem: &mut Vec<u8>, len: usize) {
        if mem.len() < len {
            mem.resize(len, 0xFF);
        }
    }

    /// One SSI frame of `bytes` bytes, MSB first.
    fn frame(&mut self, frame: u32, bytes: u32, mem: &mut Vec<u8>) -> u32 {
        (0..bytes).rev().fold(0, |acc, i| {
            acc << 8 | self.byte((frame >> (8 * i)) as u8, mem) as u32
        })
    }

    fn byte(&mut self, b: u8, mem: &mut Vec<u8>) -> u8 {
        let Some(cmd) = self.cmd else {
            self.cmd = Some(b);
            match b {
                0x06 => self.wel = true,
                0x04 => self.wel = false,
                _ => {}
            }
            return 0xFF;
        };
        self.pos += 1;
        let n = self.pos;
        let cap_mask = (1u32 << Self::capacity_log2(mem)) - 1;
        let addr_byte = |s: &mut Self| {
            s.addr = s.addr << 8 | b as u32;
            0xFF
        };
        match cmd {
            0x05 => (self.wel as u8) << 1, // status 1: BUSY never set
            0x35 => 0x02,                  // status 2: QE, as boot2 wants it
            0x15 => 0x00,
            0x9F => [0xEF, 0x40, Self::capacity_log2(mem) as u8][(n as usize - 1) % 3],
            0x4B if n <= 4 => 0xFF, // dummy bytes
            0x4B => self.unique_id[(n as usize - 5) % 8],
            0x90 if n <= 3 => addr_byte(self),
            0x90 => [0xEF, Self::capacity_log2(mem) as u8 - 7][(n as usize - 4) % 2],
            0xAB if n <= 3 => 0xFF,
            0xAB => Self::capacity_log2(mem) as u8 - 7,
            0x03 | 0x0B | 0x02 | 0x20 | 0x52 | 0xD8 if n <= 3 => addr_byte(self),
            0x0B if n == 4 => 0xFF, // dummy byte
            0x03 | 0x0B => {
                let a = (self.addr & cap_mask) as usize;
                self.addr = self.addr.wrapping_add(1);
                mem.get(a).copied().unwrap_or(0xFF)
            }
            0x02 => {
                if self.wel {
                    // Within the page: the low address byte wraps.
                    let a = self.addr & !0xFF | (self.addr + n - 4) & 0xFF;
                    let a = (a & cap_mask) as usize;
                    Self::grow(mem, a + 1);
                    mem[a] &= b;
                }
                0xFF
            }
            _ => 0xFF,
        }
    }
}

impl Default for FlashChip {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Configure for 8-bit standard SPI, TX+RX, and enable.
    fn ssi() -> (Ssi, Vec<u8>) {
        let mut s = Ssi::new();
        let mut mem = alloc::vec![0xFF; 2 << 20];
        s.write(CTRLR0, 7 << 16, &mut mem);
        s.write(SER, 1, &mut mem);
        s.write(SSIENR, 1, &mut mem);
        (s, mem)
    }

    /// A whole command with chip select forced low, as the bootrom does.
    fn cmd(s: &mut Ssi, mem: &mut Vec<u8>, tx: &[u8]) -> Vec<u8> {
        s.set_cs_override(OUTOVER_LOW, mem);
        let mut rx = Vec::new();
        for &b in tx {
            s.write(DR0, b as u32, mem);
            assert_eq!(s.read(RXFLR, mem), 1);
            rx.push(s.read(DR0, mem) as u8);
        }
        s.set_cs_override(OUTOVER_HIGH, mem);
        s.set_cs_override(0, mem);
        rx
    }

    #[test]
    fn reads_unique_and_jedec_id() {
        let (mut s, mut mem) = ssi();
        let rx = cmd(
            &mut s,
            &mut mem,
            &[0x4B, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        );
        assert_eq!(rx[5..], s.flash.unique_id);
        let rx = cmd(&mut s, &mut mem, &[0x9F, 0, 0, 0]);
        assert_eq!(rx[1..], [0xEF, 0x40, 0x15]);
    }

    #[test]
    fn programs_and_erases_only_after_write_enable() {
        let (mut s, mut mem) = ssi();
        cmd(&mut s, &mut mem, &[0x02, 0x00, 0x10, 0xFE, 0x12, 0x34]);
        assert_eq!(mem[0x10FE], 0xFF); // no WREN: ignored
        cmd(&mut s, &mut mem, &[0x06]);
        assert_eq!(cmd(&mut s, &mut mem, &[0x05, 0])[1], 0x02); // WEL
        cmd(
            &mut s,
            &mut mem,
            &[0x02, 0x00, 0x10, 0xFE, 0x12, 0x34, 0x56],
        );
        assert_eq!(mem[0x10FE..0x1100], [0x12, 0x34]);
        assert_eq!(mem[0x1000], 0x56); // wrapped within the page
        assert_eq!(cmd(&mut s, &mut mem, &[0x05, 0])[1], 0); // WEL cleared
        assert_eq!(
            cmd(&mut s, &mut mem, &[0x03, 0x00, 0x10, 0xFE, 0, 0])[4..],
            [0x12, 0x34]
        );
        cmd(&mut s, &mut mem, &[0x06]);
        cmd(&mut s, &mut mem, &[0x20, 0x00, 0x1F, 0xFF]);
        assert!(mem[0x1000..0x2000].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn ssi_driven_cs_spans_queued_frames() {
        // boot2's read_flash_sreg: two frames queued, then read back.
        let (mut s, mut mem) = ssi();
        s.write(DR0, 0x35, &mut mem);
        s.write(DR0, 0x35, &mut mem);
        assert_eq!(s.read(SR, &mut mem) & (SR_RFNE | SR_TFE), SR_RFNE | SR_TFE);
        assert_eq!(s.read(RXFLR, &mut mem), 2);
        s.read(DR0, &mut mem);
        assert_eq!(s.read(DR0, &mut mem), 0x02);
    }

    #[test]
    fn eeprom_mode_receives_ctrlr1_frames() {
        let (mut s, mut mem) = ssi();
        mem[0x100..0x108].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        s.write(SSIENR, 0, &mut mem);
        s.write(CTRLR0, 31 << 16 | TMOD_EEPROM << 8, &mut mem);
        s.write(CTRLR1, 1, &mut mem);
        s.write(SSIENR, 1, &mut mem);
        s.write(DR0, 0x0300_0100, &mut mem);
        assert_eq!(s.read(RXFLR, &mut mem), 2);
        assert_eq!(s.read(DR0, &mut mem), 0x0102_0304);
        assert_eq!(s.read(DR0, &mut mem), 0x0506_0708);
    }

    #[test]
    fn disabling_clears_fifos() {
        let (mut s, mut mem) = ssi();
        s.write(DR0, 0x05, &mut mem);
        s.write(SSIENR, 0, &mut mem);
        assert_eq!(s.read(RXFLR, &mut mem), 0);
    }
}
