//! One emulated run: firmware + board, stepped against a wall-clock budget.
//! No UI here, so it behaves the same natively and in the browser.
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use pcb::BoardEvent;
use pcb_devices::sdcard::SdCard;
use rp2040_core::system::{CYCLES_PER_US, SRAM_BASE, StopReason, System};
use web_time::Instant;

use crate::{Network, Source, boards::parse_board, network};

pub const CYCLES_PER_SEC: f64 = CYCLES_PER_US as f64 * 1e6;

/// Cycles per `System::run` call: small enough to stay responsive to the
/// host-time budget, large enough to keep the per-call overhead down.
const CHUNK: u64 = 250_000;
/// Never keep more than this much UART text per port.
const UART_KEEP: usize = 256 * 1024;
/// Never keep more than this many log messages.
const LOG_KEEP: usize = 5000;

/// A message of the firmware's defmt log and the emulated time it was read at.
pub struct LogEntry {
    pub at: f64,
    pub line: rtt::Line,
}

pub struct Session {
    pub sys: System,
    /// Why the firmware stopped for good (breakpoint), if it did.
    pub halted: Option<StopReason>,
    /// Everything each UART has sent, as text.
    pub uart: [String; 2],
    /// Board events since the UI last took them.
    pub events: Vec<BoardEvent>,
    /// The firmware's defmt-over-RTT log, oldest first, and why there is
    /// none when the firmware has no log or it cannot be decoded.
    pub log: VecDeque<LogEntry>,
    pub log_note: Option<String>,
    /// Wiring problems the emulator spotted, since the UI last took them.
    pub warnings: Vec<String>,
    /// The USB host's side: serial console, traffic log, bridges.
    pub usb: crate::usb::UsbSide,
    /// The far end of the UART lines: input for the firmware, bridges.
    pub uarts: crate::uart::UartSide,
    rtt: Option<rtt::Rtt>,
    /// The test synthesizer playing live, which hears every board event.
    pub synth: Option<Arc<Mutex<crate::synth::Live>>>,
    /// SD cards whose writes go straight to a file: part name, file.
    #[cfg(not(target_arch = "wasm32"))]
    sd_files: Vec<SdFile>,
}

#[cfg(not(target_arch = "wasm32"))]
struct SdFile {
    part: String,
    path: std::path::PathBuf,
    file: std::fs::File,
}

impl Session {
    /// Build a run from a firmware ELF or UF2, an optional board file and the
    /// shields on top of it.
    pub fn new(
        firmware: &Source,
        board: Option<&Source>,
        shields: &[Source],
        net: &Network,
    ) -> Result<Self, String> {
        Self::with_flash(firmware, board, shields, net, None)
    }

    /// Like `new`, with the flash chip holding `flash` (a whole-chip image)
    /// before the firmware goes on top: what the firmware keeps in the rest
    /// of the flash, like MicroPython's filesystem, is kept. Without an
    /// image, the chip starts from the board's `[[flash]]` entries; an image
    /// replaces them, since it already has what the firmware made of them.
    pub fn with_flash(
        firmware: &Source,
        board: Option<&Source>,
        shields: &[Source],
        net: &Network,
        flash: Option<&[u8]>,
    ) -> Result<Self, String> {
        let mut sys = System::new();
        let board = parse_board(board, shields)?;
        if let Some(image) = flash {
            sys.bus
                .load(rp2040_core::system::XIP_BASE, image)
                .then_some(())
                .ok_or("--flash: the image is larger than 16 MiB")?;
        } else {
            for (addr, data) in &board.flash {
                sys.bus.load(*addr, data); // the board parser only takes flash addresses
            }
        }
        let uf2 = rp2040_core::uf2::is_uf2(&firmware.bytes);
        sys.load_firmware(&firmware.bytes).map_err(|e| {
            let kind = if uf2 { "UF2" } else { "ELF" };
            format!("{}: cannot load {kind}: {e:?}", firmware.name)
        })?;
        sys.set_board(board);
        network::connect(&mut sys.board, net)?;
        let usb = crate::usb::UsbSide::new(&net.usb)?;
        let uarts = crate::uart::UartSide::new(&net.uart)?;
        let (rtt, log_note) = match rtt::Rtt::new(&firmware.bytes) {
            _ if uf2 => (
                None,
                Some("a UF2 has no symbols, so no defmt log: run the ELF for that".to_string()),
            ),
            Ok(Some(r)) => (Some(r), None),
            Ok(None) => (
                None,
                Some("this firmware has no defmt-over-RTT log".to_string()),
            ),
            Err(e) => (None, Some(e)),
        };
        Ok(Self {
            sys,
            halted: None,
            uart: Default::default(),
            events: Vec::new(),
            log: VecDeque::new(),
            log_note,
            warnings: Vec::new(),
            usb,
            uarts,
            rtt,
            synth: None,
            #[cfg(not(target_arch = "wasm32"))]
            sd_files: Vec::new(),
        })
        .map(|mut s: Self| {
            // A bridged port is closed until a program opens it.
            let lines = !s.usb.serial_bridged();
            s.sys.with_usb_host(|h| h.serial_lines = lines);
            s
        })
    }

