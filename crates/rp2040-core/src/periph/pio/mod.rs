//! RP2040 PIO block (PIO0 / PIO1): four state machines sharing a 32-word
//! instruction memory, per-machine FIFOs, eight IRQ flags and the pad
//! outputs, with the whole instruction set.
//!
//! Time is simulated cycle by cycle: [`Pio::advance`] is told how many
//! system clock cycles passed and steps every enabled machine on each of its
//! (fractionally divided) clock ticks, so pad edges come out with exact cycle
//! timestamps, passed to the [`PioEnv`] as they happen. When every machine is
//! stalled on something only the outside world can change (an empty FIFO, a
//! pin, an IRQ flag) the remaining cycles are skipped in one go. A machine
//! going round a loop that changes nothing outside itself (polling pins
//! that hold still) counts as stalled too, once its state comes back to an
//! earlier one: skipping then only runs it through what's left of a lap.
//!
//! Modelled details worth knowing: delay cycles start after an instruction
//! completes, side-set applies on the first cycle of an instruction even if
//! it then stalls, the pad merge lets the highest-numbered machine win a
//! cycle, inputs pass a two-cycle synchronizer unless bypassed, and
//! instructions written to `SMx_INSTR` (or run by `OUT/MOV EXEC`) do not
//! advance the program counter. Not modelled: `OUT_STICKY`, `INLINE_OUT_EN`
//! (stored only), the `IRQ` flags' routing to the system IRQs 4-7 (they are
//! machine-visible only), and exact behaviour of simultaneous cross-machine
//! reads (a machine sees the effects of lower-numbered ones in the same cycle).
use sm::{Shared, Sm, Snapshot};

use super::Peripheral;

mod sm;
#[cfg(test)]
mod tests;

pub const NUM_SM: usize = 4;
const IMEM_WORDS: usize = 32;
const FIFO_DEPTH: usize = 4;
/// The longest loop looked for, in ticks of a machine's clock.
const LOOP_TICKS: u32 = 1024;
/// The pads that exist (GPIO0..=29).
const PAD_MASK: u32 = 0x3FFF_FFFF;

// EXECCTRL
const EXEC_STATUS_SEL: u32 = 1 << 4;
const EXEC_SIDE_PINDIR: u32 = 1 << 29;
const EXEC_SIDE_EN: u32 = 1 << 30;
const EXEC_STALLED: u32 = 1 << 31;
// SHIFTCTRL
const SHIFT_AUTOPUSH: u32 = 1 << 16;
const SHIFT_AUTOPULL: u32 = 1 << 17;
const SHIFT_IN_RIGHT: u32 = 1 << 18;
const SHIFT_OUT_RIGHT: u32 = 1 << 19;
const SHIFT_FJOIN_TX: u32 = 1 << 30;
const SHIFT_FJOIN_RX: u32 = 1 << 31;
// FDEBUG fields (one bit per state machine)
const FD_RXSTALL: u32 = 0;
const FD_RXUNDER: u32 = 8;
const FD_TXOVER: u32 = 16;
const FD_TXSTALL: u32 = 24;

/// What the PIO block is wired to.
pub trait PioEnv {
    /// Pad levels as the block's inputs see them (bit n = GPIO n).
    fn pads_in(&self) -> u32;
    /// The merged pad outputs / output enables changed at absolute cycle `cycle`.
    fn pads_out(&mut self, out: u32, oe: u32, cycle: u64);
}

pub struct Pio {
    imem: [u16; IMEM_WORDS],
    sm: [Sm; NUM_SM],
    shared: Shared,
    /// Set whenever a machine executed a cycle; the host clears it.
    pub ticked: bool,
    inte: [u32; 2],
    intf: [u32; 2],
    /// Pads whose input skips the synchronizer.
    bypass: u32,
    /// Synchronizer stages and the pads they last saw.
    pipe: [u32; 2],
    /// Machines that have re-evaluated their stall since anything last changed.
    settled: u8,
    /// Machines found going round a loop with no effect outside themselves,
    /// since anything last changed, and how long a lap takes (in system
    /// cycles, divider phase included).
    looping: u8,
    lap: [u64; NUM_SM],
    /// Each machine's search for such a loop.
    search: [Search; NUM_SM],
    /// Cycles the dividers of machines that don't run haven't been given
    /// yet: they only matter once one starts, which takes a register write.
    owed: u64,
}

