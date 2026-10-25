//! One emulated run: firmware + board, stepped against a wall-clock budget.
//! No UI here, so it behaves the same natively and in the browser.
use pcb::{Board, BoardEvent};
use pcb_devices::sdcard::SdCard;
use rp2040_core::system::{CYCLES_PER_US, StopReason, System};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use web_time::{Duration, Instant};

pub const CYCLES_PER_SEC: f64 = CYCLES_PER_US as f64 * 1e6;

/// Cycles per `System::run` call: small enough to stay responsive to the
/// host-time budget, large enough to keep the per-call overhead down.
const CHUNK: u64 = 250_000;
/// While bytes are waiting for a UART receiver: about one byte time at
/// 115200 baud, so the 32-byte FIFO is fed gradually instead of overflowing.
const RX_CHUNK: u64 = 11_000;
/// Never keep more than this much UART text per port.
const UART_KEEP: usize = 256 * 1024;

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
}

pub struct Session {
    pub sys: System,
    /// Why the firmware stopped for good (breakpoint), if it did.
    pub halted: Option<StopReason>,
    /// Everything each UART has sent, as text.
    pub uart: [String; 2],
    /// Host -> firmware bytes not yet handed to each UART.
    rx: [VecDeque<u8>; 2],
    /// Board events since the UI last took them.
    pub events: Vec<BoardEvent>,
}

impl Session {
    /// Build a run from a firmware ELF and an optional board file.
    pub fn new(firmware: &Source, board: Option<&Source>) -> Result<Self, String> {
        let mut sys = System::new();
        sys.load_elf(&firmware.bytes).map_err(|e| format!("{}: cannot load ELF: {e:?}", firmware.name))?;
        if let Some(b) = board {
            sys.set_board(parse_board(b)?);
        }
        Ok(Self { sys, halted: None, uart: Default::default(), rx: Default::default(), events: Vec::new() })
    }

    /// Emulated seconds since power-up.
    pub fn seconds(&self) -> f64 {
        self.sys.cycles() as f64 / CYCLES_PER_SEC
    }

    /// Queue text for the firmware to receive on UART `n`.
    pub fn send(&mut self, n: usize, bytes: &[u8]) {
        self.rx[n].extend(bytes);
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
            self.feed_rx();
            let chunk = if self.rx.iter().any(|q| !q.is_empty()) { RX_CHUNK } else { CHUNK };
            let target = self.sys.cycles() + (end - self.sys.cycles()).min(chunk);
            match self.sys.run(target - self.sys.cycles()) {
                StopReason::CycleLimit => {}
                // Nothing can wake the cores except the host: let the time pass.
                StopReason::Sleeping => self.sys.idle_until(target),
                r => self.halted = Some(r),
            }
            self.drain();
        }
        self.sys.cycles() - start
    }

    fn feed_rx(&mut self) {
        for n in 0..2 {
            if let Some(b) = self.rx[n].pop_front() {
                self.sys.bus.uart[n].push_rx(b);
            }
        }
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
        self.events.append(&mut self.sys.board.events);
    }

    /// SD card contents by part name, to carry over into a fresh run (a reset
    /// is a power cycle; the card keeps its data).
    pub fn take_sd_cards(&mut self) -> Vec<(String, Vec<u8>, bool)> {
        let names: Vec<String> = self.sys.board.parts.iter().filter(|p| p.kind == "sdcard").map(|p| p.name.clone()).collect();
        names
            .into_iter()
            .filter_map(|n| {
                let card = self.sys.board.device_mut::<SdCard>(&n)?;
                Some((n, card.image().to_vec(), card.dirty()))
            })
            .collect()
    }

    pub fn load_sd_card(&mut self, name: &str, image: Vec<u8>) -> bool {
        match self.sys.board.device_mut::<SdCard>(name) {
            Some(card) => {
                card.load_image(image);
                true
            }
            None => false,
        }
    }
}

