#![allow(dead_code)] // each test binary uses a different subset
//! Lock-step harness: runs the same instruction(s) on armv6m's `Cpu` and
//! on Unicorn (QEMU Cortex-M0) from an identical random state, then compares
//! architectural state and memory.

use armv6m::bus::Bus;
use armv6m::cpu::Cpu;
use unicorn_engine::{Arch, Mode, Prot, RegisterARM, Unicorn, uc_error};

pub const BASE: u32 = 0x2000_0000;
pub const SIZE: usize = 0x2000;
pub const CODE: u32 = BASE + 0x1000;
/// `UC_CPU_ARM_CORTEX_M0` (the Rust bindings don't export the enum).
const UC_CPU_ARM_CORTEX_M0: i32 = 7;

/// Our side's memory: one RAM window; anything else sets `unmapped`.
pub struct MemBus {
    pub mem: Vec<u8>,
    /// Bus accesses so far, and the index of the first one outside the window.
    pub accesses: usize,
    pub first_unmapped: Option<usize>,
}

impl MemBus {
    fn touch(&mut self, addr: u32, n: usize) -> Option<usize> {
        let i = self.accesses;
        self.accesses += 1;
        let off = addr.wrapping_sub(BASE) as usize;
        if addr < BASE || off + n > SIZE {
            self.first_unmapped.get_or_insert(i);
            return None;
        }
        Some(off)
    }
    fn rd(&mut self, addr: u32, n: usize) -> u32 {
        let Some(off) = self.touch(addr, n) else { return 0 };
        (0..n).fold(0, |v, i| v | (self.mem[off + i] as u32) << (8 * i))
    }
    fn wr(&mut self, addr: u32, n: usize, val: u32) {
        let Some(off) = self.touch(addr, n) else { return };
        for i in 0..n {
            self.mem[off + i] = (val >> (8 * i)) as u8;
        }
    }
}

impl Bus for MemBus {
    fn read8(&mut self, a: u32) -> u8 { self.rd(a, 1) as u8 }
    fn read16(&mut self, a: u32) -> u16 { self.rd(a, 2) as u16 }
    fn read32(&mut self, a: u32) -> u32 { self.rd(a, 4) }
    fn write8(&mut self, a: u32, v: u8) { self.wr(a, 1, v as u32) }
    fn write16(&mut self, a: u32, v: u16) { self.wr(a, 2, v as u32) }
    fn write32(&mut self, a: u32, v: u32) { self.wr(a, 4, v) }
}

/// Architectural state we compare (thread mode only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub r: [u32; 13],
    pub msp: u32,
    pub psp: u32,
    pub lr: u32,
    pub pc: u32,
    pub nzcv: u32, // bits 31..28
    pub primask: bool,
    pub spsel: bool,
}

impl State {
    pub fn random(rng: &mut fastrand::Rng) -> Self {
        let mut r = [0; 13];
        for x in &mut r {
            *x = rand_val(rng);
        }
        State {
            r,
            msp: BASE + (rng.u32(0x400..0xC00) & !3),
            psp: BASE + (rng.u32(0x400..0xC00) & !3),
            lr: rand_val(rng),
            pc: CODE,
            nzcv: rng.u32(..) & 0xF000_0000,
            primask: rng.bool(),
            spsel: rng.bool(),
        }
    }

    pub fn of_cpu(c: &Cpu) -> Self {
        State {
            r: c.r,
            msp: c.msp,
            psp: c.psp,
            lr: c.lr,
            pc: c.pc,
            nzcv: c.xpsr() & 0xF000_0000,
            primask: c.primask,
            spsel: c.spsel,
        }
    }

    pub fn to_cpu(&self) -> Cpu {
        let mut c = Cpu::new();
        c.r = self.r;
        c.msp = self.msp;
        c.psp = self.psp;
        c.lr = self.lr;
        c.pc = self.pc;
        c.n = self.nzcv >> 31 & 1 != 0;
        c.z = self.nzcv >> 30 & 1 != 0;
        c.c = self.nzcv >> 29 & 1 != 0;
        c.v = self.nzcv >> 28 & 1 != 0;
        c.primask = self.primask;
        c.spsel = self.spsel;
        c
    }

