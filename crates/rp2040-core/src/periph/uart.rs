//! ARM PrimeCell PL011 UART.
//!
//! Characters take the time the line format says they do: a frame is a start
//! bit, 5..8 data bits, an optional parity bit and 1 or 2 stop bits, each
//! lasting 16 × (IBRD + FBRD/64) cycles of clk_peri. The system tells the UART
//! clk_peri's frequency ([`Uart::set_clk_peri`]): clk_sys's 125 MHz, as
//! embassy sets it up, or the USB PLL's 48 MHz, as pico-sdk 2 and MicroPython
//! do. Transmitted bytes reach `tx`
//! when their stop bit ends; bytes sent from the host travel the same line at
//! the same rate and land in the RX FIFO, overrunning it if the firmware does
//! not keep up. Until the firmware programs a divisor (IBRD = 0) the line is
//! untimed: TX is instantaneous, characters are 8 bits whatever LCR_H says,
//! and host bytes land as soon as there is room.
//!
//! Interrupts follow the PL011: RX is a level (FIFO at or above its trigger
//! level), TX fires when the FIFO drains through its trigger level (not merely
//! by being below it), receive timeout fires 32 bit periods after the last
//! character arrived if the FIFO still holds data, and overrun is latched.
//! TX, RT and OE are cleared through ICR. Framing, parity and break errors,
//! modem lines and loopback are not modelled.
use alloc::{collections::VecDeque, vec::Vec};

use super::Peripheral;

const FR_BUSY: u32 = 1 << 3;
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;
const FR_RXFF: u32 = 1 << 6;
const FR_TXFE: u32 = 1 << 7;

const LCR_PEN: u32 = 1 << 1;
const LCR_STP2: u32 = 1 << 3;
const LCR_FEN: u32 = 1 << 4;

const CR_UARTEN: u32 = 1 << 0;
const CR_TXE: u32 = 1 << 8;
const CR_RXE: u32 = 1 << 9;

const INT_RX: u32 = 1 << 4;
const INT_TX: u32 = 1 << 5;
const INT_RT: u32 = 1 << 6;
const INT_OE: u32 = 1 << 10;

/// Overrun error flag of a DR read (and bit 3 of RSR).
const DR_OE: u16 = 1 << 11;
const RSR_OE: u32 = 1 << 3;

const FIFO_DEPTH: usize = 32;
/// The receive timeout, in bit periods.
const RT_BITS: u64 = 32;

/// The emulator's clock, which the line's times are counted in.
const CLK_SYS_HZ: u64 = crate::system::CYCLES_PER_US * 1_000_000;

pub struct Uart {
    /// clk_peri, which the baud rate divisors divide.
    clk_peri_hz: u64,
    /// Bytes transmitted since the host last took them.
    pub tx: Vec<u8>,
    tx_fifo: VecDeque<u8>,
    /// When the character in the transmit shift register finishes, and the
    /// character itself.
    tx_shift: Option<(u64, u8)>,
    /// RX FIFO entries: the data byte plus error flags (DR bits 11:8).
    rx_fifo: VecDeque<u16>,
    /// Host bytes on the line that have not reached the receiver yet.
    rx_line: VecDeque<u8>,
    /// When the first of `rx_line` lands.
    rx_next_at: Option<u64>,
    /// When the receive timeout expires (counted from the last arrival).
    rt_at: Option<u64>,
    /// Latched TX, RT and OE raw interrupt bits.
    latched: u32,
    rsr: u32,
    /// The cycle up to which the line has been run.
    cycles: u64,
    ibrd: u32,
    fbrd: u32,
    lcr_h: u32,
    cr: u32,
    ifls: u32,
    imsc: u32,
    dmacr: u32,
}

impl Uart {
    pub fn new() -> Self {
        Self {
            clk_peri_hz: CLK_SYS_HZ,
            tx: Vec::new(),
            tx_fifo: VecDeque::new(),
            tx_shift: None,
            rx_fifo: VecDeque::new(),
            rx_line: VecDeque::new(),
            rx_next_at: None,
            rt_at: None,
            latched: 0,
            rsr: 0,
            cycles: 0,
            ibrd: 0,
            fbrd: 0,
            lcr_h: 0,
            cr: 0x300,
            ifls: 0x12,
            imsc: 0,
            dmacr: 0,
        }
    }

