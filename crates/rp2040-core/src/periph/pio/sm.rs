use alloc::collections::VecDeque;

use super::*;

/// State shared by the four machines.
#[derive(Default)]
pub(super) struct Shared {
    /// IRQ flags 0..=7.
    pub(super) irq: u8,
    /// Merged pad outputs and output enables (persistent between writes).
    pub(super) out: u32,
    pub(super) oe: u32,
    /// Pad levels after the input synchronizers, this cycle.
    pub(super) inputs: u32,
    pub(super) fdebug: u32,
    /// Something observable changed during the current step.
    pub(super) dirty: bool,
}

impl Shared {
    /// Write `value`'s low `count` bits to the pads starting at `base`
    /// (wrapping past pin 31), as outputs or as output enables.
    pub(super) fn write_pins(&mut self, base: u32, count: u32, value: u32, dirs: bool) {
        let target = if dirs { &mut self.oe } else { &mut self.out };
        for i in 0..count {
            let pin = (base + i) & 31;
            let bit = (value >> i) & 1;
            let old = *target;
            *target = *target & !(1 << pin) | bit << pin;
            self.dirty |= *target != old;
        }
    }

    pub(super) fn set_flag(&mut self, flag: u8) {
        if self.irq & 1 << flag == 0 {
            self.irq |= 1 << flag;
            self.dirty = true;
        }
    }

    pub(super) fn clear_flag(&mut self, flag: u8) {
        if self.irq & 1 << flag != 0 {
            self.irq &= !(1 << flag);
            self.dirty = true;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The instruction can't complete yet; try again next tick.
    Stall,
    Done,
    /// The instruction set the program counter itself.
    Jumped,
}

pub(super) struct Sm {
    pub(super) clkdiv: u32,
    pub(super) execctrl: u32,
    pub(super) shiftctrl: u32,
    pub(super) pinctrl: u32,
    pub(super) enabled: bool,
    pub(super) pc: u8,
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) isr: u32,
    pub(super) isr_count: u8,
    pub(super) osr: u32,
    /// Bits shifted out of the OSR; 32 = empty.
    pub(super) osr_count: u8,
    pub(super) tx: VecDeque<u32>,
    pub(super) rx: VecDeque<u32>,
    /// Delay cycles still to wait after the last instruction.
    pub(super) delay: u8,
    /// An instruction injected through `SMx_INSTR` / `OUT EXEC` / `MOV EXEC`
    /// that has not completed yet.
    pub(super) forced: Option<u16>,
    /// Instruction to inject once the current one completes (`OUT/MOV EXEC`).
    pub(super) forced_after: Option<u16>,
    /// The current instruction stalled on its last attempt.
    pub(super) stalled: bool,
    /// Side-set of the current instruction has been applied.
    pub(super) side_done: bool,
    /// Autopush data (in the ISR) waiting for room in the RX FIFO.
    pub(super) pending_push: bool,
    /// An `IRQ ... wait` has raised its flag and now waits for it to clear.
    pub(super) irq_waiting: bool,
    /// The last attempt stalled on an empty TX / full RX FIFO. (FDEBUG's
    /// TXSTALL / RXSTALL stay set while this holds.)
    pub(super) stall_tx: bool,
    pub(super) stall_rx: bool,
    /// Divider phase, in 1/256 system cycles.
    pub(super) acc: u32,
}

impl Sm {
    pub(super) fn new() -> Self {
        Self {
            clkdiv: 1 << 16,
            execctrl: 0x1F << 12,
            shiftctrl: SHIFT_IN_RIGHT | SHIFT_OUT_RIGHT,
            pinctrl: 5 << 26,
            enabled: false,
            pc: 0,
            x: 0,
            y: 0,
            isr: 0,
            isr_count: 0,
            osr: 0,
            osr_count: 32,
            tx: VecDeque::new(),
            rx: VecDeque::new(),
            delay: 0,
            forced: None,
            forced_after: None,
            stalled: false,
            side_done: false,
            pending_push: false,
            irq_waiting: false,
            stall_tx: false,
            stall_rx: false,
            acc: 0,
        }
    }

    /// Clear execution state (SM_RESTART); FIFOs, registers X/Y, PC and config stay.
    pub(super) fn restart(&mut self) {
        self.isr = 0;
        self.isr_count = 0;
        self.osr = 0;
        self.osr_count = 32;
        self.delay = 0;
        self.forced = None;
        self.forced_after = None;
        self.stalled = false;
        self.side_done = false;
        self.pending_push = false;
        self.irq_waiting = false;
    }