/// A search for a loop with Brent's method: compare each state with one
/// kept from earlier, kept again every power of two ticks.
#[derive(Clone, Copy)]
enum Search {
    /// Starts at the machine's next tick that changes nothing outside it.
    Fresh,
    /// The state at absolute cycle `at`, `ticks` ticks ago; a new one is
    /// kept after `span` ticks.
    Looking {
        kept: Snapshot,
        at: u64,
        ticks: u32,
        span: u32,
    },
    /// No loop up to [`LOOP_TICKS`] long.
    GaveUp,
}

/// What a machine's tick can change that the rest of the system, or the
/// other machines, can see.
#[derive(PartialEq, Eq)]
struct Seen {
    irq: u8,
    out: u32,
    oe: u32,
    fdebug: u32,
    tx: usize,
    rx: usize,
}

impl Pio {
    pub fn new() -> Self {
        Self {
            ticked: false,
            imem: [0; IMEM_WORDS],
            sm: core::array::from_fn(|_| Sm::new()),
            shared: Shared::default(),
            inte: [0; 2],
            intf: [0; 2],
            bypass: 0,
            pipe: [0; 2],
            settled: 0,
            looping: 0,
            lap: [0; NUM_SM],
            search: [Search::Fresh; NUM_SM],
            owed: 0,
        }
    }

    /// Give the stopped machines' dividers the cycles they're owed.
    fn settle_dividers(&mut self) {
        if self.owed != 0 {
            self.settle_owed();
        }
    }

    #[inline(never)]
    fn settle_owed(&mut self) {
        let owed = core::mem::take(&mut self.owed);
        {
            for n in 0..NUM_SM {
                if !self.runs(n) {
                    self.sm[n].run_divider(owed);
                }
            }
        }
    }

    /// Something any machine may react to changed: they all look again, but
    /// those whose stall still holds stay settled (their next ticks would
    /// find just that). Called once the change is made.
    #[inline(never)]
    fn unsettle(&mut self) {
        self.settled = (0..NUM_SM).fold(0, |m, n| {
            m | (self.sm[n].stall_holds(n, &self.imem, &self.shared) as u8) << n
        });
        self.looping = 0;
        self.search = [Search::Fresh; NUM_SM];
    }

    fn seen(&self, n: usize) -> Seen {
        Seen {
            irq: self.shared.irq,
            out: self.shared.out,
            oe: self.shared.oe,
            fdebug: self.shared.fdebug,
            tx: self.sm[n].tx.len(),
            rx: self.sm[n].rx.len(),
        }
    }

    /// Machine `n` just ticked at absolute cycle `at` without changing
    /// anything outside itself: is it back in a state it was in before?
    #[inline(never)]
    fn look_for_loop(&mut self, n: usize, at: u64) {
        let now = self.sm[n].snapshot();
        self.search[n] = match self.search[n] {
            Search::Fresh => Search::Looking {
                kept: now,
                at,
                ticks: 0,
                span: 1,
            },
            Search::Looking {
                kept,
                at: then,
                ticks,
                span,
            } => {
                let ticks = ticks + 1;
                if now == kept {
                    self.looping |= 1 << n;
                    self.lap[n] = at - then;
                    Search::GaveUp
                } else if ticks < span {
                    Search::Looking {
                        kept,
                        at: then,
                        ticks,
                        span,
                    }
                } else if span < LOOP_TICKS {
                    Search::Looking {
                        kept: now,
                        at,
                        ticks: 0,
                        span: span * 2,
                    }
                } else {
                    Search::GaveUp
                }
            }
            Search::GaveUp => Search::GaveUp,
        };
    }

    /// Whether the atomic register aliases (XOR / SET / CLR) mean anything at
    /// `off`: true for registers that hold what was written (CTRL, the input
    /// synchronizer bypass, interrupt enables and forces, and each machine's
    /// CLKDIV / EXECCTRL / SHIFTCTRL / PINCTRL).
    pub fn alias_applies(off: u32) -> bool {
        match off {
            0x000 | 0x038 | 0x12C | 0x130 | 0x138 | 0x13C => true,
            0x0C8..=0x127 => matches!((off - 0xC8) % 0x18, 0x00 | 0x04 | 0x08 | 0x14),
            _ => false,
        }
    }

    /// Merged pad outputs and output enables.
    pub fn outputs(&self) -> (u32, u32) {
        (self.shared.out & PAD_MASK, self.shared.oe & PAD_MASK)
    }

    fn runs(&self, n: usize) -> bool {
        self.sm[n].enabled || self.sm[n].forced.is_some()
    }

