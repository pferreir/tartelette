//! What sits behind the gSPI command decoder: the bus function registers
//! (F0), the backplane function (F1: its own registers plus a window onto
//! the internal bus, where the Bluetooth side also lives), and the WLAN data
//! function (F2).
use alloc::{vec, vec::Vec};

use super::{backplane::Backplane, bt::Bt, whd::Whd};

// F0 registers.
const BUS_CTRL: usize = 0x0;
const BUS_CTRL_RESPONSE_DELAY: usize = 0x1;
const INTERRUPT: usize = 0x4;
const INTERRUPT_ENABLE: usize = 0x6;
const STATUS: usize = 0x8;
const TEST_RO: usize = 0x14;
const RESP_DELAY_F1: usize = 0x1D;
const FEEDBEAD: u32 = 0xFEED_BEAD;

// F1 registers (offsets from 0x10000).
const F1_WINDOW_LOW: usize = 0x0A;
const F1_WINDOW_MID: usize = 0x0B;
const F1_WINDOW_HIGH: usize = 0x0C;
const F1_CLOCK_CSR: usize = 0x0E;
const F1_SLEEP_CSR: usize = 0x1F;
const SLEEP_CSR_KSO: u8 = 0x01;
const SLEEP_CSR_DEVICE_ON: u8 = 0x02;

const CTRL_WORD_32: u8 = 0x01;
const CTRL_ENDIAN_BIG: u8 = 0x02;
const CTRL_IRQ_HIGH: u8 = 0x20;

const IRQ_DATA_UNAVAILABLE: u16 = 0x0001;
pub const IRQ_F2_PACKET_AVAILABLE: u16 = 0x0020;
const IRQ_F1_INTR: u16 = 0x2000;

const STATUS_F2_RX_READY: u32 = 0x20;
const STATUS_F2_PKT_AVAILABLE: u32 = 0x100;

/// See [`Chip::framing`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Framing {
    Swapped16,
    Word32,
    BigEndian,
}

impl Framing {
    /// Turn a word as shifted on the wire into the value it stands for, or
    /// back (each layout is its own inverse).
    pub fn wire(self, w: u32) -> u32 {
        match self {
            Framing::Swapped16 => w.rotate_left(16),
            Framing::Word32 => w,
            Framing::BigEndian => w.swap_bytes(),
        }
    }
}

/// One bus transaction, for debugging.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    pub cycle: u64,
    pub write: bool,
    pub func: u8,
    pub addr: u32,
    pub len: usize,
    /// First four bytes read or written.
    pub head: [u8; 4],
}

/// How many transactions [`Chip::trace`] remembers.
const TRACE_LEN: usize = 1024;

pub struct Chip {
    /// The last transactions, oldest first.
    pub trace: alloc::collections::VecDeque<Transaction>,
    /// Core cycle of the access being handled.
    pub now: u64,
    cycles_per_ms: u64,
    /// WL_ON has been high since this cycle.
    pub power_on_at: Option<u64>,
    bus_ctrl: [u8; 4],
    f0: [u8; 0x40],
    irq_sticky: u16,
    irq_enable: u16,
    f1: [u8; 0x40],
    pub bp: Backplane,
    pub whd: Whd,
    pub bt: Bt,
    /// The ARM core left reset at this cycle.
    fw_at: Option<u64>,
}

impl Chip {
    /// The core clock changed.
    pub fn set_cycles_per_us(&mut self, cycles_per_us: u64) {
        self.cycles_per_ms = cycles_per_us * 1000;
        self.whd.set_cycles_per_us(cycles_per_us);
        self.bt.set_cycles_per_us(cycles_per_us);
    }

    pub fn new(whd: Whd, bt: Bt, cycles_per_us: u64) -> Self {
        Self {
            trace: alloc::collections::VecDeque::new(),
            now: 0,
            cycles_per_ms: cycles_per_us * 1000,
            power_on_at: None,
            bus_ctrl: [0; 4],
            f0: [0; 0x40],
            irq_sticky: 0,
            irq_enable: 0,
            f1: [0; 0x40],
            bp: Backplane::new(),
            whd,
            bt,
            fw_at: None,
        }
    }

