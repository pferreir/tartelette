use alloc::vec::Vec;

/// The memory bus as seen by a CPU core. Addresses are absolute; alignment is
/// checked by the CPU (unaligned accesses fault before reaching the bus).
pub trait Bus {
    fn read8(&mut self, addr: u32) -> u8;
    fn read16(&mut self, addr: u32) -> u16;
    fn read32(&mut self, addr: u32) -> u32;
    fn write8(&mut self, addr: u32, val: u8);
    fn write16(&mut self, addr: u32, val: u16);
    fn write32(&mut self, addr: u32, val: u32);
}

/// Plain little-endian byte memory, addressed by offset. Building block for
/// SRAM / flash / ROM regions in the full system bus.
#[derive(Clone, Debug)]
pub struct Ram {
    pub data: Vec<u8>,
}

impl Ram {
    pub fn new(size: usize) -> Self {
        Self { data: alloc::vec![0; size] }
    }
    pub fn read8(&self, off: u32) -> u8 {
        self.data.get(off as usize).copied().unwrap_or(0)
    }
    pub fn read16(&self, off: u32) -> u16 {
        self.read8(off) as u16 | (self.read8(off + 1) as u16) << 8
    }
    pub fn read32(&self, off: u32) -> u32 {
        self.read16(off) as u32 | (self.read16(off + 2) as u32) << 16
    }
    pub fn write8(&mut self, off: u32, val: u8) {
        if let Some(b) = self.data.get_mut(off as usize) {
            *b = val;
        }
    }
    pub fn write16(&mut self, off: u32, val: u16) {
        self.write8(off, val as u8);
        self.write8(off + 1, (val >> 8) as u8);
    }
    pub fn write32(&mut self, off: u32, val: u32) {
        self.write16(off, val as u16);
        self.write16(off + 2, (val >> 16) as u16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn little_endian_roundtrip() {
        let mut r = Ram::new(16);
        r.write32(4, 0x1122_3344);
        assert_eq!([r.read8(4), r.read8(5), r.read8(6), r.read8(7)], [0x44, 0x33, 0x22, 0x11]);
        assert_eq!(r.read16(4), 0x3344);
        assert_eq!(r.read16(6), 0x1122);
        r.write16(8, 0xBEEF);
        r.write8(10, 0xAB);
        assert_eq!(r.read32(8), 0x00AB_BEEF);
    }

    #[test]
    fn out_of_range_reads_zero_and_writes_vanish() {
        let mut r = Ram::new(4);
        r.write32(2, 0xFFFF_FFFF); // straddles the end: low half lands
        assert_eq!(r.read32(0), 0xFFFF_0000);
        assert_eq!(r.read32(100), 0);
        r.write8(100, 1);
        assert_eq!(r.data.len(), 4);
    }
}
