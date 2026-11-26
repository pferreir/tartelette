//! `mkdisk`: an SD card image for `--sd`, blank or with files copied in.
//! `tartelette mkdisk` runs it, and so does this crate's own binary.
use std::{
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use clap::Parser;
use sdimage::{Fat, Item};

/// Make a blank SD card image: an MBR with one empty FAT32 (or FAT16)
/// partition, as `embedded-sdmmc` mounts it (the file is sparse).
#[derive(Parser, Debug)]
#[command(name = "mkdisk")]
pub struct MkdiskArgs {
    pub file: PathBuf,
    /// Size in MiB: FAT32 34..32768, FAT16 4..2000
    #[arg(long, default_value_t = 64)]
    pub size_mb: u32,
    /// File system: 32 (needs 34 MiB or more) or 16
    #[arg(long, default_value = "32", value_parser = ["16", "32"])]
    pub fat: String,
    /// Make an empty top-level directory (the OS lists `data`, so its Load menu needs `--dir data`)
    #[arg(long, value_name = "NAME")]
    pub dir: Vec<String>,
    /// Copy a file or a directory (with everything in it) into the top-level
    /// directory, under its own name; names that aren't 8.3 get long names
    #[arg(long, value_name = "PATH")]
    pub add: Vec<PathBuf>,
    /// Volume label; it shows up in the OS's file list as an empty file, so
    /// it is best left off unless the disk is also meant for a PC
    #[arg(long, default_value = "NO NAME")]
    pub label: String,
    /// Replace an existing file
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: &MkdiskArgs) -> Result<(), String> {
    let MkdiskArgs {
        file,
        size_mb,
        fat,
        dir,
        add,
        label,
        force,
    } = args;
    let fat = match fat.as_str() {
        "16" => Fat::Fat16,
        _ => Fat::Fat32,
    };
    let dirs: Vec<&str> = dir.iter().map(String::as_str).collect();
    let items = add.iter().map(|p| item(p)).collect::<Result<Vec<_>, _>>()?;
    let regions = sdimage::format_with(fat, *size_mb as usize, label, &dirs, &items)
        .map_err(|e| format!("mkdisk: {e}"))?;
    let write = || -> std::io::Result<()> {
        let mut f = if *force {
            std::fs::File::create(file)?
        } else {
            std::fs::File::create_new(file)?
        };
        f.set_len(*size_mb as u64 * 1024 * 1024)?;
        for (at, bytes) in regions {
            f.seek(SeekFrom::Start(at as u64))?;
            f.write_all(&bytes)?;
        }
        Ok(())
    };
    let file = file.display();
    match write() {
        Ok(()) => {
            let (files, bytes) = count(&items);
            eprintln!(
                "{file}: {size_mb} MiB, {} \"{label}\"{}{}",
                fat.name(),
                dirs.iter().map(|d| format!(", {d}/")).collect::<String>(),
                if items.is_empty() {
                    String::new()
                } else {
                    format!(", {files} files of {bytes} bytes added")
                }
            );
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(format!(
            "{file}: already exists (use --force to replace it)"
        )),
        Err(e) => Err(format!("{file}: {e}")),
    }
}

/// A file or directory on the host as an item for the disk, directories'
/// entries sorted by name so that the same tree makes the same disk.
fn item(path: &Path) -> Result<Item, String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{}: no usable file name", path.display()))?
        .to_string();
    if !path.is_dir() {
        let data = std::fs::read(path).map_err(err)?;
        return Ok(Item::File { name, data });
    }
    let mut paths = std::fs::read_dir(path)
        .map_err(err)?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    paths.sort();
    let items = paths.iter().map(|p| item(p)).collect::<Result<_, _>>()?;
    Ok(Item::Dir { name, items })
}

/// How many files, and how many bytes in them.
fn count(items: &[Item]) -> (usize, usize) {
    items.iter().fold((0, 0), |(n, b), i| match i {
        Item::File { data, .. } => (n + 1, b + data.len()),
        Item::Dir { items, .. } => {
            let (n2, b2) = count(items);
            (n + n2, b + b2)
        }
    })
}