    /// Host -> firmware: put bytes on the RX line, behind any still in flight.
    /// They arrive one frame apart; a byte that finds the receiver disabled is
    /// lost, and one that finds the FIFO full is an overrun.
    pub fn receive(&mut self, bytes: &[u8]) {
        self.rx_line.extend(bytes);
        self.kick();
    }

    /// Bring the line up to cycle `to`: finish transmissions, land received
    /// bytes, expire the receive timeout. Returns whether anything happened.
    pub fn update(&mut self, to: u64) -> bool {
        let mut changed = false;
        while let Some(t) = self.next_event().filter(|&t| t <= to) {
            self.cycles = t;
            changed = true;
            if let Some((_, byte)) = self.tx_shift.filter(|&(at, _)| at == t) {
                self.tx.push(byte);
                self.tx_shift = None;
                self.start_tx();
            }
            if self.rx_next_at == Some(t) {
                self.rx_next_at = None;
                if let Some(byte) = self.rx_line.pop_front() {
                    self.land(byte);
                }
                self.schedule_rx();
            }
            if self.rt_at == Some(t) {
                self.rt_at = None;
                if !self.rx_fifo.is_empty() {
                    self.latched |= INT_RT;
                }
            }
        }
        self.cycles = self.cycles.max(to);
        changed
    }

    /// The cycle the character on the TX line finishes at, if one is going out.
    pub fn tx_event(&self) -> Option<u64> {
        self.tx_shift.map(|(at, _)| at)
    }

    /// The cycle at which the line next changes on its own.
    pub fn next_event(&self) -> Option<u64> {
        [self.tx_shift.map(|(at, _)| at), self.rx_next_at, self.rt_at]
            .into_iter()
            .flatten()
            .min()
    }

    /// DMA request levels (UARTDMACR enables them): TX while the FIFO has
    /// room, RX while it holds data (unless DMAONERR and an overrun).
    pub fn dreq_tx(&self) -> bool {
        self.dmacr & 2 != 0
            && self.cr & (CR_UARTEN | CR_TXE) == (CR_UARTEN | CR_TXE)
            && self.tx_fifo.len() < self.depth()
    }
    pub fn dreq_rx(&self) -> bool {
        self.dmacr & 1 != 0
            && !self.rx_fifo.is_empty()
            && !(self.dmacr & 4 != 0 && self.latched & INT_OE != 0)
    }

    /// Level of the UART's interrupt output (masked status != 0).
    pub fn irq(&self) -> bool {
        // The mask first: with nothing enabled there is no status to work out.
        self.imsc != 0 && self.ris() & self.imsc != 0
    }

    fn ris(&self) -> u32 {
        let rx = (self.rx_fifo.len() >= self.rx_trigger()) as u32 * INT_RX;
        rx | self.latched
    }

    fn enabled(&self, bit: u32) -> bool {
        self.cr & (CR_UARTEN | bit) == (CR_UARTEN | bit)
    }

    fn depth(&self) -> usize {
        if self.lcr_h & LCR_FEN != 0 {
            FIFO_DEPTH
        } else {
            1
        }
    }

    /// RX interrupt when the FIFO holds at least this many bytes.
    fn rx_trigger(&self) -> usize {
        if self.lcr_h & LCR_FEN == 0 {
            return 1;
        }
        [4, 8, 16, 24, 28][(self.ifls >> 3 & 7).min(4) as usize]
    }

    /// TX interrupt when the FIFO drains to this many bytes or fewer.
    fn tx_trigger(&self) -> usize {
        if self.lcr_h & LCR_FEN == 0 {
            return 0;
        }
        [4, 8, 16, 24, 28][(self.ifls & 7).min(4) as usize]
    }

    /// Set clk_peri's frequency (from the clock generators). Characters
    /// already on the line keep the time they started with.
    pub fn set_clk_peri(&mut self, hz: u64) {
        self.clk_peri_hz = hz.max(1);
    }

    /// The data bits of a character (all 8 on an untimed line, which has
    /// not been set up).
    fn data_mask(&self) -> u8 {
        match self.line_cycles(1) {
            Some(_) => (0xFFu32 >> (3 - (self.lcr_h >> 5 & 3))) as u8,
            None => 0xFF,
        }
    }

