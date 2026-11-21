//! Threaded dispatch for [`Cpu::run_threaded`]: every instruction's handler
//! ends by tail-calling (`become`) the next instruction's, from a runner of
//! its own per slot of the opcode table. The host's branch predictor then
//! learns each jump from where it is (after a compare, the branch usually
//! follows) instead of guessing all of them at one shared dispatch site.
//!
//! WebAssembly has tail calls with the tail-call proposal (all current
//! browsers; the repository's .cargo/config.toml turns it on). A wasm build
//! without it runs the same thing as a loop (`threaded_loop.rs`).
use super::{Cpu, thumb16::Table};
use crate::bus::Bus;

/// The threaded runner of each slot of [`Table::H`] (see
/// [`Cpu::run_threaded`]): one instance per slot, so that each has its own
/// jump to the next instruction.
type Runner<B> = fn(&mut Cpu, &mut B, u32, u32);

macro_rules! slot {
    ($k:expr) => {
        run_slot::<B, { $k }> as Runner<B>
    };
}
macro_rules! slots4 {
    ($b:expr) => {
        [
            slot!($b * 4),
            slot!($b * 4 + 1),
            slot!($b * 4 + 2),
            slot!($b * 4 + 3),
        ]
    };
}
macro_rules! slots16 {
    ($b:expr) => {
        [
            slots4!($b * 4),
            slots4!($b * 4 + 1),
            slots4!($b * 4 + 2),
            slots4!($b * 4 + 3),
        ]
    };
}
macro_rules! slots64 {
    ($b:expr) => {
        [
            slots16!($b * 4),
            slots16!($b * 4 + 1),
            slots16!($b * 4 + 2),
            slots16!($b * 4 + 3),
        ]
    };
}
macro_rules! slots256 {
    ($b:expr) => {
        [
            slots64!($b * 4),
            slots64!($b * 4 + 1),
            slots64!($b * 4 + 2),
            slots64!($b * 4 + 3),
        ]
    };
}

impl<B: Bus + ?Sized> Table<B> {
    const RUN: [Runner<B>; 1024] = {
        let nested = [slots256!(0), slots256!(1), slots256!(2), slots256!(3)];
        let mut t = [nested[0][0][0][0][0]; 1024];
        let mut i = 0;
        while i < 1024 {
            t[i] = nested[i >> 8][i >> 6 & 3][i >> 4 & 3][i >> 2 & 3][i & 3];
            i += 1;
        }
        t
    };
}

/// Run the instruction `hw` at `pc` with slot `K`'s handler, then go on to
/// the next one in the same way unless the run ends.
fn run_slot<B: Bus + ?Sized, const K: usize>(cpu: &mut Cpu, bus: &mut B, hw: u32, pc: u32) {
    let r = (const { Table::<B>::H[K] })(cpu, bus, hw, pc);
    cpu.retire(bus, r);
    if cpu.run_ends(bus) {
        return;
    }
    bus.instruction_start(cpu.cycles);
    let pc = cpu.pc;
    let hw = bus.fetch16(pc) as u32;
    become Table::<B>::RUN[(hw >> 6) as usize](cpu, bus, hw, pc)
}

/// Start a threaded run with the fetched instruction `hw` at `pc`.
pub(super) fn run<B: Bus + ?Sized>(cpu: &mut Cpu, bus: &mut B, hw: u32, pc: u32) {
    Table::<B>::RUN[(hw >> 6) as usize](cpu, bus, hw, pc)
}
