//! A formatter for blank SD card images: an MBR with one FAT32 or FAT16
//! partition and an empty file system in it, which is what `embedded-sdmmc`
//! mounts as volume 0, optionally with some empty
//! top-level directories. A formatted disk is almost all zeros, so
//! [`format()`] returns only the few regions that are not. [`read`] reads
//! what is on one.
//!
//! Readers tell FAT16 from FAT32 by the cluster count alone (FAT32 has at
//! least 65525), so FAT32 needs a disk of at least [`Fat::Fat32`]'s
//! [`min_mb`](Fat::min_mb), even with the smallest clusters.
//!
//! `no_std` (+ alloc), with no knowledge of the emulator: the `sdcard`
//! device model in `pcb-devices` takes any image, and the front panel and
//! `tartelette mkdisk` use this crate to make and browse them.
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::{format, string::String, vec, vec::Vec};

pub mod read;

const BLOCK: usize = 512;
/// The partition starts 1 MiB in, as partitioners do for SD cards.
const START: usize = 2048;
const RESERVED: usize = 1;
const FATS: usize = 2;
const ROOT_ENTRIES: usize = 512;
const ROOT_SECTORS: usize = ROOT_ENTRIES * 32 / BLOCK;
/// FAT16 holds 4085..=65524 clusters; fewer is FAT12, more needs FAT32.
const MIN_CLUSTERS: usize = 4085;
const MAX_CLUSTERS: usize = 65524;
pub const MIN_MB: usize = 4;
pub const MAX_MB: usize = 2000;
/// FAT32: the reserved sectors before the FATs (boot sector, FSInfo, the
/// backup copies of both at 6 and 7), and the smallest cluster count.
const RESERVED32: usize = 32;
const FSINFO: usize = 1;
const BACKUP_BOOT: usize = 6;
const MIN_CLUSTERS32: usize = 65525;
pub const MIN_MB32: usize = 34;
pub const MAX_MB32: usize = 32 * 1024;

/// Which file system to put on a disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Fat {
    Fat16,
    #[default]
    Fat32,
}

impl Fat {
    /// The size range (MiB) the formatter supports.
    pub fn min_mb(self) -> usize {
        match self {
            Fat::Fat16 => MIN_MB,
            Fat::Fat32 => MIN_MB32,
        }
    }
    pub fn max_mb(self) -> usize {
        match self {
            Fat::Fat16 => MAX_MB,
            Fat::Fat32 => MAX_MB32,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Fat::Fat16 => "FAT16",
            Fat::Fat32 => "FAT32",
        }
    }
}

/// What to write, at which byte offset, onto an all-zero disk of `megabytes`.
pub type Regions = Vec<(usize, Vec<u8>)>;

fn le16(v: &mut [u8], at: usize, x: u16) {
    v[at..at + 2].copy_from_slice(&x.to_le_bytes());
}
fn le32(v: &mut [u8], at: usize, x: u32) {
    v[at..at + 4].copy_from_slice(&x.to_le_bytes());
}

/// Sectors per cluster and sectors per FAT for a partition of `sectors`: the
/// smallest cluster that keeps the count within FAT16, and a FAT big enough
/// for every cluster the partition could hold.
fn geometry(sectors: usize) -> Result<(usize, usize), String> {
    for spc in [1, 2, 4, 8, 16, 32, 64] {
        let fat = (2 * (sectors / spc + 2)).div_ceil(BLOCK);
        let data = sectors.saturating_sub(RESERVED + ROOT_SECTORS + FATS * fat);
        let clusters = data / spc;
        if clusters > MAX_CLUSTERS {
            continue;
        }
        return if clusters >= MIN_CLUSTERS {
            Ok((spc, fat))
        } else {
            Err(format!("too small for FAT16 ({clusters} clusters)"))
        };
    }
    Err("too big for FAT16".into())
}

/// The label a disk without one has.
const NO_LABEL: &str = "NO NAME";

/// 1 January 1980, the earliest date a FAT entry can have.
const FAT_EPOCH: u16 = 1 << 5 | 1;