pub fn parse_board(b: &Source) -> Result<Board, String> {
    let text = std::str::from_utf8(&b.bytes).map_err(|_| format!("{}: not a text file", b.name))?;
    Board::parse_with(text, &pcb_devices::registry()).map_err(|e| format!("{}:{}: {}", b.name, e.line, e.msg))
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
        Self { running: true, speed: 1.0, last: now, owed: 0.0, measured: 0.0, window: (now, 0) }
    }

    /// Advance `s` by the wall time since the last tick.
    pub fn tick(&mut self, s: &mut Session, budget: Duration) -> u64 {
        let now = Instant::now();
        let dt = (now - self.last).as_secs_f64();
        self.last = now;
        let mut done = 0;
        if self.running && s.halted.is_none() {
            let want = if self.speed.is_finite() {
                self.owed = (self.owed + dt * self.speed * CYCLES_PER_SEC).min(MAX_OWED_SECS * self.speed * CYCLES_PER_SEC);
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

pub fn lock(r: &Shared) -> MutexGuard<'_, Run> {
    r.lock().unwrap_or_else(|e| e.into_inner())
}

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
                    let mut g = lock(&run);
                    let Run { session, clock } = &mut *g;
                    clock.tick(session, THREAD_SLICE)
                };
                drop(run);
                // Leave the UI a window to take the lock.
                std::thread::sleep(Duration::from_micros(if done == 0 { 1000 } else { 100 }));
            }
        });
    }
    shared
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_devices::is_lit;
    use std::path::PathBuf;
    use std::process::Command;

    /// Build one of the test firmwares in `firmware/` (as the CLI tests do).
    fn firmware(name: &str) -> Source {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target_dir = root.join("target/firmware").join(name);
        let status = Command::new(env!("CARGO"))
            .args(["build", "--release", "--target", "thumbv6m-none-eabi", "--manifest-path"])
            .arg(root.join(format!("firmware/{name}/Cargo.toml")))
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
        let board = Source::new("t.board", b"led status gpio=25\nbutton user gpio=2\npanel user at=0,0\n".to_vec());
        let mut s = Session::new(&firmware("button-led"), Some(&board)).unwrap();
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
    fn sleeping_firmware_still_lets_time_pass() {
        // Waits (asleep) for a button that never comes.
        let board = Source::new("t.board", b"led status gpio=25\nbutton user gpio=2\n".to_vec());
        let mut s = Session::new(&firmware("button-led"), Some(&board)).unwrap();
        assert_eq!(s.step(CYCLES_PER_SEC as u64, far()), CYCLES_PER_SEC as u64);
        assert!((s.seconds() - 1.0).abs() < 1e-9);
        assert!(s.halted.is_none());
    }

    #[test]
    fn deadline_cuts_a_step_short() {
        let mut s = Session::new(&firmware("button-led"), None).unwrap();
        assert!(s.step(u64::MAX / 2, Instant::now()) == 0);
    }

    #[test]
    fn bad_inputs_are_reported() {
        let fw = firmware("button-led");
        let err = |board: &str| Session::new(&fw, Some(&Source::new("x.board", board.as_bytes().to_vec()))).err().unwrap();
        assert_eq!(err("led a"), "x.board:1: missing gpio=N");
        assert!(Session::new(&Source::new("junk.elf", vec![1, 2, 3]), None).err().unwrap().starts_with("junk.elf: cannot load ELF"));
    }

    #[test]
    fn uart_text_is_kept_bounded() {
        let mut s = Session::new(&firmware("button-led"), None).unwrap();
        s.sys.bus.uart[0].tx = vec![b'x'; UART_KEEP + 10];
        s.drain();
        assert!(s.uart[0].len() <= UART_KEEP);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// Host speed of `Session::step` in 14 ms slices, no GUI. Run with
    /// `cargo test --release -p rp2040-app bench_ -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_headless_slices() {
        let fw = std::fs::read("../../../btl_01/software/os-embedded/target/thumbv6m-none-eabi/release/btl_os_embedded").unwrap();
        let board = std::fs::read("../../boards/btl-01.board").unwrap();
        let mut s = Session::new(&Source::new("fw", fw), Some(&Source::new("board", board))).unwrap();
        let t0 = Instant::now();
        let mut slices = 0;
        while s.sys.cycles() < 125_000_000 {
            s.step(u64::MAX / 2, Instant::now() + Duration::from_millis(14));
            slices += 1;
        }
        let dt = t0.elapsed().as_secs_f64();
        println!("{dt:.2} s host for 1 s emulated ({:.3}x), {slices} slices", 1.0 / dt);
    }
}
