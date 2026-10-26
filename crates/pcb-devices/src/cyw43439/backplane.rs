//! The chip's internal bus (the "backplane"): 512 KiB of RAM at 0, the
//! chip-ID register, and the AMBA core wrappers the driver pokes to stop and
//! start the ARM core. Everything else is a plain register file that
//! remembers what was written.
use alloc::{collections::BTreeMap, vec, vec::Vec};

pub const RAM_SIZE: usize = 512 * 1024;
pub const CHIP_ID: u16 = 43439;

pub const CHIPCOMMON: u32 = 0x1800_0000;
/// Core wrappers sit 0x100000 above their cores.
pub const ARM_WRAPPER: u32 = 0x1810_3000;
pub const SOCSRAM_WRAPPER: u32 = 0x1810_4000;
const AI_RESETCTRL: u32 = 0x800;

/// Where the real firmware keeps its shared-memory descriptor pointer:
/// `RAM_END - 4 - socram_srmem_size`. The driver's `firmware-logs` feature
/// follows it, so a started core must have something sane there.
pub const SHARED_PTR: u32 = RAM_SIZE as u32 - 4 - 64 * 1024;
const SHARED_DATA: u32 = 0x6_F000;
const SHARED_LOG: u32 = 0x6_F100;
const LOG_BUF: u32 = 0x6_F200;

pub struct Backplane {
    ram: Vec<u8>,
    /// Registers, byte by byte. Missing bytes read as `default_byte`.
    regs: BTreeMap<u32, u8>,
}

impl Default for Backplane {
    fn default() -> Self {
        Self::new()
    }
}

impl Backplane {
    pub fn new() -> Self {
        Self { ram: vec![0; RAM_SIZE], regs: BTreeMap::new() }
    }

    fn default_byte(addr: u32) -> u8 {
        match addr {
            // Chip ID, revision 1.
            a if a == CHIPCOMMON => CHIP_ID as u8,
            a if a == CHIPCOMMON + 1 => (CHIP_ID >> 8) as u8,
            a if a == CHIPCOMMON + 2 => 1,
            // The ARM and SOCSRAM cores power up held in reset.
            a if a == ARM_WRAPPER + AI_RESETCTRL || a == SOCSRAM_WRAPPER + AI_RESETCTRL => 1,
            _ => 0,
        }
    }

    pub fn byte(&self, addr: u32) -> u8 {
        if (addr as usize) < RAM_SIZE {
            self.ram[addr as usize]
        } else {
            self.regs.get(&addr).copied().unwrap_or_else(|| Self::default_byte(addr))
        }
    }

    pub fn read(&self, addr: u32, len: usize) -> Vec<u8> {
        (0..len as u32).map(|i| self.byte(addr.wrapping_add(i))).collect()
    }

    pub fn read32(&self, addr: u32) -> u32 {
        u32::from_le_bytes(self.read(addr, 4).try_into().unwrap_or_default())
    }

    pub fn write32(&mut self, addr: u32, v: u32) {
        self.write(addr, &v.to_le_bytes());
    }

    /// Store `data` at `addr`. Returns true if this took the ARM core out of
    /// reset (the firmware started running).
    pub fn write(&mut self, addr: u32, data: &[u8]) -> bool {
        let was_in_reset = self.byte(ARM_WRAPPER + AI_RESETCTRL) & 1 != 0;
        if (addr as usize) < RAM_SIZE {
            let n = data.len().min(RAM_SIZE - addr as usize);
            self.ram[addr as usize..addr as usize + n].copy_from_slice(&data[..n]);
        } else {
            for (i, &b) in data.iter().enumerate() {
                self.regs.insert(addr.wrapping_add(i as u32), b);
            }
        }
        let started = was_in_reset && self.byte(ARM_WRAPPER + AI_RESETCTRL) & 1 == 0;
        if started {
            self.boot();
        }
        started
    }

    /// What the real firmware leaves in RAM shortly after it boots: a
    /// pointer to a shared-memory descriptor whose console log is empty.
    fn boot(&mut self) {
        self.write32(SHARED_PTR, SHARED_DATA);
        self.write32(SHARED_DATA + 20, SHARED_LOG); // console_addr
        self.write32(SHARED_LOG + 8, LOG_BUF); // log.buf
        self.write32(SHARED_LOG + 12, 0x400); // log.buf_size
        self.write32(SHARED_LOG + 16, 0); // log.idx
    }

    pub fn ram(&self) -> &[u8] {
        &self.ram
    }
}
