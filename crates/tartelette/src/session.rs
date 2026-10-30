//! One emulated run: firmware + board, stepped against a wall-clock budget.
//! No UI here, so it behaves the same natively and in the browser.
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};

use pcb::{Board, BoardEvent};
use pcb_devices::sdcard::SdCard;
use rp2040_core::system::{CYCLES_PER_US, StopReason, System};
use web_time::{Duration, Instant};

pub const CYCLES_PER_SEC: f64 = CYCLES_PER_US as f64 * 1e6;

/// Cycles per `System::run` call: small enough to stay responsive to the
/// host-time budget, large enough to keep the per-call overhead down.
const CHUNK: u64 = 250_000;
/// Never keep more than this much UART text per port.
const UART_KEEP: usize = 256 * 1024;
/// Never keep more than this many log messages.
const LOG_KEEP: usize = 5000;

/// A file the run was built from.
#[derive(Clone)]
pub struct Source {
    pub name: String,
    pub bytes: Vec<u8>,
    /// Where it came from on disk (native only), for reloading.
    #[cfg(not(target_arch = "wasm32"))]
    pub path: Option<std::path::PathBuf>,
}

impl Source {
    pub fn new(name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            bytes,
            #[cfg(not(target_arch = "wasm32"))]
            path: None,
        }
    }

    /// Read a file, remembering where it came from.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_file(path: &std::path::Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        Ok(Self {
            name,
            bytes,
            path: std::fs::canonicalize(path).ok(),
        })
    }
}

/// What the board's Wi-Fi chips are connected to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(not(target_arch = "wasm32"), derive(clap::ValueEnum))]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))] // only the command line picks one
pub enum NetMode {
    /// The host's network, through a user-mode NAT (the host's 127.0.0.1 is 10.0.2.2)
    #[default]
    User,
    /// A deterministic fake network with only a gateway
    Loopback,
    /// No network, as in the browser
    None,
}

/// What the board's Bluetooth radios are connected to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))] // only the command line picks one
pub enum BtMode {
    /// No radio, as in the browser: the firmware finds and reaches nobody.
    #[default]
    None,
    /// Mock devices in software: a central, a BLE MIDI peripheral, a battery.
    Mock,
    /// A real adapter (`hciN`), through a Linux HCI user channel.
    Adapter(u16),
}

