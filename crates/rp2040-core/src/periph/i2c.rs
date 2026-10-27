//! Synopsys DesignWare I2C controller (RP2040 I2C0/I2C1), master mode only.
//!
//! The controller works at the level of whole bytes: each `IC_DATA_CMD`
//! entry in the TX FIFO becomes a START/byte/STOP step on the bus, handed to
//! an [`I2cTarget`] (the board's router) by [`I2c::pump`]. Transfers are
//! instantaneous: after the write to `IC_DATA_CMD` that queued it, a command
//! is done by the time the next instruction runs, so SCL timing registers
//! are stored but have no effect.
use alloc::collections::VecDeque;

use pcb::I2cTarget;

use super::Peripheral;

const FIFO_DEPTH: usize = 16;

// IC_CON
const CON_MASTER: u32 = 1 << 0;
const CON_RESTART_EN: u32 = 1 << 5;
// IC_DATA_CMD
const CMD_READ: u32 = 1 << 8;
const CMD_STOP: u32 = 1 << 9;
const CMD_RESTART: u32 = 1 << 10;
// Interrupt bits (same layout in RAW_INTR_STAT, INTR_MASK and INTR_STAT).
const INT_RX_UNDER: u32 = 1 << 0;
const INT_RX_OVER: u32 = 1 << 1;
const INT_RX_FULL: u32 = 1 << 2;
const INT_TX_OVER: u32 = 1 << 3;
const INT_TX_EMPTY: u32 = 1 << 4;
const INT_RD_REQ: u32 = 1 << 5;
const INT_TX_ABRT: u32 = 1 << 6;
const INT_RX_DONE: u32 = 1 << 7;
const INT_ACTIVITY: u32 = 1 << 8;
const INT_STOP_DET: u32 = 1 << 9;
const INT_START_DET: u32 = 1 << 10;
const INT_GEN_CALL: u32 = 1 << 11;
const INT_RESTART_DET: u32 = 1 << 12;
/// Bits that are a function of FIFO levels rather than latched events.
const INT_LEVEL: u32 = INT_RX_FULL | INT_TX_EMPTY;
const INT_ALL: u32 = 0x1FFF;
// IC_ENABLE
const EN_ENABLE: u32 = 1 << 0;
const EN_ABORT: u32 = 1 << 1;
const EN_TX_CMD_BLOCK: u32 = 1 << 2;
// IC_TX_ABRT_SOURCE
pub const ABRT_7B_ADDR_NOACK: u32 = 1 << 0;
pub const ABRT_TXDATA_NOACK: u32 = 1 << 3;
pub const ABRT_MASTER_DIS: u32 = 1 << 11;
pub const ABRT_USER_ABRT: u32 = 1 << 16;

pub struct I2c {
    con: u32,
    tar: u32,
    sar: u32,
    /// SS/FS/HS SCL high/low counts (0x14..=0x28), SDA hold, setup, spike len.
    timing: [u32; 6],
    sda_hold: u32,
    sda_setup: u32,
    fs_spklen: u32,
    ack_gc: u32,
    slv_data_nack_only: u32,
    rx_tl: u32,
    tx_tl: u32,
    enable: u32,
    intr_mask: u32,
    /// Latched (event) interrupt bits; level bits are derived on the fly.
    raw: u32,
    abort_source: u32,
    /// After a TX abort the TX FIFO stays flushed until IC_CLR_TX_ABRT is read.
    abort_held: bool,
    dma_cr: u32,
    dma_tdlr: u32,
    dma_rdlr: u32,
    tx: VecDeque<u32>,
    rx: VecDeque<u8>,
    /// A START has been issued and no STOP yet; `reading` is the direction.
    active: bool,
    reading: bool,
}

impl I2c {
    pub fn new() -> Self {
        Self {
            con: 0x7F,
            tar: 0x10,
            sar: 0x55,
            timing: [0x28, 0x2F, 0x06, 0x08, 0x06, 0x08],
            sda_hold: 1,
            sda_setup: 0x64,
            fs_spklen: 7,
            ack_gc: 0,
            slv_data_nack_only: 0,
            rx_tl: 0,
            tx_tl: 0,
            enable: 0,
            intr_mask: 0x8FF,
            raw: 0,
            abort_source: 0,
            abort_held: false,
            dma_cr: 0,
            dma_tdlr: 0,
            dma_rdlr: 0,
            tx: VecDeque::new(),
            rx: VecDeque::new(),
            active: false,
            reading: false,
        }
    }

