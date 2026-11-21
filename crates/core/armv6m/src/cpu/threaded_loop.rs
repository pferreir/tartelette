//! [`Cpu::run_threaded`] as a plain loop, for WebAssembly built without the
//! tail-call proposal; otherwise `threaded.rs` runs it with threaded dispatch.
use super::{Cpu, thumb16::Table};
use crate::bus::Bus;

/// The same run as a loop, for targets without tail calls.
pub(super) fn run<B: Bus + ?Sized>(cpu: &mut Cpu, bus: &mut B, mut hw: u32, mut pc: u32) {
    loop {
        let r = Table::<B>::H[(hw >> 6) as usize](cpu, bus, hw, pc);
        cpu.retire(bus, r);
        if cpu.run_ends(bus) {
            return;
        }
        bus.instruction_start(cpu.cycles);
        pc = cpu.pc;
        hw = bus.fetch16(pc) as u32;
    }
}