    /// The clk_sys cycles `bits` bit periods take, or None while no divisor
    /// is programmed. Rounded once for them all, so a frame doesn't gather
    /// the rounding of each of its bits.
    fn line_cycles(&self, bits: u64) -> Option<u64> {
        // A bit is 16 × (IBRD + FBRD/64) clk_peri cycles: the divisor in
        // 64ths, over 4. FBRD is ignored at the largest divisor.
        let fbrd = if self.ibrd == 0xFFFF { 0 } else { self.fbrd };
        let div64 = (self.ibrd as u64) << 6 | fbrd as u64;
        let per = 4 * self.clk_peri_hz;
        (self.ibrd != 0).then(|| (div64 * bits * CLK_SYS_HZ + per / 2) / per)
    }

    /// Cycles one character takes on the line.
    fn frame_cycles(&self) -> Option<u64> {
        let data = 5 + (self.lcr_h >> 5 & 3) as u64;
        let parity = (self.lcr_h & LCR_PEN != 0) as u64;
        let stop = if self.lcr_h & LCR_STP2 != 0 { 2 } else { 1 };
        self.line_cycles(1 + data + parity + stop)
    }

    /// Move the next character from the TX FIFO into the shift register, if
    /// it is free and the transmitter is on. On an untimed line it goes out
    /// at once, and so does the rest of the FIFO.
    fn start_tx(&mut self) {
        while self.tx_shift.is_none() && self.enabled(CR_TXE) {
            let Some(byte) = self.tx_fifo.pop_front() else {
                return;
            };
            // The interrupt fires as the level falls through the trigger.
            if self.tx_fifo.len() == self.tx_trigger() {
                self.latched |= INT_TX;
            }
            match self.frame_cycles() {
                Some(f) => self.tx_shift = Some((self.cycles + f, byte)),
                None => self.tx.push(byte),
            }
        }
    }

    /// A character from the line reaches the receiver.
    fn land(&mut self, byte: u8) {
        if !self.enabled(CR_RXE) {
            return;
        }
        if self.rx_fifo.len() >= self.depth() {
            // The FIFO keeps its contents; the error goes with the newest.
            self.latched |= INT_OE;
            self.rsr |= RSR_OE;
            if let Some(last) = self.rx_fifo.back_mut() {
                *last |= DR_OE;
            }
            return;
        }
        self.rx_fifo.push_back((byte & self.data_mask()) as u16);
        self.rt_at = self.line_cycles(RT_BITS).map(|c| self.cycles + c);
    }

    /// Start the next character down the RX line, if one is waiting. Untimed,
    /// characters land straight away while there is room for them.
    fn schedule_rx(&mut self) {
        if self.rx_next_at.is_some() || self.rx_line.is_empty() {
            return;
        }
        match self.frame_cycles() {
            Some(f) => self.rx_next_at = Some(self.cycles + f),
            None => {
                while self.rx_fifo.len() < self.depth() || !self.enabled(CR_RXE) {
                    let Some(byte) = self.rx_line.pop_front() else {
                        break;
                    };
                    self.land(byte);
                }
            }
        }
    }

    /// Restart whatever a register change may have unblocked.
    fn kick(&mut self) {
        self.start_tx();
        self.schedule_rx();
    }

    fn read_dr(&mut self) -> u32 {
        let v = self.rx_fifo.pop_front().unwrap_or(0) as u32;
        if self.rx_fifo.is_empty() {
            self.latched &= !INT_RT;
        }
        self.schedule_rx();
        v
    }

    fn write_dr(&mut self, val: u32) {
        if !self.enabled(CR_TXE) || self.tx_fifo.len() >= self.depth() {
            return;
        }
        self.tx_fifo.push_back(val as u8 & self.data_mask());
        if self.tx_fifo.len() > self.tx_trigger() {
            self.latched &= !INT_TX;
        }
        self.start_tx();
    }