    fn enabled(&self) -> bool {
        self.enable & EN_ENABLE != 0
    }

    /// Latched events plus the live FIFO-level bits.
    fn raw_status(&self) -> u32 {
        let mut r = self.raw & !INT_LEVEL;
        if self.rx.len() as u32 > self.rx_tl {
            r |= INT_RX_FULL;
        }
        if self.enabled() && self.tx.len() as u32 <= self.tx_tl {
            r |= INT_TX_EMPTY;
        }
        r
    }

    /// Level of the controller's interrupt line.
    pub fn irq(&self) -> bool {
        self.raw_status() & self.intr_mask != 0
    }

    /// DMA request levels (DMA_CR enables them): the TX side wants more
    /// commands, the RX side has data to collect.
    pub fn dreq_tx(&self) -> bool {
        self.dma_cr & 2 != 0 && self.enabled() && self.tx.len() as u32 <= self.dma_tdlr
    }
    pub fn dreq_rx(&self) -> bool {
        self.dma_cr & 1 != 0 && self.rx.len() as u32 > self.dma_rdlr
    }

    /// Commands are waiting to go out on the wire.
    pub fn has_work(&self) -> bool {
        !self.tx.is_empty()
            && self.enabled()
            && self.enable & EN_TX_CMD_BLOCK == 0
            && !self.abort_held
    }

    /// Run every queued command against `bus`.
    pub fn pump(&mut self, bus: &mut dyn I2cTarget) {
        while self.has_work() {
            let Some(cmd) = self.tx.pop_front() else {
                break;
            };
            self.run_command(cmd, bus);
        }
    }

    fn run_command(&mut self, cmd: u32, bus: &mut dyn I2cTarget) {
        if self.con & CON_MASTER == 0 {
            return self.abort(ABRT_MASTER_DIS, bus);
        }
        let read = cmd & CMD_READ != 0;
        let stop = cmd & CMD_STOP != 0;
        let addr = (self.tar & 0x7F) as u8;
        let restart_en = self.con & CON_RESTART_EN != 0;
        // A direction change (or an explicit RESTART) needs a repeated START,
        // or a STOP + START when restarts are disabled.
        let turnaround = self.active && (read != self.reading || cmd & CMD_RESTART != 0);
        if turnaround {
            if restart_en {
                self.raw |= INT_RESTART_DET;
            } else {
                bus.stop();
                self.raw |= INT_STOP_DET;
                self.active = false;
            }
        }
        if !self.active || turnaround {
            if !self.active {
                self.raw |= INT_START_DET | INT_ACTIVITY;
            }
            if !bus.start(addr, read) {
                self.active = true; // the START went out; abort() sends the STOP
                return self.abort(ABRT_7B_ADDR_NOACK, bus);
            }
            self.active = true;
            self.reading = read;
        }
        if read {
            let byte = bus.read(stop);
            if self.rx.len() < FIFO_DEPTH {
                self.rx.push_back(byte);
            } else {
                self.raw |= INT_RX_OVER;
            }
        } else if !bus.write(cmd as u8) {
            return self.abort(ABRT_TXDATA_NOACK, bus);
        }
        if stop {
            bus.stop();
            self.active = false;
            self.raw |= INT_STOP_DET;
        }
    }

    /// Abort the transfer: STOP on the wire, TX FIFO flushed and held.
    fn abort(&mut self, source: u32, bus: &mut dyn I2cTarget) {
        if self.active {
            bus.stop();
            self.active = false;
            self.raw |= INT_STOP_DET;
        }
        let flushed = self.tx.len() as u32;
        self.tx.clear();
        self.abort_source |= source | (flushed & 0x1FF) << 23;
        self.abort_held = true;
        self.raw |= INT_TX_ABRT;
    }

