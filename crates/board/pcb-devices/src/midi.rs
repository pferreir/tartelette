//! A MIDI in jack (behind its optocoupler): where MIDI from the host enters
//! the board. Either on a UART's RX (`uart`), which the host feeds directly,
//! or on a GPIO (`gpio`), for firmware that reads MIDI with a PIO or
//! software UART: the part then sends each byte on the pin itself, as the
//! optocoupler's output would, at 31250 baud, 8N1, idle high, a bit every
//! 32 µs of the core clock (`mhz`).
//!
//! Host action `byte` (0..255) queues a byte on a pin route; on a UART route
//! the host's session takes it to the UART. Emits `sent` with each byte as
//! it starts on the pin.
use alloc::{boxed::Box, collections::VecDeque};

use pcb::{Device, Io, Kind, Param};

/// Bytes waiting on the pin; past this the oldest go (live input shouldn't
/// lag behind).
const KEEP: usize = 1024;
const BAUD: u64 = 31_250;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Uart(usize),
    Pin(usize),
}

pub struct Midi {
    pub route: Route,
    /// Whether `mhz` fixes the clock, and a bit's time at it.
    fixed: bool,
    cycles_per_bit: u64,
    queue: VecDeque<u8>,
    /// The frame on the pin (start bit, 8 data bits LSB first, stop bit,
    /// from bit 0) and the bit being sent.
    frame: Option<(u16, u8)>,
    /// Bytes sent on the pin so far.
    pub sent: u64,
}

pub const KIND: Kind = Kind {
    name: "midi",
    summary: "MIDI in jack: host MIDI on a UART's RX or as serial on a pin",
    bus: None,
    params: &[
        Param::int("uart", 0, 1, "the UART whose RX the jack feeds; or gpio").optional(),
        Param::pin(
            "gpio",
            "the pin the jack feeds, as 31250-baud serial; or uart",
        )
        .optional(),
        Param::int(
            "mhz",
            1,
            1000,
            "on a gpio: core clock, to time the bits (default: the chip's clk_sys)",
        )
        .optional(),
    ],
    build: |p| {
        let mhz = p.opt_int("mhz");
        let route = match (p.opt_int("uart"), p.opt_pin("gpio")) {
            (Some(_), Some(_)) => return Err("gpio is only for a jack without uart".into()),
            (None, None) => return Err("give the jack a uart or a gpio".into()),
            (Some(_), None) if mhz.is_some() => {
                return Err("mhz is only for a jack on a gpio".into());
            }
            (Some(n), None) => Route::Uart(n as usize),
            (None, Some(pin)) => Route::Pin(pin),
        };
        Ok(Box::new(Midi {
            route,
            fixed: mhz.is_some(),
            cycles_per_bit: mhz.unwrap_or(125) as u64 * 1_000_000 / BAUD,
            queue: VecDeque::new(),
            frame: None,
            sent: 0,
        }))
    },
};

impl Midi {
    /// Whether a byte is on its way down the pin.
    pub fn busy(&self) -> bool {
        self.frame.is_some()
    }

    /// Put bit `n` of the frame on the pin, and wake for the next.
    fn bit(&mut self, io: &mut Io, pin: usize) {
        let Some((frame, n)) = self.frame else { return };
        io.drive(pin, Some(frame >> n & 1 != 0));
        io.wake_at(io.cycles() + self.cycles_per_bit);
    }

    /// Start the next byte, if there is one.
    fn next(&mut self, io: &mut Io, pin: usize) {
        self.frame = self.queue.pop_front().map(|b| {
            self.sent += 1;
            io.emit("sent", b as i64);
            (1 << 9 | (b as u16) << 1, 0)
        });
        self.bit(io, pin);
    }
}

impl Device for Midi {
    fn attach(&mut self, io: &mut Io) {
        if let Route::Pin(pin) = self.route {
            // The line idles high.
            io.drive(pin, Some(true));
            self.frame = None;
            self.queue.clear();
        }
    }