/// A 32-byte directory entry (`attr` 0x10 for a directory, 0x08 for a label).
fn dir_entry(name: &[u8; 11], attr: u8, cluster: u32) -> Vec<u8> {
    let mut e = vec![0u8; 32];
    e[..11].copy_from_slice(name);
    e[11] = attr;
    for at in [16, 18, 24] {
        le16(&mut e, at, FAT_EPOCH); // created, accessed, written
    }
    le16(&mut e, 20, (cluster >> 16) as u16); // FAT32 only: zero below 65536
    le16(&mut e, 26, cluster as u16);
    e
}

/// `name` as an 8.3 short name: upper case, padded with spaces.
fn short_name(name: &str) -> Result<[u8; 11], String> {
    let ok = !name.is_empty()
        && name.len() <= 8
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !ok {
        return Err(format!(
            "directory {name:?}: use 1 to 8 letters, digits or underscores"
        ));
    }
    let mut short = [b' '; 11];
    short[..name.len()].copy_from_slice(name.to_ascii_uppercase().as_bytes());
    Ok(short)
}

/// Check the size, label and directory names; returns the label to write in
/// the boot sector and the directories' short names.
fn check<'a>(
    fat: Fat,
    megabytes: usize,
    label: &'a str,
    dirs: &[&str],
) -> Result<(&'a str, Vec<[u8; 11]>), String> {
    if !(fat.min_mb()..=fat.max_mb()).contains(&megabytes) {
        let hint = match fat {
            Fat::Fat32 if megabytes < MIN_MB32 => " (FAT16 can do smaller disks)",
            _ => "",
        };
        return Err(format!(
            "{} size must be {}..{} MiB{hint}",
            fat.name(),
            fat.min_mb(),
            fat.max_mb()
        ));
    }
    if label.len() > 11 || !label.is_ascii() {
        return Err("label must be at most 11 ASCII characters".into());
    }
    let label = if label.is_empty() { NO_LABEL } else { label };
    let dirs = dirs
        .iter()
        .map(|d| short_name(d))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((label, dirs))
}

/// An MBR with one bootable partition of `kind` from `START` on, `sectors`
/// long (CHS fields unused).
fn mbr(kind: u8, sectors: usize) -> Vec<u8> {
    let mut mbr = vec![0u8; BLOCK];
    mbr[446] = 0x80;
    mbr[447..450].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    mbr[450] = kind;
    mbr[451..454].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    le32(&mut mbr, 454, START as u32);
    le32(&mut mbr, 458, sectors as u32);
    mbr[510..512].copy_from_slice(&[0x55, 0xAA]);
    mbr
}

/// The BPB fields FAT16 and FAT32 share.
fn boot_common(spc: usize, reserved: usize) -> Vec<u8> {
    let mut boot = vec![0u8; BLOCK];
    le16(&mut boot, 11, BLOCK as u16);
    boot[13] = spc as u8;
    le16(&mut boot, 14, reserved as u16);
    boot[16] = FATS as u8;
    boot[21] = 0xF8;
    le16(&mut boot, 24, 63);
    le16(&mut boot, 26, 255);
    le32(&mut boot, 28, START as u32);
    boot[510..512].copy_from_slice(&[0x55, 0xAA]);
    boot
}

/// The root directory's entries: the label's (a real label has one), then
/// one per directory, whose clusters start at `first`.
fn root_entries(label: &str, dirs: &[[u8; 11]], first: u32) -> Vec<u8> {
    let mut entries = Vec::new();
    if label != NO_LABEL {
        let mut name = [b' '; 11];
        name[..label.len()].copy_from_slice(label.as_bytes());
        entries.extend(dir_entry(&name, 0x08, 0));
    }
    for (i, name) in dirs.iter().enumerate() {
        entries.extend(dir_entry(name, 0x10, first + i as u32));
    }
    entries
}

/// A new directory's first cluster: "." and ".." (0: the root).
fn dot_entries(cluster: u32) -> Vec<u8> {
    let mut bytes = dir_entry(b".          ", 0x10, cluster);
    bytes.extend(dir_entry(b"..         ", 0x10, 0));
    bytes
}

