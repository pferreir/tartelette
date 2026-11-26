//! End to end: the `ps2-test` firmware is a PS/2 host (PIO receiving, the
//! CPU sending) for a `ps2mouse` on GPIO14 and 15. It resets the mouse,
//! turns its wheel on and enables reporting, then holds it off until 1 s;
//! the mouse moves at 0.9 s, and its left button and wheel go at 1.2 s.
use std::{path::PathBuf, process::Command};

use pcb::Board;
use rp2040_core::system::{CYCLES_PER_US, StopReason, System};

fn build() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let target_dir = root.join("target/firmware/tests");
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "--release",
            "--target",
            "thumbv6m-none-eabi",
            "-p",
            "ps2-test",
            "--manifest-path",
        ])
        .arg(root.join("firmware/tests/Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .status()
        .expect("run cargo");
    assert!(status.success(), "firmware build failed");
    target_dir.join("thumbv6m-none-eabi/release/ps2-test")
}

const BOARD: &str = "name = \"ps2\"\nparts.mouse = { kind = \"ps2mouse\", clk = 14, data = 15 }\n";

/// Down on the screen is -y to PS/2 (`fb`, with the sign bit in `28`); the
/// wheel up is -1.
const EXPECTED: &str = "start\nbat aa 00\nreset fa aa 00\nid fa fa fa fa fa fa fa 03\n\
                        enable fa\npacket 28 0a fb 00\npacket 09 00 00 ff\nps2 done\n";

#[test]
fn a_pio_host_resets_the_mouse_turns_on_its_wheel_and_reads_packets() {
    let elf = std::fs::read(build()).unwrap();
    let mut sys = System::new();
    sys.load_elf(&elf).unwrap();
    sys.set_board(Board::parse_with(BOARD, &pcb_devices::registry()).unwrap());
    let ms = 1000 * CYCLES_PER_US;
    assert_eq!(sys.run(900 * ms), StopReason::CycleLimit);
    assert!(sys.input("mouse", "move_x", 10));
    assert!(sys.input("mouse", "move_y", 5));
    assert_eq!(sys.run(300 * ms), StopReason::CycleLimit);
    assert!(sys.input("mouse", "press", 0));
    assert!(sys.input("mouse", "scroll", 1));
    assert_eq!(sys.run(500 * ms), StopReason::Breakpoint(1));
    let out = String::from_utf8_lossy(&sys.bus.uart[0].tx);
    assert_eq!(out, EXPECTED);
    let received: Vec<i64> = sys
        .board
        .events
        .iter()
        .filter(|e| e.label == "received")
        .map(|e| e.value)
        .collect();
    assert_eq!(received, [0xFF, 0xF3, 200, 0xF3, 100, 0xF3, 80, 0xF2, 0xF4]);
    assert!(sys.board.take_warnings().is_empty());
}

#[test]
fn cli_inputs_move_and_click_the_mouse() {
    let dir = std::env::temp_dir().join(format!("tartelette-ps2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ps2.toml"), BOARD).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tartelette"))
        .arg(build())
        .arg("--headless")
        .arg("--board")
        .arg(dir.join("ps2.toml"))
        .args(["--input", "mouse:move_x=10@900000"])
        .args(["--input", "mouse:move_y=5@900000"])
        .args(["--input", "mouse:press=0@1200000"])
        .args(["--input", "mouse:scroll=1@1200000"])
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), EXPECTED);
}
