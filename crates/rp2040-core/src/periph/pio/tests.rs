use alloc::vec::Vec;

use super::*;
// ---- a tiny assembler: encoders for the instruction set ----

const JMP_ALWAYS: u16 = 0;
const JMP_NOT_X: u16 = 1;
const JMP_X_DEC: u16 = 2;
const JMP_NOT_Y: u16 = 3;
const JMP_Y_DEC: u16 = 4;
const JMP_X_NE_Y: u16 = 5;
const JMP_PIN: u16 = 6;
const JMP_NOT_OSRE: u16 = 7;

fn jmp(cond: u16, addr: u16) -> u16 {
    cond << 5 | addr
}
fn wait(pol: u16, src: u16, idx: u16) -> u16 {
    0x2000 | pol << 7 | src << 5 | idx
}
fn in_(src: u16, count: u16) -> u16 {
    0x4000 | src << 5 | count & 31
}
fn out(dest: u16, count: u16) -> u16 {
    0x6000 | dest << 5 | count & 31
}
fn push(if_full: bool, block: bool) -> u16 {
    0x8000 | (if_full as u16) << 6 | (block as u16) << 5
}
fn pull(if_empty: bool, block: bool) -> u16 {
    0x8080 | (if_empty as u16) << 6 | (block as u16) << 5
}
fn mov(dest: u16, op: u16, src: u16) -> u16 {
    0xA000 | dest << 5 | op << 3 | src
}
fn irq(clr: bool, wait: bool, idx: u16) -> u16 {
    0xC000 | (clr as u16) << 6 | (wait as u16) << 5 | idx
}
fn set(dest: u16, data: u16) -> u16 {
    0xE000 | dest << 5 | data
}
/// Add a delay / side-set field (the 5 bits above the operands).
fn with(instr: u16, field: u16) -> u16 {
    instr | field << 8
}

// out/in/mov/set destination and source codes
const PINS: u16 = 0;
const X: u16 = 1;
const Y: u16 = 2;
const NULL: u16 = 3;
const PINDIRS: u16 = 4;
const PC: u16 = 5;
const ISR: u16 = 6;
const EXEC: u16 = 7; // OUT destination
const MOV_EXEC: u16 = 4;
const OSR: u16 = 7;
const STATUS: u16 = 5;
const NOP: u16 = 0xA042; // mov y, y

struct Env {
    pads: u32,
    edges: Vec<(u64, u32, u32)>,
}

impl Env {
    fn new() -> Self {
        Env {
            pads: 0,
            edges: Vec::new(),
        }
    }
}

impl PioEnv for Env {
    fn pads_in(&self) -> u32 {
        self.pads
    }
    fn pads_out(&mut self, out: u32, oe: u32, cycle: u64) {
        self.edges.push((cycle, out, oe));
    }
}

/// A block plus the cycle counter the system would keep.
struct Rig {
    pio: Pio,
    env: Env,
    now: u64,
}

impl Rig {
    fn new() -> Self {
        Rig {
            pio: Pio::new(),
            env: Env::new(),
            now: 0,
        }
    }
    fn load(&mut self, at: usize, program: &[u16]) {
        for (i, w) in program.iter().enumerate() {
            self.pio.write(0x48 + 4 * (at + i) as u32, *w as u32);
        }
    }
    fn wrap(&mut self, sm: usize, bottom: u32, top: u32) {
        let r = self.pio.sm_read(sm, 4) & !(0x3FF << 7) | bottom << 7 | top << 12;
        self.pio.sm_write(sm, 4, r);
    }
    fn pinctrl(&mut self, sm: usize, v: u32) {
        self.pio.sm_write(sm, 0x14, v);
    }
    fn enable(&mut self, mask: u32) {
        self.pio.write(0, mask);
    }
    fn run(&mut self, cycles: u64) {
        self.pio.advance(self.now, cycles, &mut self.env);
        self.now += cycles;
    }
    /// Execute `instrs` one by one through SMx_INSTR, as `pio_sm_exec` does.
    fn exec(&mut self, sm: usize, instrs: &[u16]) {
        for &i in instrs {
            self.pio.sm_write(sm, 0x10, i as u32);
            for _ in 0..50 {
                self.run(1);
                while self.pio.sm[sm].delay > 0 {
                    self.run(1);
                }
                if self.pio.sm[sm].forced.is_none() {
                    break;
                }
            }
        }
    }
    fn modify(&mut self, sm: usize, reg: u32, f: impl FnOnce(u32) -> u32) {
        let v = f(self.pio.sm_read(sm, reg));
        self.pio.sm_write(sm, reg, v);
    }
    fn sm(&self, n: usize) -> &Sm {
        &self.pio.sm[n]
    }
    fn push_tx(&mut self, sm: usize, v: u32) {
        self.pio.write(0x10 + 4 * sm as u32, v);
    }
    fn pop_rx(&mut self, sm: usize) -> u32 {
        self.pio.read(0x20 + 4 * sm as u32)
    }
}

// ---- timing: dividers, delays, wrap ----

#[test]
fn delays_and_wrap_set_the_toggle_period() {
    let mut r = Rig::new();
    r.pinctrl(0, 1 << 26); // SET_COUNT 1, SET_BASE 0
    r.load(0, &[with(set(PINS, 1), 3), with(set(PINS, 0), 3)]);
    r.wrap(0, 0, 1);
    r.pio.write(0x40, 1); // pad 0 output enable (via forced SET PINDIRS below instead)
    r.exec(0, &[set(PINDIRS, 1)]);
    r.env.edges.clear();
    r.enable(1);
    r.run(20);
    let edges: Vec<(u64, u32)> = r.env.edges.iter().map(|e| (e.0, e.1 & 1)).collect();
    // Each instruction takes 1 cycle plus 3 delay cycles.
    let base = edges[0].0;
    assert_eq!(
        edges,
        [
            (base, 1),
            (base + 4, 0),
            (base + 8, 1),
            (base + 12, 0),
            (base + 16, 1)
        ]
    );
}

#[test]
fn fractional_divider_ticks_like_a_phase_accumulator() {
    let mut r = Rig::new();
    r.pinctrl(0, 1 << 26);
    r.pio.sm_write(0, 0, 2 << 16 | 128 << 8); // divide by 2.5
    r.load(0, &[set(PINS, 1), set(PINS, 0)]);
    r.wrap(0, 0, 1);
    r.enable(1);
    r.run(16);
    let cycles: Vec<u64> = r.env.edges.iter().map(|e| e.0).collect();
    // 640/256 per tick: ticks at cycles 3, 5, 8, 10, 13, 15.
    assert_eq!(cycles, [3, 5, 8, 10, 13, 15]);
}

#[test]
fn divider_register_encoding() {
    let mut sm = Sm::new();
    assert_eq!(sm.div256(), 256, "reset value 1.0");
    sm.clkdiv = 0;
    assert_eq!(sm.div256(), 65536 * 256, "an integer part of 0 means 65536");
    sm.clkdiv = 3 << 16 | 0x80 << 8;
    assert_eq!(sm.div256(), 3 * 256 + 128);
}

#[test]
fn jump_wraps_and_taken_jumps_ignore_wrap() {
    let mut r = Rig::new();
    r.pinctrl(0, 1 << 26);
    r.load(4, &[set(X, 1), set(X, 2), jmp(JMP_ALWAYS, 5)]);
    r.wrap(0, 4, 5); // wrap after the second instruction
    r.exec(0, &[jmp(JMP_ALWAYS, 4)]);
    r.enable(1);
    r.run(1);
    assert_eq!((r.sm(0).pc, r.sm(0).x), (5, 1));
    r.run(1);
    assert_eq!(
        (r.sm(0).pc, r.sm(0).x),
        (4, 2),
        "wrapped from top to bottom"
    );
}

// ---- control flow and registers ----