    /// The machines that run (bit n = machine n): enabled, or executing an
    /// instruction written to SMn_INSTR.
    fn running(&self) -> u8 {
        (0..NUM_SM).fold(0, |m, n| m | (self.runs(n) as u8) << n)
    }

    /// Machines that can't change anything until the outside acts: stalled,
    /// or going round a loop that changes nothing outside themselves.
    fn idle_machines(&self) -> u8 {
        self.settled | self.looping
    }

    /// Nothing can happen here until the outside acts: every running machine
    /// is parked and has already seen the current pad levels.
    pub fn parked(&self, pads: u32) -> bool {
        let running = self.running();
        running & !self.idle_machines() == 0
            && (running == 0 || (self.pipe == [pads, pads] && self.shared.inputs == pads))
    }

    /// Some machine is running and not parked on an external condition.
    pub fn active(&self) -> bool {
        self.running() & !self.idle_machines() != 0
    }

    /// Step the block through `cycles` system clock cycles starting after
    /// absolute cycle `start`.
    pub fn advance(&mut self, start: u64, cycles: u64, env: &mut dyn PioEnv) {
        // Which machines run changes only through registers, or when one
        // that runs only for a forced instruction has executed it.
        let mut running = self.running();
        if running == 0 {
            self.pipe = [env.pads_in(); 2];
            self.owed += cycles;
            return;
        }
        // Stepping, every divider moves cycle by cycle.
        self.settle_dividers();
        let mut done = 0;
        while done < cycles {
            let pads = env.pads_in();
            if running == 0 {
                self.pipe = [pads, pads];
                self.skip(cycles - done);
                return;
            }
            let caught_up = self.pipe == [pads, pads] && self.shared.inputs == pads;
            // Everything is parked, the synchronizer has caught up, and the
            // machines last looked at exactly these pad levels: nothing can
            // happen until the outside world acts, so skip the rest.
            if caught_up && running & !self.idle_machines() == 0 {
                self.skip(cycles - done);
                return;
            }
            // The synchronizer has caught up: cycles before the next divider
            // tick change nothing, so jump to just before it.
            if caught_up {
                let wait = (0..NUM_SM)
                    .filter(|&n| running >> n & 1 != 0)
                    .map(|n| {
                        let sm = &self.sm[n];
                        if sm.exec_pending() {
                            return 1;
                        }
                        (sm.div256().saturating_sub(sm.acc) as u64)
                            .div_ceil(256)
                            .max(1)
                    })
                    .min()
                    .unwrap_or(1);
                let idle = (wait - 1).min(cycles - done);
                if idle > 0 {
                    for (n, sm) in self.sm.iter_mut().enumerate() {
                        if running >> n & 1 != 0 {
                            sm.acc += 256 * idle as u32;
                        } else {
                            sm.run_divider(idle);
                        }
                    }
                    done += idle;
                    continue;
                }
            }
            // Two flip-flops: the pad level from two cycles ago, unless bypassed.
            let inputs = self.bypass & pads | !self.bypass & self.pipe[1];
            let moved = inputs != self.shared.inputs;
            self.shared.inputs = inputs;
            if moved {
                // A parked machine has to look again at what it waits on.
                self.unsettle();
            }
            self.pipe = [pads, self.pipe[0]];
            let (old_out, old_oe) = (self.shared.out, self.shared.oe);
            let mut forced_ran = false;
            for n in 0..NUM_SM {
                // The dividers are free-running: a stopped machine's moves
                // too, so it starts (or runs a forced instruction) in phase.
                let div = self.sm[n].div256();
                self.sm[n].acc += 256;
                let tick = self.sm[n].acc >= div;
                if tick {
                    self.sm[n].acc -= div;
                }
                // An instruction written to SMx_INSTR runs at once, not on
                // the machine's clock (only its retries, if it stalls).
                if running >> n & 1 == 0 || !(tick || self.sm[n].exec_pending()) {
                    continue;
                }
                self.ticked = true;
                forced_ran |= !self.sm[n].enabled;
                let before = self.seen(n);
                let progressed = self.sm[n].tick(n, &self.imem, &mut self.shared);
                if self.seen(n) != before {
                    // Every machine may react to that, this one included.
                    self.unsettle();
                } else if progressed {
                    // Only this machine's own state moved, which the others
                    // can't see.
                    self.settled &= !(1 << n);
                    // (A machine that runs only for a forced instruction
                    // stops once it's done: no loop to find.)
                    if self.looping >> n & 1 == 0 && self.sm[n].enabled {
                        self.look_for_loop(n, start + done + 1);
                    }
                } else {
                    self.settled |= 1 << n;
                }
            }
            if forced_ran {
                running = self.running();
            }
            done += 1;
            if (self.shared.out, self.shared.oe) != (old_out, old_oe) {
                env.pads_out(self.shared.out, self.shared.oe, start + done);
            }
        }
    }

