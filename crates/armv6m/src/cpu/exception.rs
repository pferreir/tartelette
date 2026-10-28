//! Exception entry / return and interworking branches.

use super::*;

impl Cpu {
    /// Push the exception frame and jump to the handler for exception `exc`.
    /// The return address is the current `pc`, so callers wanting "return to
    /// next instruction" semantics must advance `pc` first.
    pub fn take_exception<B: Bus + ?Sized>(&mut self, bus: &mut B, exc: u8) {
        let sp = self.sp();
        let frame = sp.wrapping_sub(32) & !4;
        let xpsr = self.xpsr() | ((sp >> 2) & 1) << 9;
        let words = [
            self.r[0], self.r[1], self.r[2], self.r[3], self.r[12], self.lr, self.pc, xpsr,
        ];
        for (i, w) in words.iter().enumerate() {
            bus.write32(frame + 4 * i as u32, *w);
        }
        self.lr = if self.ipsr != 0 {
            0xFFFF_FFF1
        } else if self.spsel {
            0xFFFF_FFFD
        } else {
            0xFFFF_FFF9
        };
        self.set_sp(frame);
        self.ipsr = exc;
        self.pc = bus.read32(self.vtor.wrapping_add(4 * exc as u32)) & !1;
        self.sleeping = false;
        // Exception entry sets the event register, so that a WFE just after
        // an interrupt handled work for the thread doesn't sleep through it.
        self.event = true;
        self.run.resync = true;
        self.cycles += 15;
    }

    fn exception_return<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        exc_return: u32,
    ) -> Result<u32, Fault> {
        let (use_psp, to_handler) = match exc_return & 0xF {
            0x1 => (false, true),
            0x9 => (false, false),
            0xD => (true, false),
            _ => return Err(Fault),
        };
        let frame = if use_psp { self.psp } else { self.msp };
        let mut w = [0u32; 8];
        for (i, slot) in w.iter_mut().enumerate() {
            *slot = bus.read32(frame + 4 * i as u32);
        }
        self.r[0..4].copy_from_slice(&w[0..4]);
        self.r[12] = w[4];
        self.lr = w[5];
        let xpsr = w[7];
        self.set_flags(xpsr);
        self.ipsr = if to_handler { (xpsr & 0x3F) as u8 } else { 0 };
        self.run.resync = true;
        if !to_handler {
            self.spsel = use_psp;
        }
        let new_sp = (frame + 32) | ((xpsr >> 9) & 1) << 2;
        if use_psp {
            self.psp = new_sp
        } else {
            self.msp = new_sp
        }
        self.cycles += 12;
        Ok(w[6] & !1)
    }

    /// Interworking branch (BX, POP {pc}, LDR pc, ...). Handles EXC_RETURN.
    /// Returns the new pc.
    pub(super) fn bx_write<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        addr: u32,
    ) -> Result<u32, Fault> {
        if addr >= 0xFFFF_FFF0 && self.ipsr != 0 {
            self.exception_return(bus, addr)
        } else if addr & 1 == 0 {
            Err(Fault) // ARM state not supported
        } else {
            Ok(addr & !1)
        }
    }
}