    /// Emulated seconds since power-up.
    pub fn seconds(&self) -> f64 {
        self.sys.cycles() as f64 / CYCLES_PER_SEC
    }

    /// Put bytes on UART `n`'s line straight away, at the firmware's baud
    /// rate; lost if its receiver is off (for live input like MIDI; text
    /// that must arrive goes in `uarts.input`).
    pub fn send(&mut self, n: usize, bytes: &[u8]) {
        self.sys.send_uart(n, bytes);
    }

    /// Do something to a board part (see each device's docs for actions).
    pub fn input(&mut self, part: &str, action: &str, value: i64) -> bool {
        let ok = self.sys.input(part, action, value);
        self.drain();
        ok
    }

    /// Run up to `cycles`, stopping early once `deadline` has passed. Returns
    /// the cycles actually emulated (time spent asleep counts).
    pub fn step(&mut self, cycles: u64, deadline: Instant) -> u64 {
        let start = self.sys.cycles();
        let end = start + cycles;
        while self.halted.is_none() && self.sys.cycles() < end && Instant::now() < deadline {
            let target = self.sys.cycles() + (end - self.sys.cycles()).min(CHUNK);
            match self.sys.run(target - self.sys.cycles()) {
                StopReason::CycleLimit => {}
                // Nothing can wake the cores except the host: let the time pass.
                StopReason::Sleeping => self.sys.idle_until(target),
                r => {
                    if matches!(r, StopReason::Breakpoint(_)) {
                        self.sys.drain_uarts();
                    }
                    self.halted = Some(r);
                }
            }
            self.drain();
        }
        self.sys.cycles() - start
    }

    /// Move what each UART sent out of the emulator (to its bridge, and
    /// returned), and the input waiting for it onto its line.
    pub fn exchange_uarts(&mut self) -> [Vec<u8>; 2] {
        let mut sent: [Vec<u8>; 2] = Default::default();
        for (n, sent) in sent.iter_mut().enumerate() {
            let uart = &mut self.sys.bus.uart[n];
            *sent = std::mem::take(&mut uart.tx);
            let (receiving, queued) = (uart.receiving(), uart.rx_queued());
            let rx = self.uarts.exchange(n, sent, receiving, queued);
            if !rx.is_empty() {
                self.sys.send_uart(n, &rx);
            }
        }
        self.warnings.append(&mut self.uarts.notes);
        sent
    }

    /// Move UART output and board events out of the emulator.
    fn drain(&mut self) {
        for (n, tx) in self.exchange_uarts().into_iter().enumerate() {
            if !tx.is_empty() {
                let text = &mut self.uart[n];
                text.push_str(&String::from_utf8_lossy(&tx));
                if text.len() > UART_KEEP {
                    let mut cut = text.len() - UART_KEEP / 2;
                    while !text.is_char_boundary(cut) {
                        cut += 1;
                    }
                    text.drain(..cut);
                }
            }
        }
        if let Some(synth) = &self.synth {
            synth
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(&self.sys.board.events);
        }
        self.events.append(&mut self.sys.board.events);
        self.warnings.append(&mut self.sys.warnings);
        self.warnings.extend(self.sys.board.take_warnings());
        self.exchange_usb();
        #[cfg(not(target_arch = "wasm32"))]
        self.write_sd_files();
        let lines = self.poll_log();
        let at = self.seconds();
        self.log
            .extend(lines.into_iter().map(|line| LogEntry { at, line }));
        let extra = self.log.len().saturating_sub(LOG_KEEP);
        self.log.drain(..extra);
    }

