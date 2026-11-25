//! USBCTRL in device mode: the registers at `0x5011_0000`, the 4 KiB
//! dual-port RAM at `0x5010_0000` and USBCTRL_IRQ (NVIC line 5), after the
//! RP2040 datasheet, section 4.1.
//!
//! The controller is modelled at the level it works at: whole transactions.
//! The host side ([`usbhost::Host`], through [`usbhost::DevicePort`]) sends
//! a SETUP, IN or OUT token; the controller answers it from the endpoint's
//! buffer control word in DPRAM the way the hardware does (AVAILABLE, FULL,
//! PID, LENGTH, STALL, double buffering with its buffer selector), then sets
//! BUFF_STATUS and the SIE_STATUS bits that raise interrupts. Electrical
//! detail (line states, PHY overrides, suspend/resume) is not modelled; the
//! bus is never suspended. Host mode is not modelled.
//!
//! DPRAM is plain memory to the CPU, with byte access like SRAM (drivers
//! `memcpy` into it). A write below the data buffers (endpoint and buffer
//! control words) sets [`Usb::kicked`], which tells the host to try the
//! endpoints that NAKed again.
use alloc::{vec, vec::Vec};

use super::{Peripheral, rebase};
use crate::system::CYCLES_PER_US;

pub const DPRAM_SIZE: usize = 0x1000;
/// Below this, DPRAM holds the SETUP packet and the control words.
pub const DPRAM_CONTROL_END: u32 = 0x100;
/// Where endpoint 0's buffer (shared by IN and OUT) lives.
const EP0_BUFFER: usize = 0x100;
#[cfg(test)]
const CYCLES_PER_FRAME: u64 = 1000 * CYCLES_PER_US;

// Register offsets.
const ADDR_ENDP: u32 = 0x00;
const MAIN_CTRL: u32 = 0x40;
/// Reading it clears the DEV_SOF interrupt.
pub const SOF_RD: u32 = 0x48;
const SIE_CTRL: u32 = 0x4C;
const SIE_STATUS: u32 = 0x50;
const BUFF_STATUS: u32 = 0x58;
const BUFF_CPU_SHOULD_HANDLE: u32 = 0x5C;
const EP_ABORT: u32 = 0x60;
const EP_ABORT_DONE: u32 = 0x64;
const EP_STALL_ARM: u32 = 0x68;
const EP_STATUS_STALL_NAK: u32 = 0x70;
const USB_PWR: u32 = 0x78;
const INTR: u32 = 0x8C;
const INTE: u32 = 0x90;
const INTF: u32 = 0x94;
const INTS: u32 = 0x98;
/// Registers past the named ones (host-mode endpoint addresses, PHY
/// controls, ...) are stored and read back.
const REGS: usize = 0x9C / 4;

const MAIN_CTRL_CONTROLLER_EN: u32 = 1 << 0;
const MAIN_CTRL_HOST_NDEVICE: u32 = 1 << 1;

const SIE_CTRL_PULLUP_EN: u32 = 1 << 16;
const SIE_CTRL_EP0_INT_NAK: u32 = 1 << 27;
const SIE_CTRL_EP0_INT_2BUF: u32 = 1 << 28;
const SIE_CTRL_EP0_INT_1BUF: u32 = 1 << 29;
const SIE_CTRL_EP0_DOUBLE_BUF: u32 = 1 << 30;
const SIE_CTRL_EP0_INT_STALL: u32 = 1 << 31;

const ST_VBUS_DETECTED: u32 = 1 << 0;
/// LINE_STATE = J (D+ high), the idle state of a full-speed bus.
const ST_LINE_STATE_J: u32 = 1 << 2;
const ST_SUSPENDED: u32 = 1 << 4;
const ST_RESUME: u32 = 1 << 11;
const ST_CONNECTED: u32 = 1 << 16;
const ST_SETUP_REC: u32 = 1 << 17;
const ST_TRANS_COMPLETE: u32 = 1 << 18;
const ST_BUS_RESET: u32 = 1 << 19;
const ST_RX_OVERFLOW: u32 = 1 << 26;
const ST_DATA_SEQ_ERROR: u32 = 1 << 31;
/// Write-one-to-clear bits of SIE_STATUS (the rest are read-only).
const ST_WC: u32 = 0xFF0F_0810 | ST_SUSPENDED | ST_RESUME;

