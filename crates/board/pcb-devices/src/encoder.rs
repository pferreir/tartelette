//! A rotary encoder's two quadrature contacts (A = CLK, B = DT) on two pins. Each contact pulls
//! its pin to ground when closed and leaves it floating when open, so the
//! firmware needs pull-ups, as with the real part.
//!
//! Host action `turn` with value n makes |n| detents, clockwise when n > 0.
//! One detent is a full quadrature cycle of four transitions, `step_us`
//! apart, scheduled with device timers so the firmware samples them in real
//! (simulated) time:
//!
//! ```text
//! clockwise:         CLK leads    (CLK,DT) 11 -> 01 -> 00 -> 10 -> 11
//! counter-clockwise: DT leads     (CLK,DT) 11 -> 10 -> 00 -> 01 -> 11
//! ```
//!
//! Parts differ in which contact leads clockwise: `cw = "dt"` swaps the two.
//!
//! A `detent` event (value +1 / -1) is emitted as each cycle completes and
//! reaches a different detent. A turn the other way first cancels queued
//! steps of the one in progress.
//! (`mhz` is the core clock, to turn microseconds into cycles.)
use alloc::{boxed::Box, collections::VecDeque};

use pcb::{Device, Io, Kind, Param};

/// Contacts closed (CLK, DT) in each phase of a clockwise cycle.
const CLOSED: [(bool, bool); 4] = [(false, false), (true, false), (true, true), (false, true)];

pub struct Encoder {
    clk: usize,
    dt: usize,
    /// DT, not CLK, leads when turning clockwise.
    dt_leads: bool,
    /// Time between transitions, the `mhz` key if set, and the cycles the
    /// former takes at the clock.
    step_us: u64,
    mhz: Option<u64>,
    step_cycles: u64,
    /// Current phase 0..4 and the phase steps still to make (+1 / -1 each).
    phase: usize,
    queue: VecDeque<i8>,
    /// Phase steps made in all (net), and the detent last reported.
    steps: i64,
    detent: i64,
    /// A wake-up is pending.
    scheduled: bool,
}

pub const KIND: Kind = Kind {
    name: "encoder",
    summary: "rotary encoder (quadrature contacts)",
    bus: None,
    params: &[
        Param::pin("clk", "contact A"),
        Param::pin("dt", "contact B"),
        Param::choice(
            "cw",
            &["clk", "dt"],
            "the contact that leads when turning clockwise",
        )
        .default_str("clk"),
        Param::int(
            "step_us",
            1,
            1_000_000,
            "time between transitions, in microseconds",
        )
        .default_int(300),
        Param::int(
            "mhz",
            1,
            1000,
            "core clock, to turn time into cycles (default: the chip's clk_sys)",
        )
        .optional(),
    ],
    build: |p| {
        let (clk, dt, dt_leads) = (p.pin("clk"), p.pin("dt"), p.str("cw") == "dt");
        let (step_us, mhz) = (p.int("step_us") as u64, p.opt_int("mhz").map(|m| m as u64));
        Ok(Box::new(Encoder {
            clk,
            dt,
            dt_leads,
            step_us,
            mhz,
            step_cycles: step_us * mhz.unwrap_or(125),
            phase: 0,
            queue: VecDeque::new(),
            steps: 0,
            detent: 0,
            scheduled: false,
        }))
    },
};

impl Encoder {
    fn drive(&self, io: &mut Io) {
        let (mut a, mut b) = CLOSED[self.phase];
        if self.dt_leads {
            (a, b) = (b, a);
        }
        io.drive(self.clk, a.then_some(false));
        io.drive(self.dt, b.then_some(false));
    }
}

impl Device for Encoder {
    fn clock(&mut self, _at: u64, hz: u64) {
        let cycles_per_us = self.mhz.unwrap_or(hz / 1_000_000);
        self.step_cycles = self.step_us * cycles_per_us;
    }

    fn attach(&mut self, io: &mut Io) {
        self.drive(io);
    }

    fn input(&mut self, io: &mut Io, action: &str, value: i64) -> bool {
        if action != "turn" {
            return false;
        }
        let dir = if value >= 0 { 1 } else { -1 };
        for _ in 0..value.unsigned_abs() * 4 {
            // Turning back first takes back steps not made yet, so the
            // contacts reverse at once instead of finishing the old turn.
            if self.queue.back() == Some(&-dir) {
                self.queue.pop_back();
            } else {
                self.queue.push_back(dir);
            }
        }
        if !self.scheduled && !self.queue.is_empty() {
            self.scheduled = true;
            io.wake_at(io.cycles() + self.step_cycles);
        }
        true
    }