    /// Move data between the USB host's functions and the console, log and
    /// bridges.
    pub fn exchange_usb(&mut self) {
        let usb = &mut self.usb;
        self.sys
            .with_usb_host(|h| usb.exchange(h, |cycles| cycles as f64 / CYCLES_PER_SEC));
        self.warnings.append(&mut self.usb.notes);
    }

    /// Read out what the firmware has logged since the last call.
    pub fn poll_log(&mut self) -> Vec<rtt::Line> {
        let mut lines = Vec::new();
        if let Some(rtt) = self.rtt.as_mut() {
            rtt.poll(&mut self.sys.bus.sram.data, SRAM_BASE, &mut lines);
        }
        lines
    }

    /// SD card contents by part name, to carry over into a fresh run (a reset
    /// is a power cycle; the card keeps its data). Empty slots are left out:
    /// they start empty anyway.
    pub fn take_sd_cards(&mut self) -> Vec<(String, Vec<u8>, bool)> {
        self.sys
            .board
            .devices_mut::<SdCard>()
            .filter(|(_, card)| card.inserted())
            .map(|(n, card)| (n.to_string(), card.image().to_vec(), card.dirty()))
            .collect()
    }

    /// Load a disk image into the card `name`. It stops writing to any file
    /// it was writing to: that file holds a different disk.
    /// A copy of the card `name`'s image, with how many blocks the firmware
    /// has written to it so far (to tell later whether the copy is stale).
    /// None for an empty slot.
    pub fn sd_card_copy(&self, name: &str) -> Option<(Vec<u8>, u32)> {
        let card = self
            .sys
            .board
            .device::<SdCard>(name)
            .filter(|c| c.inserted())?;
        Some((card.image().to_vec(), card.blocks_written))
    }

    /// Blocks the firmware has written to the card `name` so far.
    pub fn sd_card_writes(&self, name: &str) -> Option<u32> {
        self.sys
            .board
            .device::<SdCard>(name)
            .map(|c| c.blocks_written)
    }