    fn status(&self) -> u32 {
        (self.active as u32)
            | ((self.tx.len() < FIFO_DEPTH) as u32) << 1
            | (self.tx.is_empty() as u32) << 2
            | (!self.rx.is_empty() as u32) << 3
            | ((self.rx.len() == FIFO_DEPTH) as u32) << 4
            | (self.active as u32) << 5
    }

    fn data_cmd_read(&mut self) -> u32 {
        match self.rx.pop_front() {
            Some(b) => b as u32,
            None => {
                self.raw |= INT_RX_UNDER;
                0
            }
        }
    }

    fn data_cmd_write(&mut self, val: u32) {
        if !self.enabled() || self.abort_held {
            return; // FIFO is held in reset
        }
        if self.tx.len() >= FIFO_DEPTH {
            self.raw |= INT_TX_OVER;
        } else {
            self.tx.push_back(val & 0x7FF);
        }
    }

    fn write_enable(&mut self, val: u32) {
        let was = self.enabled();
        self.enable = val & (EN_ENABLE | EN_TX_CMD_BLOCK);
        if !self.enabled() && was {
            self.tx.clear();
        }
        if val & EN_ABORT != 0 && self.enabled() && (self.active || !self.tx.is_empty()) {
            self.tx.clear();
            self.abort_source |= ABRT_USER_ABRT;
            self.abort_held = true;
            self.raw |= INT_TX_ABRT;
            // Without a target to talk to the STOP is implicit: the bus is idle again.
            self.active = false;
            self.raw |= INT_STOP_DET;
        }
    }
}

impl Default for I2c {
    fn default() -> Self {
        Self::new()
    }
}