    fn fr(&self) -> u32 {
        let mut fr = 0;
        if self.tx_shift.is_some() || !self.tx_fifo.is_empty() {
            fr |= FR_BUSY;
        }
        if self.rx_fifo.is_empty() {
            fr |= FR_RXFE;
        }
        if self.tx_fifo.len() >= self.depth() {
            fr |= FR_TXFF;
        }
        if self.rx_fifo.len() >= self.depth() {
            fr |= FR_RXFF;
        }
        if self.tx_fifo.is_empty() {
            fr |= FR_TXFE;
        }
        fr
    }
}

impl Default for Uart {
    fn default() -> Self {
        Self::new()
    }
}

impl Peripheral for Uart {
    fn read(&mut self, off: u32) -> u32 {
        match off {
            0x00 => self.read_dr(),
            0x04 => self.rsr,
            0x18 => self.fr(),
            0x24 => self.ibrd,
            0x28 => self.fbrd,
            0x2C => self.lcr_h,
            0x30 => self.cr,
            0x34 => self.ifls,
            0x38 => self.imsc,
            0x3C => self.ris(),
            0x40 => self.ris() & self.imsc,
            0x48 => self.dmacr,
            // PeriphID 0..3 and PCellID 0..3
            0xFE0 => 0x11,
            0xFE4 => 0x10,
            0xFE8 => 0x34,
            0xFEC => 0x00,
            0xFF0 => 0x0D,
            0xFF4 => 0xF0,
            0xFF8 => 0x05,
            0xFFC => 0xB1,
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, val: u32) {
        match off {
            0x00 => self.write_dr(val),
            0x04 => self.rsr = 0, // ECR: any write clears the errors
            0x24 => self.ibrd = val & 0xFFFF,
            0x28 => self.fbrd = val & 0x3F,
            0x2C => self.lcr_h = val & 0xFF,
            0x30 => self.cr = val & 0xFF87,
            0x34 => self.ifls = val & 0x3F,
            0x38 => self.imsc = val & 0x7FF,
            // ICR: RX is a level, cleared by reading; the rest are latched.
            0x44 => self.latched &= !val,
            0x48 => self.dmacr = val & 7,
            _ => return,
        }
        self.kick();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 115200 baud at 125 MHz: 1085 cycles a bit, 10850 a frame (8N1).
    const BIT: u64 = 1085;
    const FRAME: u64 = 10 * BIT;

    fn enabled() -> Uart {
        let mut u = Uart::new();
        u.write(0x2C, 0x70); // 8N1, FIFOs on
        u.write(0x30, 0x301 | CR_RXE);
        u
    }

    fn timed() -> Uart {
        let mut u = Uart::new();
        u.write(0x24, 67);
        u.write(0x28, 52);
        u.write(0x2C, 0x70);
        u.write(0x30, 0x301 | CR_RXE);
        u
    }

    #[test]
    fn rx_fifo_holds_32_bytes_then_waits() {
        let mut u = enabled();
        u.receive(&(0..40).collect::<Vec<u8>>());
        assert_eq!(u.read(0x18) & FR_RXFF, FR_RXFF);
        assert_eq!(u.read(0x00), 0); // oldest byte first
        // Untimed: the line waits for room instead of overrunning.
        assert_eq!(u.read(0x18) & FR_RXFF, FR_RXFF);
        let mut n = 1;
        while u.read(0x18) & FR_RXFE == 0 {
            assert_eq!(u.read(0x00), n);
            n += 1;
        }
        assert_eq!(n, 40);
        assert_eq!(u.read(0x00), 0); // empty FIFO reads 0
        assert_eq!(u.read(0x3C) & INT_OE, 0);
    }

    #[test]
    fn disabled_receiver_and_transmitter_drop_data() {
        let mut u = Uart::new(); // reset CR: TXE|RXE set but UARTEN clear
        u.receive(&[1]);
        assert_eq!(u.read(0x18) & FR_RXFE, FR_RXFE);
        u.write(0x00, b'a' as u32);
        assert!(u.tx.is_empty());
        u.write(0x30, CR_UARTEN | CR_RXE); // TXE clear
        u.write(0x00, b'b' as u32);
        assert!(u.tx.is_empty());
        u.receive(&[2]);
        assert_eq!(u.read(0x00), 2);
    }

    #[test]
    fn interrupt_status_follows_mask() {
        let mut u = enabled();
        u.write(0x2C, 0x60); // FIFOs off: RX triggers at one byte
        assert!(!u.irq());
        assert_eq!(u.read(0x3C), 0); // TX FIFO never drained, RX empty
        u.write(0x00, b'x' as u32); // untimed: out at once, holding reg empties
        assert_eq!(u.read(0x3C), INT_TX);
        u.write(0x38, INT_TX);
        assert!(u.irq());
        assert_eq!(u.read(0x40), INT_TX);
        u.write(0x38, INT_RX);
        assert!(!u.irq());
        u.receive(&[7]);
        assert!(u.irq());
        assert_eq!(u.read(0x40), INT_RX);
        u.write(0x44, 0x7FF);
        assert_eq!(u.read(0x3C), INT_RX); // RX is a level: only reading clears it
        u.read(0x00);
        assert!(!u.irq());
    }

    #[test]
    fn transmission_takes_a_frame_per_byte() {
        let mut u = timed();
        u.write(0x00, b'a' as u32);
        u.write(0x00, b'b' as u32);
        assert_eq!(u.read(0x18) & (FR_BUSY | FR_TXFE), FR_BUSY); // 'b' queued
        assert_eq!(u.next_event(), Some(FRAME));
        assert!(!u.update(FRAME - 1));
        assert!(u.tx.is_empty());
        assert!(u.update(FRAME));
        assert_eq!(u.tx, b"a");
        assert_eq!(u.read(0x18) & (FR_BUSY | FR_TXFE), FR_BUSY | FR_TXFE); // 'b' shifting
        u.update(2 * FRAME);
        assert_eq!(u.tx, b"ab");
        assert_eq!(u.read(0x18) & (FR_BUSY | FR_TXFE), FR_TXFE);
        assert_eq!(u.next_event(), None);
    }

    #[test]
    fn the_divisors_divide_clk_peri() {
        // 115200 baud from 48 MHz, as MicroPython sets it up: 26 + 3/64.
        let mut u = Uart::new();
        u.set_clk_peri(48_000_000);
        u.write(0x24, 26);
        u.write(0x28, 3);
        u.write(0x2C, 0x70);
        u.write(0x30, 0x301);
        u.write(0x00, b'a' as u32);
        // A bit is 16 x 26.047 / 48 MHz = 8.68 us; the frame is rounded once.
        assert_eq!(u.next_event(), Some(10853));
    }

    #[test]
    fn frame_length_follows_the_line_format() {
        let mut u = timed();
        u.write(0x2C, 0x70 | LCR_PEN | LCR_STP2); // 8E2: 12 bits
        u.write(0x00, 0);
        assert_eq!(u.next_event(), Some(12 * BIT));
        let mut u = timed();
        u.write(0x24, 813); // 9600 baud: 813 + 50/64
        u.write(0x28, 50);
        u.write(0x2C, 0x40); // 7N1, FIFOs off: 9 bits
        u.write(0x00, 0xFF);
        // 9 x 13020.5 cycles, rounded once: 117184.5.
        assert_eq!(u.next_event(), Some(117_185));
        u.update(u.next_event().unwrap());
        assert_eq!(u.tx, [0x7F]); // top bit doesn't fit in 7 data bits
    }

    #[test]
    fn tx_fifo_fills_and_drains_through_the_trigger_level() {
        let mut u = timed(); // TX trigger: 16 or fewer (IFLS reset value)
        for i in 0..34 {
            u.write(0x00, i);
        }
        // One in the shift register, 32 queued, the 34th lost.
        assert_eq!(u.read(0x18) & FR_TXFF, FR_TXFF);
        assert_eq!(u.read(0x3C) & INT_TX, 0);
        u.update(FRAME);
        assert_eq!(u.read(0x18) & FR_TXFF, 0);
        u.update(16 * FRAME); // 17th character starts: 16 left in the FIFO
        assert_eq!(u.read(0x3C) & INT_TX, INT_TX);
        u.write(0x44, INT_TX);
        u.update(40 * FRAME);
        assert_eq!(u.read(0x3C) & INT_TX, 0); // fires once, on the way through
        assert_eq!(u.tx, (0..33).collect::<Vec<u8>>());
    }

    #[test]
    fn received_bytes_arrive_a_frame_apart_then_time_out() {
        let mut u = timed();
        u.write(0x38, INT_RX | INT_RT);
        u.receive(b"abc");
        assert_eq!(u.next_event(), Some(FRAME));
        u.update(FRAME);
        assert_eq!(u.read(0x18) & FR_RXFE, 0);
        u.update(3 * FRAME - 1);
        assert!(!u.irq()); // 2 bytes: below the trigger level, not timed out
        u.update(3 * FRAME + RT_BITS * BIT - 1);
        assert!(!u.irq());
        u.update(3 * FRAME + RT_BITS * BIT);
        assert_eq!(u.read(0x40), INT_RT);
        assert_eq!(
            [u.read(0x00), u.read(0x00), u.read(0x00)],
            [b'a' as u32, b'b' as u32, b'c' as u32]
        );
        assert!(!u.irq()); // emptying the FIFO clears the timeout
        assert_eq!(u.next_event(), None);
    }

    #[test]
    fn a_full_fifo_overruns() {
        let mut u = timed();
        u.write(0x48, 1 | 4); // RX DMA, off on error
        u.receive(&[0x55; 34]);
        u.update(32 * FRAME);
        assert!(u.dreq_rx());
        assert_eq!(u.read(0x3C) & INT_OE, 0);
        u.update(34 * FRAME);
        assert_eq!(u.read(0x3C) & INT_OE, INT_OE);
        assert_eq!(u.read(0x04), RSR_OE);
        assert!(!u.dreq_rx());
        for _ in 0..31 {
            assert_eq!(u.read(0x00), 0x55);
        }
        assert_eq!(u.read(0x00), 0x55 | DR_OE as u32); // the error comes last
        u.write(0x44, INT_OE);
        u.write(0x04, 0);
        assert_eq!((u.read(0x3C) & INT_OE, u.read(0x04)), (0, 0));
    }

    #[test]
    fn bytes_on_the_line_are_lost_while_the_receiver_is_off() {
        let mut u = timed();
        u.write(0x30, CR_UARTEN | CR_TXE);
        u.receive(b"xy");
        u.update(FRAME);
        u.write(0x30, 0x301 | CR_RXE);
        u.update(2 * FRAME);
        assert_eq!(u.read(0x00), b'y' as u32);
        assert_eq!(u.read(0x18) & FR_RXFE, FR_RXFE);
    }

    #[test]
    fn registers_mask_and_identify() {
        let mut u = Uart::new();
        u.write(0x24, 0xFFFF_FFFF);
        u.write(0x28, 0xFFFF_FFFF);
        u.write(0x2C, 0xFFFF_FFFF);
        u.write(0x34, 0xFFFF_FFFF);
        u.write(0x48, 0xFFFF_FFFF);
        assert_eq!(
            [
                u.read(0x24),
                u.read(0x28),
                u.read(0x2C),
                u.read(0x34),
                u.read(0x48)
            ],
            [0xFFFF, 0x3F, 0xFF, 0x3F, 7]
        );
        let id = |u: &mut Uart, base| {
            (0..4u32)
                .map(|i| u.read(base + 4 * i))
                .collect::<alloc::vec::Vec<_>>()
        };
        assert_eq!(id(&mut u, 0xFE0), [0x11, 0x10, 0x34, 0x00]);
        assert_eq!(id(&mut u, 0xFF0), [0x0D, 0xF0, 0x05, 0xB1]);
        assert_eq!(u.read(0x04), 0); // RSR
        assert_eq!(u.read(0x100), 0); // unmapped
    }

    #[test]
    fn control_and_mask_registers_read_back() {
        let mut u = Uart::new();
        assert_eq!((u.read(0x30), u.read(0x38)), (0x300, 0)); // reset values
        assert_eq!(u.read(0x18), FR_TXFE | FR_RXFE);
        u.write(0x30, 0x0301 | 1 << 4); // bits 6:3 are not implemented
        assert_eq!(u.read(0x30), 0x0301);
        u.write(0x38, 0xFFFF);
        assert_eq!(u.read(0x38), 0x7FF);
        u.write(0x4C, 1); // unmapped write
        assert_eq!(Uart::default().read(0x30), 0x300);
    }
}
