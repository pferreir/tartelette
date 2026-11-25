//! One emulated run: firmware + board, and the host's side of its links.
//! No UI here, so it behaves the same natively, in the browser and headless.
//!
//! A front end steps the run ([`Session::step`] against a wall-clock
//! deadline, or [`Session::advance`] one stretch at a time) and takes what
//! it produced with [`Session::take_output`].
use pcb::BoardEvent;
use pcb_devices::{
    midi::{Midi, Route},
    sdcard::SdCard,
    wav::Wav,
};
use rp2040_core::system::{CYCLES_PER_US, Clock, SRAM_BASE, StopReason, System};
use web_time::Instant;

use crate::{
    Links, Output, Source,
    boards::parse_board,
    network,
    uart::{UartSide, midi::Dest},
    usb::UsbSide,
};

/// Cycles a second at clk_sys's default 125 MHz (firmware may change it:
/// see [`System::clock`]).
pub const CYCLES_PER_SEC: f64 = CYCLES_PER_US as f64 * 1e6;

/// Cycles per `System::run` call in [`Session::step`]: small enough to stay
/// responsive to the host-time budget, large enough to keep the per-call
/// overhead down.
const CHUNK: u64 = 250_000;

/// A message of the firmware's defmt log and the emulated time it was read at.
pub struct LogEntry {
    pub at: f64,
    pub line: rtt::Line,
}

/// Hears every board event as it is taken out of the emulator, on the thread
/// that steps the run (a live synth, which must not wait for the UI), with
/// the clock that turns their cycles into time.
pub type Listener = Box<dyn FnMut(&[BoardEvent], Clock) + Send>;

