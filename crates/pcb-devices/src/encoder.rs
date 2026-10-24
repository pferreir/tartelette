//! `encoder <name> clk=N dt=N [step_us=300] [mhz=125]`: a rotary encoder's
//! two quadrature contacts (A = CLK, B = DT) on two pins. Each contact pulls
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
//! A `detent` event (value +1 / -1) is emitted as each cycle completes.
//! (`mhz` is the core clock, to turn microseconds into cycles.)
use alloc::{boxed::Box, collections::VecDeque, string::String};
use pcb::{Device, Io, Params};

/// Contacts closed (CLK, DT) in each phase of a clockwise cycle.
const CLOSED: [(bool, bool); 4] = [(false, false), (true, false), (true, true), (false, true)];

pub struct Encoder {
    clk: usize,
    dt: usize,
    step_cycles: u64,
    /// Current phase 0..4 and the phase steps still to make (+1 / -1 each).
    phase: usize,
    queue: VecDeque<i8>,
    /// A wake-up is pending.
    scheduled: bool,
}

pub fn factory(p: &mut Params) -> Result<Box<dyn Device>, String> {
    let clk = p.pin("clk")?;
    let dt = p.pin("dt")?;
    let step_us = p.opt_int("step_us", 1, 1_000_000)?.unwrap_or(300) as u64;
    let mhz = p.opt_int("mhz", 1, 1000)?.unwrap_or(125) as u64;
    Ok(Box::new(Encoder { clk, dt, step_cycles: step_us * mhz, phase: 0, queue: VecDeque::new(), scheduled: false }))
}

impl Encoder {
    fn drive(&self, io: &mut Io) {
        let (a, b) = CLOSED[self.phase];
        io.drive(self.clk, a.then_some(false));
        io.drive(self.dt, b.then_some(false));
    }
}

impl Device for Encoder {
    fn attach(&mut self, io: &mut Io) {
        self.drive(io);
    }

    fn input(&mut self, io: &mut Io, action: &str, value: i64) -> bool {
        if action != "turn" {
            return false;
        }
        let dir = if value >= 0 { 1 } else { -1 };
        for _ in 0..value.unsigned_abs() * 4 {
            self.queue.push_back(dir);
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
        self.phase = (self.phase as i64 + step as i64).rem_euclid(4) as usize;
        self.drive(io);
        if self.phase == 0 {
            io.emit("detent", step as i64);
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
    use crate::testutil::FakePins;
    use alloc::vec::Vec;
    use pcb::{Board, Pins};

    const CLK: usize = 22;
    const DT: usize = 21;

    struct Rig {
        board: Board,
        pins: FakePins,
        now: u64,
    }

    impl Rig {
        fn new(extra: &str) -> Self {
            let text = alloc::format!("encoder knob clk={CLK} dt={DT} {extra}");
            let mut board = Board::parse_with(&text, &crate::registry()).unwrap();
            // Pull-ups on the pads: an open contact reads high.
            let mut pins = FakePins::default();
            pins.floating = 1 << CLK | 1 << DT;
            board.attach(&mut pins);
            Rig { board, pins, now: 0 }
        }
        fn turn(&mut self, n: i64) {
            self.board.input(&mut self.pins, "knob", "turn", n, self.now);
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
        let mut r = Rig::new("step_us=100 mhz=1");
        r.turn(1);
        assert_eq!(r.board.next_timer(), Some(100));
        let steps = r.run_to(1000);
        assert_eq!(steps, [(100, false, true), (200, false, false), (300, true, false), (400, true, true)]);
        assert_eq!(r.board.next_timer(), None);
        let ev: Vec<_> = r.board.events.iter().map(|e| (e.cycles, e.label, e.value)).collect();
        assert_eq!(ev, [(400, "detent", 1)]);
    }

    #[test]
    fn counter_clockwise_has_dt_leading() {
        let mut r = Rig::new("step_us=10 mhz=1");
        r.turn(-1);
        let steps = r.run_to(1000);
        assert_eq!(steps.iter().map(|s| (s.1, s.2)).collect::<Vec<_>>(), [(true, false), (false, false), (false, true), (true, true)]);
        assert_eq!(r.board.events[0].value, -1);
    }

    #[test]
    fn several_detents_queue_up_and_turns_can_be_extended() {
        let mut r = Rig::new("step_us=10 mhz=1");
        r.turn(2);
        r.run_to(35);
        r.turn(1); // extend while moving
        let steps = r.run_to(10_000);
        assert_eq!(r.levels(), (true, true), "back at the detent");
        let detents: Vec<i64> = r.board.events.iter().map(|e| e.value).collect();
        assert_eq!(detents, [1, 1, 1]);
        assert!(steps.len() > 4);
        // Reversing direction mid-cycle just walks the phases backwards.
        let mut r = Rig::new("step_us=10 mhz=1");
        r.turn(1);
        r.run_to(25); // two steps in: CLK and DT both closed
        assert_eq!(r.levels(), (false, false));
        r.turn(-1);
        r.run_to(10_000);
        assert_eq!(r.levels(), (true, true));
    }

    #[test]
    fn unknown_actions_are_refused_and_params_validated() {
        let mut r = Rig::new("");
        assert_eq!(r.board.input(&mut r.pins, "knob", "spin", 1, 0), pcb::InputResult::Unsupported);
        let reg = crate::registry();
        assert!(Board::parse_with("encoder e clk=1", &reg).is_err());
        assert!(Board::parse_with("encoder e clk=1 dt=2 step_us=0", &reg).is_err());
    }
}
