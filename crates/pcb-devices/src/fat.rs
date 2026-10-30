//! A formatter for blank SD card images: an MBR with one FAT16 partition and
//! an empty file system in it, which is what `embedded-sdmmc` (and so the
//! BTL-01 OS) mounts as volume 0, optionally with some empty top-level
//! directories. A formatted disk is almost all zeros, so [`format_fat16`]
//! returns only the few regions that are not. [`read`] reads what is on one.
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
fn dir_entry(name: &[u8; 11], attr: u8, cluster: u16) -> Vec<u8> {
    let mut e = vec![0u8; 32];
    e[..11].copy_from_slice(name);
    e[11] = attr;
    for at in [16, 18, 24] {
        le16(&mut e, at, FAT_EPOCH); // created, accessed, written
    }
    le16(&mut e, 26, cluster);
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

/// The non-zero parts of a blank `megabytes` MiB disk (MBR, boot sector, both
/// FATs, and the root directory's label entry and `dirs`) with the volume
/// `label` (up to 11 characters; empty means none) and an empty directory in
/// the root for each of `dirs`. A label is a directory entry like any other
/// to `embedded-sdmmc`, so the BTL-01 OS lists it as an empty file named
/// after it: leave it off unless the disk is meant for a PC too.
pub fn format_fat16(megabytes: usize, label: &str, dirs: &[&str]) -> Result<Regions, String> {
    if !(MIN_MB..=MAX_MB).contains(&megabytes) {
        return Err(format!("size must be {MIN_MB}..{MAX_MB} MiB"));
    }
    if label.len() > 11 || !label.is_ascii() {
        return Err("label must be at most 11 ASCII characters".into());
    }
    let label = if label.is_empty() { NO_LABEL } else { label };
    let dirs = dirs
        .iter()
        .map(|d| short_name(d))
        .collect::<Result<Vec<_>, _>>()?;
    if dirs.len() > ROOT_ENTRIES - 1 {
        return Err("too many directories".into());
    }
    let total = megabytes * 1024 * 1024 / BLOCK;
    let sectors = total - START;
    let (spc, fat) = geometry(sectors).map_err(|e| format!("{megabytes} MiB: {e}"))?;

    // MBR: partition 0, bootable, type 0x06 (FAT16), CHS fields unused.
    let mut mbr = vec![0u8; BLOCK];
    mbr[446] = 0x80;
    mbr[447..450].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    mbr[450] = 0x06;
    mbr[451..454].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    le32(&mut mbr, 454, START as u32);
    le32(&mut mbr, 458, sectors as u32);
    mbr[510..512].copy_from_slice(&[0x55, 0xAA]);

    // FAT16 boot sector.
    let mut boot = vec![0u8; BLOCK];
    boot[..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    boot[3..11].copy_from_slice(b"MSDOS5.0");
    le16(&mut boot, 11, BLOCK as u16);
    boot[13] = spc as u8;
    le16(&mut boot, 14, RESERVED as u16);
    boot[16] = FATS as u8;
    le16(&mut boot, 17, ROOT_ENTRIES as u16);
    if sectors < 0x1_0000 {
        le16(&mut boot, 19, sectors as u16);
    } else {
        le32(&mut boot, 32, sectors as u32);
    }
    boot[21] = 0xF8;
    le16(&mut boot, 22, fat as u16);
    le16(&mut boot, 24, 63);
    le16(&mut boot, 26, 255);
    le32(&mut boot, 28, START as u32);
    boot[36] = 0x80;
    boot[38] = 0x29;
    le32(&mut boot, 39, 0x1234_5678);
    boot[43..54].copy_from_slice(format!("{label:<11}").as_bytes());
    boot[54..62].copy_from_slice(b"FAT16   ");
    boot[510..512].copy_from_slice(&[0x55, 0xAA]);

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
    let mut entries = Vec::new();
    if label != NO_LABEL {
        let mut name = [b' '; 11];
        name[..label.len()].copy_from_slice(label.as_bytes());
        entries.extend(dir_entry(&name, 0x08, 0));
    }
    for (i, name) in dirs.iter().enumerate() {
        let cluster = 2 + i as u16;
        entries.extend(dir_entry(name, 0x10, cluster));
        let mut cluster_bytes = dir_entry(b".          ", 0x10, cluster);
        cluster_bytes.extend(dir_entry(b"..         ", 0x10, 0)); // 0: the root
        regions.push((data + i * spc * BLOCK, cluster_bytes));
    }
    if !entries.is_empty() {
        regions.push((root, entries));
    }
    Ok(regions)
}

/// The whole blank disk in memory.
pub fn blank_fat16_image(megabytes: usize, label: &str, dirs: &[&str]) -> Result<Vec<u8>, String> {
    let mut image = vec![0u8; megabytes * 1024 * 1024];
    for (at, bytes) in format_fat16(megabytes, label, dirs)? {
        image[at..at + bytes.len()].copy_from_slice(&bytes);
    }
    Ok(image)
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
        let image = blank_fat16_image(16, "BTL01", &[]).unwrap();
        assert_eq!(image.len(), 16 * 1024 * 1024);
        assert_eq!(image[510..512], [0x55, 0xAA]);
        assert_eq!(image[450], 0x06);
        let boot = &image[START * BLOCK..][..BLOCK];
        assert_eq!(&boot[43..54], b"BTL01      ");
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