/// The non-zero parts of a blank disk with the file system `fat`; see
/// [`format_fat32`] and [`format_fat16`].
pub fn format(fat: Fat, megabytes: usize, label: &str, dirs: &[&str]) -> Result<Regions, String> {
    match fat {
        Fat::Fat16 => format_fat16(megabytes, label, dirs),
        Fat::Fat32 => format_fat32(megabytes, label, dirs),
    }
}

/// The whole blank disk in memory.
pub fn blank_image(
    fat: Fat,
    megabytes: usize,
    label: &str,
    dirs: &[&str],
) -> Result<Vec<u8>, String> {
    let regions = format(fat, megabytes, label, dirs)?;
    let mut image = vec![0u8; megabytes * 1024 * 1024];
    for (at, bytes) in regions {
        image[at..at + bytes.len()].copy_from_slice(&bytes);
    }
    Ok(image)
}

/// Sectors per cluster and sectors per FAT for a FAT32 partition of
/// `sectors`: the cluster size Windows picks for the size (512 bytes up to
/// 260 MB, then 4, 8 and 16 KiB), halved until there are enough clusters
/// for FAT32.
fn geometry32(sectors: usize) -> Result<(usize, usize), String> {
    let mut spc = match sectors {
        0..=532_480 => 1,
        532_481..=16_777_216 => 8,
        16_777_217..=33_554_432 => 16,
        _ => 32,
    };
    loop {
        let fat = (4 * (sectors / spc + 2)).div_ceil(BLOCK);
        let data = sectors.saturating_sub(RESERVED32 + FATS * fat);
        let clusters = data / spc;
        if clusters >= MIN_CLUSTERS32 {
            return Ok((spc, fat));
        }
        if spc == 1 {
            return Err(format!("too small for FAT32 ({clusters} clusters)"));
        }
        spc /= 2;
    }
}