    /// Let `cycles` pass with nothing for the machines to do: only the
    /// (free-running) dividers move.
    pub fn idle(&mut self, cycles: u64) {
        self.skip(cycles);
    }

    /// Let `cycles` pass with every machine parked: the dividers move, and a
    /// machine going round a loop runs what's left over after whole laps
    /// (which leave it as it was).
    fn skip(&mut self, cycles: u64) {
        for n in 0..NUM_SM {
            if self.looping >> n & 1 != 0 {
                self.lap_through(n, cycles % self.lap[n]);
            } else if self.runs(n) {
                self.sm[n].run_divider(cycles);
            }
        }
        self.owed += cycles;
    }

    /// Run looping machine `n` for `cycles` (less than a lap), tick by tick:
    /// it changes nothing outside itself, so nothing else needs to run.
    #[inline(never)]
    fn lap_through(&mut self, n: usize, mut cycles: u64) {
        let div = self.sm[n].div256() as u64;
        while cycles > 0 {
            let acc = self.sm[n].acc as u64;
            // Cycles until the divider next fires (it does once acc reaches div).
            let wait = (div.saturating_sub(acc)).div_ceil(256).max(1);
            if wait > cycles {
                self.sm[n].acc = (acc + 256 * cycles) as u32;
                return;
            }
            self.sm[n].acc = (acc + 256 * wait - div) as u32;
            self.sm[n].tick(n, &self.imem, &mut self.shared);
            cycles -= wait;
        }
    }

    // ---- interrupts and requests ----

    fn intr(&self) -> u32 {
        let mut r = (self.shared.irq as u32 & 0xF) << 8;
        for (n, sm) in self.sm.iter().enumerate() {
            r |=
                (!sm.rx.is_empty() as u32) << n | ((sm.tx.len() < sm.tx_depth()) as u32) << (4 + n);
        }
        r
    }

    /// Level of PIO interrupt line `line` (0 or 1).
    pub fn irq(&self, line: usize) -> bool {
        if self.inte[line] == 0 {
            return false;
        }
        (self.intr() | self.intf[line]) & self.inte[line] != 0
    }

    /// DMA request: machine `n`'s TX FIFO has room / its RX FIFO has data.
    pub fn dreq_tx(&self, n: usize) -> bool {
        self.sm[n].tx.len() < self.sm[n].tx_depth()
    }
    pub fn dreq_rx(&self, n: usize) -> bool {
        !self.sm[n].rx.is_empty()
    }

    // ---- registers ----

    fn fstat(&self) -> u32 {
        let mut r = 0;
        for (n, sm) in self.sm.iter().enumerate() {
            r |= (sm.rx.len() >= sm.rx_depth()) as u32 * (1 << n);
            r |= sm.rx.is_empty() as u32 * (1 << (8 + n));
            r |= (sm.tx.len() >= sm.tx_depth()) as u32 * (1 << (16 + n));
            r |= sm.tx.is_empty() as u32 * (1 << (24 + n));
        }
        r
    }

    fn flevel(&self) -> u32 {
        self.sm.iter().enumerate().fold(0, |r, (n, sm)| {
            r | (sm.tx.len() as u32) << (8 * n) | (sm.rx.len() as u32) << (8 * n + 4)
        })
    }

    fn sm_read(&mut self, n: usize, reg: u32) -> u32 {
        let sm = &self.sm[n];
        match reg {
            0x0 => sm.clkdiv,
            0x4 => {
                sm.execctrl
                    | if sm.forced.is_some() && sm.stalled {
                        EXEC_STALLED
                    } else {
                        0
                    }
            }
            0x8 => sm.shiftctrl,
            0xC => sm.pc as u32,
            0x10 => sm.forced.unwrap_or(self.imem[sm.pc as usize]) as u32,
            _ => sm.pinctrl,
        }
    }

    #[cfg(test)]
    fn sm_write(&mut self, n: usize, reg: u32, val: u32) {
        self.settle_dividers();
        self.sm_write_reg(n, reg, val);
        self.unsettle();
    }

