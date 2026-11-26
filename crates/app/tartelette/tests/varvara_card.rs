//! The `varvara` showcase demo with an SD card end to end: `mkdisk --add`
//! puts a ROM in a directory with a long name, the menu finds it (Up wraps
//! round to the last entry, whether or not the build has a ROM built in),
//! and the ROM writes a file through the File device, which is on the card
//! after the run. The first run builds the firmware, which needs network
//! access for the crates.
use std::{path::PathBuf, process::Command};

/// The ROM, assembled by uxnasm from:
///
/// ```text
/// |a0 @File &vector $2 &success $2 &stat $2 &delete $1 &append $1 &name $2 &length $2 &read $2 &write $2
/// |0100
///     ;name .File/name DEO2
///     #0003 .File/length DEO2
///     ;text .File/write DEO2
///     ;text .File/write DEO2
///     LIT "o #18 DEO LIT "k #18 DEO #0a18 DEO
///     #800f DEO
/// BRK
/// @name "Out 20 "file.txt $1
/// @text "ok 0a
/// ```
const ROM: [u8; 59] = [
    0xa0, 0x01, 0x2b, 0x80, 0xa8, 0x37, 0xa0, 0x00, 0x03, 0x80, 0xaa, 0x37, 0xa0, 0x01, 0x38, 0x80,
    0xae, 0x37, 0xa0, 0x01, 0x38, 0x80, 0xae, 0x37, 0x80, 0x6f, 0x80, 0x18, 0x17, 0x80, 0x6b, 0x80,
    0x18, 0x17, 0xa0, 0x0a, 0x18, 0x17, 0xa0, 0x80, 0x0f, 0x17, 0x00, 0x4f, 0x75, 0x74, 0x20, 0x66,
    0x69, 0x6c, 0x65, 0x2e, 0x74, 0x78, 0x74, 0x00, 0x6f, 0x6b, 0x0a,
];

fn build() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--release", "--bin", "varvara"])
        .current_dir(root.join("firmware/showcase"))
        .env_remove("RUSTFLAGS")
        .status()
        .expect("run cargo");
    assert!(status.success(), "firmware build failed");
    root.join("target/firmware/showcase/thumbv6m-none-eabi/release/varvara")
}

#[test]
fn a_rom_from_the_card_writes_a_file_on_it() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let elf = build();
    let dir = std::env::temp_dir().join(format!("rp2040-varvara-card-{}", std::process::id()));
    let tests = dir.join("Test ROMs");
    std::fs::create_dir_all(&tests).unwrap();
    std::fs::write(tests.join("write.rom"), ROM).unwrap();
    let card = dir.join("card.img");
    let tartelette = env!("CARGO_BIN_EXE_tartelette");
    let made = Command::new(tartelette)
        .args(["mkdisk", "--fat", "16", "--size-mb", "4", "--add"])
        .arg(&tests)
        .arg(&card)
        .output()
        .unwrap();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );

    // Up and A to the directory, Up and A to the ROM: about a second for
    // the menu to come up, then 200 ms between presses.
    let mut cmd = Command::new(tartelette);
    cmd.arg(&elf)
        .arg("--board")
        .arg(root.join("firmware/showcase/boards/varvara.toml"))
        .arg("--sd")
        .arg(format!("sd={}", card.display()))
        .args(["--headless", "--max-cycles", "500000000"]);
    for (button, at) in [("up", 1000), ("a", 1200), ("up", 1400), ("a", 1600)] {
        cmd.args(["--press", &format!("{button}@{}+40000", at * 1000)]);
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stdout.contains("ok\nvarvara: halted"),
        "the ROM ran: {stdout}"
    );

    // Its name isn't 8.3, so the file has one made from it, and both
    // writes went to the one file, left open between them.
    let image = std::fs::read(&card).unwrap();
    let volume = sdimage::read::open(&image).unwrap();
    let root_dir = volume.root().unwrap();
    let file = root_dir
        .iter()
        .find(|e| e.name.starts_with("OUTF~") && e.name.ends_with(".TXT"))
        .unwrap_or_else(|| panic!("no file written: {root_dir:?}"));
    assert_eq!(volume.read(file, usize::MAX).unwrap(), b"ok\nok\n");
    std::fs::remove_dir_all(&dir).ok();
}