impl Peripheral for I2c {
    fn read(&mut self, off: u32) -> u32 {
        let clear = |s: &mut Self, bits: u32| {
            s.raw &= !bits;
            0
        };
        match off {
            0x00 => self.con,
            0x04 => self.tar,
            0x08 => self.sar,
            0x10 => self.data_cmd_read(),
            0x14..=0x28 => self.timing[((off - 0x14) >> 2) as usize],
            0x2C => self.raw_status() & self.intr_mask,
            0x30 => self.intr_mask,
            0x34 => self.raw_status(),
            0x38 => self.rx_tl,
            0x3C => self.tx_tl,
            0x40 => clear(self, INT_ALL),
            0x44 => clear(self, INT_RX_UNDER),
            0x48 => clear(self, INT_RX_OVER),
            0x4C => clear(self, INT_TX_OVER),
            0x50 => clear(self, INT_RD_REQ),
            0x54 => {
                self.abort_source = 0;
                self.abort_held = false;
                clear(self, INT_TX_ABRT)
            }
            0x58 => clear(self, INT_RX_DONE),
            0x5C => clear(self, INT_ACTIVITY),
            0x60 => clear(self, INT_STOP_DET),
            0x64 => clear(self, INT_START_DET),
            0x68 => clear(self, INT_GEN_CALL),
            0x6C => self.enable,
            0x70 => self.status(),
            0x74 => self.tx.len() as u32,
            0x78 => self.rx.len() as u32,
            0x7C => self.sda_hold,
            0x80 => self.abort_source,
            0x84 => self.slv_data_nack_only,
            0x88 => self.dma_cr,
            0x8C => self.dma_tdlr,
            0x90 => self.dma_rdlr,
            0x94 => self.sda_setup,
            0x98 => self.ack_gc,
            0x9C => self.enabled() as u32,
            0xA0 => self.fs_spklen,
            0xA8 => clear(self, INT_RESTART_DET),
            0xF4 => 0x00FF_FF6E, // COMP_PARAM_1
            0xF8 => 0x3230_312A, // COMP_VERSION
            0xFC => 0x4457_0140, // COMP_TYPE
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, val: u32) {
        match off {
            // CON, TAR and the timing registers are only writable while disabled.
            0x00 if !self.enabled() => self.con = val & 0x7FF,
            0x04 if !self.enabled() => self.tar = val & 0xFFF,
            0x08 if !self.enabled() => self.sar = val & 0x3FF,
            0x10 => self.data_cmd_write(val),
            0x14..=0x28 if !self.enabled() => {
                self.timing[((off - 0x14) >> 2) as usize] = val & 0xFFFF
            }
            0x30 => self.intr_mask = val & INT_ALL,
            0x38 => self.rx_tl = val & 0xFF,
            0x3C => self.tx_tl = val & 0xFF,
            0x6C => self.write_enable(val),
            0x7C => self.sda_hold = val & 0xFF_FFFF,
            0x84 => self.slv_data_nack_only = val & 1,
            0x88 => self.dma_cr = val & 3,
            0x8C => self.dma_tdlr = val & 0xF,
            0x90 => self.dma_rdlr = val & 0xF,
            0x94 => self.sda_setup = val & 0xFF,
            0x98 => self.ack_gc = val & 1,
            0xA0 if !self.enabled() => self.fs_spklen = val & 0xFF,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    /// A fake bus recording what the controller does.
    #[derive(Default)]
    struct Wire {
        log: Vec<&'static str>,
        written: Vec<u8>,
        /// Addresses that ACK.
        present: Vec<u8>,
        nack_data: bool,
        next: u8,
    }

    impl I2cTarget for Wire {
        fn start(&mut self, addr: u8, read: bool) -> bool {
            self.log.push(if read { "SR" } else { "SW" });
            self.present.contains(&addr)
        }
        fn write(&mut self, byte: u8) -> bool {
            self.written.push(byte);
            !self.nack_data
        }
        fn read(&mut self, last: bool) -> u8 {
            self.log.push(if last { "R!" } else { "R" });
            self.next += 1;
            self.next
        }
        fn stop(&mut self) {
            self.log.push("P");
        }
    }

    fn setup(addr: u32) -> I2c {
        let mut i = I2c::new();
        i.write(0x00, 0x65); // master, fast, restart enabled, slave disabled
        i.write(0x04, addr);
        i.write(0x6C, 1);
        i
    }

    fn wire() -> Wire {
        Wire {
            present: alloc::vec![0x20],
            ..Default::default()
        }
    }

    #[test]
    fn write_then_read_with_restart() {
        let mut i = setup(0x20);
        let mut w = wire();
        i.write(0x10, 0x09); // register pointer
        i.write(0x10, CMD_READ | CMD_RESTART);
        i.write(0x10, CMD_READ | CMD_STOP);
        assert!(i.has_work() && i.read(0x74) == 3);
        i.pump(&mut w);
        assert_eq!(w.log, ["SW", "SR", "R", "R!", "P"]);
        assert_eq!(w.written, [0x09]);
        assert_eq!((i.read(0x78), i.read(0x10), i.read(0x10)), (2, 1, 2));
        assert_eq!(i.read(0x70) & 0x25, 0x04, "idle, TX FIFO empty"); // ACTIVITY/MST/TFE
        assert_ne!(i.read(0x34) & INT_STOP_DET, 0);
        i.read(0x60);
        assert_eq!(i.read(0x34) & INT_STOP_DET, 0);
    }

    #[test]
    fn address_nack_aborts_flushes_and_blocks_until_cleared() {
        let mut i = setup(0x21);
        let mut w = wire();
        for b in 1..=3 {
            i.write(0x10, b);
        }
        i.pump(&mut w);
        assert_eq!(w.log, ["SW", "P"]);
        assert!(w.written.is_empty());
        assert_eq!(i.read(0x74), 0, "TX FIFO flushed");
        let src = i.read(0x80);
        assert_eq!(src & 0x1FF, ABRT_7B_ADDR_NOACK);
        assert_eq!(src >> 23, 2, "two commands were still queued");
        assert_ne!(i.read(0x34) & INT_TX_ABRT, 0);
        // Held flushed: new commands are dropped until the abort is cleared.
        i.write(0x10, 0x55);
        assert_eq!(i.read(0x74), 0);
        i.read(0x54);
        assert_eq!((i.read(0x80), i.read(0x34) & INT_TX_ABRT), (0, 0));
        i.write(0x6C, 0); // TAR is only writable while disabled
        i.write(0x04, 0x20);
        i.write(0x6C, 1);
        i.write(0x10, 0x55 | CMD_STOP);
        i.pump(&mut w);
        assert_eq!(w.written, [0x55]);
    }

    #[test]
    fn data_nack_aborts() {
        let mut i = setup(0x20);
        let mut w = Wire {
            nack_data: true,
            ..wire()
        };
        i.write(0x10, 1);
        i.write(0x10, 2 | CMD_STOP);
        i.pump(&mut w);
        assert_eq!(i.read(0x80) & 0x1FF, ABRT_TXDATA_NOACK);
        assert_eq!(w.log, ["SW", "P"]);
    }

    #[test]
    fn interrupt_levels_follow_fifos() {
        let mut i = setup(0x20);
        let mut w = wire();
        i.write(0x30, INT_TX_EMPTY | INT_RX_FULL);
        assert!(i.irq(), "TX FIFO empty at threshold 0");
        i.write(0x10, 1);
        assert!(!i.irq());
        i.write(0x3C, 1);
        assert!(i.irq(), "level 1 <= threshold 1");
        i.write(0x3C, 0);
        i.pump(&mut w);
        assert!(i.irq());
        i.write(0x10, CMD_READ | CMD_STOP);
        i.pump(&mut w);
        assert_ne!(i.read(0x2C) & INT_RX_FULL, 0);
        i.write(0x30, INT_RX_FULL);
        i.read(0x10);
        assert!(!i.irq());
    }

    #[test]
    fn rx_under_over_and_tx_over() {
        let mut i = setup(0x20);
        let mut w = wire();
        assert_eq!(i.read(0x10), 0);
        assert_ne!(i.read(0x34) & INT_RX_UNDER, 0);
        for n in 0..FIFO_DEPTH + 1 {
            i.write(0x10, CMD_READ | if n == FIFO_DEPTH { CMD_STOP } else { 0 });
            if n % 4 == 3 {
                i.pump(&mut w);
            }
        }
        i.pump(&mut w);
        assert_ne!(i.read(0x34) & INT_RX_OVER, 0);
        assert_eq!(i.read(0x70) & 0x10, 0x10, "RX FIFO full");
        // TX overflow: 17 commands without pumping.
        let mut j = setup(0x20);
        for n in 0..FIFO_DEPTH + 1 {
            j.write(0x10, n as u32);
        }
        assert_eq!(
            (j.read(0x74), j.read(0x34) & INT_TX_OVER),
            (16, INT_TX_OVER)
        );
        assert_eq!(j.read(0x70) & 2, 0, "TX FIFO not-full flag clear");
    }

    #[test]
    fn config_is_locked_while_enabled_and_disable_flushes() {
        let mut i = setup(0x20);
        i.write(0x04, 0x30);
        assert_eq!(i.read(0x04), 0x20);
        assert_eq!(i.read(0x9C), 1);
        i.write(0x10, 7);
        i.write(0x6C, 0);
        assert_eq!((i.read(0x74), i.read(0x9C)), (0, 0));
        i.write(0x10, 7); // disabled: dropped
        assert_eq!(i.read(0x74), 0);
        assert_eq!(i.read(0xFC), 0x4457_0140);
    }

    #[test]
    fn user_abort_and_tx_cmd_block() {
        let mut i = setup(0x20);
        let mut w = wire();
        i.write(0x6C, 1 | EN_TX_CMD_BLOCK);
        i.write(0x10, 1);
        assert!(!i.has_work());
        i.write(0x6C, 1);
        assert!(i.has_work());
        i.write(0x6C, 1 | EN_ABORT);
        assert_eq!(i.read(0x80) & 0x1FF, 0);
        assert_ne!(i.read(0x80) & ABRT_USER_ABRT, 0);
        assert_eq!(i.read(0x74), 0);
        i.pump(&mut w);
        assert!(w.log.is_empty());
    }

    #[test]
    fn dma_requests_follow_levels() {
        let mut i = setup(0x20);
        assert!(!i.dreq_tx() && !i.dreq_rx());
        i.write(0x88, 3);
        i.write(0x8C, 2);
        assert!(i.dreq_tx());
        for n in 0..4 {
            i.write(0x10, n);
        }
        assert!(!i.dreq_tx());
    }
}