#[derive(Clone, Debug, Default)]
pub struct Network {
    pub mode: NetMode,
    pub bt: BtMode,
    /// (host port, guest port): the host's `127.0.0.1:HOST` forwarded to a
    /// TCP port on the guest (user mode only).
    pub forwards: Vec<(u16, u16)>,
    /// Which of the USB device's functions reach the host OS.
    pub usb: crate::usb::UsbBridges,
}

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
    /// of the flash, like MicroPython's filesystem, is kept.
    pub fn with_flash(
        firmware: &Source,
        board: Option<&Source>,
        shields: &[Source],
        net: &Network,
        flash: Option<&[u8]>,
    ) -> Result<Self, String> {
        let mut sys = System::new();
        if let Some(image) = flash {
            sys.bus
                .load(rp2040_core::system::XIP_BASE, image)
                .then_some(())
                .ok_or("--flash: the image is larger than 16 MiB")?;
        }
        let uf2 = rp2040_core::uf2::is_uf2(&firmware.bytes);
        sys.load_firmware(&firmware.bytes).map_err(|e| {
            let kind = if uf2 { "UF2" } else { "ELF" };
            format!("{}: cannot load {kind}: {e:?}", firmware.name)
        })?;
        sys.set_board(parse_board(board, shields)?);
        connect_network(&mut sys.board, net)?;
        let usb = crate::usb::UsbSide::new(&net.usb)?;
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

    /// Queue text for the firmware to receive on UART `n`, at the firmware's
    /// baud rate.
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

    /// Move UART output and board events out of the emulator.
    fn drain(&mut self) {
        for n in 0..2 {
            let tx = std::mem::take(&mut self.sys.bus.uart[n].tx);
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
            rtt.poll(&mut self.sys, &mut lines);
        }
        lines
    }

    /// SD card contents by part name, to carry over into a fresh run (a reset
    /// is a power cycle; the card keeps its data).
    pub fn take_sd_cards(&mut self) -> Vec<(String, Vec<u8>, bool)> {
        self.sys
            .board
            .devices_mut::<SdCard>()
            .map(|(n, card)| (n.to_string(), card.image().to_vec(), card.dirty()))
            .collect()
    }

    /// Load a disk image into the card `name`. It stops writing to any file
    /// it was writing to: that file holds a different disk.
    /// A copy of the card `name`'s image, with how many blocks the firmware
    /// has written to it so far (to tell later whether the copy is stale).
    pub fn sd_card_copy(&self, name: &str) -> Option<(Vec<u8>, u32)> {
        let card = self.sys.board.device::<SdCard>(name)?;
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

/// Give every Wi-Fi chip on the board a network to talk to. The browser has no
/// user-mode NAT, so there the chip finds no networks and the panel says why.
fn connect_network(board: &mut Board, net: &Network) -> Result<(), String> {
    let mut chips = board
        .devices_mut::<pcb_devices::cyw43439::Cyw43439>()
        .peekable();
    if chips.peek().is_none() && !net.forwards.is_empty() {
        return Err("--forward: no Wi-Fi chip on the board".into());
    }
    for (_, chip) in chips {
        match net.mode {
            #[cfg(not(target_arch = "wasm32"))]
            NetMode::User => {
                let mut slirp = netstack::Slirp::new();
                for &(host, guest) in &net.forwards {
                    let addr = slirp
                        .forward(host, guest)
                        .map_err(|e| format!("--forward {host}:{guest}: {e}"))?;
                    eprintln!("forwarding {addr} to the guest's port {guest}");
                }
                chip.set_backend(Box::new(slirp));
            }
            NetMode::Loopback => chip.set_backend(pcb_devices::net::LoopbackBackend::boxed()),
            _ => {}
        }
    }
    connect_bluetooth(board, net.bt)
}

/// Give every Bluetooth radio on the board its controller. A real adapter
/// can serve only one.
fn connect_bluetooth(board: &mut Board, bt: BtMode) -> Result<(), String> {
    use pcb_devices::{
        cyw43439::bt::bt_address,
        hci::{peers, virt::VirtualController},
    };
    let mut chips = board.devices_mut::<pcb_devices::cyw43439::Cyw43439>();
    while let Some((name, chip)) = chips.next() {
        match bt {
            BtMode::None => {}
            BtMode::Mock => chip.set_bt_backend(Box::new(VirtualController::new(
                bt_address(chip.mac()),
                peers::neighbourhood(),
            ))),
            BtMode::Adapter(dev) => {
                if chips.next().is_some() {
                    return Err(format!(
                        "--bt hci{dev}: the board has more than one Bluetooth radio"
                    ));
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    chip.set_bt_backend(hcibridge::open(dev)?);
                    eprintln!("{name}: Bluetooth bridged to hci{dev}");
                }
                #[cfg(target_arch = "wasm32")]
                let _ = (name, dev);
                return Ok(());
            }
        }
        let _ = name;
    }
    Ok(())
}

/// The board used without one: a plain Raspberry Pi Pico.
pub const PICO_BOARD: &str = include_str!("../../../boards/pico.toml");

/// Build the board from its file (a plain Pico without one) and the shields on
/// top, with the RP2040's pin rules for buses.
pub fn parse_board(board: Option<&Source>, shields: &[Source]) -> Result<Board, String> {
    let mut registry = pcb_devices::registry();
    registry.set_pin_check(rp2040_core::pinmux::check_bus_pin);
    let text = |s: &Source| {
        std::str::from_utf8(&s.bytes)
            .map(str::to_owned)
            .map_err(|_| format!("{}: not a text file", s.name))
    };
    let mut b = match board {
        Some(s) => {
            Board::parse_with(&text(s)?, &registry).map_err(|e| format!("{}:{e}", s.name))?
        }
        None => Board::parse_with(PICO_BOARD, &registry).map_err(|e| format!("pico.toml:{e}"))?,
    };
    for s in shields {
        b.add_shield(&text(s)?, &registry)
            .map_err(|e| format!("{}:{e}", s.name))?;
    }
    Ok(b)
}

/// Paces a run against the wall clock.
pub struct Clock {
    pub running: bool,
    /// Emulated time per wall-clock time (`f64::INFINITY` = as fast as possible).
    pub speed: f64,
    last: Instant,
    /// Emulated cycles owed to the run (fractional, so slow speeds work).
    owed: f64,
    /// Measured speed over the last second, as a ratio to real time.
    measured: f64,
    window: (Instant, u64),
}

/// Host time the emulator may take per UI frame, so the UI stays smooth.
#[cfg(target_arch = "wasm32")]
pub const FRAME_BUDGET: Duration = Duration::from_millis(14);
/// Host time per slice on the emulator thread: short, so the UI can take the
/// lock between slices.
#[cfg(not(target_arch = "wasm32"))]
const THREAD_SLICE: Duration = Duration::from_millis(2);
/// Don't try to catch up more than this after a stall (window dragged, tab hidden).
const MAX_OWED_SECS: f64 = 0.1;

impl Clock {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            running: true,
            speed: 1.0,
            last: now,
            owed: 0.0,
            measured: 0.0,
            window: (now, 0),
        }
    }

    /// Advance `s` by the wall time since the last tick.
    pub fn tick(&mut self, s: &mut Session, budget: Duration) -> u64 {
        let now = Instant::now();
        let dt = (now - self.last).as_secs_f64();
        self.last = now;
        let mut done = 0;
        if self.running && s.halted.is_none() {
            let want = if self.speed.is_finite() {
                self.owed = (self.owed + dt * self.speed * CYCLES_PER_SEC)
                    .min(MAX_OWED_SECS * self.speed * CYCLES_PER_SEC);
                self.owed as u64
            } else {
                u64::MAX / 2
            };
            done = s.step(want, now + budget);
            self.owed = (self.owed - done as f64).max(0.0);
        } else {
            self.owed = 0.0;
        }
        self.window.1 += done;
        let span = (now - self.window.0).as_secs_f64();
        if span >= 0.5 {
            self.measured = self.window.1 as f64 / CYCLES_PER_SEC / span;
            self.window = (now, 0);
        }
        done
    }

    /// Emulated seconds per real second, measured.
    pub fn measured(&self) -> f64 {
        self.measured
    }
}

