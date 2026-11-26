//! `mkdisk` on its own, as `tartelette mkdisk` runs it, for scripts that only
//! need a disk image: it builds without the emulator and its system
//! libraries (sound, MIDI).
use clap::Parser;

fn main() -> std::process::ExitCode {
    match mkdisk::run(&mkdisk::MkdiskArgs::parse()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}