    pub(super) fn tx_depth(&self) -> usize {
        match (
            self.shiftctrl & SHIFT_FJOIN_TX != 0,
            self.shiftctrl & SHIFT_FJOIN_RX != 0,
        ) {
            (true, false) => 2 * FIFO_DEPTH,
            (false, true) => 0,
            _ => FIFO_DEPTH,
        }
    }
    pub(super) fn rx_depth(&self) -> usize {
        match (
            self.shiftctrl & SHIFT_FJOIN_TX != 0,
            self.shiftctrl & SHIFT_FJOIN_RX != 0,
        ) {
            (false, true) => 2 * FIFO_DEPTH,
            (true, false) => 0,
            _ => FIFO_DEPTH,
        }
    }

    /// Divider in 1/256 units (an integer part of 0 means 65536).
    pub(super) fn div256(&self) -> u32 {
        let int = match self.clkdiv >> 16 {
            0 => 65536,
            i => i,
        };
        int * 256 + (self.clkdiv >> 8 & 0xFF)
    }

    pub(super) fn wrap_bottom(&self) -> u8 {
        (self.execctrl >> 7 & 0x1F) as u8
    }
    pub(super) fn wrap_top(&self) -> u8 {
        (self.execctrl >> 12 & 0x1F) as u8
    }
    pub(super) fn thresh(&self, shift: u32) -> u8 {
        match (self.shiftctrl >> shift & 0x1F) as u8 {
            0 => 32,
            n => n,
        }
    }
    pub(super) fn push_thresh(&self) -> u8 {
        self.thresh(20)
    }
    pub(super) fn pull_thresh(&self) -> u8 {
        self.thresh(25)
    }
    pub(super) fn in_base(&self) -> u32 {
        self.pinctrl >> 15 & 0x1F
    }
    pub(super) fn out_base(&self) -> u32 {
        self.pinctrl & 0x1F
    }
    pub(super) fn out_count(&self) -> u32 {
        self.pinctrl >> 20 & 0x3F
    }
    pub(super) fn set_base(&self) -> u32 {
        self.pinctrl >> 5 & 0x1F
    }
    pub(super) fn set_count(&self) -> u32 {
        self.pinctrl >> 26 & 7
    }
    pub(super) fn side_base(&self) -> u32 {
        self.pinctrl >> 10 & 0x1F
    }
    pub(super) fn side_count(&self) -> u32 {
        self.pinctrl >> 29 & 7
    }

    /// The pads as seen from `IN_BASE` (rotated so IN_BASE is bit 0).
    pub(super) fn pins_in(&self, inputs: u32) -> u32 {
        inputs.rotate_right(self.in_base())
    }

    /// Absolute IRQ flag number for an IRQ/WAIT index (bit 4 = relative to this machine).
    pub(super) fn flag(&self, n: usize, idx: u32) -> u8 {
        if idx & 0x10 != 0 {
            ((idx & 4) | ((idx + n as u32) & 3)) as u8
        } else {
            (idx & 7) as u8
        }
    }

    pub(super) fn try_push(&mut self, shared: &mut Shared, n: usize) -> bool {
        if self.rx.len() < self.rx_depth() {
            self.rx.push_back(self.isr);
            self.isr = 0;
            self.isr_count = 0;
            shared.dirty = true;
            true
        } else {
            shared.fdebug |= 1 << (FD_RXSTALL + n as u32);
            self.stall_rx = true;
            false
        }
    }

    pub(super) fn try_pull(&mut self, shared: &mut Shared, n: usize) -> bool {
        match self.tx.pop_front() {
            Some(w) => {
                self.osr = w;
                self.osr_count = 0;
                shared.dirty = true;
                true
            }
            None => {
                shared.fdebug |= 1 << (FD_TXSTALL + n as u32);
                self.stall_tx = true;
                false
            }
        }
    }

    /// Apply the side-set field of `instr` and return its delay.
    pub(super) fn side_and_delay(&mut self, instr: u16, shared: &mut Shared) -> u8 {
        let field = (instr >> 8 & 0x1F) as u32;
        let n = self.side_count().min(5);
        let delay_bits = 5 - n;
        let delay = (field & ((1 << delay_bits) - 1)) as u8;
        if !self.side_done {
            self.side_done = true;
            shared.dirty = true;
            if n > 0 {
                let side = field >> delay_bits;
                let (enabled, data_bits) = if self.execctrl & EXEC_SIDE_EN != 0 {
                    (side >> (n - 1) & 1 != 0, n - 1)
                } else {
                    (true, n)
                };
                if enabled {
                    shared.write_pins(
                        self.side_base(),
                        data_bits,
                        side,
                        self.execctrl & EXEC_SIDE_PINDIR != 0,
                    );
                }
            }
        }
        delay
    }