    /// The bus logic needs a moment after WL_ON rises before it answers.
    pub fn ready(&self) -> bool {
        self.power_on_at
            .is_some_and(|t| self.now >= t + 20 * self.cycles_per_ms)
    }

    /// How 32-bit words are laid out on the wire. Until the host writes the
    /// bus control register the chip talks in 16-bit words, which puts the
    /// halves of each word in swapped order. In 32-bit mode a word goes out
    /// most significant bit first; big-endian mode (pico-sdk's driver, whose
    /// DMA byte-swaps every word) sends its bytes in the opposite order.
    pub fn framing(&self) -> Framing {
        match self.bus_ctrl[0] {
            c if c & CTRL_WORD_32 == 0 => Framing::Swapped16,
            c if c & CTRL_ENDIAN_BIG != 0 => Framing::BigEndian,
            _ => Framing::Word32,
        }
    }

    /// Whether the firmware is running and the WLAN function accepts frames.
    pub fn f2_ready(&self) -> bool {
        self.fw_at
            .is_some_and(|t| self.now >= t + 3 * self.cycles_per_ms)
    }

    pub fn interrupt(&self) -> u16 {
        self.irq_sticky
            | if self.whd.has_frame() {
                IRQ_F2_PACKET_AVAILABLE
            } else {
                0
            }
            | if self.bt.interrupt(&self.bp) {
                IRQ_F1_INTR
            } else {
                0
            }
    }

    /// Should the chip be raising its interrupt line (DIO) now?
    pub fn irq_pending(&self) -> bool {
        self.ready()
            && self.bus_ctrl[BUS_CTRL] & CTRL_IRQ_HIGH != 0
            && self.interrupt() & self.irq_enable != 0
    }

    pub fn status(&self) -> u32 {
        if !self.ready() {
            return 0;
        }
        let mut s = 0;
        if self.f2_ready() {
            s |= STATUS_F2_RX_READY;
        }
        if let Some(len) = self.whd.next_len() {
            s |= STATUS_F2_PKT_AVAILABLE | (len as u32 & 0x7FF) << 9;
        }
        s
    }

    /// Padding words the chip sends before read data on `func`: only the
    /// backplane has a response delay. A driver can set it in the bus control
    /// register's delay byte or in the backplane-specific register; left alone
    /// it is one word, which is what every driver relies on.
    pub fn pad_words(&self, func: u8) -> usize {
        if func != 1 {
            return 0;
        }
        let bytes = [
            self.f0[RESP_DELAY_F1],
            self.bus_ctrl[BUS_CTRL_RESPONSE_DELAY],
        ]
        .into_iter()
        .find(|&b| b != 0)
        .unwrap_or(4);
        bytes as usize / 4
    }

    fn record(&mut self, write: bool, func: u8, addr: u32, len: usize, data: &[u8]) {
        let mut head = [0u8; 4];
        let n = data.len().min(4);
        head[..n].copy_from_slice(&data[..n]);
        if self.trace.len() == TRACE_LEN {
            self.trace.pop_front();
        }
        self.trace.push_back(Transaction {
            cycle: self.now,
            write,
            func,
            addr,
            len,
            head,
        });
    }

    pub fn read(&mut self, func: u8, addr: u32, len: usize) -> Vec<u8> {
        let data = self.read_inner(func, addr, len);
        self.record(false, func, addr, len, &data);
        data
    }

    fn read_inner(&mut self, func: u8, addr: u32, len: usize) -> Vec<u8> {
        if !self.ready() {
            return vec![0; len];
        }
        match func {
            0 => self.f0_read(addr as usize, len),
            1 if addr & 0x1_0000 != 0 => (0..len)
                .map(|i| self.f1_read((addr as usize & 0xFFFF) + i))
                .collect(),
            1 => {
                let addr = self.window() | (addr & 0x7FFF);
                match self.bt.read32(self.now, addr).filter(|_| len == 4) {
                    Some(v) => v.to_le_bytes().to_vec(),
                    None => self.bp.read(addr, len),
                }
            }
            2 => match self.whd.pop() {
                Some(mut frame) => {
                    frame.resize(len, 0);
                    frame
                }
                None => {
                    self.irq_sticky |= IRQ_DATA_UNAVAILABLE;
                    vec![0; len]
                }
            },
            _ => vec![0; len],
        }
    }