// INTR bits.
const INT_TRANS_COMPLETE: u32 = 1 << 3;
const INT_BUFF_STATUS: u32 = 1 << 4;
const INT_ERROR_DATA_SEQ: u32 = 1 << 5;
const INT_ERROR_RX_OVERFLOW: u32 = 1 << 7;
const INT_VBUS_DETECT: u32 = 1 << 11;
const INT_BUS_RESET: u32 = 1 << 12;
const INT_DEV_CONN_DIS: u32 = 1 << 13;
const INT_DEV_SUSPEND: u32 = 1 << 14;
const INT_DEV_RESUME_FROM_HOST: u32 = 1 << 15;
const INT_SETUP_REQ: u32 = 1 << 16;
const INT_DEV_SOF: u32 = 1 << 17;
const INT_ABORT_DONE: u32 = 1 << 18;
const INT_EP_STALL_NAK: u32 = 1 << 19;

const USB_PWR_VBUS_DETECT: u32 = 1 << 2;
const USB_PWR_VBUS_DETECT_OVERRIDE_EN: u32 = 1 << 3;

// Endpoint control word.
const EP_ENABLE: u32 = 1 << 31;
const EP_DOUBLE_BUFFERED: u32 = 1 << 30;
const EP_INT_PER_BUFF: u32 = 1 << 29;
const EP_INT_PER_DOUBLE_BUFF: u32 = 1 << 28;
const EP_INT_ON_STALL: u32 = 1 << 17;
const EP_INT_ON_NAK: u32 = 1 << 16;

// One half of a buffer control word (the other half is the same, << 16).
const BUF_FULL: u32 = 1 << 15;
const BUF_LAST: u32 = 1 << 14;
const BUF_PID1: u32 = 1 << 13;
const BUF_AVAILABLE: u32 = 1 << 10;
const BUF_LEN: u32 = 0x3FF;
/// Low half only: reset the buffer selector to buffer 0, and stall.
const BUF_RESET: u32 = 1 << 12;
const BUF_STALL: u32 = 1 << 11;

pub struct Usb {
    pub dpram: Vec<u8>,
    regs: [u32; REGS],
    /// SIE_STATUS bits that latch (everything but the live ones).
    status: u32,
    buff_status: u32,
    /// Which half of each double-buffered endpoint is next, bit = BUFF_STATUS bit.
    selector: u32,
    should_handle: u32,
    abort_done: u32,
    stall_nak: u32,
    /// DEV_CONN_DIS and VBUS_DETECT, cleared through SIE_STATUS.
    conn_changed: bool,
    vbus_changed: bool,
    /// The host has reset the bus since the device last detached.
    connected: bool,
    /// Cycle of frame 0 (the end of the last reset), and the frame SOF_RD
    /// showed last (DEV_SOF is pending until it is read).
    frame_zero: Option<u64>,
    /// Frames counted before `frame_zero` (across a clk_sys change), and the
    /// cycles a 1 ms frame takes.
    frames_before: u64,
    cycles_per_frame: u64,
    frame_read: u64,
    now: u64,
    /// Held in reset by RESETS: invisible, and the pull-up is off.
    held: bool,
    /// The firmware touched a control word or register since the host last
    /// looked: endpoints that NAKed may be ready now.
    pub kicked: bool,
}

impl Usb {
    pub fn new() -> Self {
        Self {
            dpram: vec![0; DPRAM_SIZE],
            regs: [0; REGS],
            status: 0,
            buff_status: 0,
            selector: 0,
            should_handle: 0,
            abort_done: 0,
            stall_nak: 0,
            conn_changed: false,
            vbus_changed: false,
            connected: false,
            frame_zero: None,
            frames_before: 0,
            cycles_per_frame: 1000 * CYCLES_PER_US,
            frame_read: 0,
            now: 0,
            held: true,
            kicked: false,
        }
    }

    fn reg(&self, off: u32) -> u32 {
        self.regs[(off / 4) as usize]
    }

    /// RESETS holds the block (true) or lets it go. Going into reset puts
    /// the registers back to their reset values; DPRAM keeps its contents.
    pub fn set_held(&mut self, held: bool) {
        if held && !self.held {
            let (dpram, now) = (core::mem::take(&mut self.dpram), self.now);
            *self = Self::new();
            self.dpram = dpram;
            self.now = now;
            self.kicked = true;
        }
        self.held = held;
    }

