//! A formatter for SD card images: an MBR with one FAT32 or FAT16 partition
//! and a file system in it, which is what `embedded-sdmmc` mounts as volume
//! 0, either empty (optionally with some empty top-level directories) or
//! with files and directories put on it ([`format_with`]). A formatted disk
//! is mostly zeros, so the formatter returns only the regions that are not.
//! [`read`] reads what is on one.
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

/// A file or directory to put on a new disk ([`format_with`]), by its name
/// in the directory that holds it. A name that isn't an 8.3 one (in one
/// case, of the characters short names allow) gets a long name, with a
/// short alias made as Windows makes them (`LONGNA~1.TXT`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    File { name: String, data: Vec<u8> },
    Dir { name: String, items: Vec<Item> },
}

impl Item {
    pub fn name(&self) -> &str {
        match self {
            Item::File { name, .. } | Item::Dir { name, .. } => name,
        }
    }
}

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

/// A `--dir` name: 1 to 8 letters, digits or underscores, made upper case.
fn check_dir(name: &str) -> Result<Item, String> {
    let ok = !name.is_empty()
        && name.len() <= 8
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !ok {
        return Err(format!(
            "directory {name:?}: use 1 to 8 letters, digits or underscores"
        ));
    }
    Ok(Item::Dir {
        name: name.to_ascii_uppercase(),
        items: Vec::new(),
    })
}

/// A name a file or directory can have: not empty, `.` or `..`, without the
/// characters Windows refuses or a trailing dot or space, and at most the
/// 255 UTF-16 units a long name holds.
fn check_name(name: &str) -> Result<(), String> {
    let bad = name.is_empty()
        || name.ends_with(['.', ' '])
        || name.chars().any(|c| c < ' ' || "\"*/:<>?\\|".contains(c))
        || name.encode_utf16().count() > 255;
    if bad {
        return Err(format!("{name:?}: not a name a FAT file system can hold"));
    }
    Ok(())
}

/// How an item is named in its directory: its 8.3 entry's name, with the
/// case flags of byte 12 (base and extension in lower case), and its long
/// name if it needs one.
struct Name {
    short: [u8; 11],
    case: u8,
    long: Option<Vec<u16>>,
}

impl Name {
    /// The directory entries it takes: 13 UTF-16 units per long-name entry.
    fn entries(&self) -> usize {
        1 + self.long.as_ref().map_or(0, |l| l.len().div_ceil(13))
    }
}

/// What a short name may hold besides upper-case letters and digits.
fn short_char(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit() || b"!#$%&'()-@^_`{}~".contains(&b)
}

