//! Reading a FAT12/16/32 file system out of a disk image, for a host to show
//! what is on a card: either a whole-disk file system or the first FAT
//! partition of an MBR. Long file names are read; nothing is written.
//!
//! The image may be a copy taken half way through the firmware's writes, so
//! every offset is checked and every cluster chain is bounded: a broken file
//! system is an error, never a panic or a hang.
use alloc::{format, string::String, vec, vec::Vec};

/// MBR partition offsets are in 512-byte sectors.
const SECTOR: usize = 512;
const ATTR_DIR: u8 = 0x10;
const ATTR_LABEL: u8 = 0x08;
const ATTR_LFN: u8 = 0x0F;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FatType {
    Fat12,
    Fat16,
    Fat32,
}

/// A file or directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: u32,
    /// First cluster: 0 for an empty file.
    pub cluster: u32,
    /// Last written: year, month, day, hour, minute, second (if set).
    pub modified: Option<[u16; 6]>,
}

enum Root {
    /// FAT12/16: a fixed area of `entries` entries at a byte offset.
    Fixed { at: usize, entries: usize },
    /// FAT32: a cluster chain like any directory.
    Chain(u32),
}

pub struct Volume<'a> {
    disk: &'a [u8],
    pub fat_type: FatType,
    /// The volume label, from the root directory or else the boot sector
    /// (empty if there is none).
    pub label: String,
    /// Byte offset of the file system in the disk.
    pub offset: usize,
    /// Byte offset of the first FAT.
    fat: usize,
    cluster_bytes: usize,
    /// Byte offset of cluster 2, the first data cluster.
    data: usize,
    clusters: u32,
    root: Root,
}

fn get(disk: &[u8], at: usize, len: usize) -> Result<&[u8], String> {
    at.checked_add(len)
        .and_then(|end| disk.get(at..end))
        .ok_or_else(|| "the file system reaches past the end of the image".into())
}
fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The file system on `disk`: at its start, or in its first FAT partition.
pub fn open(disk: &[u8]) -> Result<Volume<'_>, String> {
    let whole = Volume::at(disk, 0);
    if whole.is_ok() || disk.get(510..512) != Some(&[0x55, 0xAA][..]) {
        return whole;
    }
    for i in 0..4 {
        let e = 0x1BE + i * 16;
        let (kind, lba) = (disk[e + 4], le32(disk, e + 8) as usize);
        if kind != 0
            && let Ok(v) = Volume::at(disk, lba * SECTOR)
        {
            return Ok(v);
        }
    }
    Err("no FAT file system, on the whole disk or in a partition".into())
}

impl<'a> Volume<'a> {
    fn at(disk: &'a [u8], offset: usize) -> Result<Self, String> {
        let b = get(disk, offset, 512).map_err(|_| "the image is too small for a file system")?;
        let bad = || String::from("no FAT boot sector");
        let bps = le16(b, 11) as u64;
        let spc = b[13] as u64;
        let reserved = le16(b, 14) as u64;
        let fats = b[16] as u64;
        let root_entries = le16(b, 17) as u64;
        let total = match le16(b, 19) {
            0 => le32(b, 32) as u64,
            n => n as u64,
        };
        let fat_sectors = match le16(b, 22) {
            0 => le32(b, 36) as u64,
            n => n as u64,
        };
        if !matches!(bps, 512 | 1024 | 2048 | 4096)
            || !spc.is_power_of_two()
            || reserved == 0
            || !(1..=4).contains(&fats)
            || fat_sectors == 0
        {
            return Err(bad());
        }
        let root_sectors = (root_entries * 32).div_ceil(bps);
        let first_data = reserved + fats * fat_sectors + root_sectors;
        if total <= first_data {
            return Err(bad());
        }
        let clusters = (total - first_data) / spc;
        let fat_type = match clusters {
            0..4085 => FatType::Fat12,
            4085..65525 => FatType::Fat16,
            _ => FatType::Fat32,
        };
        let clusters = u32::try_from(clusters).map_err(|_| bad())?;
        let at = |sector: u64| {
            usize::try_from(sector * bps)
                .ok()
                .and_then(|n| n.checked_add(offset))
                .ok_or_else(bad)
        };
        let (root, label_at) = if fat_type == FatType::Fat32 {
            (Root::Chain(le32(b, 44)), 66)
        } else {
            let root = Root::Fixed {
                at: at(reserved + fats * fat_sectors)?,
                entries: root_entries as usize,
            };
            (root, 38)
        };
        // An extended boot signature says the label field is there.
        let boot_label = if b[label_at] == 0x29 {
            short_text(&b[label_at + 5..label_at + 16])
        } else {
            String::new()
        };
        let mut v = Volume {
            disk,
            fat_type,
            label: String::new(),
            offset,
            fat: at(reserved)?,
            cluster_bytes: (spc * bps) as usize,
            data: at(first_data)?,
            clusters,
            root,
        };
        v.label = match v.root_bytes().map(|d| parse_dir(&d, fat_type)) {
            Ok((_, Some(label))) => label,
            _ if boot_label == "NO NAME" => String::new(),
            _ => boot_label,
        };
        Ok(v)
    }