    pub fn held(&self) -> bool {
        self.held
    }

    /// Let time pass (for the frame counter and SOF interrupts).
    pub fn update(&mut self, now: u64) {
        self.now = now;
    }

    fn frame(&self) -> u64 {
        self.frame_zero.filter(|_| self.pulled_up()).map_or(0, |t| {
            self.frames_before + self.now.saturating_sub(t) / self.cycles_per_frame
        })
    }

    /// The next cycle a SOF interrupt can fire, if one is enabled.
    pub fn next_event(&self) -> Option<u64> {
        if self.reg(INTE) & INT_DEV_SOF == 0 || !self.pulled_up() {
            return None;
        }
        let zero = self.frame_zero?;
        let into = self.frame() + 1 - self.frames_before;
        Some(zero + into * self.cycles_per_frame)
    }

    /// clk_sys changed to `hz` at `now`.
    pub fn set_hz(&mut self, now: u64, hz: u64) {
        self.now = now;
        let new = hz / 1000;
        if let Some(zero) = self.frame_zero {
            let (frames, base) = rebase(now, zero, self.cycles_per_frame, new, 1);
            (self.frames_before, self.frame_zero) = (self.frames_before + frames, Some(base));
        }
        self.cycles_per_frame = new;
    }

    fn pulled_up(&self) -> bool {
        !self.held
            && self.reg(MAIN_CTRL) & (MAIN_CTRL_CONTROLLER_EN | MAIN_CTRL_HOST_NDEVICE)
                == MAIN_CTRL_CONTROLLER_EN
            && self.reg(SIE_CTRL) & SIE_CTRL_PULLUP_EN != 0
    }

    fn vbus(&self) -> bool {
        let pwr = self.reg(USB_PWR);
        if pwr & USB_PWR_VBUS_DETECT_OVERRIDE_EN != 0 {
            pwr & USB_PWR_VBUS_DETECT != 0
        } else {
            // The cable is always plugged in.
            true
        }
    }

    fn sie_status(&self) -> u32 {
        let mut s = self.status;
        if self.vbus() {
            s |= ST_VBUS_DETECTED;
        }
        if self.connected {
            s |= ST_CONNECTED | ST_LINE_STATE_J;
        }
        s
    }

    /// INTR: the raw interrupt sources.
    fn intr(&self) -> u32 {
        let s = self.status;
        let mut r = 0;
        let mut src = |cond: bool, bit: u32| {
            if cond {
                r |= bit;
            }
        };
        src(s & ST_TRANS_COMPLETE != 0, INT_TRANS_COMPLETE);
        src(self.buff_status != 0, INT_BUFF_STATUS);
        src(s & ST_DATA_SEQ_ERROR != 0, INT_ERROR_DATA_SEQ);
        src(s & ST_RX_OVERFLOW != 0, INT_ERROR_RX_OVERFLOW);
        src(self.vbus_changed, INT_VBUS_DETECT);
        src(s & ST_BUS_RESET != 0, INT_BUS_RESET);
        src(self.conn_changed, INT_DEV_CONN_DIS);
        src(s & ST_SUSPENDED != 0, INT_DEV_SUSPEND);
        src(s & ST_RESUME != 0, INT_DEV_RESUME_FROM_HOST);
        src(s & ST_SETUP_REC != 0, INT_SETUP_REQ);
        src(
            self.frame_zero.is_some() && self.frame() != self.frame_read,
            INT_DEV_SOF,
        );
        src(self.abort_done != 0, INT_ABORT_DONE);
        src(self.stall_nak != 0, INT_EP_STALL_NAK);
        r
    }

    fn ints(&self) -> u32 {
        (self.intr() | self.reg(INTF)) & self.reg(INTE)
    }

    /// USBCTRL_IRQ.
    pub fn irq(&self) -> bool {
        // Firmware without USB never enables a source: skip working them out.
        !self.held && self.reg(INTE) != 0 && self.ints() != 0
    }

    fn word(&self, off: usize) -> u32 {
        u32::from_le_bytes(self.dpram[off..off + 4].try_into().unwrap())
    }

