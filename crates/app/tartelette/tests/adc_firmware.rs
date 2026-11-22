//! End to end: the `adc-test` firmware reads a `pot` on GPIO26 and a `wav`
//! player on GPIO27 through the ADC, by polling, round robin into the FIFO
//! with its interrupt, and DMA paced by the divider.
use std::{path::PathBuf, process::Command};

use pcb::Board;
use pcb_devices::wav::Wav;
use rp2040_core::system::{StopReason, System};

fn build() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let target_dir = root.join("target/firmware/adc-test");
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "--release",
            "--target",
            "thumbv6m-none-eabi",
            "--manifest-path",
        ])
        .arg(root.join("firmware/tests/adc-test/Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .status()
        .expect("run cargo");
    assert!(status.success(), "firmware build failed");
    target_dir.join("thumbv6m-none-eabi/release/adc-test")
}

/// The pot at a quarter (0.825 V, code 1024); the player on GPIO27.
const BOARD: &str = "name = \"adc\"\nparts.knob = { kind = \"pot\", gpio = 26, value = 250 }\nparts.player = { kind = \"wav\", gpio = 27, file = \"ramp.wav\" }\n";

/// A mono 16-bit file at 48 kHz rising by 64 a sample from silence: four
/// ADC codes a sample at the default bias and gain.
fn ramp() -> Vec<u8> {
    let data: Vec<u8> = (0..500i16).flat_map(|i| (i * 64).to_le_bytes()).collect();
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    for v in [16u32, 1 | 1 << 16, 48_000, 96_000, 2 | 16 << 16] {
        f.extend_from_slice(&v.to_le_bytes());
    }
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(&data);
    f
}

fn numbers(out: &str, key: &str) -> Vec<u32> {
    let line = out
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .unwrap_or_else(|| panic!("no {key:?} in {out}"));
    line.split_whitespace()
        .map(|n| n.parse().unwrap())
        .collect()
}

/// What the firmware read, against the board above with the player started
/// at power-up.
fn check(out: &str) {
    assert!(
        out.starts_with("start\n") && out.ends_with("adc done\n"),
        "{out}"
    );
    assert_eq!(numbers(out, "ain0 "), [1024]);
    assert_eq!(numbers(out, "temp "), [876], "27 °C");
    // AIN0 first, then AIN1 (somewhere up the ramp), and round again.
    let rr = numbers(out, "rr ");
    assert_eq!((rr[0], rr[2]), (1024, 1024), "{rr:?}");
    assert!(rr[1] > 2048 && rr[3] > rr[1], "{rr:?}");
    // One sample of the file per conversion, wherever they fall between
    // two of its samples.
    let dma = numbers(out, "dma ");
    assert_eq!(dma.len(), 32);
    for w in dma.windows(2) {
        assert!((3..=5).contains(&(w[1] - w[0])), "{dma:?}");
    }
    // 31 periods of 1000 clk_adc cycles, plus a conversion.
    let us = numbers(out, "us ")[0];
    assert!((646..=650).contains(&us), "{us} µs");
}

#[test]
fn adc_reads_a_pot_and_a_wav_file() {
    let elf = std::fs::read(build()).unwrap();
    let mut sys = System::new();
    sys.load_elf(&elf).unwrap();
    let mut board = Board::parse_with(BOARD, &pcb_devices::registry()).unwrap();
    board
        .device_mut::<Wav>("player")
        .unwrap()
        .load("ramp.wav", &ramp())
        .unwrap();
    sys.set_board(board);
    assert!(sys.input("player", "play", 0));
    assert_eq!(sys.run(10_000_000), StopReason::Breakpoint(1));
    check(&String::from_utf8_lossy(&sys.bus.uart[0].tx));
}

#[test]
fn cli_loads_the_board_s_wav_file() {
    let dir = std::env::temp_dir().join(format!("tartelette-adc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("adc.toml"), BOARD).unwrap();
    std::fs::write(dir.join("ramp.wav"), ramp()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tartelette"))
        .arg(build())
        .arg("--headless")
        .arg("--board")
        .arg(dir.join("adc.toml"))
        .args(["--input", "player:play@0"])
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert!(out.status.success(), "{out:?}");
    check(&String::from_utf8_lossy(&out.stdout));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("player playing=1"), "{stderr}");
}
