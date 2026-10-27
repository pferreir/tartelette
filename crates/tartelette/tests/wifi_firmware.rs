//! Real Pico W firmware (Embassy `cyw43` + `cyw43-pio`, `firmware/examples/pico-w-wifi`)
//! against the emulated CYW43439. Opt-in: the radio firmware blobs are not in
//! this repository, so set `CYW43_FIRMWARE_DIR` to a directory holding
//! `43439A0.bin` and `43439A0_clm.bin` (Embassy's `cyw43-firmware/`). The first
//! run builds the firmware, which needs network access for the crates.
use std::{path::PathBuf, process::Command, sync::OnceLock};

use pcb::Board;
use pcb_devices::{
    cyw43439::{Cyw43439, Link},
    net::LoopbackBackend,
};
use rp2040_core::system::{CYCLES_PER_US, StopReason, System};

fn build(bin: &str) -> Option<Vec<u8>> {
    static BUILT: OnceLock<Option<PathBuf>> = OnceLock::new();
    let target_dir = BUILT
        .get_or_init(|| {
            let Ok(fw_dir) = std::env::var("CYW43_FIRMWARE_DIR") else {
                eprintln!("CYW43_FIRMWARE_DIR not set: skipped");
                return None;
            };
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
            let target_dir = root.join("target/firmware/pico-w-wifi");
            let status = Command::new(env!("CARGO"))
                .args([
                    "build",
                    "--release",
                    "--target",
                    "thumbv6m-none-eabi",
                    "--manifest-path",
                ])
                .arg(root.join("firmware/examples/pico-w-wifi/Cargo.toml"))
                .arg("--target-dir")
                .arg(&target_dir)
                .env("CYW43_FIRMWARE_DIR", fw_dir)
                .env("DEFMT_LOG", "debug")
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

/// Run `elf` on the Pico W board for `ms` of emulated time.
fn run(elf: &[u8], network: bool, ms: u64) -> Run {
    run_with(elf, network, ms, true)
}

fn run_with(elf: &[u8], network: bool, ms: u64, batching: bool) -> Run {
    let mut rtt = rtt::Rtt::new(elf)
        .unwrap()
        .expect("firmware logs over defmt-RTT");
    let mut sys = System::new();
    sys.batching = batching;
    sys.load_elf(elf).unwrap();
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../boards/pico-w.toml"
    ))
    .unwrap();
    let mut board = Board::parse_with(&text, &pcb_devices::registry()).unwrap();
    if network {
        board
            .device_mut::<Cyw43439>("wifi")
            .unwrap()
            .set_backend(LoopbackBackend::boxed());
    }
    sys.set_board(board);
    let (mut lines, mut events) = (Vec::new(), Vec::new());
    let mut raw = Vec::new();
    while sys.cycles() < ms * 1000 * CYCLES_PER_US {
        match sys.run(250_000) {
            StopReason::Sleeping => sys.idle_until(sys.cycles() + 250_000),
            StopReason::CycleLimit => {}
            r => panic!("{r:?}"),
        }
        rtt.poll(&mut sys, &mut raw);
        for e in std::mem::take(&mut sys.board.events) {
            events.push((
                e.cycles as f64 / CYCLES_PER_US as f64 / 1000.0,
                e.label,
                e.value,
            ));
        }
    }
    assert_eq!(rtt.malformed, 0);
    lines.extend(raw.into_iter().map(|l| l.text));
    Run { sys, lines, events }
}

impl Run {
    fn logged(&self, what: &str) -> bool {
        self.lines.iter().any(|l| l.contains(what))
    }
}

#[test]
fn blinky_blinks_the_wifi_chips_led() {
    let Some(elf) = build("wifi_blinky") else {
        return;
    };
    let r = run(&elf, false, 4300);
    let led: Vec<_> = r.events.iter().filter(|e| e.1 == "led").collect();
    assert_eq!(
        led.iter().map(|e| e.2).collect::<Vec<_>>(),
        [1, 0, 1, 0],
        "{led:?}"
    );
    // About a second apart.
    for pair in led.windows(2) {
        let gap = pair[1].0 - pair[0].0;
        assert!((900.0..1100.0).contains(&gap), "{led:?}");
    }
    assert!(r.logged("led on!") && r.logged("led off!"), "{:?}", r.lines);
    assert!(
        r.sys
            .board
            .device::<Cyw43439>("wifi")
            .unwrap()
            .unknown_requests()
            .is_empty()
    );
}

#[test]
fn scan_finds_the_mock_networks() {
    let Some(elf) = build("wifi_scan") else {
        return;
    };
    let r = run(&elf, true, 1500);
    assert!(r.logged("scanned TarteletteNet"), "{:?}", r.lines);
    assert!(r.logged("scanned FreeCafe"));
    assert!(r.logged("rssi=-45"));
    assert!(r.logged("scan done: 2 networks"));
}

#[test]
fn scan_without_a_network_finds_nothing_and_says_so() {
    let Some(elf) = build("wifi_scan") else {
        return;
    };
    let r = run(&elf, false, 1500);
    assert!(r.logged("scan done: 0 networks"), "{:?}", r.lines);
    assert!(
        r.events.iter().any(|e| e.1 == "unavailable"),
        "{:?}",
        r.events
    );
}

#[test]
fn join_gets_a_lease_and_talks_to_the_gateway() {
    let Some(elf) = build("wifi_join") else {
        return;
    };
    let r = run(&elf, true, 1800);
    assert!(r.logged("joined TarteletteNet"), "{:?}", r.lines);
    assert!(r.logged("DHCP up: [10, 0, 2, 15]"));
    assert!(r.logged("echo from 10.0.2.2:7: hello from pico"));
    let chip = r.sys.board.device::<Cyw43439>("wifi").unwrap();
    assert_eq!(chip.link(), Link::Up(0));
    assert_eq!(chip.lease(), Some([10, 0, 2, 15]));
    assert!(r.events.iter().any(|e| e.1 == "join" && e.2 == 1));
}

/// Skipping quiet stretches in batches must not change what the chip, its
/// timers and the network do.
#[test]
fn batching_does_not_change_the_wifi_run() {
    let Some(elf) = build("wifi_join") else {
        return;
    };
    let (a, b) = (
        run_with(&elf, true, 1800, true),
        run_with(&elf, true, 1800, false),
    );
    assert_eq!(a.events, b.events);
    assert_eq!(a.lines, b.lines);
    assert_eq!(a.sys.cycles(), b.sys.cycles());
}