pub struct Session {
    pub sys: System,
    /// Why the firmware stopped for good (breakpoint, fault), if it did
    /// during [`Session::step`].
    pub halted: Option<StopReason>,
    /// Why there is no defmt log, when the firmware has none or it cannot be
    /// decoded.
    pub log_note: Option<String>,
    /// The USB host's side: input for the serial port, bridges.
    pub usb: UsbSide,
    /// The far end of the UART lines: input for the firmware, bridges.
    pub uarts: UartSide,
    output: Output,
    rtt: Option<rtt::Rtt>,
    listener: Option<Listener>,
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
        links: &Links,
    ) -> Result<Self, String> {
        Self::with_flash(firmware, board, shields, links, None)
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
        links: &Links,
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
        network::connect(&mut sys.board, links)?;
        let usb = UsbSide::new(&links.usb)?;
        let uarts = UartSide::new(&links.uart)?;
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
        let mut s = Self {
            sys,
            halted: None,
            log_note,
            usb,
            uarts,
            output: Output::default(),
            rtt,
            listener: None,
            #[cfg(not(target_arch = "wasm32"))]
            sd_files: Vec::new(),
        };
        // A bridged port is closed until a program opens it.
        let lines = !s.usb.serial_bridged();
        s.sys.with_usb_host(|h| h.serial_lines = lines);
        Ok(s)
    }

    /// Emulated seconds since power-up.
    pub fn seconds(&self) -> f64 {
        self.sys.seconds()
    }

    /// Put bytes on UART `n`'s line straight away, at the firmware's baud
    /// rate; lost if its receiver is off (for live input like MIDI; text
    /// that must arrive goes in `uarts.input`).
    pub fn send(&mut self, n: usize, bytes: &[u8]) {
        self.sys.send_uart(n, bytes);
    }

    /// Hand MIDI bytes to `dest`: a UART's receiver, or a `midi` part (whose
    /// own UART, or which sends them on its pin).
    pub fn midi(&mut self, dest: &Dest, bytes: &[u8]) {
        match dest {
            Dest::Uart(n) => self.send(*n, bytes),
            Dest::Part(name) => match self.sys.board.device::<Midi>(name).map(|m| m.route) {
                Some(Route::Uart(n)) => self.send(n, bytes),
                Some(Route::Pin(_)) => {
                    for &b in bytes {
                        self.sys.input(name, "byte", b as i64);
                    }
                    self.collect();
                }
                None => {}
            },
        }
    }

    /// Do something to a board part (see each device's docs for actions).
    /// A MIDI jack on a UART takes `byte` here: the host feeds that UART.
    pub fn input(&mut self, part: &str, action: &str, value: i64) -> bool {
        if action == "byte"
            && let Some(Route::Uart(n)) = self.sys.board.device::<Midi>(part).map(|m| m.route)
        {
            self.send(n, &[value as u8]);
            return true;
        }
        let ok = self.sys.input(part, action, value);
        self.collect();
        ok
    }

    /// Let `listener` hear every board event from now on, in place of the
    /// one before.
    pub fn listen(&mut self, listener: Listener) {
        self.listener = Some(listener);
    }

    pub fn stop_listening(&mut self) {
        self.listener = None;
    }

    /// Everything the run produced since the last call.
    pub fn take_output(&mut self) -> Output {
        std::mem::take(&mut self.output)
    }

    /// Run up to `cycles`, stopping early once `deadline` has passed, or for
    /// good on a breakpoint or fault (see `halted`). With every core asleep
    /// time passes anyway: the host may wake them. Returns the cycles
    /// emulated (time spent asleep counts).
    pub fn step(&mut self, cycles: u64, deadline: Instant) -> u64 {
        let start = self.sys.cycles();
        let end = start + cycles;
        while self.halted.is_none() && self.sys.cycles() < end && Instant::now() < deadline {
            let target = self.sys.cycles() + (end - self.sys.cycles()).min(CHUNK);
            match self.run(target - self.sys.cycles()) {
                StopReason::CycleLimit => {}
                // Nothing can wake the cores except the host: let the time pass.
                StopReason::Sleeping => self.sys.idle_until(target),
                r => self.halted = Some(r),
            }
            self.collect();
        }
        self.sys.cycles() - start
    }

    /// Run up to `cycles` once, and say why it stopped; what to do about
    /// sleeping cores or a breakpoint is up to the caller.
    pub fn advance(&mut self, cycles: u64) -> StopReason {
        let r = self.run(cycles);
        self.collect();
        r
    }

    /// Is input from the host on its way to the firmware: bytes on a UART's
    /// line, or waiting for a receiver that is on? They may wake it.
    pub fn input_pending(&self) -> bool {
        (0..2).any(|n| {
            let uart = &self.sys.bus.uart[n];
            uart.next_event().is_some()
                || (uart.receiving() && uart.rx_queued() == 0 && !self.uarts.input[n].is_empty())
        })
    }

    fn run(&mut self, cycles: u64) -> StopReason {
        let r = self.sys.run(cycles);
        if matches!(r, StopReason::Breakpoint(_)) {
            // The UARTs keep sending what is in their FIFOs, as on the chip.
            self.sys.drain_uarts();
        }
        r
    }

    /// Move what the emulator produced into the output, and what the host
    /// has for the firmware onto its lines.
    fn collect(&mut self) {
        for n in 0..2 {
            let uart = &mut self.sys.bus.uart[n];
            let sent = std::mem::take(&mut uart.tx);
            let (receiving, queued) = (uart.receiving(), uart.rx_queued());
            let rx = self.uarts.exchange(n, &sent, receiving, queued);
            if !rx.is_empty() {
                self.sys.send_uart(n, &rx);
            }
            self.output.uart(n, &sent);
        }
        self.output.notes(self.uarts.notes.drain(..));
        let events = std::mem::take(&mut self.sys.board.events);
        if let Some(listener) = &mut self.listener
            && !events.is_empty()
        {
            listener(&events, self.sys.clock());
        }
        self.output.events(events);
        self.output.warnings(self.sys.warnings.drain(..));
        self.output.warnings(self.sys.board.take_warnings());
        let (usb, output, clock) = (&mut self.usb, &mut self.output, self.sys.clock());
        self.sys
            .with_usb_host(|h| usb.exchange(h, |cycles| clock.seconds(cycles), output));
        #[cfg(not(target_arch = "wasm32"))]
        self.write_sd_files();
        if let Some(rtt) = self.rtt.as_mut() {
            let mut lines = Vec::new();
            rtt.poll(&mut self.sys.bus.sram.data, SRAM_BASE, &mut lines);
            let at = self.sys.seconds();
            for line in lines {
                self.output.log(LogEntry { at, line });
            }
        }
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

    /// Load a WAV file into the player `name`: it waits at the file's start,
    /// or plays it if the player has `autoplay`. False if there is no such
    /// player.
    pub fn load_wav(&mut self, name: &str, file: &Source) -> Result<bool, String> {
        let Some(wav) = self.sys.board.device_mut::<Wav>(name) else {
            return Ok(false);
        };
        wav.load(&file.name, &file.bytes)?;
        if wav.autoplay {
            self.input(name, "play", 0);
        }
        Ok(true)
    }

    /// Load a disk image into the card `name`. It stops writing to any file
    /// it was writing to: that file holds a different disk.
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
    fn write_sd_files(&mut self) {
        use std::io::{Seek, SeekFrom, Write};
        let board = &mut self.sys.board;
        let mut failed = Vec::new();
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
                    failed.push(format!(
                        "{}: {e}; no longer writing SD card {} to it",
                        f.path.display(),
                        f.part
                    ))
                })
                .is_ok()
        });
        self.output.warnings(failed);
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
            &Links::default(),
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
        let out = s.take_output();
        assert_eq!(out.uart[0], b"done\n");
        // Events are moved out of the board; nothing piles up there.
        assert!(s.sys.board.events.is_empty());
        let labels: Vec<_> = out.events.iter().map(|e| e.label).collect();
        assert_eq!(labels, ["pressed", "on", "released", "off"]);
        assert!(s.take_output().is_empty(), "taken once");
        // A halted run stays put.
        let t = s.sys.cycles();
        assert_eq!(s.step(1000, far()), 0);
        assert_eq!(s.sys.cycles(), t);
    }

    #[test]
    fn midi_goes_down_a_jacks_pin() {
        let board = Source::new(
            "t.toml",
            b"parts.din = { kind = \"midi\", gpio = 5 }".to_vec(),
        );
        let mut s = Session::new(
            &firmware("button-led"),
            Some(&board),
            &[],
            &Links::default(),
        )
        .unwrap();
        s.step(1000, far());
        s.take_output();
        s.midi(&Dest::Part("din".into()), &[0x90, 0x3C, 0x64]);
        s.step(125 * 2000, far());
        let sent: Vec<_> = s
            .take_output()
            .events
            .iter()
            .filter(|e| e.label == "sent")
            .map(|e| (e.cycles, e.value))
            .collect();
        assert_eq!(
            sent.iter().map(|e| e.1).collect::<Vec<_>>(),
            [0x90, 0x3C, 0x64]
        );
        // A byte every 10 bits of 32 us.
        assert_eq!(sent[1].0 - sent[0].0, 125 * 320);
        assert!(
            s.sys.bus.gpio.pin_level(5),
            "idle high after the last stop bit"
        );
    }

    #[test]
    fn midi_into_a_jack_on_a_uart_reaches_the_firmware() {
        let board = Source::new(
            "t.toml",
            b"parts.trs = { kind = \"midi\", uart = 0 }".to_vec(),
        );
        let mut s =
            Session::new(&firmware("uart-echo"), Some(&board), &[], &Links::default()).unwrap();
        while !s.sys.bus.uart[0].receiving() {
            s.step(125 * 100, far());
        }
        s.take_output();
        // The echo upper-cases what arrives.
        s.midi(&Dest::Part("trs".into()), b"ab");
        s.midi(&Dest::Uart(0), b"c");
        s.step(125 * 5000, far());
        assert_eq!(s.take_output().uart[0], b"ABC");
    }

    #[test]
    fn a_flash_image_goes_under_the_firmware() {
        let fw = firmware("button-led");
        let plain = Session::new(&fw, None, &[], &Links::default()).unwrap();
        let image = vec![0xA5; 4 << 20];
        let s = Session::with_flash(&fw, None, &[], &Links::default(), Some(&image)).unwrap();
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
            Session::with_flash(&fw, None, &[], &Links::default(), Some(&vec![0; 17 << 20]))
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
        let net = Links::default();
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
            &Links::default(),
        )
        .unwrap();
        assert_eq!(s.step(CYCLES_PER_SEC as u64, far()), CYCLES_PER_SEC as u64);
        assert!((s.seconds() - 1.0).abs() < 1e-9);
        assert!(s.halted.is_none());
    }

    #[test]
    fn deadline_cuts_a_step_short() {
        let mut s = Session::new(&firmware("button-led"), None, &[], &Links::default()).unwrap();
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
                &Links::default(),
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
                &Links::default()
            )
            .err()
            .unwrap()
            .starts_with("junk.elf: cannot load ELF")
        );
    }

    const BUTTON_LED: &[u8] = b"parts.status = { kind = \"led\", gpio = 25 }\nparts.user = { kind = \"button\", gpio = 2 }";

    /// The button-led board, pressed at 1 ms and released at 2 ms, stepped
    /// by `step`: what it printed, and its events (when and what).
    fn pressed_and_released(
        mut step: impl FnMut(&mut Session, u64),
    ) -> (Vec<u8>, Vec<(u64, &'static str)>) {
        let board = Source::new("t.toml", BUTTON_LED.to_vec());
        let mut s = Session::new(
            &firmware("button-led"),
            Some(&board),
            &[],
            &Links::default(),
        )
        .unwrap();
        let (mut uart, mut events) = (Vec::new(), Vec::new());
        let mut take = |s: &mut Session| {
            let o = s.take_output();
            uart.extend(&o.uart[0]);
            events.extend(o.events.iter().map(|e| (e.cycles, e.label)));
        };
        for (at, action) in [(125_000, "press"), (250_000, "release")] {
            let n = at - s.sys.cycles();
            step(&mut s, n);
            take(&mut s);
            s.input("user", action, 0);
        }
        step(&mut s, 10_000_000);
        take(&mut s);
        (uart, events)
    }

    #[test]
    fn advance_and_step_produce_the_same_output() {
        let by_step = pressed_and_released(|s, n| {
            s.step(n, far());
        });
        let by_advance = pressed_and_released(|s, n| {
            let end = s.sys.cycles() + n;
            while s.sys.cycles() < end {
                match s.advance(end - s.sys.cycles()) {
                    StopReason::CycleLimit => {}
                    StopReason::Sleeping => s.sys.idle_until(end),
                    _ => break,
                }
            }
        });
        assert_eq!(by_step.0, b"done\n");
        assert_eq!(by_step.1.len(), 4);
        assert_eq!(by_step, by_advance);
    }

    #[test]
    fn the_listener_hears_every_event_the_output_gets() {
        let board = Source::new("t.toml", BUTTON_LED.to_vec());
        let mut s = Session::new(
            &firmware("button-led"),
            Some(&board),
            &[],
            &Links::default(),
        )
        .unwrap();
        let heard = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let h = heard.clone();
        s.listen(Box::new(move |events: &[BoardEvent], _| {
            h.lock().unwrap().extend(events.iter().map(|e| e.label));
        }));
        s.step(1000, far());
        s.input("user", "press", 0);
        s.step(1000, far());
        s.stop_listening();
        s.input("user", "release", 0);
        let labels: Vec<_> = s.take_output().events.iter().map(|e| e.label).collect();
        assert_eq!(labels, ["pressed", "on", "released"]);
        assert_eq!(*heard.lock().unwrap(), ["pressed", "on"]);
    }
}