    /// Human-readable list of differing fields (self = ours, other = unicorn).
    pub fn diff(&self, other: &State) -> Vec<String> {
        let mut d = vec![];
        for i in 0..13 {
            if self.r[i] != other.r[i] {
                d.push(format!("r{i}: ours={:#010x} uc={:#010x}", self.r[i], other.r[i]));
            }
        }
        macro_rules! f {
            ($($n:ident),*) => {$(
                if self.$n != other.$n { d.push(format!("{}: ours={:#x} uc={:#x}", stringify!($n), self.$n, other.$n)); }
            )*};
        }
        f!(msp, psp, lr, pc, nzcv);
        if self.primask != other.primask { d.push(format!("primask: ours={} uc={}", self.primask, other.primask)); }
        if self.spsel != other.spsel { d.push(format!("spsel: ours={} uc={}", self.spsel, other.spsel)); }
        d
    }
}

const INTERESTING: [u32; 17] = [
    0, 1, 2, 3, 0x1f, 0x20, 0x21, 0xff, 0x100, 0x8000, 0xffff, 0x7fff_ffff, 0x8000_0000,
    0x8000_0001, 0xffff_0000, 0xffff_fffe, 0xffff_ffff,
];

/// Mix of RAM pointers (any alignment), small offsets, edge values, noise.
pub fn rand_val(rng: &mut fastrand::Rng) -> u32 {
    match rng.u32(0..10) {
        0..=2 => BASE + rng.u32(0x400..0xC00),
        3..=4 => rng.u32(0..0x100),
        5..=6 => INTERESTING[rng.usize(..INTERESTING.len())],
        _ => rng.u32(..),
    }
}

pub fn new_unicorn() -> Unicorn<'static, ()> {
    let mut uc = Unicorn::new(Arch::ARM, Mode::MCLASS | Mode::THUMB).expect("unicorn");
    uc.ctl_set_cpu_model(UC_CPU_ARM_CORTEX_M0).expect("cortex-m0 model");
    uc.mem_map(BASE as u64, SIZE as u64, Prot::ALL).expect("map");
    uc
}

const GP: [RegisterARM; 13] = [
    RegisterARM::R0, RegisterARM::R1, RegisterARM::R2, RegisterARM::R3, RegisterARM::R4,
    RegisterARM::R5, RegisterARM::R6, RegisterARM::R7, RegisterARM::R8, RegisterARM::R9,
    RegisterARM::R10, RegisterARM::R11, RegisterARM::R12,
];

pub fn uc_set_state(uc: &mut Unicorn<'_, ()>, s: &State) {
    let w = |uc: &mut Unicorn<'_, ()>, r, v: u32| uc.reg_write(r, v as u64).expect("reg_write");
    // CONTROL first so MSP/PSP below are unambiguous about banking.
    w(uc, RegisterARM::CONTROL, (s.spsel as u32) << 1);
    for (i, r) in GP.iter().enumerate() {
        w(uc, *r, s.r[i]);
    }
    w(uc, RegisterARM::MSP, s.msp);
    w(uc, RegisterARM::PSP, s.psp);
    w(uc, RegisterARM::LR, s.lr);
    w(uc, RegisterARM::PRIMASK, s.primask as u32);
    // Whole APSR, not just NZCV: Unicorn executes ARMv7-M DSP encodings that
    // we fault on, and the Q flag they set would otherwise leak into later
    // cases (visible through MRS).
    w(uc, RegisterARM::APSR, s.nzcv);
    w(uc, RegisterARM::PC, s.pc);
}