/// A session with its pacing clock. Natively a thread steps it in the
/// background (the UI only locks it to draw); on the web the UI ticks it.
pub struct Run {
    pub session: Session,
    pub clock: Clock,
}

pub type Shared = Arc<Mutex<Run>>;

/// How many UI callers are waiting for the lock; the emulator thread backs
/// off while any are, and otherwise keeps stepping without a pause.
#[cfg(not(target_arch = "wasm32"))]
static UI_WAITING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn lock(r: &Shared) -> MutexGuard<'_, Run> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::sync::atomic::Ordering::SeqCst;
        UI_WAITING.fetch_add(1, SeqCst);
        let g = r.lock().unwrap_or_else(|e| e.into_inner());
        UI_WAITING.fetch_sub(1, SeqCst);
        g
    }
    #[cfg(target_arch = "wasm32")]
    r.lock().unwrap_or_else(|e| e.into_inner())
}

// In the browser the run never leaves the UI thread, and a USB MIDI bridge
// holds WebMIDI connections (JS objects, not `Send`); natively they are.
#[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
pub fn start(session: Session, running: bool, speed: f64) -> Shared {
    let mut clock = Clock::new();
    clock.running = running;
    clock.speed = speed;
    let shared = Arc::new(Mutex::new(Run { session, clock }));
    #[cfg(not(target_arch = "wasm32"))]
    {
        // Ends by itself once the UI drops its handle (reset, new firmware).
        let weak = Arc::downgrade(&shared);
        std::thread::spawn(move || {
            while let Some(run) = weak.upgrade() {
                let done = {
                    let mut g = run.lock().unwrap_or_else(|e| e.into_inner());
                    let Run { session, clock } = &mut *g;
                    clock.tick(session, THREAD_SLICE)
                };
                drop(run);
                // Leave the UI a window to take the lock, only when it wants it.
                if done == 0 {
                    std::thread::sleep(Duration::from_micros(1000));
                } else if UI_WAITING.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                    std::thread::sleep(Duration::from_micros(100));
                }
            }
        });
    }
    shared
}

/// End a run. Natively this waits for the emulator thread to let go of it, so
/// its host sockets (port forwards) are closed before a new run binds them.
pub fn stop(run: Shared) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let weak = Arc::downgrade(&run);
        drop(run);
        while weak.strong_count() > 0 {
            std::thread::sleep(Duration::from_micros(100));
        }
    }
    #[cfg(target_arch = "wasm32")]
    drop(run);
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, process::Command};

    use pcb_devices::is_lit;

    use super::*;

    /// Build one of the test firmwares in `firmware/tests/` (as the CLI tests do).
    fn firmware(name: &str) -> Source {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
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
            "/../../boards/btl-01.toml"
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
    use super::*;

    /// Host speed of `Session::step` in 14 ms slices, no GUI. Run with
    /// `cargo test --release -p tartelette bench_ -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_headless_slices() {
        let fw = std::fs::read("../../../btl_01/software/os-embedded/target/thumbv6m-none-eabi/release/btl_os_embedded").unwrap();
        let board = std::fs::read("../../boards/btl-01.toml").unwrap();
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