    fn sm_write_reg(&mut self, n: usize, reg: u32, val: u32) {
        let sm = &mut self.sm[n];
        match reg {
            0x0 => sm.clkdiv = val & 0xFFFF_FF00,
            0x4 => sm.execctrl = val & !EXEC_STALLED,
            0x8 => {
                let joins = SHIFT_FJOIN_TX | SHIFT_FJOIN_RX;
                if (sm.shiftctrl ^ val) & joins != 0 {
                    // Changing the FIFO join discards the FIFO contents.
                    sm.tx.clear();
                    sm.rx.clear();
                }
                sm.shiftctrl = val;
            }
            0x10 => {
                sm.forced = Some(val as u16);
                sm.written = true;
                sm.side_done = false;
            }
            0x14 => sm.pinctrl = val,
            _ => {}
        }
    }
}

impl Default for Pio {
    fn default() -> Self {
        Self::new()
    }
}

impl Peripheral for Pio {
    fn read(&mut self, off: u32) -> u32 {
        let v = self.read_reg(off);
        // Anything the firmware looks at or changes may unpark a machine.
        self.unsettle();
        v
    }

    fn write(&mut self, off: u32, val: u32) {
        // Which machines run may change: the stopped ones catch up first.
        self.settle_dividers();
        self.write_reg(off, val);
        self.unsettle();
    }
}

impl Pio {
    fn read_reg(&mut self, off: u32) -> u32 {
        match off {
            0x000 => (0..NUM_SM).fold(0, |r, n| r | (self.sm[n].enabled as u32) << n),
            0x004 => self.fstat(),
            0x008 => self.shared.fdebug,
            0x00C => self.flevel(),
            0x020..=0x02C => {
                let sm = &mut self.sm[((off - 0x20) >> 2) as usize];
                match sm.rx.pop_front() {
                    Some(v) => v,
                    None => {
                        self.shared.fdebug |= 1 << (FD_RXUNDER + ((off - 0x20) >> 2));
                        0
                    }
                }
            }
            0x030 => self.shared.irq as u32,
            0x038 => self.bypass,
            0x03C => self.shared.out & PAD_MASK,
            0x040 => self.shared.oe & PAD_MASK,
            0x044 => 0x0020_0404, // 32-instruction memory, 4 machines, FIFO depth 4
            0x048..=0x0C4 => self.imem[((off - 0x48) >> 2) as usize] as u32,
            0x0C8..=0x127 => {
                let rel = off - 0xC8;
                self.sm_read((rel / 0x18) as usize, rel % 0x18)
            }
            0x128 => self.intr(),
            0x12C => self.inte[0],
            0x130 => self.intf[0],
            0x134 => (self.intr() | self.intf[0]) & self.inte[0],
            0x138 => self.inte[1],
            0x13C => self.intf[1],
            0x140 => (self.intr() | self.intf[1]) & self.inte[1],
            _ => 0,
        }
    }

    fn write_reg(&mut self, off: u32, val: u32) {
        match off {
            0x000 => {
                for (n, sm) in self.sm.iter_mut().enumerate() {
                    sm.enabled = val >> n & 1 != 0;
                    if val >> (4 + n) & 1 != 0 {
                        sm.restart();
                    }
                    if val >> (8 + n) & 1 != 0 {
                        sm.acc = 0;
                    }
                }
            }
            0x008 => {
                // A stall flag can't be cleared while its machine is still stalled that way.
                let held = self.sm.iter().enumerate().fold(0, |m, (n, sm)| {
                    m | (sm.stall_tx as u32) << (FD_TXSTALL as usize + n)
                        | (sm.stall_rx as u32) << (FD_RXSTALL as usize + n)
                });
                self.shared.fdebug &= !(val & !held);
            }
            0x010..=0x01C => {
                let sm = &mut self.sm[((off - 0x10) >> 2) as usize];
                if sm.tx.len() < sm.tx_depth() {
                    sm.tx.push_back(val);
                } else {
                    self.shared.fdebug |= 1 << (FD_TXOVER + ((off - 0x10) >> 2));
                }
            }
            0x030 => self.shared.irq &= !(val as u8),
            0x034 => self.shared.irq |= val as u8,
            0x038 => self.bypass = val,
            0x048..=0x0C4 => self.imem[((off - 0x48) >> 2) as usize] = val as u16,
            0x0C8..=0x127 => {
                let rel = off - 0xC8;
                self.sm_write_reg((rel / 0x18) as usize, rel % 0x18, val);
            }
            0x12C => self.inte[0] = val & 0xFFF,
            0x130 => self.intf[0] = val & 0xFFF,
            0x138 => self.inte[1] = val & 0xFFF,
            0x13C => self.intf[1] = val & 0xFFF,
            _ => {}
        }
    }
}
