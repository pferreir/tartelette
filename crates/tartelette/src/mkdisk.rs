//! `mkdisk`: a blank SD card image for `--sd`.
use std::io::{Seek, SeekFrom, Write};

use sdimage::Fat;

use crate::cli::MkdiskArgs;

pub fn run(args: &MkdiskArgs) -> Result<(), String> {
    let MkdiskArgs {
        file,
        size_mb,
        fat,
        dir,
        label,
        force,
    } = args;
    let fat = match fat.as_str() {
        "16" => Fat::Fat16,
        _ => Fat::Fat32,
    };
    let dirs: Vec<&str> = dir.iter().map(String::as_str).collect();
    let regions = sdimage::format(fat, *size_mb as usize, label, &dirs)
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
            eprintln!(
                "{file}: {size_mb} MiB, {} \"{label}\"{}",
                fat.name(),
                dirs.iter().map(|d| format!(", {d}/")).collect::<String>()
            );
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(format!(
            "{file}: already exists (use --force to replace it)"
        )),
        Err(e) => Err(format!("{file}: {e}")),
    }
}
