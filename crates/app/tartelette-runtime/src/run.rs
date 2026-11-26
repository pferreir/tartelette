//! A session paced against the wall clock, natively on a thread of its own.
use std::sync::{Arc, Mutex, MutexGuard};

use web_time::{Duration, Instant};

use crate::session::Session;

/// Paces a run against the wall clock.
pub struct Clock {
    pub running: bool,
    /// Emulated time per wall-clock time (`f64::INFINITY` = as fast as possible).
    pub speed: f64,
    last: Instant,
    /// Emulated seconds owed to the run.
    owed: f64,
    /// Measured speed over the last second, as a ratio to real time, and
    /// the emulated seconds run since `window.0`.
    measured: f64,
    window: (Instant, f64),
}

/// Host time per slice of a run: short, so the UI gets its turn between
/// slices (natively it takes the lock, in a browser it draws a frame).
pub const SLICE: Duration = Duration::from_millis(2);
/// Don't try to catch up more than this after a stall (window dragged, tab hidden).
const MAX_OWED_SECS: f64 = 0.1;

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    /// Running at real time.
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            running: true,
            speed: 1.0,
            last: now,
            owed: 0.0,
            measured: 0.0,
            window: (now, 0.0),
        }
    }

    /// Advance `s` by the wall time since the last tick.
    pub fn tick(&mut self, s: &mut Session, budget: Duration) -> u64 {
        let now = Instant::now();
        let dt = (now - self.last).as_secs_f64();
        self.last = now;
        let mut done = 0;
        let before = s.seconds();
        if self.running && s.halted.is_none() {
            // In cycles at clk_sys as it is now (firmware may change it).
            let hz = s.sys.sys_hz() as f64;
            let want = if self.speed.is_finite() {
                self.owed = (self.owed + dt * self.speed).min(MAX_OWED_SECS * self.speed);
                (self.owed * hz) as u64
            } else {
                u64::MAX / 2
            };
            done = s.step(want, now + budget);
            self.owed = (self.owed - done as f64 / hz).max(0.0);
        } else {
            self.owed = 0.0;
        }
        self.window.1 += s.seconds() - before;
        let span = (now - self.window.0).as_secs_f64();
        if span >= 0.5 {
            self.measured = self.window.1 / span;
            self.window = (now, 0.0);
        }
        done
    }

    /// Emulated seconds per real second, measured.
    pub fn measured(&self) -> f64 {
        self.measured
    }
}

/// A session with its pacing clock. Something other than the UI steps it
/// (natively a thread, in a browser tasks between frames), and the UI only
/// locks it to draw.
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
                    clock.tick(session, SLICE)
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