/// The non-zero parts of a blank `megabytes` MiB FAT32 disk: the MBR, the
/// boot sector and FSInfo (and their backups), both FATs, and the root
/// directory (cluster 2) with the label entry and `dirs`. As
/// [`format_fat16`] for `label` and `dirs`.
pub fn format_fat32(megabytes: usize, label: &str, dirs: &[&str]) -> Result<Regions, String> {
    let (label, dirs) = check(Fat::Fat32, megabytes, label, dirs)?;
    let sectors = megabytes * 1024 * 1024 / BLOCK - START;
    let (spc, fat) = geometry32(sectors).map_err(|e| format!("{megabytes} MiB: {e}"))?;
    let clusters = (sectors - RESERVED32 - FATS * fat) / spc;
    let entries = root_entries(label, &dirs, 3);
    if entries.len() > spc * BLOCK {
        return Err("too many directories".into());
    }

    let mut boot = boot_common(spc, RESERVED32);
    boot[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    boot[3..11].copy_from_slice(b"MSWIN4.1");
    le32(&mut boot, 32, sectors as u32);
    le32(&mut boot, 36, fat as u32);
    le32(&mut boot, 44, 2); // the root directory's cluster
    le16(&mut boot, 48, FSINFO as u16);
    le16(&mut boot, 50, BACKUP_BOOT as u16);
    boot[64] = 0x80;
    boot[66] = 0x29;
    le32(&mut boot, 67, 0x1234_5678);
    boot[71..82].copy_from_slice(format!("{label:<11}").as_bytes());
    boot[82..90].copy_from_slice(b"FAT32   ");

    // FSInfo: the free cluster count and where to look for one.
    let used = 1 + dirs.len();
    let mut info = vec![0u8; BLOCK];
    le32(&mut info, 0, 0x4161_5252);
    le32(&mut info, 484, 0x6141_7272);
    le32(&mut info, 488, (clusters - used) as u32);
    le32(&mut info, 492, (2 + used) as u32);
    le32(&mut info, 508, 0xAA55_0000);

    // Both FATs: media descriptor, end-of-chain marker, then one
    // end-of-chain entry for the root directory and each directory.
    let mut fat_bytes = Vec::new();
    for v in [0x0FFF_FFF8u32, 0x0FFF_FFFF]
        .into_iter()
        .chain((0..used).map(|_| 0x0FFF_FFFF))
    {
        fat_bytes.extend(v.to_le_bytes());
    }
    let part = START * BLOCK;
    let mut regions = vec![
        (0, mbr(0x0C, sectors)),
        (part, boot.clone()),
        (part + FSINFO * BLOCK, info.clone()),
        (part + BACKUP_BOOT * BLOCK, boot),
        (part + (BACKUP_BOOT + 1) * BLOCK, info),
    ];
    for f in 0..FATS {
        regions.push(((START + RESERVED32 + f * fat) * BLOCK, fat_bytes.clone()));
    }
    let data = (START + RESERVED32 + FATS * fat) * BLOCK;
    if !entries.is_empty() {
        regions.push((data, entries));
    }
    for i in 0..dirs.len() {
        let cluster = 3 + i as u32;
        regions.push((data + (1 + i) * spc * BLOCK, dot_entries(cluster)));
    }
    Ok(regions)
}

/// The non-zero parts of a blank `megabytes` MiB disk (MBR, boot sector, both
/// FATs, and the root directory's label entry and `dirs`) with the volume
/// `label` (up to 11 characters; empty means none) and an empty directory in
/// the root for each of `dirs`. A label is a directory entry like any other
/// to `embedded-sdmmc`, so a firmware using it lists it as an empty file named
/// after it: leave it off unless the disk is meant for a PC too.
pub fn format_fat16(megabytes: usize, label: &str, dirs: &[&str]) -> Result<Regions, String> {
    let (label, dirs) = check(Fat::Fat16, megabytes, label, dirs)?;
    if dirs.len() > ROOT_ENTRIES - 1 {
        return Err("too many directories".into());
    }
    let total = megabytes * 1024 * 1024 / BLOCK;
    let sectors = total - START;
    let (spc, fat) = geometry(sectors).map_err(|e| format!("{megabytes} MiB: {e}"))?;

    // MBR: type 0x06 (FAT16).
    let mbr = mbr(0x06, sectors);

    // FAT16 boot sector.
    let mut boot = boot_common(spc, RESERVED);
    boot[..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    boot[3..11].copy_from_slice(b"MSDOS5.0");
    le16(&mut boot, 17, ROOT_ENTRIES as u16);
    if sectors < 0x1_0000 {
        le16(&mut boot, 19, sectors as u16);
    } else {
        le32(&mut boot, 32, sectors as u32);
    }
    le16(&mut boot, 22, fat as u16);
    boot[36] = 0x80;
    boot[38] = 0x29;
    le32(&mut boot, 39, 0x1234_5678);
    boot[43..54].copy_from_slice(format!("{label:<11}").as_bytes());
    boot[54..62].copy_from_slice(b"FAT16   ");

    // Both FATs start with the media descriptor and the end-of-chain marker,
    // then hold one end-of-chain entry per directory (one cluster each, from 2).
    let mut fat_bytes = vec![0xF8, 0xFF, 0xFF, 0xFF];
    fat_bytes.extend(dirs.iter().flat_map(|_| [0xFF, 0xFF]));
    let mut regions = vec![(0, mbr), (START * BLOCK, boot)];
    for f in 0..FATS {
        regions.push(((START + RESERVED + f * fat) * BLOCK, fat_bytes.clone()));
    }
    // The root directory: the label's entry (a real label has one), then the
    // directories. Each directory's cluster starts with "." and "..".
    let root = (START + RESERVED + FATS * fat) * BLOCK;
    let data = root + ROOT_SECTORS * BLOCK;
    let entries = root_entries(label, &dirs, 2);
    for i in 0..dirs.len() {
        regions.push((data + i * spc * BLOCK, dot_entries(2 + i as u32)));
    }
    if !entries.is_empty() {
        regions.push((root, entries));
    }
    Ok(regions)
}

/// The whole blank FAT16 disk in memory.
pub fn blank_fat16_image(megabytes: usize, label: &str, dirs: &[&str]) -> Result<Vec<u8>, String> {
    blank_image(Fat::Fat16, megabytes, label, dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_size_gets_a_valid_fat16_geometry() {
        for mb in [MIN_MB, 5, 16, 32, 33, 64, 100, 128, 512, 1024, 2000] {
            let sectors = mb * 1024 * 1024 / BLOCK - START;
            let (spc, fat) = geometry(sectors).unwrap_or_else(|e| panic!("{mb} MiB: {e}"));
            let clusters = (sectors - RESERVED - ROOT_SECTORS - FATS * fat) / spc;
            assert!(
                (MIN_CLUSTERS..=MAX_CLUSTERS).contains(&clusters),
                "{mb} MiB: {clusters} clusters"
            );
            assert!(
                fat * BLOCK >= 2 * (clusters + 2),
                "{mb} MiB: the FAT must cover every cluster"
            );
            assert!(spc.is_power_of_two() && spc <= 64);
        }
    }

    #[test]
    fn sizes_and_labels_out_of_range_are_refused() {
        assert!(format_fat16(MIN_MB - 1, "X", &[]).is_err());
        assert!(format_fat16(MAX_MB + 1, "X", &[]).is_err());
        assert!(format_fat16(16, "TWELVE CHARS", &[]).is_err());
        assert!(format_fat16(16, "ÄÖ", &[]).is_err());
        assert!(format_fat16(16, "", &[]).is_ok());
        assert_eq!(
            format_fat16(16, "", &[]).unwrap().len(),
            4,
            "no label entry without a label"
        );
    }

    #[test]
    fn regions_describe_one_partition_and_two_fats() {
        let image = blank_fat16_image(16, "SDCARD", &[]).unwrap();
        assert_eq!(image.len(), 16 * 1024 * 1024);
        assert_eq!(image[510..512], [0x55, 0xAA]);
        assert_eq!(image[450], 0x06);
        let boot = &image[START * BLOCK..][..BLOCK];
        assert_eq!(&boot[43..54], b"SDCARD     ");
        assert_eq!(&boot[54..62], b"FAT16   ");
        let fat = u16::from_le_bytes([boot[22], boot[23]]) as usize;
        for f in 0..FATS {
            let at = (START + RESERVED + f * fat) * BLOCK;
            assert_eq!(image[at..at + 4], [0xF8, 0xFF, 0xFF, 0xFF]);
        }
        // Everything else is zero.
        let set = image.iter().filter(|&&b| b != 0).count();
        assert!(set < 100, "{set} non-zero bytes");
    }

    #[test]
    fn directory_names_must_be_plain_short_names() {
        for bad in ["", "toolongname", "a b", "a.b", "ä", "a/b"] {
            assert!(format_fat16(16, "", &[bad]).is_err(), "{bad:?}");
        }
        assert!(format_fat16(16, "", &["data", "Songs_2"]).is_ok());
    }

    #[test]
    fn directories_get_a_cluster_each_with_dot_entries() {
        let image = blank_fat16_image(16, "", &["data", "x"]).unwrap();
        let boot = &image[START * BLOCK..][..BLOCK];
        let (spc, fat) = (
            boot[13] as usize,
            u16::from_le_bytes([boot[22], boot[23]]) as usize,
        );
        let root = (START + RESERVED + FATS * fat) * BLOCK;
        assert_eq!(&image[root..root + 12], b"DATA       \x10");
        assert_eq!(&image[root + 32..root + 44], b"X          \x10");
        assert_eq!(u16::from_le_bytes([image[root + 26], image[root + 27]]), 2);
        assert_eq!(u16::from_le_bytes([image[root + 58], image[root + 59]]), 3);
        let data = root + ROOT_SECTORS * BLOCK;
        assert_eq!(&image[data..data + 11], b".          ");
        assert_eq!(&image[data + 32..data + 43], b"..         ");
        assert_eq!(&image[data + spc * BLOCK..][..11], b".          ");
        // Their FAT entries end the chains.
        let fat0 = (START + RESERVED) * BLOCK;
        assert_eq!(image[fat0 + 4..fat0 + 8], [0xFF; 4]);
    }

    #[test]
    fn every_fat32_size_gets_enough_clusters_and_a_big_enough_fat() {
        let sectors = |mb: usize| mb * 1024 * 1024 / BLOCK - START;
        assert!(
            geometry32(sectors(MIN_MB32 - 1)).is_err(),
            "MIN_MB32 is the least"
        );
        for mb in [MIN_MB32, 64, 260, 1024, 8192, 16 * 1024, MAX_MB32] {
            let (spc, fat) = geometry32(sectors(mb)).unwrap_or_else(|e| panic!("{mb} MiB: {e}"));
            let clusters = (sectors(mb) - RESERVED32 - FATS * fat) / spc;
            assert!(clusters >= MIN_CLUSTERS32, "{mb} MiB: {clusters} clusters");
            assert!(fat * BLOCK >= 4 * (clusters + 2), "{mb} MiB: FAT too small");
            assert!(spc.is_power_of_two() && spc <= 32);
        }
        assert_eq!(
            geometry32(sectors(1024)).unwrap().0,
            8,
            "4 KiB clusters at 1 GiB"
        );
        assert!(
            format_fat32(MIN_MB32 - 1, "", &[])
                .unwrap_err()
                .contains("FAT16 can do")
        );
        assert!(format_fat32(MAX_MB32 + 1, "", &[]).is_err());
    }

    #[test]
    fn a_fat32_disk_has_its_boot_sector_fsinfo_backups_and_root_cluster() {
        let image = blank_image(Fat::Fat32, 64, "SDCARD", &["data"]).unwrap();
        assert_eq!(image[450], 0x0C);
        let part = START * BLOCK;
        let boot = &image[part..][..BLOCK];
        assert_eq!(&boot[82..90], b"FAT32   ");
        assert_eq!(&boot[71..82], b"SDCARD     ");
        assert_eq!(boot[510..512], [0x55, 0xAA]);
        assert_eq!(u16::from_le_bytes([boot[17], boot[18]]), 0, "no fixed root");
        assert_eq!(
            u16::from_le_bytes([boot[22], boot[23]]),
            0,
            "no 16-bit FAT size"
        );
        assert_eq!(&image[part + BACKUP_BOOT * BLOCK..][..BLOCK], boot);
        let info = &image[part + BLOCK..][..BLOCK];
        assert_eq!(&info[..4], b"RRaA");
        assert_eq!(&image[part + 7 * BLOCK..][..BLOCK], info);
        let (spc, fat) = (
            boot[13] as usize,
            u32::from_le_bytes(boot[36..40].try_into().unwrap()) as usize,
        );
        let fat0 = (START + RESERVED32) * BLOCK;
        let entry = |n: usize| u32::from_le_bytes(image[fat0 + 4 * n..][..4].try_into().unwrap());
        assert_eq!(
            (entry(0), entry(1), entry(2), entry(3), entry(4)),
            (0x0FFF_FFF8, 0x0FFF_FFFF, 0x0FFF_FFFF, 0x0FFF_FFFF, 0)
        );
        let data = (START + RESERVED32 + FATS * fat) * BLOCK;
        assert_eq!(&image[data..data + 12], b"SDCARD     \x08");
        assert_eq!(&image[data + 32..data + 44], b"DATA       \x10");
        assert_eq!(&image[data + spc * BLOCK..][..11], b".          ");
        let clusters = (64 * 2048 - START - RESERVED32 - FATS * fat) / spc;
        assert_eq!(
            u32::from_le_bytes(info[488..492].try_into().unwrap()) as usize,
            clusters - 2,
            "free: all but the root's and data's clusters"
        );
    }

    #[test]
    fn a_big_disk_uses_the_32_bit_sector_count() {
        let regions = format_fat16(1024, "", &[]).unwrap();
        let boot = &regions[1].1;
        assert_eq!(u16::from_le_bytes([boot[19], boot[20]]), 0);
        assert_eq!(
            u32::from_le_bytes(boot[32..36].try_into().unwrap()) as usize,
            1024 * 2048 - START
        );
    }
}
