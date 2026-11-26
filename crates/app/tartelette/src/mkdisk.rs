//! `mkdisk`: an SD card image for `--sd`, blank or with files copied in.
use std::{
    io::{Seek, SeekFrom, Write},
    path::Path,
};

use sdimage::{Fat, Item};

use crate::cli::MkdiskArgs;

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