    /// The root directory's entries.
    pub fn root(&self) -> Result<Vec<Entry>, String> {
        Ok(parse_dir(&self.root_bytes()?, self.fat_type).0)
    }

    /// The entries of directory `dir` (cluster 0 is the root, as in `..`).
    pub fn dir(&self, dir: &Entry) -> Result<Vec<Entry>, String> {
        if dir.cluster == 0 {
            return self.root();
        }
        let mut bytes = Vec::new();
        for c in self.chain(dir.cluster, None)? {
            bytes.extend_from_slice(self.cluster(c)?);
        }
        Ok(parse_dir(&bytes, self.fat_type).0)
    }

    /// The first `limit` bytes of `file` (all of it if it is shorter).
    pub fn read(&self, file: &Entry, limit: usize) -> Result<Vec<u8>, String> {
        let want = (file.size as usize).min(limit);
        if want == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(want);
        for c in self.chain(file.cluster, Some(want.div_ceil(self.cluster_bytes)))? {
            let bytes = self.cluster(c)?;
            let n = bytes.len().min(want - out.len());
            out.extend_from_slice(&bytes[..n]);
        }
        Ok(out)
    }

    fn root_bytes(&self) -> Result<Vec<u8>, String> {
        match self.root {
            Root::Fixed { at, entries } => Ok(get(self.disk, at, entries * 32)?.to_vec()),
            Root::Chain(first) => {
                let mut bytes = Vec::new();
                for c in self.chain(first, None)? {
                    bytes.extend_from_slice(self.cluster(c)?);
                }
                Ok(bytes)
            }
        }
    }

    fn cluster(&self, c: u32) -> Result<&'a [u8], String> {
        let at = (c as usize - 2)
            .checked_mul(self.cluster_bytes)
            .and_then(|n| n.checked_add(self.data))
            .ok_or("cluster past the end of the image")?;
        get(self.disk, at, self.cluster_bytes)
    }

    /// The clusters from `first`: exactly `count` of them, or up to the
    /// end-of-chain mark. A chain may not loop or leave the volume.
    fn chain(&self, first: u32, count: Option<usize>) -> Result<Vec<u32>, String> {
        let valid = |c: u32| (2..self.clusters.saturating_add(2)).contains(&c);
        let broken = || String::from("broken cluster chain (a write in progress?)");
        if !valid(first) {
            return Err(broken());
        }
        let most = self.clusters as usize;
        let mut chain = vec![first];
        let mut c = first;
        while count.is_none_or(|n| chain.len() < n) {
            match self.next(c)? {
                None if count.is_none() => break,
                None => return Err("a file is shorter than its size (a write in progress?)".into()),
                Some(n) if valid(n) && chain.len() < most => {
                    chain.push(n);
                    c = n;
                }
                Some(_) => return Err(broken()),
            }
        }
        // Visiting a cluster twice is a loop, even within a file's length.
        let mut sorted = chain.clone();
        sorted.sort_unstable();
        if sorted.windows(2).any(|w| w[0] == w[1]) {
            return Err(broken());
        }
        Ok(chain)
    }

    /// The FAT entry for cluster `c`: the next cluster, or `None` at the end.
    fn next(&self, c: u32) -> Result<Option<u32>, String> {
        let c = c as usize;
        let (v, end) = match self.fat_type {
            FatType::Fat12 => {
                let w = le16(get(self.disk, self.fat + c * 3 / 2, 2)?, 0) as u32;
                (if c & 1 == 1 { w >> 4 } else { w & 0xFFF }, 0xFF8)
            }
            FatType::Fat16 => (le16(get(self.disk, self.fat + c * 2, 2)?, 0) as u32, 0xFFF8),
            FatType::Fat32 => (
                le32(get(self.disk, self.fat + c * 4, 4)?, 0) & 0x0FFF_FFFF,
                0x0FFF_FFF8,
            ),
        };
        Ok((v < end).then_some(v))
    }
}

