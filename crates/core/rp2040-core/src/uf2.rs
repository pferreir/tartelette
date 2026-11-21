//! Minimal UF2 reader: extracts the RP2040 blocks of a drag-and-drop image.
//!
//! A UF2 file is a run of 512-byte blocks: a 32-byte header (two magic
//! words, flags, target address, payload size, block number, block count,
//! family ID), up to 476 bytes of payload, and a closing magic word.
use alloc::vec::Vec;

const BLOCK: usize = 512;
const MAGIC_START0: u32 = 0x0A32_4655;
const MAGIC_START1: u32 = 0x9E5D_5157;
const MAGIC_END: u32 = 0x0AB1_6F30;
const FLAG_NOT_MAIN_FLASH: u32 = 0x1;
const FLAG_FAMILY_ID: u32 = 0x2000;
const FAMILY_RP2040: u32 = 0xE48B_FF56;

#[derive(Debug, PartialEq, Eq)]
pub enum Uf2Error {
    /// The length is not a whole number of blocks.
    TooShort,
    /// Block `n` has bad magic or a payload that does not fit.
    BadBlock(usize),
    /// Every block is for another chip (or not meant for flash).
    NoRp2040Blocks,
}

pub struct Block<'a> {
    pub addr: u32,
    pub data: &'a [u8],
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// Whether `bytes` starts like a UF2 file.
pub fn is_uf2(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && u32_at(bytes, 0) == MAGIC_START0 && u32_at(bytes, 4) == MAGIC_START1
}

pub fn parse(bytes: &[u8]) -> Result<Vec<Block<'_>>, Uf2Error> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(BLOCK) {
        return Err(Uf2Error::TooShort);
    }
    let mut blocks = Vec::new();
    for (n, b) in bytes.as_chunks::<BLOCK>().0.iter().enumerate() {
        let (flags, addr, size, family) = (
            u32_at(b, 8),
            u32_at(b, 12),
            u32_at(b, 16) as usize,
            u32_at(b, 28),
        );
        if u32_at(b, 0) != MAGIC_START0
            || u32_at(b, 4) != MAGIC_START1
            || u32_at(b, 508) != MAGIC_END
            || size > 476
        {
            return Err(Uf2Error::BadBlock(n));
        }
        // Files can carry blocks for several chips (RP2350 next to RP2040,
        // say) and extra data the bootloader does not flash: skip those.
        if flags & FLAG_NOT_MAIN_FLASH != 0
            || flags & FLAG_FAMILY_ID != 0 && family != FAMILY_RP2040
        {
            continue;
        }
        blocks.push(Block {
            addr,
            data: &b[32..32 + size],
        });
    }
    if blocks.is_empty() {
        return Err(Uf2Error::NoRp2040Blocks);
    }
    Ok(blocks)
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    /// A UF2 with one block per `(flags, family, addr, payload)`.
    pub fn build(blocks: &[(u32, u32, u32, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (n, &(flags, family, addr, data)) in blocks.iter().enumerate() {
            for w in [
                MAGIC_START0,
                MAGIC_START1,
                flags,
                addr,
                data.len() as u32,
                n as u32,
                blocks.len() as u32,
                family,
            ] {
                out.extend_from_slice(&w.to_le_bytes());
            }
            out.extend_from_slice(data);
            out.resize(out.len() + 476 - data.len(), 0);
            out.extend_from_slice(&MAGIC_END.to_le_bytes());
        }
        out
    }

    /// One RP2040 flash block.
    pub fn rp2040(addr: u32, data: &[u8]) -> (u32, u32, u32, &[u8]) {
        (FLAG_FAMILY_ID, FAMILY_RP2040, addr, data)
    }
}

#[cfg(test)]
mod tests {
    use super::{testutil::*, *};

    #[test]
    fn keeps_rp2040_flash_blocks_only() {
        let file = build(&[
            rp2040(0x1000_0000, &[1, 2]),
            (FLAG_FAMILY_ID, 0xE48B_FF59, 0x1000_0000, &[9]), // RP2350
            (FLAG_NOT_MAIN_FLASH, 0, 0x1000_0100, &[9]),
            (0, 0, 0x1000_0200, &[3]), // no family: taken as ours
        ]);
        assert!(is_uf2(&file));
        let blocks = parse(&file).unwrap();
        let got: Vec<_> = blocks.iter().map(|b| (b.addr, b.data)).collect();
        assert_eq!(got, [(0x1000_0000, &[1, 2][..]), (0x1000_0200, &[3][..])]);
    }

    #[test]
    fn reports_errors() {
        assert_eq!(parse(&[]).err(), Some(Uf2Error::TooShort));
        assert_eq!(parse(&[0; 100]).err(), Some(Uf2Error::TooShort));
        let mut file = build(&[rp2040(0, &[1]), rp2040(4, &[2])]);
        file[512 + 508] ^= 1; // second block's end magic
        assert_eq!(parse(&file).err(), Some(Uf2Error::BadBlock(1)));
        let file = build(&[(FLAG_FAMILY_ID, 0xE48B_FF59, 0, &[1])]);
        assert_eq!(parse(&file).err(), Some(Uf2Error::NoRp2040Blocks));
        assert!(!is_uf2(b"\x7fELF...."));
    }
}