    fn set_word(&mut self, off: usize, v: u32) {
        self.dpram[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// The CPU wrote DPRAM at `off`: a write to a buffer control word
    /// with RESET set points the endpoint's selector back at buffer 0.
    pub fn dpram_written(&mut self, off: u32) {
        if off >= DPRAM_CONTROL_END {
            return;
        }
        self.kicked = true;
        if off >= 0x80 {
            let word = (off & !3) as usize;
            if self.word(word) & BUF_RESET != 0 {
                self.selector &= !(1 << ((word - 0x80) / 4));
            }
        }
    }

    /// Endpoint `ep`'s setup in one direction.
    fn endpoint(&self, ep: u8, out: bool) -> Endpoint {
        let bit = 2 * ep as u32 + out as u32;
        let ctrl_word = 0x80 + 4 * bit as usize;
        if ep == 0 {
            let sie = self.reg(SIE_CTRL);
            return Endpoint {
                bit,
                ctrl_word,
                buffer: EP0_BUFFER,
                enabled: true,
                double: sie & SIE_CTRL_EP0_DOUBLE_BUF != 0,
                per_buff: sie & SIE_CTRL_EP0_INT_1BUF != 0,
                per_double: sie & SIE_CTRL_EP0_INT_2BUF != 0,
                on_nak: sie & SIE_CTRL_EP0_INT_NAK != 0,
                on_stall: sie & SIE_CTRL_EP0_INT_STALL != 0,
                armed_stall: self.reg(EP_STALL_ARM) >> out as u32 & 1 != 0,
            };
        }
        let ctrl = self.word(8 * ep as usize + 4 * out as usize);
        Endpoint {
            bit,
            ctrl_word,
            buffer: (ctrl & 0xFFC0) as usize,
            enabled: ctrl & EP_ENABLE != 0,
            double: ctrl & EP_DOUBLE_BUFFERED != 0,
            per_buff: ctrl & EP_INT_PER_BUFF != 0,
            per_double: ctrl & EP_INT_PER_DOUBLE_BUFF != 0,
            on_nak: ctrl & EP_INT_ON_NAK != 0,
            on_stall: ctrl & EP_INT_ON_STALL != 0,
            armed_stall: true,
        }
    }

    /// Find the buffer for a data packet on `ep`: the selected half if it is
    /// AVAILABLE, else the handshake the host gets.
    fn claim(&mut self, ep: &Endpoint) -> Result<(usize, u32), usbhost::Handshake> {
        use usbhost::Handshake;
        let ctrl = self.word(ep.ctrl_word);
        if ctrl & BUF_STALL != 0 && ep.armed_stall {
            if ep.on_stall {
                self.stall_nak |= 1 << ep.bit;
            }
            return Err(Handshake::Stall);
        }
        let half = if ep.double {
            (self.selector >> ep.bit & 1) as usize
        } else {
            0
        };
        let h = ctrl >> (16 * half) & 0xFFFF;
        if h & BUF_AVAILABLE == 0 {
            if ep.on_nak {
                self.stall_nak |= 1 << ep.bit;
            }
            return Err(Handshake::Nak);
        }
        Ok((half, h))
    }

    /// A buffer is done: give it back to the CPU with `h` as its new
    /// control half, and raise what the endpoint asks for.
    fn complete(&mut self, ep: &Endpoint, half: usize, h: u32) {
        let mask = 0xFFFF << (16 * half);
        let ctrl = self.word(ep.ctrl_word) & !mask | h << (16 * half);
        self.set_word(ep.ctrl_word, ctrl);
        let bit = 1 << ep.bit;
        if ep.double {
            self.selector ^= bit;
        }
        let raise = if ep.double {
            ep.per_buff || (ep.per_double && half == 1)
        } else {
            ep.per_buff || ep.per_double
        };
        if raise {
            self.buff_status |= bit;
            if half == 1 {
                self.should_handle |= bit;
            } else {
                self.should_handle &= !bit;
            }
        }
        if h & BUF_LAST != 0 {
            self.status |= ST_TRANS_COMPLETE;
        }
    }

    fn addressed(&self, address: u8) -> bool {
        self.pulled_up() && self.connected && address == (self.reg(ADDR_ENDP) & 0x7F) as u8
    }
}

/// What the controller needs to know about one direction of an endpoint.
struct Endpoint {
    /// Its bit in BUFF_STATUS and friends.
    bit: u32,
    ctrl_word: usize,
    buffer: usize,
    enabled: bool,
    double: bool,
    per_buff: bool,
    per_double: bool,
    on_nak: bool,
    on_stall: bool,
    /// Endpoint 0 only stalls while EP_STALL_ARM allows it.
    armed_stall: bool,
}

impl Default for Usb {
    fn default() -> Self {
        Self::new()
    }
}

impl usbhost::DevicePort for Usb {
    fn attached(&self) -> bool {
        self.pulled_up()
    }

    fn bus_reset(&mut self) {
        self.status |= ST_BUS_RESET;
        if !self.connected {
            self.connected = true;
            self.conn_changed = true;
        }
        self.frame_zero = Some(self.now);
        self.frames_before = 0;
        self.frame_read = 0;
    }

    fn setup(&mut self, address: u8, packet: [u8; 8]) -> Result<(), usbhost::Handshake> {
        if !self.addressed(address) {
            return Err(usbhost::Handshake::Timeout);
        }
        // A SETUP is always taken, whatever endpoint 0's buffers say.
        self.dpram[..8].copy_from_slice(&packet);
        self.status |= ST_SETUP_REC;
        self.regs[(EP_STALL_ARM / 4) as usize] = 0;
        self.selector &= !3;
        Ok(())
    }

    fn token_in(
        &mut self,
        address: u8,
        ep: u8,
        max: u16,
    ) -> Result<(bool, Vec<u8>), usbhost::Handshake> {
        let e = self.endpoint(ep, false);
        if !self.addressed(address) || !e.enabled {
            return Err(usbhost::Handshake::Timeout);
        }
        let (half, h) = self.claim(&e)?;
        let len = ((h & BUF_LEN) as usize).min(max as usize);
        let at = e.buffer + 64 * half;
        let data = self.dpram[at..(at + len).min(DPRAM_SIZE)].to_vec();
        self.complete(&e, half, h & !(BUF_AVAILABLE | BUF_FULL));
        Ok((h & BUF_PID1 != 0, data))
    }

    fn token_out(
        &mut self,
        address: u8,
        ep: u8,
        data1: bool,
        data: &[u8],
    ) -> Result<(), usbhost::Handshake> {
        let e = self.endpoint(ep, true);
        if !self.addressed(address) || !e.enabled {
            return Err(usbhost::Handshake::Timeout);
        }
        let (half, h) = self.claim(&e)?;
        // The buffer's PID is the one expected; a mismatch is flagged (the
        // data is still taken, as some drivers don't keep OUT PIDs).
        if (h & BUF_PID1 != 0) != data1 {
            self.status |= ST_DATA_SEQ_ERROR;
        }
        let room = (h & BUF_LEN) as usize;
        if data.len() > room {
            self.status |= ST_RX_OVERFLOW;
        }
        let len = data.len().min(room);
        let at = e.buffer + 64 * half;
        let end = (at + len).min(DPRAM_SIZE);
        self.dpram[at..end].copy_from_slice(&data[..end - at]);
        let pid = if data1 { BUF_PID1 } else { 0 };
        let h = h & !(BUF_AVAILABLE | BUF_LEN | BUF_PID1) | BUF_FULL | pid | len as u32;
        self.complete(&e, half, h);
        Ok(())
    }
}

impl Peripheral for Usb {
    fn read(&mut self, off: u32) -> u32 {
        match off {
            SOF_RD => {
                self.frame_read = self.frame();
                self.frame_read as u32 & 0x7FF
            }
            SIE_STATUS => self.sie_status(),
            BUFF_STATUS => self.buff_status,
            BUFF_CPU_SHOULD_HANDLE => self.should_handle,
            EP_ABORT_DONE => self.abort_done,
            EP_STATUS_STALL_NAK => self.stall_nak,
            INTR => self.intr(),
            INTS => self.ints(),
            o if (o as usize) < REGS * 4 => self.reg(o),
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, val: u32) {
        self.kicked = true;
        match off {
            SIE_STATUS => {
                self.status &= !(val & ST_WC);
                if val & ST_CONNECTED != 0 {
                    self.conn_changed = false;
                }
                if val & ST_VBUS_DETECTED != 0 {
                    self.vbus_changed = false;
                }
            }
            BUFF_STATUS => {
                self.buff_status &= !val;
                self.should_handle &= !val;
            }
            EP_ABORT => {
                // Nothing is ever in flight between transactions: an abort
                // is done at once.
                self.regs[(EP_ABORT / 4) as usize] = val;
                self.abort_done |= val;
            }
            EP_ABORT_DONE => self.abort_done &= !val,
            EP_STATUS_STALL_NAK => self.stall_nak &= !val,
            SOF_RD | BUFF_CPU_SHOULD_HANDLE | INTR | INTS => {}
            USB_PWR => {
                let before = self.vbus();
                self.regs[(USB_PWR / 4) as usize] = val;
                if self.vbus() != before {
                    self.vbus_changed = true;
                }
            }
            o if (o as usize) < REGS * 4 => {
                let was = self.pulled_up();
                self.regs[(o / 4) as usize] = val;
                if was && !self.pulled_up() {
                    // The host sees the device go: next time it starts over.
                    self.connected = false;
                    self.conn_changed = true;
                    self.frame_zero = None;
                }
            }
            _ => {}
        }
    }
}

impl Usb {
    /// Is `off` a write-one-to-clear register? The atomic set/clear/xor
    /// aliases clear the bits they name in those, whatever the alias.
    pub fn write_clears(off: u32) -> bool {
        matches!(
            off,
            SIE_STATUS | BUFF_STATUS | EP_ABORT_DONE | EP_STATUS_STALL_NAK
        )
    }
}

#[cfg(test)]
mod tests {
    use usbhost::{DevicePort, Handshake};

    use super::*;

    /// A controller as drivers leave it after init: enabled, pulled up,
    /// endpoint 0 interrupting per buffer, address 0, bus reset seen.
    fn ready() -> Usb {
        let mut u = Usb::new();
        u.set_held(false);
        u.write(
            USB_PWR,
            USB_PWR_VBUS_DETECT | USB_PWR_VBUS_DETECT_OVERRIDE_EN,
        );
        u.write(MAIN_CTRL, MAIN_CTRL_CONTROLLER_EN);
        u.write(INTE, INT_BUFF_STATUS | INT_BUS_RESET | INT_SETUP_REQ);
        u.write(SIE_CTRL, SIE_CTRL_EP0_INT_1BUF | SIE_CTRL_PULLUP_EN);
        assert!(u.attached());
        u.bus_reset();
        assert!(u.irq());
        u.write(SIE_STATUS, ST_BUS_RESET);
        assert!(!u.irq());
        u
    }

    fn ctrl(u: &Usb, off: usize) -> u32 {
        u.word(off)
    }

    #[test]
    fn nothing_answers_until_pulled_up_and_reset() {
        let mut u = Usb::new();
        assert!(!u.attached());
        u.set_held(false);
        u.write(MAIN_CTRL, MAIN_CTRL_CONTROLLER_EN);
        assert!(!u.attached());
        u.write(SIE_CTRL, SIE_CTRL_PULLUP_EN);
        assert!(u.attached());
        assert_eq!(u.setup(0, [0; 8]), Err(Handshake::Timeout));
        u.bus_reset();
        assert_eq!(
            u.read(SIE_STATUS) & (ST_CONNECTED | ST_BUS_RESET),
            ST_CONNECTED | ST_BUS_RESET
        );
        assert_eq!(u.setup(0, [0; 8]), Ok(()));
        // Going into reset drops the pull-up and the registers.
        u.set_held(true);
        assert!(!u.attached());
        u.set_held(false);
        assert_eq!(u.read(MAIN_CTRL), 0);
    }

    #[test]
    fn setup_lands_in_dpram_and_raises_setup_req() {
        let mut u = ready();
        u.write(EP_STALL_ARM, 3);
        u.setup(0, [0x80, 6, 0, 1, 0, 0, 18, 0]).unwrap();
        assert_eq!(&u.dpram[..8], &[0x80, 6, 0, 1, 0, 0, 18, 0]);
        assert_eq!(u.read(INTS), INT_SETUP_REQ);
        assert_eq!(u.read(EP_STALL_ARM), 0, "a SETUP disarms the EP0 stalls");
        u.write(SIE_STATUS, ST_SETUP_REC);
        assert!(!u.irq());
    }

    #[test]
    fn ep0_in_sends_the_armed_buffer_once() {
        let mut u = ready();
        assert_eq!(u.token_in(0, 0, 64), Err(Handshake::Nak));
        u.dpram[0x100..0x104].copy_from_slice(&[1, 2, 3, 4]);
        u.set_word(0x80, BUF_FULL | BUF_PID1 | BUF_AVAILABLE | 4);
        u.dpram_written(0x80);
        assert!(u.kicked);
        assert_eq!(u.token_in(0, 0, 64), Ok((true, vec![1, 2, 3, 4])));
        // The buffer goes back to the CPU: not available, not full.
        assert_eq!(ctrl(&u, 0x80), BUF_PID1 | 4);
        assert_eq!(u.read(BUFF_STATUS), 1);
        assert_eq!(u.read(INTS), INT_BUFF_STATUS);
        assert_eq!(u.token_in(0, 0, 64), Err(Handshake::Nak));
        u.write(BUFF_STATUS, 1);
        assert!(!u.irq());
    }

    #[test]
    fn ep0_out_fills_the_buffer() {
        let mut u = ready();
        u.set_word(0x84, BUF_PID1 | BUF_AVAILABLE | 64);
        u.token_out(0, 0, true, &[9, 8, 7]).unwrap();
        assert_eq!(ctrl(&u, 0x84), BUF_FULL | BUF_PID1 | 3);
        assert_eq!(&u.dpram[0x100..0x103], &[9, 8, 7]);
        assert_eq!(u.read(BUFF_STATUS), 2);
        assert_eq!(u.read(SIE_STATUS) & ST_DATA_SEQ_ERROR, 0);
        // The wrong PID is flagged.
        u.set_word(0x84, BUF_AVAILABLE | 64);
        u.token_out(0, 0, true, &[]).unwrap();
        assert_ne!(u.read(SIE_STATUS) & ST_DATA_SEQ_ERROR, 0);
    }

    #[test]
    fn ep0_stall_needs_arming() {
        let mut u = ready();
        u.set_word(0x80, BUF_STALL);
        assert_eq!(u.token_in(0, 0, 64), Err(Handshake::Nak));
        u.write(EP_STALL_ARM, 1);
        assert_eq!(u.token_in(0, 0, 64), Err(Handshake::Stall));
        // OUT is armed separately.
        u.set_word(0x84, BUF_STALL);
        assert_eq!(u.token_out(0, 0, true, &[]), Err(Handshake::Nak));
    }

    #[test]
    fn other_endpoints_need_enabling_and_use_their_buffer() {
        let mut u = ready();
        assert_eq!(u.token_in(0, 1, 64), Err(Handshake::Timeout));
        // EP1 IN control word at 0x08, buffer at 0x180.
        u.set_word(0x08, EP_ENABLE | EP_INT_PER_BUFF | 2 << 26 | 0x180);
        u.dpram[0x180] = 0x55;
        u.set_word(0x88, BUF_FULL | BUF_AVAILABLE | 1);
        assert_eq!(u.token_in(0, 1, 64), Ok((false, vec![0x55])));
        assert_eq!(u.read(BUFF_STATUS), 1 << 2);
        // EP2 OUT at 0x14, its buffer control at 0x94.
        u.set_word(0x14, EP_ENABLE | EP_INT_PER_BUFF | 2 << 26 | 0x1C0);
        u.set_word(0x94, BUF_AVAILABLE | 64);
        u.token_out(0, 2, false, &[1, 2]).unwrap();
        assert_eq!(&u.dpram[0x1C0..0x1C2], &[1, 2]);
        assert_eq!(u.read(BUFF_STATUS), 1 << 2 | 1 << 5);
        // Atomic clear of one bit.
        u.write(BUFF_STATUS, 1 << 2);
        assert_eq!(u.read(BUFF_STATUS), 1 << 5);
        // The wrong address gets nothing.
        u.write(ADDR_ENDP, 5);
        assert_eq!(u.token_out(0, 2, false, &[]), Err(Handshake::Timeout));
    }

    #[test]
    fn double_buffered_in_alternates_and_interrupts_per_pair() {
        let mut u = ready();
        u.set_word(
            0x08,
            EP_ENABLE | EP_DOUBLE_BUFFERED | EP_INT_PER_DOUBLE_BUFF | 2 << 26 | 0x180,
        );
        u.dpram[0x180] = 0xA0;
        u.dpram[0x1C0] = 0xA1;
        let half = |pid: u32, len: u32| BUF_FULL | pid | BUF_AVAILABLE | len;
        u.set_word(0x88, half(0, 64) | half(BUF_PID1, 1) << 16 | BUF_RESET);
        u.dpram_written(0x88);
        let (pid, d) = u.token_in(0, 1, 64).unwrap();
        assert_eq!((pid, d[0], d.len()), (false, 0xA0, 64));
        assert_eq!(u.read(BUFF_STATUS), 0, "one buffer of the pair");
        assert_eq!(u.token_in(0, 1, 64), Ok((true, vec![0xA1])));
        assert_eq!(u.read(BUFF_STATUS), 1 << 2);
        assert_eq!(u.read(BUFF_CPU_SHOULD_HANDLE), 1 << 2);
        assert_eq!(ctrl(&u, 0x88) & (BUF_AVAILABLE | BUF_AVAILABLE << 16), 0);
    }

    #[test]
    fn nak_and_stall_interrupts() {
        let mut u = ready();
        u.write(INTE, INT_EP_STALL_NAK);
        u.write(SIE_CTRL, u.reg(SIE_CTRL) | SIE_CTRL_EP0_INT_NAK);
        assert_eq!(u.token_in(0, 0, 64), Err(Handshake::Nak));
        assert_eq!(u.read(EP_STATUS_STALL_NAK), 1);
        assert!(u.irq());
        u.write(EP_STATUS_STALL_NAK, 1);
        assert!(!u.irq());
    }

    #[test]
    fn sof_counts_frames_and_interrupts_until_read() {
        let mut u = ready();
        u.write(INTE, INT_DEV_SOF);
        assert_eq!(u.next_event(), Some(CYCLES_PER_FRAME));
        u.update(CYCLES_PER_FRAME * 3 + 5);
        assert!(u.irq());
        assert_eq!(u.read(SOF_RD), 3);
        assert!(!u.irq());
        assert_eq!(u.next_event(), Some(CYCLES_PER_FRAME * 4));
    }

    #[test]
    fn pulling_down_disconnects() {
        let mut u = ready();
        u.write(INTE, INT_DEV_CONN_DIS);
        u.write(SIE_STATUS, ST_CONNECTED);
        assert!(!u.irq());
        u.write(SIE_CTRL, 0);
        assert!(!u.attached());
        assert_eq!(u.read(SIE_STATUS) & ST_CONNECTED, 0);
        assert!(u.irq());
    }

    #[test]
    fn a_host_enumerates_a_scripted_firmware() {
        // Endpoint 0 answers the host by hand, as a driver would, to check
        // the two fit: a GET_DESCRIPTOR goes through every stage.
        let mut u = ready();
        let mut host = usbhost::Host::new(CYCLES_PER_US);
        let mut now = 0;
        let mut answered = 0;
        for _ in 0..10_000 {
            let kicked = core::mem::take(&mut u.kicked);
            let wake = host.poll(&mut u, now, kicked);
            if u.read(SIE_STATUS) & ST_SETUP_REC != 0 {
                u.write(SIE_STATUS, ST_SETUP_REC);
                let s = usbhost::descriptor::Setup::from_bytes(u.dpram[..8].try_into().unwrap());
                if s.request == 6 && s.value == 0x0100 {
                    let d = [18, 1, 0, 2, 0, 0, 0, 64, 1, 2, 3, 4, 0, 1, 0, 0, 0, 1];
                    u.dpram[0x100..0x112].copy_from_slice(&d);
                    u.set_word(0x80, BUF_FULL | BUF_PID1 | BUF_AVAILABLE | 18);
                    u.set_word(0x84, BUF_PID1 | BUF_AVAILABLE);
                    u.kicked = true;
                    answered += 1;
                    if answered == 2 {
                        break;
                    }
                } else if s.request == 5 {
                    u.set_word(0x80, BUF_FULL | BUF_PID1 | BUF_AVAILABLE);
                    u.kicked = true;
                }
            }
            if u.read(BUFF_STATUS) & 1 != 0 && u.dpram[1] == 5 && u.reg(ADDR_ENDP) == 0 {
                u.write(ADDR_ENDP, u.dpram[2] as u32);
            }
            u.write(BUFF_STATUS, 3);
            u.kicked = false;
            now = wake.unwrap_or(now + 125).max(now + 1);
            u.update(now);
        }
        assert_eq!(answered, 2, "the host asked again after SET_ADDRESS");
        assert_eq!(u.reg(ADDR_ENDP), 1);
    }
}
