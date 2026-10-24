//! Minimal ELF32 little-endian loader: extracts PT_LOAD segments.
use alloc::vec::Vec;

#[derive(Debug, PartialEq, Eq)]
pub enum ElfError {
    TooShort,
    NotElf32Le,
    NotArm,
    BadSegment,
}

pub struct Segment<'a> {
    /// Load (physical) address. For flash images this is where `.data` is
    /// stored, which is what a real device would see.
    pub addr: u32,
    pub data: &'a [u8],
    /// Bytes of zero-fill after `data` (.bss-like), `memsz - filesz`.
    pub zero_fill: u32,
}

pub struct Elf<'a> {
    pub entry: u32,
    pub segments: Vec<Segment<'a>>,
}

fn u16_at(b: &[u8], o: usize) -> Result<u16, ElfError> {
    Ok(u16::from_le_bytes(
        b.get(o..o + 2)
            .ok_or(ElfError::TooShort)?
            .try_into()
            .unwrap(),
    ))
}
fn u32_at(b: &[u8], o: usize) -> Result<u32, ElfError> {
    Ok(u32::from_le_bytes(
        b.get(o..o + 4)
            .ok_or(ElfError::TooShort)?
            .try_into()
            .unwrap(),
    ))
}

pub fn parse(bytes: &[u8]) -> Result<Elf<'_>, ElfError> {
    if bytes.len() < 52 {
        return Err(ElfError::TooShort);
    }
    // magic, EI_CLASS = 1 (32-bit), EI_DATA = 1 (little endian)
    if &bytes[0..4] != b"\x7fELF" || bytes[4] != 1 || bytes[5] != 1 {
        return Err(ElfError::NotElf32Le);
    }
    if u16_at(bytes, 18)? != 40 {
        return Err(ElfError::NotArm);
    }
    let entry = u32_at(bytes, 24)?;
    let phoff = u32_at(bytes, 28)? as usize;
    let phentsize = u16_at(bytes, 42)? as usize;
    let phnum = u16_at(bytes, 44)? as usize;

    let mut segments = Vec::new();
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if u32_at(bytes, ph)? != 1 {
            continue; // not PT_LOAD
        }
        let offset = u32_at(bytes, ph + 4)? as usize;
        let paddr = u32_at(bytes, ph + 12)?;
        let filesz = u32_at(bytes, ph + 16)? as usize;
        let memsz = u32_at(bytes, ph + 20)?;
        let data = bytes
            .get(offset..offset + filesz)
            .ok_or(ElfError::BadSegment)?;
        if filesz == 0 && memsz == 0 {
            continue;
        }
        segments.push(Segment {
            addr: paddr,
            data,
            zero_fill: memsz.saturating_sub(filesz as u32),
        });
    }
    Ok(Elf { entry, segments })
}

#[cfg(test)]
pub(crate) mod testutil {
    use alloc::vec::Vec;

    /// Build a little-endian ARM ELF32 with one PT_LOAD per `(paddr, data, memsz)`.
    pub fn build(entry: u32, segs: &[(u32, &[u8], u32)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"\x7fELF\x01\x01\x01");
        b.resize(18, 0);
        b.extend_from_slice(&40u16.to_le_bytes()); // e_machine = ARM
        b.extend_from_slice(&1u32.to_le_bytes()); // e_version
        b.extend_from_slice(&entry.to_le_bytes());
        b.extend_from_slice(&52u32.to_le_bytes()); // e_phoff
        b.resize(42, 0);
        b.extend_from_slice(&32u16.to_le_bytes()); // e_phentsize
        b.extend_from_slice(&(segs.len() as u16).to_le_bytes());
        b.resize(52, 0);
        let mut offset = 52 + 32 * segs.len() as u32;
        for (paddr, data, memsz) in segs {
            for w in [1, offset, *paddr, *paddr, data.len() as u32, *memsz, 5, 4] {
                b.extend_from_slice(&w.to_le_bytes());
            }
            offset += data.len() as u32;
        }
        for (_, data, _) in segs {
            b.extend_from_slice(data);
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::build;
    use super::*;

    #[test]
    fn rejects_malformed_headers() {
        assert_eq!(parse(&[0; 10]).err(), Some(ElfError::TooShort));
        let mut e = build(0, &[]);
        e[0] = b'X';
        assert_eq!(parse(&e).err(), Some(ElfError::NotElf32Le));
        let mut e = build(0, &[]);
        e[4] = 2; // ELFCLASS64
        assert_eq!(parse(&e).err(), Some(ElfError::NotElf32Le));
        let mut e = build(0, &[]);
        e[5] = 2; // big endian
        assert_eq!(parse(&e).err(), Some(ElfError::NotElf32Le));
        let mut e = build(0, &[]);
        e[18] = 62; // x86-64
        assert_eq!(parse(&e).err(), Some(ElfError::NotArm));
    }

    #[test]
    fn parses_segments_entry_and_zero_fill() {
        let e = build(
            0x1000_0101,
            &[(0x1000_0000, &[1, 2, 3, 4], 4), (0x2000_0000, &[9, 9], 10)],
        );
        let elf = parse(&e).unwrap();
        assert_eq!(elf.entry, 0x1000_0101);
        assert_eq!(elf.segments.len(), 2);
        assert_eq!(
            (
                elf.segments[0].addr,
                elf.segments[0].data,
                elf.segments[0].zero_fill
            ),
            (0x1000_0000, &[1u8, 2, 3, 4][..], 0)
        );
        assert_eq!(
            (
                elf.segments[1].addr,
                elf.segments[1].data,
                elf.segments[1].zero_fill
            ),
            (0x2000_0000, &[9u8, 9][..], 8)
        );
    }

    #[test]
    fn skips_empty_and_non_load_segments() {
        let mut e = build(0, &[(0x2000_0000, &[], 0), (0x2000_0100, &[7], 1)]);
        assert_eq!(parse(&e).unwrap().segments.len(), 1);
        e[52] = 4; // first program header becomes PT_NOTE
        e[52 + 32] = 4; // second too
        assert!(parse(&e).unwrap().segments.is_empty());
    }

    #[test]
    fn truncated_segment_or_phdr_table_is_an_error() {
        let mut e = build(0, &[(0x2000_0000, &[1, 2, 3, 4], 4)]);
        e.truncate(e.len() - 2);
        assert_eq!(parse(&e).err(), Some(ElfError::BadSegment));
        let mut e = build(0, &[(0x2000_0000, &[1], 1)]);
        e.truncate(60);
        assert_eq!(parse(&e).err(), Some(ElfError::TooShort));
    }
}
