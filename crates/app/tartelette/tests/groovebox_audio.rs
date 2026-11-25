//! The `groovebox-audio` showcase demo end to end: its drums and bass,
//! synthesized on the Pico, leave GPIO16 as a PWM whose compare level a DMA
//! channel paced by the PWM's wrap sets every period, and the board's
//! speaker turns the pin back into sound. The first run builds the firmware,
//! which needs network access for the crates.
use std::{path::PathBuf, process::Command};

fn build() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    // From its directory, for its .cargo/config.toml (target, target
    // directory, defmt level).
    let status = Command::new(env!("CARGO"))
        .args(["build", "--release", "--bin", "groovebox-audio"])
        .current_dir(root.join("firmware/showcase"))
        .env_remove("RUSTFLAGS")
        .status()
        .expect("run cargo");
    assert!(status.success(), "firmware build failed");
    root.join("target/firmware/showcase/thumbv6m-none-eabi/release/groovebox-audio")
}

#[test]
fn the_groove_comes_out_of_the_speaker() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let elf = build();
    let dir = std::env::temp_dir().join(format!("rp2040-groovebox-audio-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("out.wav");
    let out = Command::new(env!("CARGO_BIN_EXE_tartelette"))
        .arg(&elf)
        .arg("--board")
        .arg(root.join("firmware/showcase/boards/groovebox-audio.toml"))
        .args(["--shield", "pico-display-2", "--headless"])
        .args(["--max-cycles", "125000000", "--wav"])
        .arg(&wav)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        !stderr.contains("speaker level"),
        "the sound isn't logged: {stderr}"
    );

    // A second at 48 kHz.
    let bytes = std::fs::read(&wav).unwrap();
    let samples: Vec<i32> = bytes[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b) as i32)
        .collect();
    assert_eq!(samples.len(), 48_000);
    // Loud 10 ms stretches: the hits of the first groove (kick, snare and
    // bass, at 112 BPM).
    let loud: Vec<usize> = samples
        .chunks(480)
        .enumerate()
        .filter(|(_, c)| c.iter().map(|s| s.abs()).max().unwrap() > 8_000)
        .map(|(i, _)| i)
        .collect();
    assert!(loud.len() > 10, "{loud:?}");
    let peak = samples.iter().map(|s| s.abs()).max().unwrap();
    assert!(peak < 32_767, "clipping");
    std::fs::remove_dir_all(&dir).ok();
}