    fn input(&mut self, io: &mut Io, action: &str, value: i64) -> bool {
        if action != "byte" {
            return false;
        }
        let Route::Pin(pin) = self.route else {
            io.warn("byte: this jack feeds a UART, which the host feeds".into());
            return true;
        };
        if self.queue.len() == KEEP {
            self.queue.pop_front();
        }
        self.queue.push_back(value as u8);
        if self.frame.is_none() {
            self.next(io, pin);
        }
        true
    }

    fn clock(&mut self, _at: u64, hz: u64) {
        if !self.fixed {
            self.cycles_per_bit = hz / BAUD;
        }
    }

    fn timer(&mut self, io: &mut Io) {
        let Route::Pin(pin) = self.route else { return };
        let Some((frame, n)) = self.frame else { return };
        if n + 1 < 10 {
            self.frame = Some((frame, n + 1));
            self.bit(io, pin);
        } else {
            self.next(io, pin);
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use pcb::Pins;

    use super::*;
    use crate::testutil::FakePins;

    #[test]
    fn takes_a_uart_or_a_gpio() {
        let err = |t: &str| {
            let text = alloc::format!("parts.m = {{ kind = \"midi\", {t} }}");
            pcb::Board::parse_with(&text, &crate::registry())
                .err()
                .map(|e| e.msg)
        };
        assert_eq!(err("uart = 1"), None);
        assert_eq!(err("gpio = 5"), None);
        assert!(err("").unwrap().contains("a uart or a gpio"));
        assert!(
            err("uart = 0, gpio = 5")
                .unwrap()
                .contains("gpio is only for")
        );
        assert!(
            err("uart = 0, mhz = 100")
                .unwrap()
                .contains("mhz is only for")
        );
    }

    #[test]
    fn sends_bytes_on_the_pin_at_31250_baud() {
        // 1 MHz: exactly 32 cycles a bit.
        let mut m = Midi {
            route: Route::Pin(5),
            fixed: true,
            cycles_per_bit: 32,
            queue: VecDeque::new(),
            frame: None,
            sent: 0,
        };
        let mut pins = FakePins::default();
        let mut events = Vec::new();
        let mut timers: Vec<u64> = Vec::new();
        let mut levels = Vec::new();
        let mut call = |m: &mut Midi, t: u64, byte: Option<u8>, timers: &mut Vec<u64>| {
            let mut pending = pcb::Pending::default();
            let mut io = Io::new(&mut pins, t, &mut events, 0, false).with_pending(&mut pending);
            match byte {
                Some(b) => assert!(m.input(&mut io, "byte", b as i64)),
                None if t == 0 => m.attach(&mut io),
                None => m.timer(&mut io),
            }
            timers.extend(pending.timers.iter().map(|&(t, _)| t));
            pins.level(5)
        };
        assert!(call(&mut m, 0, None, &mut timers), "idles high");
        levels.push((10, call(&mut m, 10, Some(0x90), &mut timers)));
        call(&mut m, 12, Some(0x3C), &mut timers);
        while let Some(t) = timers.first().copied() {
            timers.remove(0);
            levels.push((t, call(&mut m, t, None, &mut timers)));
        }
        // 0x90, then 0x3C straight after: start, LSB first, stop.
        let bits: Vec<bool> = levels.iter().map(|l| l.1).collect();
        let frame = |b: u8| {
            let mut v = alloc::vec![false];
            v.extend((0..8).map(|i| b >> i & 1 != 0));
            v.push(true);
            v
        };
        let mut want = frame(0x90);
        want.extend(frame(0x3C));
        want.push(true);
        assert_eq!(bits, want);
        assert_eq!(levels[1].0, 42, "a bit every 32 cycles");
        assert_eq!(levels[10].0, 330, "the next byte right after the stop bit");
        assert_eq!(m.sent, 2);
        assert!(!m.busy());
    }
}