#[cfg(test)]
mod log_tests {
    use web_time::Duration;

    use super::*;

    /// Set `DEFMT_FIRMWARE` to a firmware that logs over defmt-rtt, and
    /// `DEFMT_BOARD` to its board file if it needs more than a plain Pico.
    #[test]
    fn session_collects_the_firmware_log() {
        let Ok(path) = std::env::var("DEFMT_FIRMWARE") else {
            eprintln!("DEFMT_FIRMWARE not set: skipped");
            return;
        };
        let board = std::env::var("DEFMT_BOARD")
            .ok()
            .map(|b| Source::new("board", std::fs::read(b).unwrap()));
        let mut s = Session::new(
            &Source::new("fw", std::fs::read(path).unwrap()),
            board.as_ref(),
            &[],
            &Links::default(),
        )
        .unwrap();
        assert_eq!(s.log_note, None);
        s.step(125_000_000, Instant::now() + Duration::from_secs(60));
        let log = s.take_output().log;
        assert!(!log.is_empty(), "the firmware logs during boot");
        let first = log.front().unwrap();
        assert!(first.at > 0.0 && first.at < 1.0);
    }
}

#[cfg(test)]
mod bench {
    use web_time::Duration;

    use super::*;

    /// Host speed of `Session::step` in 14 ms slices, no GUI, running the
    /// firmware in `BENCH_FIRMWARE` on the board in `BENCH_BOARD` (a plain
    /// Pico if unset). Run with
    /// `cargo test --release -p tartelette-runtime bench_ -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_headless_slices() {
        let fw =
            std::fs::read(std::env::var("BENCH_FIRMWARE").expect("set BENCH_FIRMWARE")).unwrap();
        let board = std::env::var("BENCH_BOARD")
            .ok()
            .map(|b| Source::new("board", std::fs::read(b).unwrap()));
        let mut s = Session::new(
            &Source::new("fw", fw),
            board.as_ref(),
            &[],
            &Links::default(),
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