/// A space-padded name field as text (code page bytes taken as Latin-1).
fn short_text(b: &[u8]) -> String {
    b.iter()
        .map(|&c| c as char)
        .collect::<String>()
        .trim_end()
        .into()
}

/// An 8.3 entry's name, lower-cased where the entry says so (bits 3 and 4 of
/// byte 12, as Windows writes all-lowercase short names).
fn short_name(e: &[u8]) -> String {
    let mut base = e[..8].to_vec();
    if base[0] == 0x05 {
        base[0] = 0xE5;
    }
    let mut name = short_text(&base);
    if e[12] & 0x08 != 0 {
        name = name.to_lowercase();
    }
    let mut ext = short_text(&e[8..11]);
    if e[12] & 0x10 != 0 {
        ext = ext.to_lowercase();
    }
    if !ext.is_empty() {
        name = format!("{name}.{ext}");
    }
    name
}

fn lfn_checksum(short: &[u8]) -> u8 {
    short
        .iter()
        .fold(0u8, |sum, &c| sum.rotate_right(1).wrapping_add(c))
}

/// Long-name pieces collected for the entry that follows them.
#[derive(Default)]
struct LongName {
    /// UTF-16 units by piece (piece 1 first), and the short entry's checksum.
    pieces: Vec<Option<[u16; 13]>>,
    sum: u8,
}

impl LongName {
    fn add(&mut self, e: &[u8]) {
        let n = (e[0] & 0x1F) as usize;
        if e[0] & 0x40 != 0 {
            self.pieces = vec![None; n];
            self.sum = e[13];
        }
        if n == 0 || n > self.pieces.len() || e[13] != self.sum {
            self.pieces.clear();
            return;
        }
        let mut units = [0u16; 13];
        let offsets = (1..11)
            .step_by(2)
            .chain((14..26).step_by(2))
            .chain([28, 30]);
        for (u, at) in units.iter_mut().zip(offsets) {
            *u = le16(e, at);
        }
        self.pieces[n - 1] = Some(units);
    }

    /// The long name for short entry `e`, if every piece is there and they
    /// belong to it.
    fn take(&mut self, e: &[u8]) -> Option<String> {
        let pieces = core::mem::take(&mut self.pieces);
        if pieces.is_empty() || self.sum != lfn_checksum(&e[..11]) {
            return None;
        }
        let mut units = Vec::new();
        for p in pieces {
            units.extend_from_slice(&p?);
        }
        let len = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        Some(
            char::decode_utf16(units[..len].iter().copied())
                .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect(),
        )
    }
}

