//! Steps a run in the browser, where there is no thread to give it: in short
//! slices, each its own task on the page's event loop, so that the browser
//! draws frames in between. Ticking the run from the UI frame instead caps
//! it to a share of each frame, and a frame that overruns the display's
//! refresh halves that again.
//!
//! The frames then come out of the run's time: while it is behind, the UI
//! spaces them out ([`frame_interval`]) so that they take at most about a
//! third of the page's time, and come no more than about 60 times a second.
use std::{cell::RefCell, sync::Arc};

use eframe::{wasm_bindgen::JsValue, web_sys};
use tartelette_runtime::run::{self, Run, Shared};
use wasm_bindgen_futures::JsFuture;
use web_time::{Duration, Instant};

/// How long to wait while the run is caught up with the wall clock.
const IDLE_MS: i32 = 1;
/// How often to look again while the page is hidden.
const HIDDEN_MS: i32 = 200;
/// How often the cost of a frame is measured again.
const WINDOW: Duration = Duration::from_millis(500);
/// The shortest interval between frames while the run is behind: a frame per
/// refresh at 60 Hz, every other one at 120 Hz.
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(12);

/// What the page took between slices while the run was behind, measured
/// over a window.
struct Load {
    since: Instant,
    /// Wall time between slices so far.
    away: Duration,
    frames: u32,
    /// Was the run behind for the whole window so far?
    behind: bool,
    /// What a frame, and whatever else the page did meanwhile, took in the
    /// last window; zero if the run caught up in it.
    frame_cost: Duration,
}

impl Load {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            away: Duration::ZERO,
            frames: 0,
            behind: true,
            frame_cost: Duration::ZERO,
        }
    }

    /// Count `away`, the time the page took after a slice that ended
    /// behind (`None` after one that caught up), and once a window is over,
    /// work out what a frame cost in it.
    fn measure(&mut self, away: Option<Duration>) {
        match away {
            Some(t) => self.away += t,
            None => self.behind = false,
        }
        if self.since.elapsed() >= WINDOW {
            if !self.behind {
                self.frame_cost = Duration::ZERO;
            } else if self.frames > 0 {
                self.frame_cost = self.away / self.frames;
            }
            *self = Self {
                frame_cost: self.frame_cost,
                ..Self::new()
            };
        }
    }
}

thread_local! {
    static LOAD: RefCell<Load> = RefCell::new(Load::new());
}

/// The UI is drawing a frame.
pub fn frame_drawn() {
    LOAD.with_borrow_mut(|l| l.frames += 1);
}

/// How long the UI should wait before its next frame while a run goes.
pub fn frame_interval() -> Duration {
    LOAD.with_borrow(|l| match l.frame_cost {
        Duration::ZERO => Duration::ZERO,
        cost => (cost * 3).max(MIN_FRAME_INTERVAL),
    })
}

/// Step `run` until the UI drops its last handle on it.
pub fn start(run: &Shared) {
    let weak = Arc::downgrade(run);
    wasm_bindgen_futures::spawn_local(async move {
        let Ok(channel) = web_sys::MessageChannel::new() else {
            return;
        };
        let document = web_sys::window().and_then(|w| w.document());
        while let Some(run) = weak.upgrade() {
            // A hidden page draws no frames, and the run waits for it as it
            // would if the frames stepped it.
            if document.as_ref().is_some_and(|d| d.hidden()) {
                drop(run);
                sleep(HIDDEN_MS).await;
                continue;
            }
            let start = Instant::now();
            {
                let mut g = run::lock(&run);
                let Run { session, clock } = &mut *g;
                clock.tick(session, run::SLICE);
            }
            drop(run);
            if start.elapsed() >= run::SLICE {
                // Behind: carry on as soon as the browser has had its turn.
                let away = Instant::now();
                yield_now(&channel).await;
                LOAD.with_borrow_mut(|l| l.measure(Some(away.elapsed())));
            } else {
                LOAD.with_borrow_mut(|l| l.measure(None));
                sleep(IDLE_MS).await;
            }
        }
    });
}

/// Let the browser run its other tasks (input, frames) and come back right
/// after. A message to self isn't clamped like a chain of zero timeouts is
/// (to 4 ms each).
async fn yield_now(channel: &web_sys::MessageChannel) {
    let done = js_sys::Promise::new(&mut |resolve, _| {
        channel.port1().set_onmessage(Some(&resolve));
        let _ = channel.port2().post_message(&JsValue::NULL);
    });
    let _ = JsFuture::from(done).await;
}

async fn sleep(ms: i32) {
    let done = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(w) = web_sys::window() {
            let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = JsFuture::from(done).await;
}
