//! Shared by the firmware tests.
use std::path::{Path, PathBuf};

/// The directory with the CYW43439 firmware blobs (Infineon's, so not in
/// this repository) holding all of `files`: `CYW43_FIRMWARE_DIR`, or else the
/// newest `cyw43-firmware/` in Cargo's git checkouts of Embassy, which anyone
/// who has built Embassy's `cyw43` from git has. None (with a note saying how
/// to set it) if there is neither.
pub fn cyw43_firmware_dir(files: &[&str]) -> Option<PathBuf> {
    let has_all = |d: &Path| files.iter().all(|f| d.join(f).is_file());
    if let Some(dir) = std::env::var_os("CYW43_FIRMWARE_DIR").map(PathBuf::from) {
        if has_all(&dir) {
            return Some(dir);
        }
        eprintln!(
            "CYW43_FIRMWARE_DIR={} lacks one of {files:?}: skipped",
            dir.display()
        );
        return None;
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;
    let found = std::fs::read_dir(cargo_home.join("git/checkouts"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|repo| repo.file_name().to_string_lossy().starts_with("embassy-"))
        .flat_map(|repo| {
            std::fs::read_dir(repo.path())
                .into_iter()
                .flatten()
                .flatten()
        })
        .map(|rev| rev.path().join("cyw43-firmware"))
        .filter(|d| has_all(d))
        .max_by_key(|d| std::fs::metadata(d).and_then(|m| m.modified()).ok());
    match &found {
        Some(dir) => eprintln!("CYW43 firmware from {}", dir.display()),
        None => eprintln!(
            "no CYW43 firmware: set CYW43_FIRMWARE_DIR to Embassy's cyw43-firmware/ \
             (with {files:?}): skipped"
        ),
    }
    found
}