/// A directory's entries (without `.` and `..`), and its volume label entry.
fn parse_dir(bytes: &[u8], fat_type: FatType) -> (Vec<Entry>, Option<String>) {
    let mut entries = Vec::new();
    let mut label = None;
    let mut long = LongName::default();
    for e in bytes.as_chunks::<32>().0 {
        match e[0] {
            0 => break,
            0xE5 => {
                long.pieces.clear();
                continue;
            }
            _ => {}
        }
        let attr = e[11];
        if attr & 0x3F == ATTR_LFN {
            long.add(e);
            continue;
        }
        if attr & ATTR_LABEL != 0 {
            long.pieces.clear();
            label = Some(short_text(&e[..11]));
            continue;
        }
        let name = long.take(e).unwrap_or_else(|| short_name(e));
        if name == "." || name == ".." {
            continue;
        }
        let hi = if fat_type == FatType::Fat32 {
            le16(e, 20) as u32
        } else {
            0
        };
        let (time, date) = (le16(e, 22), le16(e, 24));
        entries.push(Entry {
            name,
            dir: attr & ATTR_DIR != 0,
            size: le32(e, 28),
            cluster: hi << 16 | le16(e, 26) as u32,
            modified: (date != 0).then(|| {
                [
                    1980 + (date >> 9),
                    date >> 5 & 15,
                    date & 31,
                    time >> 11,
                    time >> 5 & 63,
                    (time & 31) * 2,
                ]
            }),
        });
    }
    (entries, label)
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use super::*;

    /// A disk of `mb` formatted by the `fatfs` crate as `kind`, filled by `fill`.
    fn disk(
        mb: usize,
        kind: fatfs::FatType,
        fill: impl FnOnce(&fatfs::Dir<'_, &mut Cursor<Vec<u8>>>),
    ) -> Vec<u8> {
        let mut c = Cursor::new(vec![0u8; mb * 1024 * 1024]);
        fatfs::format_volume(
            &mut c,
            fatfs::FormatVolumeOptions::new()
                .fat_type(kind)
                .volume_label(*b"TESTVOL    "),
        )
        .unwrap();
        {
            let fs = fatfs::FileSystem::new(&mut c, fatfs::FsOptions::new()).unwrap();
            fill(&fs.root_dir());
        }
        c.into_inner()
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    fn sample(root: &fatfs::Dir<'_, &mut Cursor<Vec<u8>>>) {
        root.create_file("README.TXT")
            .unwrap()
            .write_all(b"hello\n")
            .unwrap();
        root.create_file("A long file name.txt")
            .unwrap()
            .write_all(b"long")
            .unwrap();
        root.create_file("café ünïcode.md").unwrap();
        let sub = root.create_dir("Logs").unwrap();
        sub.create_file("big.bin")
            .unwrap()
            .write_all(&pattern(20_000))
            .unwrap();
        sub.create_dir("deeper").unwrap();
    }

    fn names(v: &[Entry]) -> Vec<&str> {
        v.iter().map(|e| e.name.as_str()).collect()
    }

    fn check_sample(v: &Volume) {
        let root = v.root().unwrap();
        assert_eq!(
            names(&root),
            [
                "README.TXT",
                "A long file name.txt",
                "café ünïcode.md",
                "Logs"
            ]
        );
        assert_eq!(v.read(&root[0], usize::MAX).unwrap(), b"hello\n");
        assert_eq!(v.read(&root[1], 2).unwrap(), b"lo");
        assert_eq!((root[2].size, v.read(&root[2], 10).unwrap()), (0, vec![]));
        assert!(root[3].dir && !root[0].dir);
        assert_eq!(
            root[0].modified, None,
            "fatfs without chrono writes no dates"
        );
        let logs = v.dir(&root[3]).unwrap();
        assert_eq!(names(&logs), ["big.bin", "deeper"]);
        assert_eq!(logs[0].size, 20_000);
        assert_eq!(v.read(&logs[0], usize::MAX).unwrap(), pattern(20_000));
        assert!(v.dir(&logs[1]).unwrap().is_empty());
    }

    #[test]
    fn reads_fat12_fat16_and_fat32_with_long_names() {
        for (mb, kind, want) in [
            (2, fatfs::FatType::Fat12, FatType::Fat12),
            (8, fatfs::FatType::Fat16, FatType::Fat16),
            (40, fatfs::FatType::Fat32, FatType::Fat32),
        ] {
            let d = disk(mb, kind, sample);
            let v = open(&d).unwrap();
            assert_eq!(
                (v.fat_type, v.label.as_str(), v.offset),
                (want, "TESTVOL", 0)
            );
            check_sample(&v);
        }
    }

    #[test]
    fn finds_the_file_system_in_an_mbr_partition() {
        // Our own formatter: an MBR with FAT16 1 MiB in, label in the root.
        let d = crate::fat::blank_fat16_image(8, "CARD", &["SAMPLES", "PATTERNS"]).unwrap();
        let v = open(&d).unwrap();
        assert_eq!(
            (v.fat_type, v.label.as_str(), v.offset),
            (FatType::Fat16, "CARD", 1 << 20)
        );
        let root = v.root().unwrap();
        assert_eq!(names(&root), ["SAMPLES", "PATTERNS"]);
        assert!(root.iter().all(|e| e.dir));
        assert!(v.dir(&root[0]).unwrap().is_empty(), "just . and ..");
        // And a fatfs volume in the second partition slot.
        let fs = disk(8, fatfs::FatType::Fat16, sample);
        let mut d = vec![0u8; 4096 * SECTOR + fs.len()];
        d[4096 * SECTOR..].copy_from_slice(&fs);
        d[0x1CE + 4] = 0x0E;
        d[0x1CE + 8..0x1CE + 12].copy_from_slice(&4096u32.to_le_bytes());
        d[510..512].copy_from_slice(&[0x55, 0xAA]);
        check_sample(&open(&d).unwrap());
    }

    #[test]
    fn our_fat32_disks_read_here_and_in_fatfs() {
        use std::io::{Read, Write};
        let mut d = crate::fat::blank_image(crate::fat::Fat::Fat32, 40, "CARD", &["DATA"]).unwrap();
        let v = open(&d).unwrap();
        assert_eq!(
            (v.fat_type, v.label.as_str(), v.offset),
            (FatType::Fat32, "CARD", 1 << 20)
        );
        let root = v.root().unwrap();
        assert_eq!(names(&root), ["DATA"]);
        assert!(v.dir(&root[0]).unwrap().is_empty(), "just . and ..");
        // fatfs agrees, and can write a file into the directory and read it back.
        let mut part = Cursor::new(d.split_off(1 << 20));
        {
            let fs = fatfs::FileSystem::new(&mut part, fatfs::FsOptions::new()).unwrap();
            assert_eq!(fs.fat_type(), fatfs::FatType::Fat32);
            assert_eq!(fs.volume_label(), "CARD");
            let stats = fs.stats().unwrap();
            assert_eq!(stats.free_clusters(), stats.total_clusters() - 2);
            let data = fs.root_dir().open_dir("DATA").unwrap();
            data.create_file("A.TXT")
                .unwrap()
                .write_all(b"hello")
                .unwrap();
            let mut back = String::new();
            data.open_file("A.TXT")
                .unwrap()
                .read_to_string(&mut back)
                .unwrap();
            assert_eq!(back, "hello");
        }
    }

    #[test]
    fn entry_names_and_dates_decode() {
        let mut e = [b' '; 32];
        e[..8].copy_from_slice(b"\x05ABC    ");
        e[8..11].copy_from_slice(b"TXT");
        e[12] = 0x10;
        assert_eq!(short_name(&e), "\u{e5}ABC.txt");
        e[12] = 0x08;
        e[8..11].copy_from_slice(b"   ");
        assert_eq!(short_name(&e), "\u{e5}abc");
        // 2026-10-29 21:48:16, as a directory entry stores it.
        e[22..24].copy_from_slice(&(21u16 << 11 | 48 << 5 | 8).to_le_bytes());
        e[24..26].copy_from_slice(&(46u16 << 9 | 10 << 5 | 29).to_le_bytes());
        e[11] = 0x20;
        let (entries, _) = parse_dir(&e, FatType::Fat16);
        assert_eq!(entries[0].modified, Some([2026, 10, 29, 21, 48, 16]));
    }

    #[test]
    fn broken_file_systems_are_errors() {
        assert!(open(&[]).is_err());
        assert!(open(&[0; 4096]).is_err());
        let d = disk(2, fatfs::FatType::Fat12, sample);
        // Cut off: the data region is gone.
        let data = open(&d).unwrap().data;
        let v = open(&d[..data]).unwrap();
        let root = v.root().unwrap();
        assert!(v.dir(&root[3]).is_err());
        // A chain that loops, and one into free space.
        let d = disk(40, fatfs::FatType::Fat32, sample);
        let v = open(&d).unwrap();
        let big = v.dir(&v.root().unwrap()[3]).unwrap().remove(0);
        let fat = v.fat;
        let mut looped = d.clone();
        let at = fat + big.cluster as usize * 4;
        looped[at..at + 4].copy_from_slice(&big.cluster.to_le_bytes());
        assert!(open(&looped).unwrap().read(&big, usize::MAX).is_err());
        let mut cut = d.clone();
        cut[at..at + 4].fill(0);
        assert!(open(&cut).unwrap().read(&big, usize::MAX).is_err());
        // A directory chain that loops stops at the volume size.
        let logs = v.root().unwrap().remove(3);
        let at = fat + logs.cluster as usize * 4;
        looped[at..at + 4].copy_from_slice(&logs.cluster.to_le_bytes());
        let v = open(&looped).unwrap();
        assert!(v.dir(&logs).is_err());
    }

    #[test]
    fn mangled_images_never_panic() {
        // Flip bytes in the boot sector, FATs and root of a small volume, and
        // walk whatever comes out.
        let d = disk(2, fatfs::FatType::Fat12, sample);
        let mut seed = 0x1234_5678u32;
        let mut rand = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..3000 {
            let mut m = d.clone();
            for _ in 0..1 + rand() % 8 {
                let at = rand() as usize % 24_576;
                m[at] = rand() as u8;
            }
            let Ok(v) = open(&m) else { continue };
            let mut todo = v.root().unwrap_or_default();
            let mut seen = 0;
            while let Some(e) = todo.pop() {
                seen += 1;
                if seen > 200 {
                    break;
                }
                if e.dir {
                    todo.extend(v.dir(&e).unwrap_or_default());
                } else {
                    let _ = v.read(&e, 1 << 20);
                }
            }
        }
    }
}