    fn timer(&mut self, io: &mut Io) {
        let Some(step) = self.queue.pop_front() else {
            self.scheduled = false;
            return;
        };
        self.steps += step as i64;
        self.phase = self.steps.rem_euclid(4) as usize;
        self.drive(io);
        // Back at the detent it started from is no detent.
        if self.phase == 0 && self.steps / 4 != self.detent {
            io.emit("detent", self.steps / 4 - self.detent);
            self.detent = self.steps / 4;
        }
        if self.queue.is_empty() {
            self.scheduled = false;
        } else {
            io.wake_at(io.cycles() + self.step_cycles);
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use pcb::{Board, Pins};

    use crate::testutil::FakePins;

    const CLK: usize = 22;
    const DT: usize = 21;

    struct Rig {
        board: Board,
        pins: FakePins,
        now: u64,
    }

    impl Rig {
        fn new(extra: &str) -> Self {
            let sep = if extra.is_empty() { "" } else { ", " };
            let text = alloc::format!(
                "parts.knob = {{ kind = \"encoder\", clk = {CLK}, dt = {DT}{sep}{extra} }}"
            );
            let mut board = Board::parse_with(&text, &crate::registry()).unwrap();
            // Pull-ups on the pads: an open contact reads high.
            let mut pins = FakePins::default();
            pins.floating = 1 << CLK | 1 << DT;
            board.attach(&mut pins);
            Rig {
                board,
                pins,
                now: 0,
            }
        }
        fn turn(&mut self, n: i64) {
            self.board
                .input(&mut self.pins, "knob", "turn", n, self.now);
        }
        fn levels(&self) -> (bool, bool) {
            (self.pins.level(CLK), self.pins.level(DT))
        }
        /// Advance to `t`, returning the (CLK, DT) levels after each timer.
        fn run_to(&mut self, t: u64) -> Vec<(u64, bool, bool)> {
            let mut seen = Vec::new();
            while let Some(next) = self.board.next_timer().filter(|&n| n <= t) {
                self.now = next;
                self.board.fire_timers(&mut self.pins, next);
                let (a, b) = self.levels();
                seen.push((next, a, b));
            }
            self.now = t;
            seen
        }
    }

    #[test]
    fn idle_encoder_leaves_both_pins_floating_high() {
        let r = Rig::new("");
        assert_eq!(r.levels(), (true, true));
        assert_eq!(r.board.next_timer(), None);
    }

    #[test]
    fn one_clockwise_detent_is_the_gray_cycle_with_clk_leading() {
        let mut r = Rig::new("step_us = 100, mhz = 1");
        r.turn(1);
        assert_eq!(r.board.next_timer(), Some(100));
        let steps = r.run_to(1000);
        assert_eq!(
            steps,
            [
                (100, false, true),
                (200, false, false),
                (300, true, false),
                (400, true, true)
            ]
        );
        assert_eq!(r.board.next_timer(), None);
        let ev: Vec<_> = r
            .board
            .events
            .iter()
            .map(|e| (e.cycles, e.label, e.value))
            .collect();
        assert_eq!(ev, [(400, "detent", 1)]);
    }

    #[test]
    fn counter_clockwise_has_dt_leading() {
        let mut r = Rig::new("step_us = 10, mhz = 1");
        r.turn(-1);
        let steps = r.run_to(1000);
        assert_eq!(
            steps.iter().map(|s| (s.1, s.2)).collect::<Vec<_>>(),
            [(true, false), (false, false), (false, true), (true, true)]
        );
        assert_eq!(r.board.events[0].value, -1);
    }

    #[test]
    fn cw_dt_makes_dt_lead_clockwise() {
        let mut r = Rig::new("cw = \"dt\", step_us = 10, mhz = 1");
        r.turn(1);
        let steps = r.run_to(1000);
        assert_eq!(
            steps.iter().map(|s| (s.1, s.2)).collect::<Vec<_>>(),
            [(true, false), (false, false), (false, true), (true, true)]
        );
        assert_eq!(r.board.events[0].value, 1);
    }

    #[test]
    fn several_detents_queue_up_and_turns_can_be_extended() {
        let mut r = Rig::new("step_us = 10, mhz = 1");
        r.turn(2);
        r.run_to(35);
        r.turn(1); // extend while moving
        let steps = r.run_to(10_000);
        assert_eq!(r.levels(), (true, true), "back at the detent");
        let detents: Vec<i64> = r.board.events.iter().map(|e| e.value).collect();
        assert_eq!(detents, [1, 1, 1]);
        assert!(steps.len() > 4);
        // Reversing direction mid-cycle just walks the phases backwards.
        let mut r = Rig::new("step_us = 10, mhz = 1");
        r.turn(1);
        r.run_to(25); // two steps in: CLK and DT both closed
        assert_eq!(r.levels(), (false, false));
        r.turn(-1);
        r.run_to(10_000);
        assert_eq!(r.levels(), (true, true));
    }

    #[test]
    fn turning_back_cancels_queued_steps() {
        let mut r = Rig::new("step_us = 10, mhz = 1");
        r.turn(3);
        r.run_to(15); // one step in
        r.turn(-4);
        let steps = r.run_to(10_000);
        // One step back to the start detent, then one detent the other way.
        assert_eq!(steps.len(), 1 + 4);
        assert_eq!(r.levels(), (true, true));
        let detents: Vec<i64> = r.board.events.iter().map(|e| e.value).collect();
        assert_eq!(detents, [-1]);
    }

    #[test]
    fn unknown_actions_are_refused_and_params_validated() {
        let mut r = Rig::new("");
        assert_eq!(
            r.board.input(&mut r.pins, "knob", "spin", 1, 0),
            pcb::InputResult::Unsupported
        );
        let reg = crate::registry();
        assert!(Board::parse_with("parts.e = { kind = \"encoder\", clk = 1 }", &reg).is_err());
        assert!(
            Board::parse_with(
                "parts.e = { kind = \"encoder\", clk = 1, dt = 2, step_us = 0 }",
                &reg
            )
            .is_err()
        );
        assert!(
            Board::parse_with(
                "parts.e = { kind = \"encoder\", clk = 1, dt = 2, cw = \"a\" }",
                &reg
            )
            .is_err()
        );
    }
}