    pub fn load_sd_card(&mut self, name: &str, image: Vec<u8>) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        self.sd_files.retain(|f| f.part != name);
        match self.sys.board.device_mut::<SdCard>(name) {
            Some(card) => {
                card.load_image(image);
                true
            }
            None => false,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Session {
    /// From now on, write each block the firmware writes to the card `name`
    /// to the file at `path` too, which should hold the card's image (it was
    /// loaded from there). Host tools can then read the card while it runs.
    pub fn write_sd_card_to(&mut self, name: &str, path: &std::path::Path) -> Result<(), String> {
        let card = self
            .sys
            .board
            .device_mut::<SdCard>(name)
            .ok_or_else(|| format!("no SD card named {name:?} on the board"))?;
        // Earlier writes went nowhere.
        card.take_written();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        self.sd_files.retain(|f| f.part != name);
        self.sd_files.push(SdFile {
            part: name.into(),
            path: path.into(),
            file,
        });
        Ok(())
    }

    /// Write the blocks written since last time to the cards' files. A file
    /// that fails is dropped, with a warning.
    pub fn write_sd_files(&mut self) {
        use std::io::{Seek, SeekFrom, Write};
        let board = &mut self.sys.board;
        let warnings = &mut self.warnings;
        self.sd_files.retain_mut(|f| {
            let Some(card) = board.device_mut::<SdCard>(&f.part) else {
                return false;
            };
            let mut write = || -> std::io::Result<()> {
                for b in card.take_written() {
                    let at = b as usize * pcb_devices::sdcard::BLOCK;
                    f.file.seek(SeekFrom::Start(at as u64))?;
                    f.file
                        .write_all(&card.image()[at..at + pcb_devices::sdcard::BLOCK])?;
                }
                Ok(())
            };
            write()
                .map_err(|e| {
                    warnings.push(format!(
                        "{}: {e}; no longer writing SD card {} to it",
                        f.path.display(),
                        f.part
                    ))
                })
                .is_ok()
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, process::Command};

    use pcb_devices::is_lit;
    use web_time::Duration;

    use super::*;

    /// Build one of the test firmwares in `firmware/tests/` (as the CLI tests do).
    fn firmware(name: &str) -> Source {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let target_dir = root.join("target/firmware").join(name);
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "--release",
                "--target",
                "thumbv6m-none-eabi",
                "--manifest-path",
            ])
            .arg(root.join(format!("firmware/tests/{name}/Cargo.toml")))
            .arg("--target-dir")
            .arg(&target_dir)
            .status()
            .expect("run cargo");
        assert!(status.success(), "firmware build failed");
        let elf = target_dir.join(format!("thumbv6m-none-eabi/release/{name}"));
        Source::new(name, std::fs::read(elf).unwrap())
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn steps_inputs_and_halts_like_the_cli() {
        let board = Source::new("t.toml", b"parts.status = { kind = \"led\", gpio = 25 }\nparts.user = { kind = \"button\", gpio = 2 }\npanel.user = { at = [0, 0] }".to_vec());
        let mut s = Session::new(
            &firmware("button-led"),
            Some(&board),
            &[],
            &Network::default(),
        )
        .unwrap();
        assert_eq!(s.step(125 * 100, far()), 125 * 100);
        assert_eq!(is_lit(&s.sys.board, "status"), Some(false));

        assert!(s.input("user", "press", 0));
        s.step(1000, far());
        assert_eq!(is_lit(&s.sys.board, "status"), Some(true));
        assert!(s.input("user", "release", 0));
        s.step(10_000_000, far());
        assert_eq!(s.halted, Some(StopReason::Breakpoint(1)));
        assert_eq!(s.uart[0], "done\n");
        // Events are moved out of the board for the UI; nothing piles up there.
        assert!(s.sys.board.events.is_empty());
        let labels: Vec<_> = s.events.iter().map(|e| e.label).collect();
        assert_eq!(labels, ["pressed", "on", "released", "off"]);
        // A halted run stays put.
        let t = s.sys.cycles();
        assert_eq!(s.step(1000, far()), 0);
        assert_eq!(s.sys.cycles(), t);
    }

    #[test]
    fn a_flash_image_goes_under_the_firmware() {
        let fw = firmware("button-led");
        let plain = Session::new(&fw, None, &[], &Network::default()).unwrap();
        let image = vec![0xA5; 4 << 20];
        let s = Session::with_flash(&fw, None, &[], &Network::default(), Some(&image)).unwrap();
        let flash = &s.sys.bus.flash.data;
        assert_eq!(flash.len(), 4 << 20, "the chip is as large as the image");
        // The firmware is where it was; past it, the image is kept.
        let fw_len = plain
            .sys
            .bus
            .flash
            .data
            .iter()
            .rposition(|&b| b != 0xFF)
            .unwrap()
            + 1;
        assert_eq!(flash[..fw_len], plain.sys.bus.flash.data[..fw_len]);
        assert!(
            flash[fw_len.next_multiple_of(4096) + 4096..]
                .iter()
                .all(|&b| b == 0xA5)
        );
        assert!(
            Session::with_flash(
                &fw,
                None,
                &[],
                &Network::default(),
                Some(&vec![0; 17 << 20])
            )
            .is_err()
        );
    }

    #[test]
    fn the_boards_flash_entries_go_in_unless_there_is_an_image() {
        let fw = firmware("button-led");
        let board = Source::new(
            "b.toml",
            b"[[flash]]\naddr = 0x101ff000\nu16 = [63985, 990]\n".to_vec(),
        );
        let net = Network::default();
        let s = Session::new(&fw, Some(&board), &[], &net).unwrap();
        let settings = &s.sys.bus.flash.data[0x1f_f000..];
        assert_eq!(settings[..4], [0xF1, 0xF9, 0xDE, 0x03]);
        assert!(
            settings[4..].iter().all(|&b| b == 0xFF),
            "the rest stays erased"
        );
        // An image already has what the firmware made of them.
        let image = vec![0xA5; 2 << 20];
        let s = Session::with_flash(&fw, Some(&board), &[], &net, Some(&image)).unwrap();
        assert_eq!(s.sys.bus.flash.data[0x1f_f000..0x1f_f004], [0xA5; 4]);
    }

    #[test]
    fn sleeping_firmware_still_lets_time_pass() {
        // Waits (asleep) for a button that never comes.
        let board = Source::new("t.toml", b"parts.status = { kind = \"led\", gpio = 25 }\nparts.user = { kind = \"button\", gpio = 2 }".to_vec());
        let mut s = Session::new(
            &firmware("button-led"),
            Some(&board),
            &[],
            &Network::default(),
        )
        .unwrap();
        assert_eq!(s.step(CYCLES_PER_SEC as u64, far()), CYCLES_PER_SEC as u64);
        assert!((s.seconds() - 1.0).abs() < 1e-9);
        assert!(s.halted.is_none());
    }

    #[test]
    fn deadline_cuts_a_step_short() {
        let mut s = Session::new(&firmware("button-led"), None, &[], &Network::default()).unwrap();
        assert!(s.step(u64::MAX / 2, Instant::now()) == 0);
    }

    #[test]
    fn bad_inputs_are_reported() {
        let fw = firmware("button-led");
        let err = |board: &str| {
            Session::new(
                &fw,
                Some(&Source::new("x.toml", board.as_bytes().to_vec())),
                &[],
                &Network::default(),
            )
            .err()
            .unwrap()
        };
        assert_eq!(
            err("parts.a = { kind = \"led\" }"),
            "x.toml:1:7: missing gpio (the pin it hangs off)"
        );
        assert!(
            Session::new(
                &Source::new("junk.elf", vec![1, 2, 3]),
                None,
                &[],
                &Network::default()
            )
            .err()
            .unwrap()
            .starts_with("junk.elf: cannot load ELF")
        );
    }

    #[test]
    fn uart_text_is_kept_bounded() {
        let mut s = Session::new(&firmware("button-led"), None, &[], &Network::default()).unwrap();
        s.sys.bus.uart[0].tx = vec![b'x'; UART_KEEP + 10];
        s.drain();
        assert!(s.uart[0].len() <= UART_KEEP);
    }
}

#[cfg(test)]
mod log_tests {
    use web_time::Duration;

    use super::*;

    /// The BTL-01 OS logs over defmt-rtt; set `BTL_FIRMWARE` to its ELF.
    #[test]
    fn session_collects_the_firmware_log() {
        let Ok(path) = std::env::var("BTL_FIRMWARE") else {
            eprintln!("BTL_FIRMWARE not set: skipped");
            return;
        };
        let board = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../boards/btl-01.toml"
        ))
        .unwrap();
        let mut s = Session::new(
            &Source::new("fw", std::fs::read(path).unwrap()),
            Some(&Source::new("board", board)),
            &[],
            &Network::default(),
        )
        .unwrap();
        assert_eq!(s.log_note, None);
        s.step(125_000_000, Instant::now() + Duration::from_secs(60));
        assert!(
            s.log
                .iter()
                .any(|e| e.line.text.contains("Entering main loop")),
            "{:?}",
            s.log.iter().map(|e| &e.line.text).collect::<Vec<_>>()
        );
        let first = s.log.front().unwrap();
        assert!(first.at > 0.0 && first.at < 1.0);
    }
}

#[cfg(test)]
mod bench {
    use web_time::Duration;

    use super::*;

    /// Host speed of `Session::step` in 14 ms slices, no GUI. Run with
    /// `cargo test --release -p tartelette-runtime bench_ -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_headless_slices() {
        let fw = std::fs::read("../../../../btl_01/software/os-embedded/target/thumbv6m-none-eabi/release/btl_os_embedded").unwrap();
        let board = std::fs::read("../../../boards/btl-01.toml").unwrap();
        let mut s = Session::new(
            &Source::new("fw", fw),
            Some(&Source::new("board", board)),
            &[],
            &Network::default(),
        )
        .unwrap();
        let t0 = Instant::now();
        let mut slices = 0;
        while s.sys.cycles() < 125_000_000 {
            s.step(u64::MAX / 2, Instant::now() + Duration::from_millis(14));
            slices += 1;
        }
        let dt = t0.elapsed().as_secs_f64();
        println!(
            "{dt:.2} s host for 1 s emulated ({:.3}x), {slices} slices",
            1.0 / dt
        );
    }
}
