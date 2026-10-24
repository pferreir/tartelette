//! ARM PrimeCell PL011 UART.
//!
//! Transmission is instantaneous: bytes written to DR go straight to `tx`
//! (drained by the host) and the TX FIFO always reads empty. Baud rate
//! registers are stored but have no effect yet. Interrupts are not wired
//! up; `RIS`/`MIS` are reported but nothing raises an IRQ line.
use super::Peripheral;
use alloc::{collections::VecDeque, vec::Vec};

const FR_RXFE: u32 = 1 << 4;
const FR_RXFF: u32 = 1 << 6;
const FR_TXFE: u32 = 1 << 7;

const CR_UARTEN: u32 = 1 << 0;
const CR_TXE: u32 = 1 << 8;
const CR_RXE: u32 = 1 << 9;

const RX_DEPTH: usize = 32;

pub struct Uart {
    /// Bytes transmitted since the host last took them.
    pub tx: Vec<u8>,
    rx: VecDeque<u8>,
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
            tx: Vec::new(),
            rx: VecDeque::new(),
            ibrd: 0,
            fbrd: 0,
            lcr_h: 0,
            cr: 0x300,
            ifls: 0x12,
            imsc: 0,
            dmacr: 0,
        }
    }

    /// Host -> firmware: queue a received byte. Dropped if the FIFO is full
    /// or the receiver is disabled.
    pub fn push_rx(&mut self, byte: u8) {
        if self.cr & (CR_UARTEN | CR_RXE) == (CR_UARTEN | CR_RXE) && self.rx.len() < RX_DEPTH {
            self.rx.push_back(byte);
        }
    }

    /// DMA request levels (UARTDMACR enables them): transmission is instant
    /// so TX always has room; RX has data waiting.
    pub fn dreq_tx(&self) -> bool {
        self.dmacr & 2 != 0 && self.cr & (CR_UARTEN | CR_TXE) == (CR_UARTEN | CR_TXE)
    }
    pub fn dreq_rx(&self) -> bool {
        self.dmacr & 1 != 0 && !self.rx.is_empty()
    }

    /// Level of the UART's interrupt output (masked status != 0).
    pub fn irq(&self) -> bool {
        self.ris() & self.imsc != 0
    }

    /// Raw interrupt status: RX when data is waiting (bit 4), TX while the
    /// (always empty) FIFO is below its threshold (bit 5).
    fn ris(&self) -> u32 {
        (!self.rx.is_empty() as u32) << 4 | 1 << 5
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
            0x00 => self.rx.pop_front().unwrap_or(0) as u32,
            0x04 => 0,
            0x18 => {
                // TX is never busy or full: transmission is instantaneous.
                let mut fr = FR_TXFE;
                if self.rx.is_empty() {
                    fr |= FR_RXFE;
                }
                if self.rx.len() >= RX_DEPTH {
                    fr |= FR_RXFF;
                }
                fr
            }
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
            0x00 => {
                if self.cr & (CR_UARTEN | CR_TXE) == (CR_UARTEN | CR_TXE) {
                    self.tx.push(val as u8);
                }
            }
            0x24 => self.ibrd = val & 0xFFFF,
            0x28 => self.fbrd = val & 0x3F,
            0x2C => self.lcr_h = val & 0xFF,
            0x30 => self.cr = val & 0xFF87,
            0x34 => self.ifls = val & 0x3F,
            0x38 => self.imsc = val & 0x7FF,
            0x44 => {} // ICR: interrupts are level-derived for now
            0x48 => self.dmacr = val & 7,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> Uart {
        let mut u = Uart::new();
        u.write(0x30, 0x301 | CR_RXE);
        u
    }

    #[test]
    fn rx_fifo_holds_32_bytes_then_drops() {
        let mut u = enabled();
        for i in 0..40 {
            u.push_rx(i);
        }
        assert_eq!(u.read(0x18) & FR_RXFF, FR_RXFF);
        assert_eq!(u.read(0x00), 0); // oldest byte first
        assert_eq!(u.read(0x18) & FR_RXFF, 0);
        let mut n = 1;
        while u.read(0x18) & FR_RXFE == 0 {
            assert_eq!(u.read(0x00), n);
            n += 1;
        }
        assert_eq!(n, 32); // bytes 32..40 were dropped
        assert_eq!(u.read(0x00), 0); // empty FIFO reads 0
    }

    #[test]
    fn disabled_receiver_and_transmitter_drop_data() {
        let mut u = Uart::new(); // reset CR: TXE|RXE set but UARTEN clear
        u.push_rx(1);
        assert_eq!(u.read(0x18) & FR_RXFE, FR_RXFE);
        u.write(0x00, b'a' as u32);
        assert!(u.tx.is_empty());
        u.write(0x30, CR_UARTEN | CR_RXE); // TXE clear
        u.write(0x00, b'b' as u32);
        assert!(u.tx.is_empty());
        u.push_rx(2);
        assert_eq!(u.read(0x00), 2);
    }

    #[test]
    fn interrupt_status_follows_mask() {
        let mut u = enabled();
        assert!(!u.irq());
        assert_eq!(u.read(0x3C), 1 << 5); // TX below threshold, RX empty
        u.write(0x38, 1 << 5);
        assert!(u.irq());
        assert_eq!(u.read(0x40), 1 << 5);
        u.write(0x38, 1 << 4);
        assert!(!u.irq());
        u.push_rx(7);
        assert!(u.irq());
        assert_eq!(u.read(0x40), 1 << 4);
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
    fn control_and_mask_registers_read_back_and_icr_is_inert() {
        let mut u = Uart::new();
        assert_eq!((u.read(0x30), u.read(0x38)), (0x300, 0)); // reset values
        u.write(0x30, 0x0301 | 1 << 4); // bits 6:3 are not implemented
        assert_eq!(u.read(0x30), 0x0301);
        u.write(0x38, 0xFFFF);
        assert_eq!(u.read(0x38), 0x7FF);
        let (ris, mis) = (u.read(0x3C), u.read(0x40));
        u.write(0x44, 0x7FF); // ICR: status is level-derived, nothing to clear
        assert_eq!((u.read(0x3C), u.read(0x40)), (ris, mis));
        u.write(0x4C, 1); // unmapped write
        assert_eq!(Uart::default().read(0x30), 0x300);
    }
}