#[test]
fn jmp_conditions() {
    let mut r = Rig::new();
    let pc_after = |r: &mut Rig, instr| {
        r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
        r.pio.sm[0].pc = 7;
        r.exec(0, &[instr]);
        r.sm(0).pc
    };
    r.exec(0, &[set(X, 0), set(Y, 5)]);
    assert_eq!(pc_after(&mut r, jmp(JMP_NOT_X, 20)), 20, "!X: X is zero");
    assert_eq!(
        pc_after(&mut r, jmp(JMP_NOT_Y, 20)),
        7,
        "!Y: Y is non-zero: not taken (forced instructions don't advance)"
    );
    assert_eq!(pc_after(&mut r, jmp(JMP_X_NE_Y, 21)), 21);
    // X--: test, then decrement either way.
    r.exec(0, &[set(X, 1)]);
    assert_eq!(pc_after(&mut r, jmp(JMP_X_DEC, 22)), 22);
    assert_eq!(r.sm(0).x, 0);
    assert_eq!(
        pc_after(&mut r, jmp(JMP_X_DEC, 22)),
        7,
        "X was 0: not taken"
    );
    assert_eq!(r.sm(0).x, u32::MAX, "but it still decremented");
    r.exec(0, &[set(Y, 2)]);
    assert_eq!(pc_after(&mut r, jmp(JMP_Y_DEC, 23)), 23);
    assert_eq!(r.sm(0).y, 1);
    // PIN uses EXECCTRL.JMP_PIN on the (synchronized) pads.
    r.modify(0, 4, |v| v | 9 << 24);
    r.env.pads = 1 << 9;
    r.run(4);
    assert_eq!(pc_after(&mut r, jmp(JMP_PIN, 24)), 24);
    r.env.pads = 0;
    r.run(4);
    assert_eq!(pc_after(&mut r, jmp(JMP_PIN, 24)), 7);
    // !OSRE: taken while the OSR still holds unshifted bits.
    r.exec(0, &[mov(OSR, 0, X)]);
    assert_eq!(pc_after(&mut r, jmp(JMP_NOT_OSRE, 25)), 25);
    r.exec(0, &[out(NULL, 0)]);
    assert_eq!(pc_after(&mut r, jmp(JMP_NOT_OSRE, 25)), 7, "OSR empty");
}

#[test]
fn set_mov_and_pin_writes() {
    let mut r = Rig::new();
    r.pinctrl(0, 3 << 20 | 4 | 5 << 26 | 8 << 5); // OUT_BASE 4 x3, SET_BASE 8 x5
    r.exec(0, &[set(PINDIRS, 0x1F), set(X, 21)]);
    assert_eq!(r.pio.outputs().1, 0x1F << 8);
    r.exec(0, &[set(PINS, 0b10101)]);
    assert_eq!(r.pio.outputs().0, 0b10101 << 8);
    r.exec(0, &[set(PINS, 0b00100)]);
    assert_eq!(r.pio.outputs().0, 0b00100 << 8);
    // MOV PINS writes OUT_COUNT bits at OUT_BASE.
    r.exec(0, &[mov(PINS, 0, X)]); // X = 0b10101, 3 pins: 0b101
    assert_eq!(r.pio.outputs().0 >> 4 & 7, 0b101);
    // OUT PINDIRS too.
    r.exec(0, &[mov(OSR, 0, X), out(PINDIRS, 3)]);
    assert_eq!(r.pio.outputs().1 >> 4 & 7, 0b101);
}

#[test]
fn mov_operations_and_sources() {
    let mut r = Rig::new();
    r.exec(0, &[set(X, 0b10110), mov(Y, 1, X)]);
    assert_eq!(r.sm(0).y, !0b10110u32, "invert");
    r.exec(0, &[mov(Y, 2, X)]);
    assert_eq!(r.sm(0).y, 0b10110u32.reverse_bits(), "bit reverse");
    r.exec(0, &[mov(X, 0, NULL)]);
    assert_eq!(r.sm(0).x, 0);
    r.exec(0, &[set(Y, 3), mov(ISR, 0, Y)]);
    assert_eq!((r.sm(0).isr, r.sm(0).isr_count), (3, 0));
    r.exec(0, &[mov(X, 0, ISR), mov(OSR, 0, X)]);
    assert_eq!((r.sm(0).x, r.sm(0).osr, r.sm(0).osr_count), (3, 3, 0));
    r.exec(0, &[mov(Y, 0, OSR)]);
    assert_eq!(r.sm(0).y, 3);
    // MOV X, PINS reads the pads from IN_BASE.
    r.env.pads = 0b1010_0000;
    r.pinctrl(0, 4 << 15 | 1 << 26);
    r.run(4);
    r.exec(0, &[mov(X, 0, PINS)]);
    assert_eq!(r.sm(0).x, 0b1010_0000u32.rotate_right(4));
}

#[test]
fn mov_status_compares_fifo_level() {
    let mut r = Rig::new();
    // STATUS_SEL=0 (TX level), STATUS_N=2: true while fewer than 2 words queued.
    r.pio.sm_write(0, 4, 2);
    r.exec(0, &[mov(X, 0, STATUS)]);
    assert_eq!(r.sm(0).x, u32::MAX);
    r.push_tx(0, 1);
    r.push_tx(0, 2);
    r.exec(0, &[mov(X, 0, STATUS)]);
    assert_eq!(r.sm(0).x, 0);
    // RX level.
    r.pio.sm_write(0, 4, EXEC_STATUS_SEL | 1);
    r.exec(0, &[mov(X, 0, STATUS)]);
    assert_eq!(r.sm(0).x, u32::MAX, "RX FIFO empty: level 0 < 1");
}

