//! ARMv6-M (Cortex-M0+) interpreter.
//!
//! Layout: this file holds the core state and `step`; `alu` the arithmetic
//! helpers, `exception` entry/return, `thumb16` / `thumb32` instruction decode.
//!
//! One `Cpu` is one core. It owns no memory: every access goes through a
//! [`Bus`], so two cores can share a system bus. Interrupt *prioritisation*
//! (NVIC) lives outside; this module provides exception entry/return and the
//! hook points (`take_exception`, `sleeping`, `event`).

use crate::bus::Bus;

mod alu;
mod exception;
mod thumb16;
mod thumb32;

use alu::*;

/// Raised for anything that ends up as a HardFault (unaligned access,
/// undefined instruction, bad EXC_RETURN, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault;

pub const EXC_HARDFAULT: u8 = 3;
pub const EXC_SVCALL: u8 = 11;
pub const EXC_PENDSV: u8 = 14;
pub const EXC_SYSTICK: u8 = 15;
/// External interrupt `n` is exception number `16 + n`.
pub const EXC_IRQ_BASE: u8 = 16;

#[derive(Clone, Debug, Default)]
pub struct Cpu {
    /// r0-r12
    pub r: [u32; 13],
    pub lr: u32,
    /// Address of the instruction about to execute.
    pub pc: u32,
    pub msp: u32,
    pub psp: u32,
    pub n: bool,
    pub z: bool,
    pub c: bool,
    pub v: bool,
    /// Active exception number, 0 = thread mode.
    pub ipsr: u8,
    pub primask: bool,
    /// CONTROL.SPSEL: thread mode uses PSP.
    pub spsel: bool,
    pub vtor: u32,
    /// Set by WFI / WFE; the system clears it when an interrupt/event arrives.
    pub sleeping: bool,
    /// The sleep was started by WFE, so an event (not only an interrupt)
    /// ends it. The system clears this together with `sleeping`.
    pub wfe: bool,
    /// Event register (SEV sets it, WFE consumes it).
    pub event: bool,
    /// A SEV was executed. SEV signals every processor in the system, so the
    /// system forwards the event to the other cores and clears this.
    pub sev_sent: bool,
    /// Set by BKPT with its immediate; the runner should check and clear it.
    pub bkpt: Option<u8>,
    /// Cycles consumed since reset.
    pub cycles: u64,
}

impl Cpu {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start executing at `pc` with the given initial main stack pointer.
    pub fn reset_to(&mut self, sp: u32, pc: u32) {
        *self = Self {
            msp: sp & !3,
            pc: pc & !1,
            vtor: self.vtor,
            ..Self::default()
        };
    }

    /// Reset using the vector table at `vtor` (initial SP, reset handler).
    pub fn reset_from_vector_table<B: Bus + ?Sized>(&mut self, bus: &mut B, vtor: u32) {
        let sp = bus.read32(vtor);
        let pc = bus.read32(vtor + 4);
        self.vtor = vtor;
        self.reset_to(sp, pc);
    }

    pub fn thread_mode(&self) -> bool {
        self.ipsr == 0
    }
    fn use_psp(&self) -> bool {
        self.ipsr == 0 && self.spsel
    }
    pub fn sp(&self) -> u32 {
        if self.use_psp() { self.psp } else { self.msp }
    }
    pub fn set_sp(&mut self, v: u32) {
        if self.use_psp() {
            self.psp = v & !3;
        } else {
            self.msp = v & !3;
        }
    }

    /// Register read as seen by an instruction operand (PC reads as pc+4).
    pub fn reg(&self, n: usize) -> u32 {
        match n {
            0..=12 => self.r[n],
            13 => self.sp(),
            14 => self.lr,
            _ => self.pc.wrapping_add(4),
        }
    }

    /// Write r0-r12, sp or lr (pc writes are branches and handled by callers).
    fn write_reg(&mut self, n: usize, v: u32) {
        match n {
            0..=12 => self.r[n] = v,
            13 => self.set_sp(v),
            14 => self.lr = v,
            _ => unreachable!("pc writes are handled by the caller"),
        }
    }

    fn flags(&self) -> u32 {
        (self.n as u32) << 31
            | (self.z as u32) << 30
            | (self.c as u32) << 29
            | (self.v as u32) << 28
    }
    fn set_flags(&mut self, x: u32) {
        self.n = x >> 31 & 1 != 0;
        self.z = x >> 30 & 1 != 0;
        self.c = x >> 29 & 1 != 0;
        self.v = x >> 28 & 1 != 0;
    }
    pub fn xpsr(&self) -> u32 {
        self.flags() | 1 << 24 | self.ipsr as u32
    }

    fn set_nz(&mut self, res: u32) {
        self.n = res >> 31 != 0;
        self.z = res == 0;
    }
    fn set_nzc(&mut self, res: u32, c: bool) {
        self.set_nz(res);
        self.c = c;
    }
    fn set_nzcv(&mut self, res: u32, c: bool, v: bool) {
        self.set_nz(res);
        self.c = c;
        self.v = v;
    }

    fn cond(&self, cond: u32) -> bool {
        match cond {
            0 => self.z,
            1 => !self.z,
            2 => self.c,
            3 => !self.c,
            4 => self.n,
            5 => !self.n,
            6 => self.v,
            7 => !self.v,
            8 => self.c && !self.z,
            9 => !self.c || self.z,
            10 => self.n == self.v,
            11 => self.n != self.v,
            12 => !self.z && self.n == self.v,
            13 => self.z || self.n != self.v,
            _ => true,
        }
    }

    // ---- memory access with alignment checks -----------------------------

    fn ld32<B: Bus + ?Sized>(&self, bus: &mut B, a: u32) -> Result<u32, Fault> {
        if a & 3 != 0 {
            Err(Fault)
        } else {
            Ok(bus.read32(a))
        }
    }
    fn ld16<B: Bus + ?Sized>(&self, bus: &mut B, a: u32) -> Result<u16, Fault> {
        if a & 1 != 0 {
            Err(Fault)
        } else {
            Ok(bus.read16(a))
        }
    }
    fn st32<B: Bus + ?Sized>(&self, bus: &mut B, a: u32, v: u32) -> Result<(), Fault> {
        if a & 3 != 0 {
            return Err(Fault);
        }
        bus.write32(a, v);
        Ok(())
    }
    fn st16<B: Bus + ?Sized>(&self, bus: &mut B, a: u32, v: u16) -> Result<(), Fault> {
        if a & 1 != 0 {
            return Err(Fault);
        }
        bus.write16(a, v);
        Ok(())
    }

    // ---- execution -------------------------------------------------------

    /// Execute one instruction (or idle one cycle while asleep).
    /// Returns the number of cycles consumed.
    pub fn step<B: Bus + ?Sized>(&mut self, bus: &mut B) -> u32 {
        if self.sleeping {
            self.cycles += 1;
            return 1;
        }
        let cycles = match self.exec16(bus) {
            Ok(c) => c,
            Err(Fault) => {
                self.take_exception(bus, EXC_HARDFAULT);
                15
            }
        };
        self.cycles += cycles as u64;
        cycles
    }
}

#[cfg(test)]
mod tests;
