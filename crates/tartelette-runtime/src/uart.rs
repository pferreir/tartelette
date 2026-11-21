//! The far end of the board's two UART lines, outside the emulator: text
//! typed for the firmware, and the bridges that hand a UART to the host OS as
//! a pseudo-terminal (see [`UartBridges`]).
//!
//! What goes to the firmware waits here until its receiver is on and then
//! trickles onto the line a few bytes ahead of the receiver, so nothing is
//! lost to a UART that is not set up yet, and a program writing to a bridge
//! faster than the baud rate is held back by its terminal instead of piling
//! up in the emulator. (A firmware too slow for the baud rate still overruns,
//! as on the chip.)
use std::collections::VecDeque;

/// Host bytes put on a UART's line ahead of the receiver at most.
const LINE_AHEAD: usize = 64;

/// Which UARTs go to the host OS (from the command line).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UartBridges {
    /// UART `n` as a pseudo-terminal (Linux).
    pub tty: [bool; 2],
    /// A symlink to UART `n`'s pseudo-terminal, for scripts.
    pub links: [Option<std::path::PathBuf>; 2],
}

pub struct UartSide {
    /// Waiting for UART `n`'s receiver: typed in the console, or stdin.
    pub input: [VecDeque<u8>; 2],
    /// News from the bridges, for the UI to show once.
    pub notes: Vec<String>,
    #[cfg(target_os = "linux")]
    pty: [Option<usbbridge::Pty>; 2],
}

impl UartSide {
    pub fn new(bridges: &UartBridges) -> Result<Self, String> {
        #[cfg(not(target_os = "linux"))]
        if bridges.tty.iter().any(|&t| t) {
            return Err("--uart-bridge needs Linux (pseudo-terminals)".into());
        }
        #[cfg(target_os = "linux")]
        let pty = {
            let open = |n: usize| -> Result<_, String> {
                bridges.tty[n]
                    .then(|| usbbridge::Pty::open(bridges.links[n].as_deref()))
                    .transpose()
            };
            [open(0)?, open(1)?]
        };
        let mut side = Self {
            input: Default::default(),
            notes: Vec::new(),
            #[cfg(target_os = "linux")]
            pty,
        };
        for n in 0..2 {
            if let Some(name) = side.bridge(n) {
                side.notes.push(format!("UART{n} bridged to {name}"));
            }
        }
        Ok(side)
    }

    /// The pseudo-terminal UART `n` is bridged to.
    pub fn bridge(&self, _n: usize) -> Option<String> {
        #[cfg(target_os = "linux")]
        return self.pty[_n].as_ref().map(|p| p.name());
        #[cfg(not(target_os = "linux"))]
        None
    }

    /// Is UART `n` bridged to the host OS?
    pub fn bridged(&self, n: usize) -> bool {
        self.bridge(n).is_some()
    }

    /// Is any UART bridged (so that input can come from there)?
    pub fn any_bridged(&self) -> bool {
        (0..2).any(|n| self.bridged(n))
    }

    /// Hand what UART `n` sent to its bridge, and return what the firmware
    /// should receive next: queued input first, then the bridge's, no more
    /// than the line has room for (`queued` bytes are on it already).
    pub fn exchange(&mut self, n: usize, _tx: &[u8], receiving: bool, queued: usize) -> Vec<u8> {
        #[cfg(target_os = "linux")]
        if let Some(pty) = &mut self.pty[n] {
            pty.write(_tx);
        }
        if !receiving {
            return Vec::new();
        }
        let room = LINE_AHEAD.saturating_sub(queued);
        let take = room.min(self.input[n].len());
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut rx: Vec<u8> = self.input[n].drain(..take).collect();
        #[cfg(target_os = "linux")]
        if let Some(pty) = &mut self.pty[n] {
            rx.extend(pty.read_at_most(room - take));
        }
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_waits_for_the_receiver_and_for_room_on_the_line() {
        let mut u = UartSide::new(&UartBridges::default()).unwrap();
        u.input[1].extend(vec![b'x'; 100]);
        assert!(u.exchange(1, b"", false, 0).is_empty());
        assert_eq!(u.exchange(1, b"", true, 10).len(), LINE_AHEAD - 10);
        assert!(u.exchange(1, b"", true, LINE_AHEAD).is_empty());
        assert_eq!(u.exchange(1, b"", true, 0).len(), 100 - (LINE_AHEAD - 10));
        assert!(u.input[1].is_empty() && u.input[0].is_empty());
        assert!(!u.any_bridged());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_bridged_uart_talks_to_its_terminal() {
        use std::io::{Read, Write};
        let link = std::env::temp_dir().join(format!("tartelette-uart-{}", std::process::id()));
        let mut u = UartSide::new(&UartBridges {
            tty: [false, true],
            links: [None, Some(link.clone())],
        })
        .unwrap();
        assert!(u.bridged(1) && !u.bridged(0));
        assert!(u.notes[0].starts_with("UART1 bridged to "));
        let mut tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&link)
            .unwrap();
        tty.write_all(&[b'k'; 100]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Typed input goes first; the terminal holds what doesn't fit yet.
        u.input[1].extend(b"ab");
        let rx = u.exchange(1, b"hi", true, 0);
        assert_eq!(rx.len(), LINE_AHEAD);
        assert!(rx.starts_with(b"abk"));
        let mut buf = [0u8; 2];
        tty.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hi");
        assert_eq!(u.exchange(1, b"", true, 0).len(), 100 - (LINE_AHEAD - 2));
        drop(u);
        assert!(!link.exists());
    }
}