#[test]
fn mov_pc_and_exec_redirect_execution() {
    let mut r = Rig::new();
    r.load(0, &[mov(PC, 0, X), set(Y, 1), set(Y, 2), set(Y, 3)]);
    r.exec(0, &[set(X, 2), jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    r.run(1);
    assert_eq!(r.sm(0).pc, 2, "MOV PC, X");
    r.run(1);
    assert_eq!(r.sm(0).y, 2);
    // MOV EXEC runs the register value as an instruction in place of the next fetch.
    let mut r = Rig::new();
    r.load(0, &[mov(MOV_EXEC, 0, X), set(Y, 9), set(Y, 4)]);
    r.exec(0, &[set(X, 0), mov(X, 1, X)]); // X = 0xFFFFFFFF: low 16 bits = 0xFFFF
    r.pio.sm[0].x = set(Y, 7) as u32;
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    r.run(2);
    assert_eq!(
        r.sm(0).y,
        7,
        "the exec'd SET ran instead of the instruction at pc 1"
    );
    assert_eq!(r.sm(0).pc, 1, "and did not advance the program counter");
    r.run(1);
    assert_eq!(r.sm(0).y, 9);
}

// ---- shifting, FIFOs, autopush/autopull ----

#[test]
fn out_shifts_in_the_configured_direction() {
    let mut r = Rig::new();
    r.push_tx(0, 0xDEAD_BEEF);
    r.exec(0, &[pull(false, true), out(X, 8)]); // shift right (default)
    assert_eq!(r.sm(0).x, 0xEF);
    assert_eq!(r.sm(0).osr, 0x00DE_ADBE);
    r.modify(0, 8, |v| v & !SHIFT_OUT_RIGHT); // shift left
    r.push_tx(0, 0xDEAD_BEEF);
    r.exec(0, &[pull(false, true), out(Y, 8)]);
    assert_eq!(r.sm(0).y, 0xDE);
    assert_eq!(r.sm(0).osr, 0xADBE_EF00);
    r.exec(0, &[out(X, 0)]); // count 0 = 32 bits
    assert_eq!(r.sm(0).x, 0xADBE_EF00);
    assert_eq!(r.sm(0).osr_count, 32);
    r.exec(0, &[out(ISR, 4)]);
    assert_eq!(
        (r.sm(0).isr, r.sm(0).isr_count),
        (0, 4),
        "OUT ISR sets the shift count"
    );
}

#[test]
fn in_shifts_into_the_isr_and_counts() {
    let mut r = Rig::new();
    r.exec(0, &[set(X, 0b11001), in_(X, 5)]); // right: new bits enter at the top
    assert_eq!((r.sm(0).isr, r.sm(0).isr_count), (0b11001 << 27, 5));
    r.exec(0, &[set(Y, 0b101), in_(Y, 3)]);
    assert_eq!(r.sm(0).isr, 0b101 << 29 | 0b11001 << 24);
    r.modify(0, 8, |v| v & !SHIFT_IN_RIGHT); // shift left
    r.exec(0, &[mov(ISR, 0, NULL), in_(X, 5), in_(Y, 3)]);
    assert_eq!(r.sm(0).isr, 0b11001 << 3 | 0b101);
    assert_eq!(r.sm(0).isr_count, 8);
    r.exec(0, &[in_(NULL, 32)]);
    assert_eq!((r.sm(0).isr, r.sm(0).isr_count), (0, 32));
    r.exec(0, &[mov(OSR, 0, X), in_(OSR, 4)]);
    assert_eq!(r.sm(0).isr & 0xF, 0b11001 & 0xF);
}

#[test]
fn push_and_pull_variants() {
    let mut r = Rig::new();
    // Blocking push/pull move data through the FIFOs.
    r.exec(0, &[set(X, 7), mov(ISR, 0, X), push(false, true)]);
    assert_eq!(r.pop_rx(0), 7);
    assert_eq!((r.sm(0).isr, r.sm(0).isr_count), (0, 0));
    r.push_tx(0, 0x55);
    r.exec(0, &[pull(false, true), out(Y, 0)]);
    assert_eq!(r.sm(0).y, 0x55);
    // IfFull / IfEmpty do nothing below the threshold.
    r.exec(0, &[set(X, 1), mov(ISR, 0, X), push(true, true)]);
    assert!(r.sm(0).rx.is_empty(), "isr_count 0 < threshold 32: no push");
    r.exec(0, &[in_(X, 0), push(true, true)]);
    assert_eq!(r.sm(0).rx.len(), 1);
    r.push_tx(0, 9);
    r.exec(0, &[mov(OSR, 0, X), pull(true, true)]); // OSR full: no pull
    assert_eq!(r.sm(0).tx.len(), 1);
    // Non-blocking pull on an empty FIFO copies X.
    r.sm_clear(0);
    r.exec(0, &[set(X, 12), pull(false, false)]);
    assert_eq!((r.sm(0).osr, r.sm(0).osr_count), (12, 0));
    // Non-blocking push on a full FIFO drops the data.
    for v in 0..4 {
        r.exec(0, &[set(X, v), mov(ISR, 0, X), push(false, false)]);
    }
    assert_eq!(r.sm(0).rx.len(), 4);
    r.exec(0, &[set(X, 30), mov(ISR, 0, X), push(false, false)]);
    assert_eq!((r.sm(0).rx.len(), r.sm(0).isr), (4, 0));
    assert_eq!(
        (0..4).map(|_| r.pop_rx(0)).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
}

impl Rig {
    fn sm_clear(&mut self, n: usize) {
        self.pio.sm[n].tx.clear();
        self.pio.sm[n].rx.clear();
    }
}

#[test]
fn blocking_push_and_pull_stall_until_the_fifo_allows() {
    let mut r = Rig::new();
    r.load(0, &[pull(false, true), out(X, 0), push(false, true)]);
    r.wrap(0, 0, 2);
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    r.run(10);
    assert_eq!(r.sm(0).pc, 0, "stalled on PULL with an empty TX FIFO");
    assert!(r.sm(0).stalled);
    assert_eq!(r.pio.read(8) >> FD_TXSTALL & 1, 1, "TXSTALL flagged");
    r.push_tx(0, 77);
    r.run(5);
    assert_eq!(r.sm(0).x, 77);
    // PUSH of the (empty) ISR moves a zero into RX; fill RX to make it block.
    r.sm_clear(0);
    for _ in 0..4 {
        r.pio.sm[0].rx.push_back(0);
    }
    r.push_tx(0, 5);
    r.run(10);
    assert_eq!(r.sm(0).pc, 2, "stalled on PUSH with a full RX FIFO");
    assert_eq!(r.pio.read(8) >> FD_RXSTALL & 1, 1);
    r.pop_rx(0);
    r.run(5);
    assert_eq!(
        r.sm(0).rx.len(),
        4,
        "the push went through once there was room"
    );
}

#[test]
fn autopull_refills_the_osr_and_stalls_when_the_fifo_is_empty() {
    let mut r = Rig::new();
    // Pull threshold 32 (field 0), shift left: bytes come out MSB first.
    r.pio.sm_write(0, 8, SHIFT_AUTOPULL | SHIFT_IN_RIGHT);
    r.load(0, &[out(X, 8)]);
    r.wrap(0, 0, 0);
    r.push_tx(0, 0x1122_3344);
    r.push_tx(0, 0x5566_7788);
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    let mut got = Vec::new();
    for _ in 0..4 {
        r.run(1);
        got.push(r.sm(0).x);
    }
    assert_eq!(got, [0x11, 0x22, 0x33, 0x44]);
    assert!(
        r.sm(0).tx.is_empty(),
        "the second word was pulled as soon as the OSR ran dry"
    );
    r.run(1);
    assert_eq!(r.sm(0).x, 0x55);
    r.run(3);
    assert_eq!(r.sm(0).x, 0x88);
    r.run(1);
    assert!(r.sm(0).stalled, "FIFO empty: stalled before the next OUT");
    assert_eq!(r.pio.read(8) >> FD_TXSTALL & 1, 1);
    // A threshold below 32 pulls a fresh word once that many bits are used.
    let mut r = Rig::new();
    r.pio
        .sm_write(0, 8, SHIFT_AUTOPULL | SHIFT_IN_RIGHT | 8 << 25);
    r.load(0, &[out(X, 8)]);
    r.wrap(0, 0, 0);
    r.push_tx(0, 0x1122_3344);
    r.push_tx(0, 0x5566_7788);
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    r.run(2);
    assert_eq!(r.sm(0).x, 0x55, "the rest of the first word was discarded");
}

#[test]
fn autopush_pushes_at_the_threshold_and_stalls_on_a_full_fifo() {
    let mut r = Rig::new();
    r.pio
        .sm_write(0, 8, SHIFT_AUTOPUSH | SHIFT_IN_RIGHT | 8 << 20);
    r.load(0, &[in_(X, 4)]);
    r.wrap(0, 0, 0);
    r.exec(0, &[set(X, 0xA), jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    r.run(2);
    assert_eq!(
        r.pop_rx(0),
        0xAA00_0000,
        "two nibbles reached the 8-bit threshold"
    );
    r.run(8);
    assert_eq!(r.sm(0).rx.len(), 4, "four more words queued up");
    r.run(4);
    assert_eq!(r.sm(0).rx.len(), 4);
    assert!(
        r.sm(0).pending_push && r.sm(0).stalled,
        "stalled holding a finished word"
    );
    r.pop_rx(0);
    r.run(2);
    assert!(!r.sm(0).pending_push);
    assert_eq!(r.sm(0).rx.len(), 4, "one slot freed, immediately refilled");
}

#[test]
fn fifo_registers_flags_and_joins() {
    let mut r = Rig::new();
    assert_eq!(r.pio.read(4), 0x0F00_0F00, "all empty, none full");
    for v in 0..4 {
        r.push_tx(1, v);
    }
    assert_eq!(r.pio.read(4) >> 16 & 2, 2, "TXFULL for SM1");
    assert_eq!(r.pio.read(4) >> 24 & 2, 0);
    assert_eq!(r.pio.read(0xC) >> 8 & 0xF, 4, "FLEVEL TX1");
    r.push_tx(1, 99);
    assert_eq!(r.pio.read(8) >> FD_TXOVER & 2, 2, "TXOVER sticky");
    r.pio.write(8, 2 << FD_TXOVER);
    assert_eq!(r.pio.read(8) >> FD_TXOVER & 2, 0, "write 1 to clear");
    assert_eq!(r.pop_rx(2), 0);
    assert_eq!(
        r.pio.read(8) >> FD_RXUNDER & 4,
        4,
        "read of an empty RX FIFO"
    );
    // Joining TX doubles its depth and removes RX; changing the join clears the FIFOs.
    r.modify(1, 8, |v| v | SHIFT_FJOIN_TX);
    assert!(r.sm(1).tx.is_empty());
    for v in 0..8 {
        r.push_tx(1, v);
    }
    assert_eq!(r.pio.read(4) >> 16 & 2, 2);
    assert_eq!(r.pio.read(0xC) >> 8 & 0xF, 8);
    assert_eq!(
        r.pio.read(4) >> 1 & 1,
        1,
        "RX FIFO of a TX-joined machine always reads full"
    );
    r.pio
        .sm_write(1, 8, SHIFT_IN_RIGHT | SHIFT_OUT_RIGHT | SHIFT_FJOIN_RX);
    for v in 0..8 {
        r.pio.sm[1].rx.push_back(v);
    }
    assert_eq!(r.pio.read(0xC) >> 12 & 0xF, 8);
    assert!(r.sm(1).tx_depth() == 0);
}

// ---- IRQ flags and WAIT ----

#[test]
fn irq_instruction_sets_clears_waits_and_indexes_relatively() {
    let mut r = Rig::new();
    r.exec(0, &[irq(false, false, 2)]);
    assert_eq!(r.pio.read(0x30), 1 << 2);
    r.exec(0, &[irq(true, false, 2)]);
    assert_eq!(r.pio.read(0x30), 0);
    // Relative: flag = (idx + sm) mod 4, keeping bit 2.
    r.exec(3, &[irq(false, false, 0x10 | 2)]); // (2 + 3) & 3 = 1
    assert_eq!(r.pio.read(0x30), 1 << 1);
    r.exec(2, &[irq(false, false, 0x10 | 4 | 3)]); // 4 | ((3 + 2) & 3) = 5
    assert_eq!(r.pio.read(0x30), 1 << 1 | 1 << 5);
    // CPU clears with a write of 1s, and can force flags.
    r.pio.write(0x30, 1 << 1);
    assert_eq!(r.pio.read(0x30), 1 << 5);
    r.pio.write(0x34, 1 << 7);
    assert_eq!(r.pio.read(0x30), 1 << 5 | 1 << 7);
}

#[test]
fn irq_wait_blocks_until_another_agent_clears_the_flag() {
    let mut r = Rig::new();
    r.load(0, &[irq(false, true, 1), set(X, 5)]);
    r.wrap(0, 0, 1);
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    r.run(6);
    assert_eq!(r.pio.read(0x30), 2, "flag raised");
    assert!(r.sm(0).stalled && r.sm(0).pc == 0 && r.sm(0).x == 0);
    r.pio.write(0x30, 2); // CPU acknowledges
    r.run(3);
    assert_eq!(r.sm(0).x, 5, "released and moved on");
}

#[test]
fn wait_on_gpio_pin_and_irq_with_polarity() {
    let mut r = Rig::new();
    r.pio.write(0x38, !0); // bypass the synchronizers for this test
    r.pinctrl(0, 2 << 15 | 1 << 26); // IN_BASE 2
    r.load(
        0,
        &[
            wait(1, 0, 5),
            set(X, 1),
            wait(0, 1, 1),
            set(X, 2),
            wait(1, 2, 3),
            set(X, 3),
        ],
    );
    r.wrap(0, 0, 5);
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    r.run(5);
    assert_eq!((r.sm(0).pc, r.sm(0).x), (0, 0), "waiting for GPIO 5 high");
    r.env.pads = 1 << 5;
    r.run(2);
    assert_eq!(r.sm(0).x, 1);
    // WAIT PIN 1 low: GPIO IN_BASE+1 = 3, currently low, so it passes at once.
    r.run(2);
    assert_eq!(r.sm(0).x, 2);
    // WAIT IRQ 3 high stalls until the flag is set, then clears it.
    r.run(4);
    assert_eq!(r.sm(0).pc, 4);
    r.pio.write(0x34, 1 << 3);
    r.run(2);
    assert_eq!(
        (r.sm(0).x, r.pio.read(0x30)),
        (3, 0),
        "flag consumed by the WAIT"
    );
}

// ---- side-set ----

#[test]
fn side_set_applies_first_and_splits_the_delay_field() {
    let mut r = Rig::new();
    // 2 side-set bits on pins 10-11, so the field has 3 delay bits.
    r.pinctrl(0, 2 << 29 | 10 << 10 | 1 << 26);
    r.exec(0, &[mov(PC, 0, NULL)]); // (just to make sure forced instrs work) then enable pindirs
    r.pio.shared.oe = 3 << 10;
    r.load(0, &[with(set(X, 1), 2 << 3 | 5), with(set(X, 2), 1 << 3)]);
    r.wrap(0, 0, 1);
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.env.edges.clear();
    r.enable(1);
    r.run(8);
    let sides: Vec<(u64, u32)> = r.env.edges.iter().map(|e| (e.0, e.1 >> 10 & 3)).collect();
    // First instruction: side 0b10, delay 5; second (6 cycles later): side 0b01, delay 0.
    let t0 = sides[0].0;
    assert_eq!(sides[..2], [(t0, 2), (t0 + 6, 1)]);
    assert_eq!(sides[2], (t0 + 7, 2), "and the program wraps straight back");
}

#[test]
fn side_set_enable_bit_and_pindirs_mode() {
    let mut r = Rig::new();
    // SIDE_EN: top of the 2 side-set bits is the enable, 1 data bit on pin 12.
    r.pinctrl(0, 2 << 29 | 12 << 10);
    r.modify(0, 4, |v| v | EXEC_SIDE_EN);
    r.pio.shared.oe = 1 << 12;
    r.load(
        0,
        &[with(NOP, 0b11 << 3), with(NOP, 0), with(NOP, 0b01 << 3)],
    );
    r.wrap(0, 0, 2);
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    r.run(1);
    assert_eq!(r.pio.outputs().0 >> 12 & 1, 1, "enable=1, data=1");
    r.run(1);
    assert_eq!(r.pio.outputs().0 >> 12 & 1, 1, "enable=0: pin left alone");
    r.run(1);
    assert_eq!(
        r.pio.outputs().0 >> 12 & 1,
        1,
        "enable=0 even with data bit set"
    );
    // Side-set to pin directions.
    let mut r = Rig::new();
    r.pinctrl(0, 1 << 29 | 3 << 10);
    r.modify(0, 4, |v| v | EXEC_SIDE_PINDIR);
    r.load(0, &[with(NOP, 1 << 4)]);
    r.wrap(0, 0, 0);
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    r.run(1);
    assert_eq!(
        r.pio.outputs().1 >> 3 & 1,
        1,
        "side-set wrote the direction"
    );
    assert_eq!(r.pio.outputs().0, 0);
}

#[test]
fn side_set_happens_even_when_the_instruction_stalls() {
    let mut r = Rig::new();
    r.pinctrl(0, 1 << 29 | 7 << 10);
    r.pio.shared.oe = 1 << 7;
    r.load(0, &[with(pull(false, true), 1 << 4)]); // side 1, then block on an empty FIFO
    r.wrap(0, 0, 0);
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    r.run(3);
    assert_eq!(r.pio.outputs().0 >> 7 & 1, 1, "pin high while stalled");
    assert!(r.sm(0).stalled);
}

// ---- pads: merging, synchronizers, EXEC ----

#[test]
fn highest_machine_wins_a_pad_and_the_value_persists() {
    let mut r = Rig::new();
    for n in 0..2 {
        r.pinctrl(n, 1 << 26 | 3 << 5); // SET_BASE 3
    }
    r.exec(0, &[set(PINDIRS, 1)]);
    r.load(0, &[set(PINS, 1)]);
    r.load(1, &[set(PINS, 0)]);
    r.wrap(0, 0, 0);
    r.wrap(1, 1, 1);
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.exec(1, &[jmp(JMP_ALWAYS, 1)]);
    r.env.edges.clear();
    r.enable(0b11);
    r.run(4);
    // Both write pin 3 every cycle; machine 1 comes later in the cycle and wins.
    assert_eq!(r.pio.outputs().0 >> 3 & 1, 0);
    assert!(
        r.env.edges.iter().all(|e| e.1 >> 3 & 1 == 0 || e.0 == 1),
        "no lasting high: {:?}",
        r.env.edges
    );
    r.enable(0b01);
    r.run(2);
    assert_eq!(
        r.pio.outputs().0 >> 3 & 1,
        1,
        "machine 0 alone now drives it high"
    );
    r.enable(0);
    r.run(2);
    assert_eq!(
        r.pio.outputs().0 >> 3 & 1,
        1,
        "and it stays when nothing writes"
    );
}

#[test]
fn input_synchronizer_adds_two_cycles_unless_bypassed() {
    fn latency(bypass: bool) -> u64 {
        let mut r = Rig::new();
        if bypass {
            r.pio.write(0x38, 1);
        }
        r.pinctrl(0, 1 << 26);
        r.load(0, &[wait(1, 0, 0), set(PINDIRS, 1), set(PINS, 1)]);
        r.wrap(0, 0, 2);
        r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
        r.enable(1);
        r.run(6);
        r.env.edges.clear();
        r.env.pads = 1;
        let start = r.now;
        r.run(12);
        // Cycles from the pad change until the program reacts.
        r.env.edges[0].0 - start
    }
    assert_eq!(latency(true) + 2, latency(false));
}

#[test]
fn forced_instruction_runs_while_disabled_and_reports_stalls() {
    let mut r = Rig::new();
    r.exec(0, &[set(X, 4)]);
    assert_eq!(
        (r.sm(0).x, r.sm(0).pc),
        (4, 0),
        "executed while disabled, pc untouched"
    );
    r.pio.sm_write(0, 0x10, pull(false, true) as u32);
    r.run(3);
    assert_eq!(r.pio.sm_read(0, 4) >> 31, 1, "EXEC_STALLED");
    r.push_tx(0, 3);
    r.run(2);
    assert_eq!(r.pio.sm_read(0, 4) >> 31, 0);
    assert_eq!(r.sm(0).osr, 3);
    // OUT EXEC.
    r.push_tx(0, set(Y, 6) as u32);
    r.exec(0, &[pull(false, true), out(EXEC, 16)]);
    r.run(2);
    assert_eq!(r.sm(0).y, 6);
    // SMx_INSTR reads back the instruction at PC.
    r.pio.write(0x48, 0x1234);
    assert_eq!(r.pio.sm_read(0, 0x10), 0x1234);
}

#[test]
fn restart_clears_execution_state_but_not_registers() {
    let mut r = Rig::new();
    r.exec(0, &[set(X, 9), set(Y, 8), set(Y, 8), in_(X, 4)]);
    r.pio.sm[0].delay = 5;
    r.pio.write(0, 1 << 4); // SM_RESTART for machine 0
    let sm = r.sm(0);
    assert_eq!(
        (sm.isr, sm.isr_count, sm.osr_count, sm.delay),
        (0, 0, 32, 0)
    );
    assert_eq!((sm.x, sm.y), (9, 8));
    assert_eq!(r.pio.read(0), 0, "restart bits are self-clearing");
}

// ---- interrupts, requests and the idle skip ----

#[test]
fn interrupt_lines_combine_fifo_state_and_flags() {
    let mut r = Rig::new();
    r.pio.write(0x12C, 1 << 8 | 1 << 1 | 1 << 4); // IRQ0: flag 0, SM1 RX not empty, SM0 TX not full
    assert!(r.pio.irq(0), "TX FIFO of SM0 has room");
    r.pio.write(0x12C, 1 << 8 | 1 << 1);
    assert!(!r.pio.irq(0));
    r.pio.write(0x34, 1);
    assert!(r.pio.irq(0), "IRQ flag 0");
    assert_eq!(r.pio.read(0x134) & 1 << 8, 1 << 8);
    r.pio.write(0x30, 1);
    assert!(!r.pio.irq(0));
    r.pio.sm[1].rx.push_back(1);
    assert!(r.pio.irq(0), "SM1 RX not empty");
    assert!(!r.pio.irq(1), "line 1 has its own enables");
    r.pio.write(0x138, 1 << 9);
    r.pio.write(0x13C, 1 << 9); // force SM1's flag bit
    assert!(r.pio.irq(1));
    assert_eq!(r.pio.read(0x140), 1 << 9);
    // Flags 4..7 are machine-visible only.
    r.pio.write(0x34, 0xF0);
    assert_eq!(r.pio.read(0x128) >> 8 & 0xF, 0);
}

#[test]
fn dma_requests_follow_fifo_levels() {
    let mut r = Rig::new();
    assert!(r.pio.dreq_tx(2) && !r.pio.dreq_rx(2));
    for v in 0..4 {
        r.push_tx(2, v);
    }
    assert!(!r.pio.dreq_tx(2));
    r.pio.sm[2].rx.push_back(1);
    assert!(r.pio.dreq_rx(2));
}

#[test]
fn config_registers_reset_values_and_masks() {
    let mut r = Rig::new();
    assert_eq!(r.pio.sm_read(0, 0), 0x0001_0000);
    assert_eq!(r.pio.sm_read(0, 4), 0x0001_F000);
    assert_eq!(r.pio.sm_read(0, 8), 0x000C_0000);
    assert_eq!(r.pio.sm_read(0, 0x14), 0x1400_0000);
    assert_eq!(r.pio.read(0x44), 0x0020_0404);
    // SM register windows are 0x18 apart.
    r.pio.write(0xC8 + 0x18 * 3 + 0x14, 0x1234);
    assert_eq!(r.pio.read(0xC8 + 0x18 * 3 + 0x14), 0x1234);
    assert_eq!(r.pio.read(0xC8 + 0x14), 0x1400_0000);
    r.pio.write(0x48 + 4 * 31, 0xFFFF_ABCD);
    assert_eq!(r.pio.read(0x48 + 4 * 31), 0xABCD);
}

#[test]
fn skipping_idle_cycles_matches_stepping_them() {
    fn scenario(chunks: &[u64]) -> (Vec<(u64, u32, u32)>, u32, u8) {
        let mut r = Rig::new();
        r.pinctrl(0, 1 << 26 | 1 << 29 | 2 << 10);
        r.pio.sm_write(0, 0, 3 << 16 | 77 << 8); // an awkward divider
        r.pio.shared.oe = 1 << 2 | 1;
        r.load(
            0,
            &[
                pull(false, true),
                with(out(PINS, 1), 1 << 4),
                with(wait(1, 0, 4), 3),
            ],
        );
        r.wrap(0, 0, 2);
        r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
        r.enable(1);
        r.run(chunks[0]);
        r.push_tx(0, 1);
        r.run(chunks[1]);
        r.env.pads = 1 << 4;
        r.run(chunks[2]);
        let (acc, pc) = (r.sm(0).acc, r.sm(0).pc);
        (r.env.edges, acc, pc)
    }
    let big = scenario(&[1000, 4000, 4000]);
    let small = {
        // Same timeline, run one cycle at a time.
        let mut r = Rig::new();
        r.pinctrl(0, 1 << 26 | 1 << 29 | 2 << 10);
        r.pio.sm_write(0, 0, 3 << 16 | 77 << 8);
        r.pio.shared.oe = 1 << 2 | 1;
        r.load(
            0,
            &[
                pull(false, true),
                with(out(PINS, 1), 1 << 4),
                with(wait(1, 0, 4), 3),
            ],
        );
        r.wrap(0, 0, 2);
        r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
        r.enable(1);
        for _ in 0..1000 {
            r.run(1);
        }
        r.push_tx(0, 1);
        for _ in 0..4000 {
            r.run(1);
        }
        r.env.pads = 1 << 4;
        for _ in 0..4000 {
            r.run(1);
        }
        let (acc, pc) = (r.sm(0).acc, r.sm(0).pc);
        (r.env.edges, acc, pc)
    };
    assert_eq!(big, small);
    assert!(!big.0.is_empty());
}

#[test]
fn a_parked_machine_does_not_cost_per_cycle_work() {
    let mut r = Rig::new();
    r.load(0, &[pull(false, true)]);
    r.wrap(0, 0, 0);
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    r.run(100);
    assert!(!r.pio.active(), "parked on an empty TX FIFO");
    let t = std::time::Instant::now();
    r.run(2_000_000_000);
    assert!(
        t.elapsed().as_millis() < 500,
        "a skip, not 2e9 cycles of work"
    );
    r.push_tx(0, 1);
    assert!(r.pio.active(), "a FIFO write unparks it");
}

// ---- the BTL-01 firmware's own programs ----

/// ws2812.pio from the firmware: 1 side-set bit, T1=2, T2=5, T3=3.
const WS2812: [u16; 4] = [0x6221, 0x1123, 0x1400, 0xA442];

/// spi_tx.pio from the firmware: 2 side-set bits (CS, CLK), 24-bit frames.
const SPI_TX: [u16; 7] = [0xE836, 0x6841, 0xA002, 0x7141, 0x0042, 0xA002, 0xB142];

/// encoder.pio from the firmware (quadrature decoder with a jump table).
const ENCODER: [u16; 28] = [
    0x0010, 0x0015, 0x0017, 0x0010, 0x0017, 0x0010, 0x0010, 0x0015, 0x0015, 0x0010, 0x0010, 0x0017,
    0x0010, 0x0017, 0x0015, 0x0010, 0xA0C3, 0x4042, 0xA040, 0x4042, 0xA0A6, 0x005A, 0x001A, 0xA029,
    0x0059, 0xA029, 0xA0C1, 0x8000,
];

#[test]
fn ws2812_program_produces_the_right_pulse_widths() {
    let mut r = Rig::new();
    r.load(0, &WS2812);
    r.wrap(0, 0, 3);
    // 1 side-set pin on pin 14; autopull at 24 bits, shifting left.
    r.pinctrl(0, 1 << 29 | 14 << 10);
    r.pio
        .sm_write(0, 8, SHIFT_AUTOPULL | SHIFT_IN_RIGHT | 24 << 25);
    r.pio.shared.oe = 1 << 14;
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.push_tx(0, 0xA5_0000 << 8); // 24 bits in the top: 1010_0101 then zeros
    r.env.edges.clear();
    r.enable(1);
    r.run(400);
    let mut high_since = None;
    let mut pulses = Vec::new();
    for &(t, out, _) in &r.env.edges {
        match (out >> 14 & 1 != 0, high_since) {
            (true, None) => high_since = Some(t),
            (false, Some(s)) => {
                pulses.push((t - s) as i64);
                high_since = None;
            }
            _ => {}
        }
    }
    // T1 + T2 = 7 cycles for a one, T1 = 2 for a zero; every bit is 10 cycles long.
    let bits: Vec<u8> = pulses.iter().take(8).map(|&p| (p == 7) as u8).collect();
    assert_eq!(bits, [1, 0, 1, 0, 0, 1, 0, 1]);
    assert!(pulses.iter().all(|&p| p == 7 || p == 2), "{pulses:?}");
    assert_eq!(
        pulses.len(),
        24,
        "24 bits, then the FIFO runs dry and the line stays low"
    );
    let rises: Vec<u64> = r
        .env
        .edges
        .iter()
        .filter(|e| e.1 >> 14 & 1 != 0)
        .map(|e| e.0)
        .collect();
    assert!(
        rises.windows(2).all(|w| w[1] - w[0] == 10),
        "one bit every 10 cycles"
    );
}

#[test]
fn spi_tx_program_clocks_out_a_24_bit_frame() {
    let mut r = Rig::new();
    r.load(0, &SPI_TX);
    r.wrap(0, 0, 6);
    // OUT pin 4 (data); side-set pins 5 (CS) and 6 (CLK).
    r.pinctrl(0, 1 << 20 | 4 | 2 << 29 | 5 << 10);
    r.pio.sm_write(
        0,
        8,
        SHIFT_AUTOPULL | SHIFT_IN_RIGHT | 24 << 25 & (0x1F << 25),
    ); // shift left, threshold 24
    r.pio.shared.oe = 1 << 4 | 1 << 5 | 1 << 6;
    let frame: u32 = 0x12_3456;
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.push_tx(0, frame << 8);
    r.env.edges.clear();
    r.enable(1);
    r.run(300);
    // Decode: data sampled at each falling CLK edge while CS is low.
    let (mut prev, mut bits) = ((0u32, 0u32), Vec::new());
    let mut cs_rises = 0;
    for &(_, out, _) in &r.env.edges {
        let (clk, cs, dt) = (out >> 6 & 1, out >> 5 & 1, out >> 4 & 1);
        if prev.0 == 1 && clk == 0 && prev.1 == 0 {
            bits.push(dt);
        }
        if prev.1 == 0 && cs == 1 {
            cs_rises += 1;
        }
        prev = (clk, cs);
    }
    let got = bits.iter().fold(0u32, |a, &b| a << 1 | b);
    assert_eq!(bits.len(), 24, "{:?}", r.env.edges);
    assert_eq!(got, frame);
    assert!(cs_rises >= 1, "CS returns high after the frame");
}

#[test]
fn encoder_program_counts_quadrature_steps_both_ways() {
    let mut r = Rig::new();
    r.load(0, &ENCODER);
    r.wrap(0, 16, 27);
    r.pinctrl(0, 21 << 15); // IN_BASE = 21: DT = bit 0, CLK = bit 1
    // As the driver configures it: shift left, no autopush, RX FIFO joined.
    r.pio.sm_write(0, 8, SHIFT_OUT_RIGHT | SHIFT_FJOIN_RX);
    r.exec(0, &[set(X, 0), jmp(JMP_ALWAYS, 16)]);
    r.pio.write(0x38, 3 << 21);
    r.enable(1);
    // Pins idle high; one step = (CLK,DT): 11 -> 01 -> 00 -> 10 -> 11, clockwise (CLK leads).
    let set_pins = |r: &mut Rig, clk: u32, dt: u32| {
        r.env.pads = clk << 22 | dt << 21;
        r.run(40);
    };
    set_pins(&mut r, 1, 1);
    let count = |r: &mut Rig| r.pio.sm[0].rx.back().copied().unwrap_or(0) as i32;
    // The program pushes the counter on every pass (also when nothing changed).
    for (clk, dt) in [(0, 1), (0, 0), (1, 0), (1, 1)] {
        set_pins(&mut r, clk, dt);
    }
    let up = count(&mut r);
    for (clk, dt) in [(1, 0), (0, 0), (0, 1), (1, 1)] {
        set_pins(&mut r, clk, dt);
    }
    let down = count(&mut r);
    assert_eq!(up, 4, "CLK leading DT counts up, four steps per cycle");
    assert_eq!(down, 0, "turning back returns to the start");
}

// ---- cases ported from rp2040js (src/peripherals/pio.spec.ts, MIT) ----
//
// An independent implementation's expectations, re-expressed in this
// harness. They run forced instructions the way `pio_sm_exec` does and
// check registers, FIFO levels, debug pads and interrupt bits.

const DBG_PADOUT: u32 = 0x3C;
const VALID_PINS: u32 = 0x3FFF_FFFF;
const FLEVEL: u32 = 0x0C;
const INTR: u32 = 0x128;

/// A fresh block with the reset-state config the reference tests start from.
fn reference_rig() -> Rig {
    Rig::new()
}

fn padout(r: &mut Rig) -> u32 {
    r.pio.read(DBG_PADOUT)
}

fn addr(r: &mut Rig) -> u32 {
    r.pio.sm_read(0, 0xC)
}

/// Run `instrs` on machine 0 and read PC back.
fn pc_after(r: &mut Rig, instrs: &[u16]) -> u32 {
    r.exec(0, instrs);
    addr(r)
}

#[test]
fn rp2040js_set_and_mov_to_pins() {
    let mut r = reference_rig();
    r.pinctrl(0, 5 << 26);
    r.exec(0, &[set(PINS, 13)]);
    assert_eq!(padout(&mut r) & 0x1F, 13);

    let mut r = reference_rig();
    r.pinctrl(0, 32 << 20);
    r.exec(0, &[set(X, 8), mov(PINS, 0, X)]);
    assert_eq!(padout(&mut r), 8);

    let mut r = reference_rig();
    r.pinctrl(0, 32 << 20);
    r.exec(0, &[set(X, 29), mov(PINS, 1, X)]);
    assert_eq!(padout(&mut r), !29 & VALID_PINS, "MOV PINS, ~X");

    let mut r = reference_rig();
    r.pinctrl(0, 32 << 20);
    r.exec(0, &[set(X, 0b11001), mov(PINS, 2, X)]);
    assert_eq!(padout(&mut r), 0x9800_0000 & VALID_PINS, "MOV PINS, ::X");

    let mut r = reference_rig();
    r.pinctrl(0, 32 << 20);
    r.exec(0, &[set(X, 11), mov(Y, 0, X), mov(PINS, 0, Y)]);
    assert_eq!(padout(&mut r), 11, "MOV Y, X");
}

#[test]
fn rp2040js_mov_pc_and_status() {
    let mut r = reference_rig();
    assert_eq!(addr(&mut r), 0);
    assert_eq!(pc_after(&mut r, &[set(Y, 23), mov(PC, 0, Y)]), 23);

    // STATUS_SEL = 0: TX level < N (N = 2).
    let mut r = reference_rig();
    r.pio.sm_write(0, 4, 2);
    r.exec(0, &[mov(ISR, 0, STATUS), push(false, false)]);
    assert_eq!(r.pop_rx(0), 0xFFFF_FFFF);
    r.push_tx(0, 1);
    r.push_tx(0, 2);
    r.exec(0, &[mov(ISR, 0, STATUS), push(false, false)]);
    assert_eq!(r.pop_rx(0), 0);

    // STATUS_SEL = 1: RX level < N (N = 1): empty then one pushed.
    let mut r = reference_rig();
    r.pio.sm_write(0, 4, 1 | EXEC_STATUS_SEL);
    r.exec(
        0,
        &[
            mov(ISR, 0, STATUS),
            push(false, false),
            mov(ISR, 0, STATUS),
            push(false, false),
        ],
    );
    assert_eq!((r.pop_rx(0), r.pop_rx(0)), (0xFFFF_FFFF, 0));
}

#[test]
fn rp2040js_jmp_conditions() {
    let mut r = reference_rig();
    assert_eq!(pc_after(&mut r, &[jmp(JMP_ALWAYS, 10)]), 10);

    let mut r = reference_rig();
    assert_eq!(
        pc_after(&mut r, &[set(X, 5), jmp(JMP_NOT_X, 8)]),
        0,
        "!X with X=5: not taken"
    );
    assert_eq!(pc_after(&mut r, &[set(X, 0), jmp(JMP_NOT_X, 8)]), 8);

    let mut r = reference_rig();
    r.pinctrl(0, 32 << 20);
    assert_eq!(pc_after(&mut r, &[set(X, 5), jmp(JMP_X_DEC, 12)]), 12);
    r.exec(0, &[mov(PINS, 0, X)]);
    assert_eq!(padout(&mut r), 4, "X decremented");
    assert_eq!(
        pc_after(&mut r, &[set(X, 0), jmp(JMP_X_DEC, 6)]),
        12,
        "X=0: not taken"
    );

    let mut r = reference_rig();
    assert_eq!(pc_after(&mut r, &[set(Y, 6), jmp(JMP_NOT_Y, 8)]), 0);
    assert_eq!(pc_after(&mut r, &[set(Y, 0), jmp(JMP_NOT_Y, 8)]), 8);

    let mut r = reference_rig();
    r.pinctrl(0, 32 << 20);
    assert_eq!(pc_after(&mut r, &[set(Y, 15), jmp(JMP_Y_DEC, 12)]), 12);
    r.exec(0, &[mov(PINS, 0, Y)]);
    assert_eq!(padout(&mut r), 14, "Y decremented");
    assert_eq!(pc_after(&mut r, &[set(Y, 0), jmp(JMP_Y_DEC, 6)]), 12);

    let mut r = reference_rig();
    assert_eq!(
        pc_after(&mut r, &[set(X, 23), set(Y, 23), jmp(JMP_X_NE_Y, 26)]),
        0
    );
    assert_eq!(pc_after(&mut r, &[set(X, 3), jmp(JMP_X_NE_Y, 26)]), 26);

    // !OSRE: the OSR counts as full after MOV OSR, NULL, and empty once shifted out.
    let mut r = reference_rig();
    assert_eq!(
        pc_after(&mut r, &[mov(OSR, 0, NULL), jmp(JMP_NOT_OSRE, 11)]),
        11
    );
    assert_eq!(
        pc_after(&mut r, &[out(NULL, 32), jmp(JMP_NOT_OSRE, 22)]),
        11
    );
}

#[test]
fn rp2040js_pull_out_push_and_exec() {
    let mut r = reference_rig();
    r.pinctrl(0, 32 << 20);
    r.push_tx(0, 0x42F0_0D43);
    assert_eq!(r.pio.read(FLEVEL) & 0xF, 1, "TX0 holds one item");
    r.exec(0, &[pull(false, false)]);
    assert_eq!(r.pio.read(FLEVEL) & 0xF, 0);
    r.exec(0, &[out(PINS, 32)]);
    assert_eq!(padout(&mut r), 0x42F0_0D43 & VALID_PINS);

    let mut r = reference_rig();
    r.pinctrl(0, 32 << 20);
    r.push_tx(0, jmp(JMP_ALWAYS, 16) as u32);
    assert_eq!(pc_after(&mut r, &[pull(false, false)]), 0);
    assert_eq!(
        pc_after(&mut r, &[out(EXEC, 32)]),
        16,
        "OUT EXEC runs the jump"
    );

    let mut r = reference_rig();
    r.push_tx(0, 29);
    assert_eq!(pc_after(&mut r, &[pull(false, false)]), 0);
    assert_eq!(pc_after(&mut r, &[out(PC, 32)]), 29);

    // PUSH twice: the first carries X through the ISR, the second a zeroed ISR.
    let mut r = reference_rig();
    assert_eq!(r.pio.read(FLEVEL) >> 4 & 0xF, 0);
    r.exec(0, &[set(X, 9), in_(X, 32), push(false, false)]);
    assert_eq!(r.pio.read(FLEVEL) >> 4 & 0xF, 1);
    r.exec(0, &[push(false, false)]);
    assert_eq!(r.pio.read(FLEVEL) >> 4 & 0xF, 2);
    assert_eq!(r.pop_rx(0), 9);
    assert_eq!(r.pio.read(FLEVEL) >> 4 & 0xF, 1);
    assert_eq!(r.pop_rx(0), 0, "ISR was cleared by the first push");
    assert_eq!(r.pio.read(FLEVEL) >> 4 & 0xF, 0);
}

#[test]
fn rp2040js_irq_instructions_and_waits() {
    let mut r = reference_rig();
    r.pio.write(0x30, 0xFF);
    r.exec(0, &[irq(false, false, 2)]);
    assert_eq!(r.pio.read(0x30), 1 << 2);
    r.exec(0, &[irq(true, false, 2)]);
    assert_eq!(r.pio.read(0x30), 0);

    // Relative index on machine 2 then machine 0.
    r.exec(2, &[irq(false, false, 0x13)]);
    assert_eq!(r.pio.read(0x30), 1 << 1);
    r.exec(2, &[irq(true, false, 0x13)]);
    assert_eq!(r.pio.read(0x30), 0);
    r.exec(0, &[irq(false, false, 0x13)]);
    assert_eq!(r.pio.read(0x30), 1 << 3);

    // INTR reflects flags 0-3 in bits 8-11.
    let mut r = reference_rig();
    r.exec(2, &[irq(false, false, 2)]);
    assert_eq!(r.pio.read(INTR) & 0xF00, 1 << 10);

    // WAIT 1 IRQ 7: waits for the flag, then clears it.
    let mut r = reference_rig();
    r.load(0, &[mov(X, 0, X), wait(1, 2, 7), jmp(JMP_ALWAYS, 2)]);
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    r.run(5);
    assert_eq!(addr(&mut r), 1, "parked on the WAIT");
    r.exec(2, &[irq(false, false, 5)]);
    r.run(3);
    assert_eq!(addr(&mut r), 1, "a different flag doesn't release it");
    r.exec(2, &[irq(false, false, 7)]);
    r.run(3);
    assert_eq!(addr(&mut r), 2);
    assert_eq!(r.pio.read(0x30), 1 << 5, "the WAIT consumed flag 7");

    // WAIT 0 IRQ 7: waits for the flag to be clear.
    let mut r = reference_rig();
    r.exec(0, &[irq(false, false, 7)]);
    r.load(0, &[mov(X, 0, X), wait(0, 2, 7), jmp(JMP_ALWAYS, 2)]);
    r.exec(0, &[jmp(JMP_ALWAYS, 0)]);
    r.enable(1);
    r.run(5);
    assert_eq!(addr(&mut r), 1);
    r.pio.write(0x30, 1 << 7);
    r.run(3);
    assert_eq!(addr(&mut r), 2);
}

#[test]
fn rp2040js_mov_x_not_null_compares_equal_to_a_pulled_all_ones() {
    let mut r = reference_rig();
    r.push_tx(0, 0xFFFF_FFFF);
    r.exec(
        0,
        &[
            pull(false, false),
            mov(X, 1, NULL),
            out(Y, 32),
            jmp(JMP_ALWAYS, 8),
        ],
    );
    assert_eq!(
        pc_after(&mut r, &[jmp(JMP_X_NE_Y, 16)]),
        8,
        "X == Y: not taken"
    );
}

#[test]
fn rp2040js_program_wraps_at_wrap_top() {
    // jmp 2 / [wrap_target] jmp 1 / mov x, x [wrap] / jmp 3
    let mut r = reference_rig();
    r.wrap(0, 1, 2);
    r.load(
        0,
        &[
            jmp(JMP_ALWAYS, 2),
            jmp(JMP_ALWAYS, 1),
            mov(X, 0, X),
            jmp(JMP_ALWAYS, 3),
        ],
    );
    r.enable(1);
    r.run(4);
    assert_eq!(
        addr(&mut r),
        1,
        "ended up in the infinite loop at the wrap target"
    );
}

#[test]
fn rp2040js_autopull_and_autopush() {
    // Autopull, threshold 4, IN shifting left: 5, 6, 7 -> ISR 0x567.
    let mut r = reference_rig();
    r.pio
        .sm_write(0, 8, SHIFT_AUTOPULL | 4 << 25 | SHIFT_OUT_RIGHT);
    for v in [5, 6, 7] {
        r.push_tx(0, v);
    }
    r.exec(
        0,
        &[
            out(X, 4),
            in_(X, 4),
            out(X, 4),
            in_(X, 4),
            out(X, 4),
            in_(X, 4),
            push(false, false),
        ],
    );
    assert_eq!(r.pop_rx(0), 0x567);

    // An OUT bigger than the threshold doesn't pull again mid-instruction.
    let mut r = reference_rig();
    r.pio
        .sm_write(0, 8, SHIFT_AUTOPULL | 4 << 25 | SHIFT_OUT_RIGHT);
    r.push_tx(0, 0x25);
    r.push_tx(0, 0x36);
    r.exec(0, &[out(X, 8), in_(X, 8), push(false, false)]);
    assert_eq!(r.pop_rx(0), 0x25);

    // OUT with autopull stalls on an empty FIFO and goes on once it is filled.
    let mut r = reference_rig();
    r.pio
        .sm_write(0, 8, SHIFT_AUTOPULL | 4 << 25 | SHIFT_OUT_RIGHT);
    r.pio.sm_write(0, 0x10, out(X, 4) as u32);
    r.run(3);
    assert_eq!(r.pio.sm_read(0, 4) >> 31, 1, "EXEC_STALLED");
    r.push_tx(0, 0x36);
    r.run(2);
    assert_eq!(r.pio.sm_read(0, 4) >> 31, 0);

    // Autopush at the threshold, only at the end of the IN.
    let mut r = reference_rig();
    r.pio
        .sm_write(0, 8, SHIFT_AUTOPUSH | 8 << 20 | SHIFT_OUT_RIGHT);
    r.exec(0, &[set(X, 0x13), in_(X, 8)]);
    assert_eq!(r.pop_rx(0), 0x13);
    let mut r = reference_rig();
    r.pio
        .sm_write(0, 8, SHIFT_AUTOPUSH | 8 << 20 | SHIFT_OUT_RIGHT);
    r.exec(0, &[mov(X, 1, NULL), in_(X, 16)]);
    assert_eq!(r.pop_rx(0), 0xFFFF);
}

#[test]
fn rp2040js_autopush_stalls_on_a_full_rx_fifo_and_resumes() {
    let mut r = reference_rig();
    r.pio
        .sm_write(0, 8, SHIFT_AUTOPUSH | 8 << 20 | SHIFT_OUT_RIGHT);
    for v in [15, 16, 17, 18] {
        r.exec(0, &[set(X, v), in_(X, 8)]);
    }
    r.pio.sm_write(0, 0x10, set(X, 19) as u32);
    r.run(2);
    r.pio.sm_write(0, 0x10, in_(X, 8) as u32);
    r.run(4);
    assert_eq!(r.pio.sm_read(0, 4) >> 31, 1, "fills the RX FIFO and stalls");
    assert_eq!(r.pop_rx(0) & 0xFFFF, 15, "popping unstalls the machine");
    r.run(3);
    assert_eq!(r.pio.sm_read(0, 4) >> 31, 0);
    assert_eq!(
        (r.pop_rx(0), r.pop_rx(0), r.pop_rx(0), r.pop_rx(0)),
        (16, 17, 18, 19)
    );
}

#[test]
fn rp2040js_fifo_interrupt_bits_follow_the_levels() {
    // TXNFULL: set unless the TX FIFO is full.
    let mut r = reference_rig();
    r.pio.write(0x12C, 1 << 4);
    assert_eq!(r.pio.read(INTR) & 1 << 4, 1 << 4);
    assert!(r.pio.irq(0));
    for v in [1, 2, 3] {
        r.push_tx(0, v);
    }
    assert_eq!(
        r.pio.read(INTR) & 1 << 4,
        1 << 4,
        "three of four: still not full"
    );
    r.push_tx(0, 3);
    assert_eq!(r.pio.read(INTR) & 1 << 4, 0, "full");
    assert!(!r.pio.irq(0));
    r.exec(0, &[pull(false, false)]);
    assert_eq!(r.pio.read(INTR) & 1 << 4, 1 << 4, "a PULL made room");
    assert!(r.pio.irq(0));

    // RXNEMPTY: set while the RX FIFO holds data.
    let mut r = reference_rig();
    r.pio.write(0x12C, 1);
    assert_eq!(r.pio.read(INTR) & 1, 0);
    assert!(!r.pio.irq(0));
    r.exec(0, &[push(false, false)]);
    assert_eq!(r.pio.read(INTR) & 1, 1);
    assert!(r.pio.irq(0));
    r.pop_rx(0);
    assert_eq!(r.pio.read(INTR) & 1, 0);
    assert!(!r.pio.irq(0));
}

#[test]
fn rp2040js_txstall_flag_stays_while_the_machine_is_still_stalled() {
    let mut r = reference_rig();
    r.pio.write(0x08, 0xFFFF_FFFF);
    assert_eq!(r.pio.read(0x08), 0);
    r.pio.sm_write(0, 0x10, pull(false, true) as u32);
    r.run(3);
    let txstall = 1 << FD_TXSTALL;
    assert_eq!(r.pio.read(0x08) & txstall, txstall);
    r.pio.write(0x08, txstall);
    assert_eq!(
        r.pio.read(0x08) & txstall,
        txstall,
        "still stalled: not clearable"
    );
    r.push_tx(0, 42);
    r.run(2);
    r.pio.write(0x08, txstall);
    assert_eq!(r.pio.read(0x08) & txstall, 0, "unstalled: cleared");
}

/// The same encodings rp2040js's assembler spec expects.
#[test]
fn rp2040js_instruction_encodings() {
    assert_eq!(wait(1, 0, 12), 0x208C);
    assert_eq!(in_(X, 12), 0x402C);
    assert_eq!(out(Y, 30), 0x605E);
    assert_eq!(with(push(true, true), 12), 0x8C60);
    assert_eq!(pull(true, false), 0x80C0);
    assert_eq!(mov(X, 1, STATUS), 0xA02D);
    assert_eq!(irq(false, false, 4), 0xC004);
    assert_eq!(set(X, 12), 0xE02C);
}