/// `name` as an 8.3 name with its case flags, if it is one: a base of 1 to
/// 8 characters, an extension of up to 3, each all in upper or lower case.
fn plain_short(name: &str) -> Option<([u8; 11], u8)> {
    let (base, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    if base.is_empty() || name.ends_with('.') {
        return None;
    }
    let mut short = [b' '; 11];
    let mut case = 0;
    for (part, max, at, flag) in [(base, 8, 0, 0x08), (ext, 3, 8, 0x10)] {
        let upper = part.to_ascii_uppercase();
        if part.len() > max || !upper.bytes().all(short_char) {
            return None;
        }
        let lower = part.bytes().any(|b| b.is_ascii_lowercase());
        if lower && part.bytes().any(|b| b.is_ascii_uppercase()) {
            return None;
        }
        if lower {
            case |= flag;
        }
        short[at..at + part.len()].copy_from_slice(upper.as_bytes());
    }
    Some((short, case))
}

/// A short alias for a long name, not in `taken`: the name upper cased,
/// without spaces or leading dots, other characters short names can't hold
/// as `_`; then as much of its base as fits before `~N`, and the first 3
/// characters of its extension.
fn alias(name: &str, taken: &[[u8; 11]]) -> [u8; 11] {
    let clean = |s: &str| -> Vec<u8> {
        s.chars()
            .filter(|&c| c != ' ' && c != '.')
            .map(|c| {
                let c = c.to_ascii_uppercase();
                if c.is_ascii() && short_char(c as u8) {
                    c as u8
                } else {
                    b'_'
                }
            })
            .collect()
    };
    let name = name.trim_start_matches('.');
    let (base, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    let (mut base, ext) = (clean(base), clean(ext));
    if base.is_empty() {
        base.push(b'_');
    }
    let mut short = [b' '; 11];
    let ext = &ext[..ext.len().min(3)];
    short[8..8 + ext.len()].copy_from_slice(ext);
    for n in 1.. {
        let tail = format!("~{n}");
        let keep = base.len().min(8 - tail.len());
        short[..8].fill(b' ');
        short[..keep].copy_from_slice(&base[..keep]);
        short[keep..keep + tail.len()].copy_from_slice(tail.as_bytes());
        if !taken.contains(&short) {
            break;
        }
    }
    short
}

/// The names of a directory's items: checked, told apart regardless of
/// case, the 8.3 ones first so that no alias takes their short name.
fn name_items(items: &[Item]) -> Result<Vec<Name>, String> {
    let mut seen = Vec::new();
    let mut taken = Vec::new();
    let mut names: Vec<Option<Name>> = Vec::new();
    for item in items {
        let name = item.name();
        check_name(name)?;
        let folded = name.to_lowercase();
        if seen.contains(&folded) {
            return Err(format!("{name:?}: two items with that name"));
        }
        seen.push(folded);
        names.push(plain_short(name).map(|(short, case)| {
            taken.push(short);
            Name {
                short,
                case,
                long: None,
            }
        }));
    }
    Ok(items
        .iter()
        .zip(names)
        .map(|(item, name)| {
            name.unwrap_or_else(|| {
                let short = alias(item.name(), &taken);
                taken.push(short);
                Name {
                    short,
                    case: 0,
                    long: Some(item.name().encode_utf16().collect()),
                }
            })
        })
        .collect())
}

/// The long-name entries before short entry `short`, last piece first, as
/// they are on disk: 13 units each, ended by a 0 and padded with 0xFFFF.
fn long_entries(long: &[u16], short: &[u8; 11]) -> Vec<u8> {
    let sum = short
        .iter()
        .fold(0u8, |s, &c| s.rotate_right(1).wrapping_add(c));
    let pieces = long.len().div_ceil(13);
    let mut out = Vec::new();
    for n in (1..=pieces).rev() {
        let mut e = [0u8; 32];
        e[0] = n as u8 | if n == pieces { 0x40 } else { 0 };
        e[11] = 0x0F;
        e[13] = sum;
        let offsets = (1..11)
            .step_by(2)
            .chain((14..26).step_by(2))
            .chain([28, 30]);
        for (i, at) in offsets.enumerate() {
            let k = (n - 1) * 13 + i;
            let unit = match k.cmp(&long.len()) {
                core::cmp::Ordering::Less => long[k],
                core::cmp::Ordering::Equal => 0,
                core::cmp::Ordering::Greater => 0xFFFF,
            };
            le16(&mut e, at, unit);
        }
        out.extend(e);
    }
    out
}

/// The bytes of a directory's entries: `head` of them, then its items'.
fn dir_bytes(head: usize, names: &[Name]) -> usize {
    (head + names.iter().map(Name::entries).sum::<usize>()) * 32
}

/// Check the size and label, and the `--dir` names; returns the label to
/// write in the boot sector and the items to put in the root: the
/// directories, then `items`.
fn check<'a>(
    fat: Fat,
    megabytes: usize,
    label: &'a str,
    dirs: &[&str],
    items: &[Item],
) -> Result<(&'a str, Vec<Item>), String> {
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
    let mut root = dirs
        .iter()
        .map(|d| check_dir(d))
        .collect::<Result<Vec<_>, _>>()?;
    root.extend_from_slice(items);
    Ok((label, root))
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

/// The root directory's label entry, if the disk has a label.
fn label_entry(label: &str) -> Vec<u8> {
    if label == NO_LABEL {
        return Vec::new();
    }
    let mut name = [b' '; 11];
    name[..label.len()].copy_from_slice(label.as_bytes());
    dir_entry(&name, 0x08, 0)
}

/// A directory's first entries: "." (its own cluster) and ".." (its
/// parent's, 0 for the root).
fn dot_entries(cluster: u32, parent: u32) -> Vec<u8> {
    let mut bytes = dir_entry(b".          ", 0x10, cluster);
    bytes.extend(dir_entry(b"..         ", 0x10, parent));
    bytes
}

/// Clusters handed out in order from cluster 2, each file and directory in
/// one run of them, and what goes in them.
struct Layout {
    /// Byte offset of cluster 2.
    data: usize,
    cluster_bytes: usize,
    /// How many clusters the volume has.
    clusters: usize,
    /// Each run: its first cluster and length.
    runs: Vec<(u32, usize)>,
    next: u32,
    regions: Regions,
}

impl Layout {
    fn new(data: usize, cluster_bytes: usize, clusters: usize) -> Self {
        Self {
            data,
            cluster_bytes,
            clusters,
            runs: Vec::new(),
            next: 2,
            regions: Vec::new(),
        }
    }

    /// The first of enough clusters in a row for `bytes`; 0 for none, as
    /// an empty file has.
    fn alloc(&mut self, bytes: usize) -> Result<u32, String> {
        let n = bytes.div_ceil(self.cluster_bytes);
        if n == 0 {
            return Ok(0);
        }
        let first = self.next;
        if first as usize - 2 + n > self.clusters {
            return Err("the files don't fit on the disk".into());
        }
        self.runs.push((first, n));
        self.next += n as u32;
        Ok(first)
    }

    fn put(&mut self, cluster: u32, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            let at = self.data + (cluster as usize - 2) * self.cluster_bytes;
            self.regions.push((at, bytes));
        }
    }

    /// A directory's entries: `head` (its label, or "." and ".."), then one
    /// (or more, with a long name) per item, putting each item on the disk
    /// as it goes. `me` is what the subdirectories' ".." point at.
    fn dir(
        &mut self,
        items: &[Item],
        names: &[Name],
        me: u32,
        mut head: Vec<u8>,
    ) -> Result<Vec<u8>, String> {
        for (item, name) in items.iter().zip(names) {
            let (attr, cluster, size) = match item {
                Item::File { data, name } => {
                    let size = u32::try_from(data.len())
                        .map_err(|_| format!("{name:?}: too big for FAT"))?;
                    let c = self.alloc(data.len())?;
                    self.put(c, data.clone());
                    (0x20, c, size)
                }
                Item::Dir { items, .. } => {
                    let names = name_items(items)?;
                    let c = self.alloc(dir_bytes(2, &names))?;
                    let bytes = self.dir(items, &names, c, dot_entries(c, me))?;
                    self.put(c, bytes);
                    (0x10, c, 0)
                }
            };
            if let Some(long) = &name.long {
                head.extend(long_entries(long, &name.short));
            }
            let mut e = dir_entry(&name.short, attr, cluster);
            e[12] = name.case;
            le32(&mut e, 28, size);
            head.extend(e);
        }
        Ok(head)
    }

    /// The FAT, as entries of `width` bytes: the media descriptor and the
    /// end-of-chain marker `end`, then each run's clusters chained, its
    /// last ending the chain.
    fn fat(&self, end: u32, width: usize) -> Vec<u8> {
        let mut fat = vec![0u32; self.next as usize];
        fat[0] = end & !7;
        fat[1] = end;
        for &(first, n) in &self.runs {
            for c in first..first + n as u32 {
                fat[c as usize] = c + 1;
            }
            fat[first as usize + n - 1] = end;
        }
        fat.iter()
            .flat_map(|&v| v.to_le_bytes().into_iter().take(width))
            .collect()
    }
}

/// The non-zero parts of a blank disk with the file system `fat`; see
/// [`format_fat32`] and [`format_fat16`].
pub fn format(fat: Fat, megabytes: usize, label: &str, dirs: &[&str]) -> Result<Regions, String> {
    format_with(fat, megabytes, label, dirs, &[])
}

/// As [`format`], with `items` in the root directory after `dirs`, laid
/// out one after the other from the start of the data area.
pub fn format_with(
    fat: Fat,
    megabytes: usize,
    label: &str,
    dirs: &[&str],
    items: &[Item],
) -> Result<Regions, String> {
    let (label, root) = check(fat, megabytes, label, dirs, items)?;
    match fat {
        Fat::Fat16 => fat16(megabytes, label, &root),
        Fat::Fat32 => fat32(megabytes, label, &root),
    }
}

/// The whole blank disk in memory.
pub fn blank_image(
    fat: Fat,
    megabytes: usize,
    label: &str,
    dirs: &[&str],
) -> Result<Vec<u8>, String> {
    image_with(fat, megabytes, label, dirs, &[])
}

/// The whole disk of [`format_with`] in memory.
pub fn image_with(
    fat: Fat,
    megabytes: usize,
    label: &str,
    dirs: &[&str],
    items: &[Item],
) -> Result<Vec<u8>, String> {
    let regions = format_with(fat, megabytes, label, dirs, items)?;
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
    format_with(Fat::Fat32, megabytes, label, dirs, &[])
}

fn fat32(megabytes: usize, label: &str, root: &[Item]) -> Result<Regions, String> {
    let sectors = megabytes * 1024 * 1024 / BLOCK - START;
    let (spc, fat) = geometry32(sectors).map_err(|e| format!("{megabytes} MiB: {e}"))?;
    let clusters = (sectors - RESERVED32 - FATS * fat) / spc;
    let data = (START + RESERVED32 + FATS * fat) * BLOCK;
    let mut layout = Layout::new(data, spc * BLOCK, clusters);
    // The root directory comes first, at cluster 2, as long as it needs.
    let names = name_items(root)?;
    let head = label_entry(label);
    let first = layout.alloc(dir_bytes(head.len() / 32, &names).max(1))?;
    let entries = layout.dir(root, &names, 0, head)?;
    layout.put(first, entries);

    let mut boot = boot_common(spc, RESERVED32);
    boot[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    boot[3..11].copy_from_slice(b"MSWIN4.1");
    le32(&mut boot, 32, sectors as u32);
    le32(&mut boot, 36, fat as u32);
    le32(&mut boot, 44, first);
    le16(&mut boot, 48, FSINFO as u16);
    le16(&mut boot, 50, BACKUP_BOOT as u16);
    boot[64] = 0x80;
    boot[66] = 0x29;
    le32(&mut boot, 67, 0x1234_5678);
    boot[71..82].copy_from_slice(format!("{label:<11}").as_bytes());
    boot[82..90].copy_from_slice(b"FAT32   ");

    // FSInfo: the free cluster count and where to look for one.
    let used = layout.next as usize - 2;
    let mut info = vec![0u8; BLOCK];
    le32(&mut info, 0, 0x4161_5252);
    le32(&mut info, 484, 0x6141_7272);
    le32(&mut info, 488, (clusters - used) as u32);
    le32(&mut info, 492, layout.next);
    le32(&mut info, 508, 0xAA55_0000);

    let part = START * BLOCK;
    let mut regions = vec![
        (0, mbr(0x0C, sectors)),
        (part, boot.clone()),
        (part + FSINFO * BLOCK, info.clone()),
        (part + BACKUP_BOOT * BLOCK, boot),
        (part + (BACKUP_BOOT + 1) * BLOCK, info),
    ];
    let fat_bytes = layout.fat(0x0FFF_FFFF, 4);
    for f in 0..FATS {
        regions.push(((START + RESERVED32 + f * fat) * BLOCK, fat_bytes.clone()));
    }
    regions.append(&mut layout.regions);
    Ok(regions)
}

/// The non-zero parts of a blank `megabytes` MiB disk (MBR, boot sector, both
/// FATs, and the root directory's label entry and `dirs`) with the volume
/// `label` (up to 11 characters; empty means none) and an empty directory in
/// the root for each of `dirs`. A label is a directory entry like any other
/// to `embedded-sdmmc`, so a firmware using it lists it as an empty file named
/// after it: leave it off unless the disk is meant for a PC too.
pub fn format_fat16(megabytes: usize, label: &str, dirs: &[&str]) -> Result<Regions, String> {
    format_with(Fat::Fat16, megabytes, label, dirs, &[])
}

fn fat16(megabytes: usize, label: &str, root: &[Item]) -> Result<Regions, String> {
    let total = megabytes * 1024 * 1024 / BLOCK;
    let sectors = total - START;
    let (spc, fat) = geometry(sectors).map_err(|e| format!("{megabytes} MiB: {e}"))?;
    let clusters = (sectors - RESERVED - ROOT_SECTORS - FATS * fat) / spc;

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

    // The root directory is a fixed area before the data clusters, which
    // start at 2.
    let root_at = (START + RESERVED + FATS * fat) * BLOCK;
    let data = root_at + ROOT_SECTORS * BLOCK;
    let mut layout = Layout::new(data, spc * BLOCK, clusters);
    let names = name_items(root)?;
    let head = label_entry(label);
    if dir_bytes(head.len() / 32, &names) > ROOT_ENTRIES * 32 {
        return Err(format!(
            "too many items for the root directory ({ROOT_ENTRIES} entries)"
        ));
    }
    let entries = layout.dir(root, &names, 0, head)?;

    let mut regions = vec![(0, mbr), (START * BLOCK, boot)];
    let fat_bytes = layout.fat(0xFFFF, 2);
    for f in 0..FATS {
        regions.push(((START + RESERVED + f * fat) * BLOCK, fat_bytes.clone()));
    }
    regions.append(&mut layout.regions);
    if !entries.is_empty() {
        regions.push((root_at, entries));
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

    fn file(name: &str, data: &[u8]) -> Item {
        Item::File {
            name: name.into(),
            data: data.into(),
        }
    }

    fn dir(name: &str, items: Vec<Item>) -> Item {
        Item::Dir {
            name: name.into(),
            items,
        }
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 13 + i / 509) as u8).collect()
    }

    /// Short and long names, an empty file, files over several clusters,
    /// nested directories, and one with more entries than a cluster holds.
    fn sample() -> Vec<Item> {
        let many = (0..40)
            .map(|i| file(&format!("a long name number {i}.txt"), &[i as u8]))
            .collect();
        vec![
            file("README.TXT", b"hello\n"),
            file("donsol.rom", &pattern(12_953)),
            file("audio.channels.rom", &pattern(1227)),
            file(".theme", b""),
            dir(
                "Demos",
                vec![dir("deeper", vec![file("x.bin", &pattern(5000))])],
            ),
            dir("many", many),
        ]
    }

    #[test]
    fn items_read_back_here_and_in_fatfs() {
        use std::io::{Cursor, Read};
        for (fat, mb) in [(Fat::Fat16, 8), (Fat::Fat32, 40)] {
            let mut d = image_with(fat, mb, "", &["data"], &sample()).unwrap();
            let v = read::open(&d).unwrap();
            let root = v.root().unwrap();
            let names: Vec<_> = root.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(
                names,
                [
                    "DATA",
                    "README.TXT",
                    "donsol.rom",
                    "audio.channels.rom",
                    ".theme",
                    "Demos",
                    "many"
                ],
                "{fat:?}"
            );
            assert_eq!(v.read(&root[2], usize::MAX).unwrap(), pattern(12_953));
            assert_eq!((root[4].size, root[4].cluster), (0, 0));
            let demos = v.dir(&root[5]).unwrap();
            let deeper = v.dir(&demos[0]).unwrap();
            assert_eq!(v.read(&deeper[0], usize::MAX).unwrap(), pattern(5000));
            let many = v.dir(&root[6]).unwrap();
            assert_eq!(many.len(), 40);
            assert_eq!(many[39].name, "a long name number 39.txt");
            assert_eq!(v.read(&many[39], 1).unwrap(), [39]);

            let mut part = Cursor::new(d.split_off(1 << 20));
            let fs = fatfs::FileSystem::new(&mut part, fatfs::FsOptions::new()).unwrap();
            let mut back = Vec::new();
            fs.root_dir()
                .open_file("Demos/deeper/x.bin")
                .unwrap()
                .read_to_end(&mut back)
                .unwrap();
            assert_eq!(back, pattern(5000));
            let short: Vec<_> = fs
                .root_dir()
                .iter()
                .map(|e| e.unwrap().short_file_name())
                .collect();
            assert_eq!(
                short,
                [
                    "DATA",
                    "README.TXT",
                    "DONSOL.ROM",
                    "AUDIOC~1.ROM",
                    "THEME~1",
                    "DEMOS~1",
                    "MANY"
                ]
            );
            let many = fs.root_dir().open_dir("many").unwrap();
            assert_eq!(many.iter().count(), 42, "40 and . and ..");
            // The free count fatfs works out from the FAT matches FSInfo's.
            let stats = fs.stats().unwrap();
            assert!(stats.free_clusters() < stats.total_clusters());
        }
    }

    #[test]
    fn aliases_count_up_past_taken_short_names() {
        let items = [
            file("LONGNA~1.TXT", b""),
            file("long name 1.txt", b""),
            file("Long Name 2.txt", b""),
            file("café", b""),
        ];
        let names = name_items(&items).unwrap();
        let shorts: Vec<_> = names.iter().map(|n| &n.short).collect();
        assert_eq!(
            shorts,
            [
                b"LONGNA~1TXT",
                b"LONGNA~2TXT",
                b"LONGNA~3TXT",
                b"CAF_~1     "
            ]
        );
        assert!(names[0].long.is_none() && names[1].long.is_some());
        let (short, case) = plain_short("donsol.Rom").map_or((None, 0), |(s, c)| (Some(s), c));
        assert_eq!((short, case), (None, 0), "mixed case needs a long name");
        assert_eq!(plain_short("abc.TXT").unwrap().1, 0x08);
    }

    #[test]
    fn bad_names_and_full_disks_are_refused() {
        for bad in ["", ".", "..", "a/b", "a:b", "trailing.", "x\u{1}"] {
            let r = format_with(Fat::Fat16, 8, "", &[], &[file(bad, b"")]);
            assert!(r.is_err(), "{bad:?}");
        }
        let twins = [file("A.TXT", b""), file("a.txt", b"")];
        assert!(format_with(Fat::Fat16, 8, "", &[], &twins).is_err());
        let big = [file("BIG", &vec![1; 5 * 1024 * 1024])];
        assert!(
            format_with(Fat::Fat16, 4, "", &[], &big)
                .unwrap_err()
                .contains("don't fit")
        );
        let crowd: Vec<_> = (0..600).map(|i| file(&format!("F{i}"), b"")).collect();
        assert!(format_with(Fat::Fat16, 8, "", &[], &crowd).is_err());
        assert!(format_with(Fat::Fat32, 40, "", &[], &crowd).is_ok());
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