    pub fn write(&mut self, func: u8, addr: u32, data: &[u8]) {
        self.record(true, func, addr, data.len(), data);
        if !self.ready() {
            return;
        }
        match func {
            0 => self.f0_write(addr as usize, data),
            1 if addr & 0x1_0000 != 0 => {
                for (i, &b) in data.iter().enumerate() {
                    if let Some(r) = self.f1.get_mut((addr as usize & 0xFFFF) + i) {
                        *r = b;
                    }
                }
            }
            1 => {
                let addr = self.window() | (addr & 0x7FFF);
                if !self.bt.write(self.now, addr, data, &mut self.bp) && self.bp.write(addr, data) {
                    self.fw_at = Some(self.now);
                }
            }
            2 => self.whd.host_frame(self.now, data),
            _ => {}
        }
    }

    fn window(&self) -> u32 {
        (self.f1[F1_WINDOW_LOW] as u32) << 8
            | (self.f1[F1_WINDOW_MID] as u32) << 16
            | (self.f1[F1_WINDOW_HIGH] as u32) << 24
    }

    fn f0_read(&self, addr: usize, len: usize) -> Vec<u8> {
        let mut r = self.f0;
        r[BUS_CTRL..BUS_CTRL + 4].copy_from_slice(&self.bus_ctrl);
        r[INTERRUPT..INTERRUPT + 2].copy_from_slice(&self.interrupt().to_le_bytes());
        r[INTERRUPT_ENABLE..INTERRUPT_ENABLE + 2].copy_from_slice(&self.irq_enable.to_le_bytes());
        r[STATUS..STATUS + 4].copy_from_slice(&self.status().to_le_bytes());
        r[TEST_RO..TEST_RO + 4].copy_from_slice(&FEEDBEAD.to_le_bytes());
        (0..len)
            .map(|i| r.get(addr + i).copied().unwrap_or(0))
            .collect()
    }

    fn f0_write(&mut self, addr: usize, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            match addr + i {
                a @ 0..4 => self.bus_ctrl[a - BUS_CTRL] = b,
                // Interrupt status: write 1 to clear.
                a @ 4..6 => self.irq_sticky &= !((b as u16) << (8 * (a - INTERRUPT))),
                a @ 6..8 => {
                    let shift = 8 * (a - INTERRUPT_ENABLE);
                    self.irq_enable = self.irq_enable & !(0xFF << shift) | (b as u16) << shift;
                }
                // Status and the read-only test pattern.
                8..12 | 0x14..0x18 => {}
                a => {
                    if let Some(r) = self.f0.get_mut(a) {
                        *r = b;
                    }
                }
            }
        }
    }

    fn f1_read(&self, off: usize) -> u8 {
        let Some(&stored) = self.f1.get(off) else {
            return 0;
        };
        // Keep-SDIO-on: the chip never sleeps, so it is on whenever asked.
        if off == F1_SLEEP_CSR {
            let on = if stored & SLEEP_CSR_KSO != 0 {
                SLEEP_CSR_DEVICE_ON
            } else {
                0
            };
            return stored | on;
        }
        if off != F1_CLOCK_CSR {
            return stored;
        }
        // Clock control: requests read back; the clocks come up after a
        // delay once the firmware is running.
        let fw_up = self.fw_at.is_some();
        let ht_ok = self
            .fw_at
            .is_some_and(|t| self.now >= t + 2 * self.cycles_per_ms);
        let alp = stored & 0x18 != 0 || fw_up;
        let ht = (stored & 0x10 != 0 || fw_up) && ht_ok;
        stored & 0x3F | if alp { 0x40 } else { 0 } | if ht { 0x80 } else { 0 }
    }
}