    /// Run `instr`. `n` is this machine's number.
    pub(super) fn exec(&mut self, n: usize, instr: u16, shared: &mut Shared) -> Outcome {
        let arg = (instr & 0x1F) as u32;
        let bits = if arg == 0 { 32 } else { arg };
        match instr >> 13 {
            0 => {
                // JMP
                let take = match instr >> 5 & 7 {
                    0 => true,
                    1 => self.x == 0,
                    2 => {
                        let t = self.x != 0;
                        self.x = self.x.wrapping_sub(1);
                        t
                    }
                    3 => self.y == 0,
                    4 => {
                        let t = self.y != 0;
                        self.y = self.y.wrapping_sub(1);
                        t
                    }
                    5 => self.x != self.y,
                    6 => shared.inputs >> (self.execctrl >> 24 & 0x1F) & 1 != 0,
                    _ => self.osr_count < self.pull_thresh(),
                };
                if take {
                    self.pc = arg as u8;
                    Outcome::Jumped
                } else {
                    Outcome::Done
                }
            }
            1 => {
                // WAIT
                let pol = instr >> 7 & 1 != 0;
                let value = match instr >> 5 & 3 {
                    0 => shared.inputs >> arg & 1 != 0,
                    1 => shared.inputs >> ((self.in_base() + arg) & 31) & 1 != 0,
                    2 => shared.irq >> self.flag(n, arg) & 1 != 0,
                    _ => false,
                };
                if value != pol {
                    return Outcome::Stall;
                }
                if instr >> 5 & 3 == 2 && pol {
                    shared.clear_flag(self.flag(n, arg));
                }
                Outcome::Done
            }
            2 => {
                // IN
                if self.pending_push {
                    return if self.try_push(shared, n) {
                        self.pending_push = false;
                        Outcome::Done
                    } else {
                        Outcome::Stall
                    };
                }
                let data = match instr >> 5 & 7 {
                    0 => self.pins_in(shared.inputs),
                    1 => self.x,
                    2 => self.y,
                    6 => self.isr,
                    7 => self.osr,
                    _ => 0,
                };
                let data = if bits == 32 {
                    data
                } else {
                    data & ((1 << bits) - 1)
                };
                self.isr = if self.shiftctrl & SHIFT_IN_RIGHT != 0 {
                    if bits == 32 {
                        data
                    } else {
                        self.isr >> bits | data << (32 - bits)
                    }
                } else if bits == 32 {
                    data
                } else {
                    self.isr << bits | data
                };
                self.isr_count = (self.isr_count as u32 + bits).min(32) as u8;
                shared.dirty = true;
                if self.shiftctrl & SHIFT_AUTOPUSH != 0 && self.isr_count >= self.push_thresh() {
                    if self.try_push(shared, n) {
                        return Outcome::Done;
                    }
                    self.pending_push = true;
                    return Outcome::Stall;
                }
                Outcome::Done
            }
            3 => {
                // OUT
                if self.shiftctrl & SHIFT_AUTOPULL != 0
                    && self.osr_count >= self.pull_thresh()
                    && !self.try_pull(shared, n)
                {
                    return Outcome::Stall;
                }
                let data = if self.shiftctrl & SHIFT_OUT_RIGHT != 0 {
                    let d = if bits == 32 {
                        self.osr
                    } else {
                        self.osr & ((1 << bits) - 1)
                    };
                    self.osr = if bits == 32 { 0 } else { self.osr >> bits };
                    d
                } else {
                    let d = if bits == 32 {
                        self.osr
                    } else {
                        self.osr >> (32 - bits)
                    };
                    self.osr = if bits == 32 { 0 } else { self.osr << bits };
                    d
                };
                self.osr_count = (self.osr_count as u32 + bits).min(32) as u8;
                shared.dirty = true;
                let mut outcome = Outcome::Done;
                match instr >> 5 & 7 {
                    0 => shared.write_pins(self.out_base(), self.out_count(), data, false),
                    1 => self.x = data,
                    2 => self.y = data,
                    4 => shared.write_pins(self.out_base(), self.out_count(), data, true),
                    5 => {
                        self.pc = (data & 0x1F) as u8;
                        outcome = Outcome::Jumped;
                    }
                    6 => {
                        self.isr = data;
                        self.isr_count = bits as u8;
                    }
                    7 => self.forced_after = Some(data as u16),
                    _ => {}
                }
                // Autopull refills the OSR as soon as the OUT has emptied it.
                if self.shiftctrl & SHIFT_AUTOPULL != 0
                    && self.osr_count >= self.pull_thresh()
                    && !self.tx.is_empty()
                {
                    self.try_pull(shared, n);
                }
                outcome
            }
            4 => {
                let block = instr >> 5 & 1 != 0;
                let cond = instr >> 6 & 1 != 0;
                if instr >> 7 & 1 == 0 {
                    // PUSH
                    if cond && self.isr_count < self.push_thresh() {
                        return Outcome::Done;
                    }
                    if self.try_push(shared, n) {
                        Outcome::Done
                    } else if block {
                        Outcome::Stall
                    } else {
                        // Non-blocking push on a full FIFO drops the data.
                        self.isr = 0;
                        self.isr_count = 0;
                        shared.dirty = true;
                        Outcome::Done
                    }
                } else {
                    // PULL
                    if cond && self.osr_count < self.pull_thresh() {
                        return Outcome::Done;
                    }
                    if self.try_pull(shared, n) {
                        Outcome::Done
                    } else if block {
                        Outcome::Stall
                    } else {
                        // Non-blocking pull on an empty FIFO copies X.
                        self.osr = self.x;
                        self.osr_count = 0;
                        shared.dirty = true;
                        Outcome::Done
                    }
                }
            }
            5 => {
                // MOV
                let src = match instr & 7 {
                    0 => self.pins_in(shared.inputs),
                    1 => self.x,
                    2 => self.y,
                    5 => {
                        let level = if self.execctrl & EXEC_STATUS_SEL == 0 {
                            self.tx.len()
                        } else {
                            self.rx.len()
                        };
                        if (level as u32) < self.execctrl & 0xF {
                            u32::MAX
                        } else {
                            0
                        }
                    }
                    6 => self.isr,
                    7 => self.osr,
                    _ => 0,
                };
                let val = match instr >> 3 & 3 {
                    1 => !src,
                    2 => src.reverse_bits(),
                    _ => src,
                };
                shared.dirty = true;
                match instr >> 5 & 7 {
                    0 => shared.write_pins(self.out_base(), self.out_count(), val, false),
                    1 => self.x = val,
                    2 => self.y = val,
                    4 => self.forced_after = Some(val as u16),
                    5 => {
                        self.pc = (val & 0x1F) as u8;
                        return Outcome::Jumped;
                    }
                    6 => {
                        self.isr = val;
                        self.isr_count = 0;
                    }
                    7 => {
                        self.osr = val;
                        self.osr_count = 0;
                    }
                    _ => {}
                }
                Outcome::Done
            }
            6 => {
                // IRQ
                let flag = self.flag(n, arg);
                if instr >> 6 & 1 != 0 {
                    shared.clear_flag(flag);
                    return Outcome::Done;
                }
                if self.irq_waiting {
                    if shared.irq >> flag & 1 == 0 {
                        self.irq_waiting = false;
                        return Outcome::Done;
                    }
                    return Outcome::Stall;
                }
                shared.set_flag(flag);
                if instr >> 5 & 1 != 0 {
                    self.irq_waiting = true;
                    return Outcome::Stall;
                }
                Outcome::Done
            }
            _ => {
                // SET
                let data = arg;
                match instr >> 5 & 7 {
                    0 => shared.write_pins(self.set_base(), self.set_count(), data, false),
                    1 => self.x = data,
                    2 => self.y = data,
                    4 => shared.write_pins(self.set_base(), self.set_count(), data, true),
                    _ => {}
                }
                shared.dirty = true;
                Outcome::Done
            }
        }
    }

    /// One tick of this machine's clock. Returns whether it changed anything
    /// anyone can observe (false = it stalled again on the same condition).
    pub(super) fn tick(&mut self, n: usize, imem: &[u16; IMEM_WORDS], shared: &mut Shared) -> bool {
        shared.dirty = false;
        self.stall_tx = false;
        self.stall_rx = false;
        if self.delay > 0 {
            self.delay -= 1;
            return true;
        }
        let forced = self.forced;
        let instr = forced.unwrap_or(imem[self.pc as usize]);
        // Side-set happens on the first cycle, even if the instruction then stalls.
        let delay = self.side_and_delay(instr, shared);
        match self.exec(n, instr, shared) {
            Outcome::Stall => {
                self.stalled = true;
            }
            outcome => {
                self.stalled = false;
                self.side_done = false;
                self.delay = delay;
                if forced.is_some() {
                    self.forced = None;
                } else if outcome == Outcome::Done {
                    self.pc = if self.pc == self.wrap_top() {
                        self.wrap_bottom()
                    } else {
                        (self.pc + 1) & 31
                    };
                }
                if let Some(next) = self.forced_after.take() {
                    self.forced = Some(next);
                }
                return true;
            }
        }
        shared.dirty
    }
}