pub fn uc_get_state(uc: &Unicorn<'_, ()>) -> State {
    let g = |r| uc.reg_read(r).expect("reg_read") as u32;
    let mut r = [0; 13];
    for (i, reg) in GP.iter().enumerate() {
        r[i] = g(*reg);
    }
    State {
        r,
        msp: g(RegisterARM::MSP),
        psp: g(RegisterARM::PSP),
        lr: g(RegisterARM::LR),
        pc: g(RegisterARM::PC),
        nzcv: g(RegisterARM::APSR_NZCV) & 0xF000_0000,
        primask: g(RegisterARM::PRIMASK) & 1 != 0,
        spsel: g(RegisterARM::CONTROL) & 2 != 0,
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Class {
    /// Instruction retired normally.
    Ok,
    /// Fault / exception / breakpoint (HardFault, SVC, BKPT, UDF, ...).
    Trap,
    /// Touched memory outside the RAM window.
    Unmapped,
}

pub struct Pair {
    pub uc: Unicorn<'static, ()>,
    pub cpu: Cpu,
    pub bus: MemBus,
}

pub struct StepResult {
    /// Unicorn state right after the step (before any reset).
    pub uc_state: State,
    pub uc_mem: Vec<u8>,
    pub ours: Class,
    pub theirs: Class,
    pub uc_err: Option<uc_error>,
}

impl Pair {
    pub fn new() -> Self {
        Pair { uc: new_unicorn(), cpu: Cpu::new(), bus: MemBus { mem: vec![0; SIZE], accesses: 0, first_unmapped: None } }
    }

    /// Load identical state and memory into both sides.
    pub fn load(&mut self, st: &State, mem: &[u8]) {
        self.bus.mem.copy_from_slice(mem);
        self.bus.accesses = 0;
        self.bus.first_unmapped = None;
        self.cpu = st.to_cpu();
        self.uc.mem_write(BASE as u64, mem).unwrap();
        // The same code address holds a different instruction every case.
        self.uc.ctl_remove_cache(BASE as u64, (BASE as u64) + SIZE as u64).unwrap();
        uc_set_state(&mut self.uc, st);
    }

    /// Execute exactly one instruction on both sides.
    pub fn step(&mut self) -> StepResult {
        self.bus.accesses = 0;
        self.bus.first_unmapped = None;
        self.cpu.bkpt = None;
        self.cpu.step(&mut self.bus);
        // Exception entry is exactly 9 bus accesses (8 frame writes + the
        // vector read); only an unmapped access before those is the
        // instruction's own.
        let entry = if self.cpu.ipsr != 0 { 9 } else { 0 };
        let data_unmapped = self.bus.first_unmapped.is_some_and(|i| i + entry < self.bus.accesses);
        let ours = if data_unmapped {
            Class::Unmapped
        } else if self.cpu.ipsr != 0 || self.cpu.bkpt.is_some() {
            Class::Trap
        } else {
            Class::Ok
        };

        let pc = self.uc.reg_read(RegisterARM::PC).unwrap();
        let res = self.uc.emu_start(pc | 1, 0, 0, 1);
        // ARMv6-M: SP[1:0] are RAZ/WI. QEMU keeps them; mask so a stray bit
        // can't leak into later instructions of a chain.
        for r in [RegisterARM::MSP, RegisterARM::PSP] {
            let v = self.uc.reg_read(r).unwrap();
            if v & 3 != 0 {
                self.uc.reg_write(r, v & !3).unwrap();
            }
        }
        let (theirs, uc_err) = match res {
            Ok(()) => (Class::Ok, None),
            // Unicorn reports a faulting *next* fetch (branch to garbage) from
            // the same emu_start; the caller resolves that by comparing state.
            Err(e @ uc_error::FETCH_UNMAPPED) => (Class::Trap, Some(e)),
            // M-profile turns a bus error on a data access into an exception.
            Err(e @ (uc_error::READ_UNMAPPED | uc_error::WRITE_UNMAPPED)) => (Class::Unmapped, Some(e)),
            Err(e) => (Class::Trap, Some(e)),
        };
        let (uc_state, uc_mem) = (uc_get_state(&self.uc), self.uc_mem());
        if uc_err.is_some() {
            // After an exception Unicorn stays in handler mode with stale
            // exception state, which would poison every following case.
            self.uc = new_unicorn();
        }
        StepResult { uc_state, uc_mem, ours, theirs, uc_err }
    }

    fn uc_mem(&self) -> Vec<u8> {
        let mut b = vec![0; SIZE];
        self.uc.mem_read(BASE as u64, &mut b).unwrap();
        b
    }
}

pub fn rounds() -> u32 {
    std::env::var("DIFFTEST_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(4)
}

pub fn seed() -> u64 {
    std::env::var("DIFFTEST_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x5eed_0001)
}

/// Random memory with `code` placed at CODE.
pub fn random_mem(rng: &mut fastrand::Rng, code: &[u16]) -> Vec<u8> {
    let mut mem = vec![0u8; SIZE];
    rng.fill(&mut mem);
    for (i, hw) in code.iter().enumerate() {
        let off = (CODE - BASE) as usize + 2 * i;
        mem[off..off + 2].copy_from_slice(&hw.to_le_bytes());
    }
    mem
}

// ---- oracle: which differences are real, which are known QEMU deviations ----

pub fn describe(hw: u16, hw2: Option<u16>) -> String {
    match hw2 {
        Some(h2) => format!("{hw:04x} {h2:04x}"),
        None => format!("{hw:04x}"),
    }
}

pub fn is_wide(hw: u16) -> bool {
    hw >> 11 >= 0b11101
}

/// Encodings whose behaviour the ARM ARM leaves UNPREDICTABLE; not comparable.
pub fn unpredictable(hw: u16) -> bool {
    match hw {
        // BX / BLX: bits [2:0] are SBZ; BLX PC is UNPREDICTABLE
        0x4700..=0x47FF => hw & 7 != 0 || (hw & 0x80 != 0 && hw >> 3 & 0xF == 15),
        // PUSH / POP / STM / LDM with an empty register list
        0xB400..=0xB4FF | 0xBC00..=0xBCFF => hw & 0xFF == 0,
        0xC000..=0xCFFF => hw & 0xFF == 0,
        _ => false,
    }
}

/// Cases our core deliberately doesn't model, or where Unicorn can't be read
/// back faithfully.
pub fn not_comparable(hw: u16, hw2: Option<u16>, st: &State) -> bool {
    // MSR CONTROL with nPRIV (bit 0) set: we don't model privilege levels, and
    // Unicorn then returns 0 for MSP/PSP/PRIMASK reads (unprivileged access).
    if let Some(h2) = hw2 {
        // MSR / MRS with Rn/Rd = PC (UNPREDICTABLE)
        if (hw & 0xFFF0 == 0xF380 && hw & 0xF == 15 && h2 & 0xFF00 == 0x8800)
            || (hw == 0xF3EF && h2 & 0xF000 == 0x8000 && h2 >> 8 & 0xF == 15)
        {
            return true;
        }
        if hw & 0xFFF0 == 0xF380 && h2 == 0x8814 {
            let rn = (hw & 0xF) as usize;
            let sp = if st.spsel { st.psp } else { st.msp };
            let v = match rn { 0..=12 => st.r[rn], 13 => sp, _ => st.lr };
            return v & 1 != 0;
        }
    }
    false
}

/// True if `hw`(+`hw2`) is an encoding that ARMv7-M defines but ARMv6-M
/// leaves undefined. Our core (correctly) faults on these; QEMU's Cortex-M0
/// model executes them.
pub fn v7m_only(hw: u16, hw2: Option<u16>) -> bool {
    if let Some(h2) = hw2 {
        let valid = (hw >> 11 == 0b11110 && h2 & 0xD000 == 0xD000) // BL
            || (hw & 0xFFF0 == 0xF380 && h2 & 0xFF00 == 0x8800) // MSR
            || (hw == 0xF3EF && h2 & 0xF000 == 0x8000) // MRS
            || (hw == 0xF3BF && matches!(h2 & 0xFFF0, 0x8F40 | 0x8F50 | 0x8F60)); // barriers
        return !valid;
    }
    match hw {
        0xB100..=0xBBFF if hw & 0xF500 == 0xB100 => true, // CBZ / CBNZ
        0xB600..=0xB6FF => hw & 0xFFEF != 0xB662, // SETEND, CPS a/f
        0xBF00..=0xBFFF => hw & 0xF != 0, // IT
        _ => false,
    }
}

/// True if the (single, 16-bit) instruction `hw` performs a load/store whose
/// effective address is misaligned for its size in state `st`. ARMv6-M faults
/// on that; QEMU's Cortex-M0 model performs the access instead.
pub fn misaligned_access(hw: u16, st: &State) -> bool {
    let r = |n: u16| st.r[n as usize & 7];
    let h = hw as u32;
    let sp = if st.spsel { st.psp } else { st.msp };
    let (addr, align) = match hw >> 11 {
        0x0A | 0x0B => {
            let size = match hw >> 9 & 7 { 0 | 4 => 4, 1 | 5 | 7 => 2, _ => 1 };
            (r(hw >> 3).wrapping_add(r(hw >> 6)), size)
        }
        0x0C | 0x0D => (r(hw >> 3).wrapping_add((h >> 6 & 0x1F) * 4), 4),
        0x10 | 0x11 => (r(hw >> 3).wrapping_add((h >> 6 & 0x1F) * 2), 2),
        0x12 | 0x13 => (sp.wrapping_add((h & 0xFF) * 4), 4),
        0x18 | 0x19 => (r(hw >> 8), 4), // STM / LDM
        _ => return false,
    };
    addr % align != 0
}


#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Same behaviour.
    Match,
    /// Differs only in a documented way (ARMv6-M vs what QEMU's M0 does).
    Deviation(&'static str),
    Mismatch(String),
}

/// Decide whether one executed step agrees. `before` is our state before the
/// step, `(hw, hw2)` the instruction.
pub fn judge(res: &StepResult, pair: &Pair, hw: u16, hw2: Option<u16>, before: &State) -> Verdict {
    let ours_state = State::of_cpu(&pair.cpu);
    let same_after = ours_state == res.uc_state && pair.bus.mem == res.uc_mem;

    // Unicorn reports a faulting fetch of the *next* instruction (branch to
    // garbage) from the same emu_start; the instruction itself retired.
    if res.ours == Class::Ok && res.theirs == Class::Trap && same_after {
        return Verdict::Match;
    }
    // M-profile turns a bus error on a data access into an exception.
    if res.ours == Class::Unmapped
        && res.theirs == Class::Trap
        && res.uc_err == Some(unicorn_engine::uc_error::EXCEPTION)
    {
        return Verdict::Match;
    }
    if res.ours == Class::Trap && res.theirs != Class::Trap && v7m_only(hw, hw2) {
        return Verdict::Deviation("ARMv7-M-only encoding");
    }
    if res.ours == Class::Trap
        && matches!(res.theirs, Class::Ok | Class::Unmapped)
        && !is_wide(hw)
        && misaligned_access(hw, before)
    {
        return Verdict::Deviation("unaligned access");
    }
    if res.ours != res.theirs {
        return Verdict::Mismatch(format!("class {:?} vs {:?} ({:?})", res.ours, res.theirs, res.uc_err));
    }
    if res.ours == Class::Ok {
        let d = ours_state.diff(&res.uc_state);
        if !d.is_empty() {
            return Verdict::Mismatch(d.join("; "));
        }
        if pair.bus.mem != res.uc_mem {
            return Verdict::Mismatch("memory differs".into());
        }
    }
    Verdict::Match
}
