//! Real Pico W Bluetooth firmware (trouble's example apps over Embassy
//! `cyw43`, `firmware/examples/pico-w-ble`) against the emulated CYW43439 and the
//! virtual controller's mock devices. The firmware blobs are found as for the
//! Wi-Fi tests, and these need `43439A0_btfw.bin` and `nvram_rp2040.bin` too
//! (Embassy's `cyw43-firmware/`). The first run builds the firmware, which
//! needs network access.
mod common;

use std::{path::PathBuf, process::Command, sync::OnceLock};

use pcb::Board;
use pcb_devices::{
    cyw43439::{Cyw43439, bt::bt_address},
    hci::{peers, virt::VirtualController},
};
use rp2040_core::system::{CYCLES_PER_US, SRAM_BASE, StopReason, System};

fn build(bin: &str) -> Option<Vec<u8>> {
    static BUILT: OnceLock<Option<PathBuf>> = OnceLock::new();
    let target_dir = BUILT
        .get_or_init(|| {
            // nvram_rp2040.bin is in newer Embassy's cyw43-firmware/ only.
            let fw_dir = common::cyw43_firmware_dir(&[
                "43439A0.bin",
                "43439A0_clm.bin",
                "43439A0_btfw.bin",
                "nvram_rp2040.bin",
            ])?;
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
            let target_dir = root.join("target/firmware/pico-w-ble");
            let status = Command::new(env!("CARGO"))
                .args([
                    "build",
                    "--release",
                    "--target",
                    "thumbv6m-none-eabi",
                    "--manifest-path",
                ])
                .arg(root.join("firmware/examples/pico-w-ble/Cargo.toml"))
                .arg("--target-dir")
                .arg(&target_dir)
                .env("CYW43_FIRMWARE_DIR", fw_dir)
                .env("DEFMT_LOG", "info")
                .status()
                .expect("run cargo");
            assert!(status.success(), "firmware build failed");
            Some(target_dir)
        })
        .as_ref()?;
    Some(std::fs::read(target_dir.join(format!("thumbv6m-none-eabi/release/{bin}"))).unwrap())
}

struct Run {
    sys: System,
    lines: Vec<String>,
    events: Vec<(f64, &'static str, i64)>,
}

/// Run `elf` on the Pico W board for `ms` of emulated time, with the mock
/// devices in range or with no radio at all.
fn run(elf: &[u8], mock: bool, ms: u64) -> Run {
    run_with(elf, mock, ms, true)
}

fn run_with(elf: &[u8], mock: bool, ms: u64, batching: bool) -> Run {
    let mut rtt = rtt::Rtt::new(elf)
        .unwrap()
        .expect("firmware logs over defmt-RTT");
    let mut sys = System::new();
    sys.batching = batching;
    sys.load_elf(elf).unwrap();
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../boards/pico-w.toml"
    ))
    .unwrap();
    let mut board = Board::parse_with(&text, &pcb_devices::registry()).unwrap();
    if mock {
        let chip = board.device_mut::<Cyw43439>("wifi").unwrap();
        chip.set_bt_backend(Box::new(VirtualController::new(
            bt_address(chip.mac()),
            peers::neighbourhood(),
        )));
    }
    sys.set_board(board);
    let (mut events, mut raw) = (Vec::new(), Vec::new());
    while sys.cycles() < ms * 1000 * CYCLES_PER_US {
        match sys.run(250_000) {
            StopReason::Sleeping => sys.idle_until(sys.cycles() + 250_000),
            StopReason::CycleLimit => {}
            r => panic!("{r:?}"),
        }
        rtt.poll(&mut sys.bus.sram.data, SRAM_BASE, &mut raw);
        for e in std::mem::take(&mut sys.board.events) {
            events.push((
                e.cycles as f64 / CYCLES_PER_US as f64 / 1000.0,
                e.label,
                e.value,
            ));
        }
    }
    assert_eq!(rtt.malformed, 0);
    Run {
        sys,
        lines: raw.into_iter().map(|l| l.text).collect(),
        events,
    }
}

impl Run {
    fn logged(&self, what: &str) -> bool {
        self.lines.iter().any(|l| l.contains(what))
    }

    fn values(&self, label: &str) -> Vec<i64> {
        self.events
            .iter()
            .filter(|e| e.1 == label)
            .map(|e| e.2)
            .collect()
    }
}

#[test]
fn the_mock_central_connects_to_the_peripheral_and_gets_its_notifications() {
    let Some(elf) = build("ble_bas_peripheral") else {
        return;
    };
    let r = run(&elf, true, 3500);
    assert!(r.logged("[adv] connection established"), "{:?}", r.lines);
    assert_eq!(r.values("ble_connected"), [1]);
    // trouble notifies a tick every two seconds; the first goes out before
    // the central has subscribed.
    assert_eq!(r.values("ble_notify"), [2], "{:?}", r.events);
    assert_eq!(
        r.sys
            .board
            .device::<Cyw43439>("wifi")
            .unwrap()
            .bt_activity(),
        (false, false, 1)
    );
}

#[test]
fn the_central_finds_the_mock_battery_reads_it_and_subscribes() {
    let Some(elf) = build("ble_bas_central") else {
        return;
    };
    let r = run(&elf, true, 3500);
    assert!(r.logged("Connected, creating gatt client"), "{:?}", r.lines);
    assert!(r.logged("Read value: 99"));
    assert!(r.logged("Got notification: [98] (val: 98)"));
    assert!(r.logged("Got notification: [97] (val: 97)"));
}

#[test]
fn the_scanner_discovers_the_mock_peripherals() {
    let Some(elf) = build("ble_scanner") else {
        return;
    };
    let r = run(&elf, true, 1500);
    assert!(
        r.logged("discovered: BdAddr([131, 36, 37, 70, 30, 226])"),
        "{:?}",
        r.lines
    );
    assert!(r.logged("discovered: BdAddr([255, 143, 26, 5, 228, 255])"));
}

#[test]
fn without_a_radio_the_scanner_finds_nothing_and_says_so() {
    let Some(elf) = build("ble_scanner") else {
        return;
    };
    let r = run(&elf, false, 1500);
    assert!(r.logged("[host] initialized"), "{:?}", r.lines);
    assert!(!r.logged("discovered"));
    assert_eq!(r.values("bt_unavailable"), [1]);
}

/// Skipping quiet stretches in batches must not change what the chip, its
/// timers and the virtual radio do.
#[test]
fn batching_does_not_change_the_bluetooth_run() {
    let Some(elf) = build("ble_bas_central") else {
        return;
    };
    let (a, b) = (
        run_with(&elf, true, 2500, true),
        run_with(&elf, true, 2500, false),
    );
    assert_eq!(a.events, b.events);
    assert_eq!(a.lines, b.lines);
    assert_eq!(a.sys.cycles(), b.sys.cycles());
}
