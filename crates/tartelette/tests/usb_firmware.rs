//! Real USB device firmware against the emulated USB controller and host: the
//! same composite device (serial port, boot keyboard, USB-MIDI) on two
//! stacks, embassy-usb (`firmware/examples/pico-usb`) and rp2040-hal +
//! usb-device (`firmware/examples/pico-usb-hal`). The first run builds the
//! firmware, which needs network access for the crates.
use std::{io::Write, path::PathBuf, process::Command};

use rp2040_core::system::{CYCLES_PER_US, System};
use usbhost::function::Function;

const STACKS: [(&str, &str); 2] = [
    ("pico-usb", "USB gadgets (embassy)"),
    ("pico-usb-hal", "USB gadgets (rp2040-hal)"),
];
const MS: u64 = 1000 * CYCLES_PER_US;

fn build(example: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target_dir = root.join("target/firmware").join(example);
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "--release",
            "--target",
            "thumbv6m-none-eabi",
            "--manifest-path",
        ])
        .arg(root.join(format!("firmware/examples/{example}/Cargo.toml")))
        .arg("--target-dir")
        .arg(&target_dir)
        .env("DEFMT_LOG", "info")
        .status()
        .expect("run cargo");
    assert!(status.success(), "firmware build failed");
    target_dir.join("thumbv6m-none-eabi/release/usb_composite")
}

/// Run until `done` holds (checked every millisecond), for at most `ms`.
fn run_until(sys: &mut System, ms: u64, mut done: impl FnMut(&mut System) -> bool) -> bool {
    let end = sys.cycles() + ms * MS;
    while sys.cycles() < end {
        sys.run(MS);
        if done(sys) {
            return true;
        }
    }
    false
}

fn midi_rx(sys: &mut System) -> Vec<(u8, Vec<u8>)> {
    sys.with_usb_host(|h| {
        h.functions
            .iter_mut()
            .find_map(|f| match f {
                Function::Midi(m) => Some(m.rx.drain(..).collect()),
                _ => None,
            })
            .unwrap_or_default()
    })
}

#[test]
fn both_stacks_enumerate_and_every_function_works() {
    for (example, product) in STACKS {
        let elf = std::fs::read(build(example)).unwrap();
        let mut sys = System::new();
        sys.load_elf(&elf).unwrap();

        assert!(
            run_until(&mut sys, 200, |s| s.usb_host().device().is_some()),
            "{example}: not configured"
        );
        let info = sys.usb_host().device().unwrap().clone();
        assert_eq!(info.product.as_deref(), Some(product));
        assert_eq!(info.manufacturer.as_deref(), Some("Tartelette"));
        let kinds: Vec<&str> = sys
            .usb_host()
            .functions
            .iter()
            .map(|f| match f {
                Function::Serial(_) => "serial",
                Function::Hid(_) => "hid",
                Function::Midi(_) => "midi",
                Function::Other(_) => "other",
            })
            .collect();
        assert_eq!(kinds, ["serial", "hid", "other", "midi"], "{example}");

        // Serial: an echo in upper case; the `!` types an `a` on the keyboard.
        sys.with_usb_host(|h| h.serial().unwrap().tx.extend(b"hello, usb!"));
        let mut rx = Vec::new();
        assert!(
            run_until(&mut sys, 100, |s| {
                rx.extend(
                    s.with_usb_host(|h| h.serial().unwrap().rx.drain(..).collect::<Vec<_>>()),
                );
                rx.len() >= 11
            }),
            "{example}: no echo"
        );
        assert_eq!(rx, b"HELLO, USB!", "{example}");
        let mut reports = Vec::new();
        assert!(
            run_until(&mut sys, 100, |s| {
                s.with_usb_host(|h| {
                    for f in &mut h.functions {
                        if let Function::Hid(k) = f {
                            assert!(k.ready);
                            assert_eq!(k.report_descriptor.len(), 63);
                            reports.extend(k.input.drain(..));
                        }
                    }
                });
                reports.len() >= 2
            }),
            "{example}: no key reports"
        );
        assert_eq!(reports[0], [0, 0, 4, 0, 0, 0, 0, 0], "{example}: A down");
        assert_eq!(reports[1], [0; 8], "{example}: and up");

        // MIDI: notes come back an octave up; the rest unchanged.
        let sysex = vec![0xF0, 0x7D, 1, 2, 3, 4, 0xF7];
        sys.with_usb_host(|h| {
            for f in &mut h.functions {
                if let Function::Midi(m) = f {
                    m.tx.push_back((0, vec![0x90, 60, 100]));
                    m.tx.push_back((0, vec![0xB0, 7, 99]));
                    m.tx.push_back((0, sysex.clone()));
                }
            }
        });
        let mut got = Vec::new();
        assert!(
            run_until(&mut sys, 100, |s| {
                got.extend(midi_rx(s));
                got.len() >= 3
            }),
            "{example}: no MIDI echo"
        );
        assert_eq!(
            got,
            [(0, vec![0x90, 72, 100]), (0, vec![0xB0, 7, 99]), (0, sysex)],
            "{example}"
        );
    }
}

#[test]
fn headless_wires_stdin_and_stdout_to_the_serial_port() {
    for (example, product) in STACKS {
        let elf = build(example);
        let mut child = Command::new(env!("CARGO_BIN_EXE_tartelette"))
            .arg(&elf)
            .args(["--headless", "--usb-log", "--max-cycles", "125000000"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"abc\r\n").unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ABC\r\n", "{example}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(&format!("usb configured 1209:0001 \"{product}\"")),
            "{err}"
        );
        assert!(
            err.contains("usb OUT ep1   5 bytes 61 62 63 0d 0a"),
            "{err}"
        );
    }
}
