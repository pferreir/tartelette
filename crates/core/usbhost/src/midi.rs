//! USB-MIDI 1.0 event packets: four bytes, a cable number and code index
//! number (CIN) followed by up to three MIDI bytes. Messages go in and come
//! out whole (system exclusive included), as MIDI ports deliver them.
use alloc::vec::Vec;

/// Turns event packets from the device back into whole MIDI messages.
#[derive(Default)]
pub struct Decoder {
    /// System exclusive being assembled, per cable.
    sysex: [Vec<u8>; 16],
}

impl Decoder {
    /// Feed one packet; returns the message it completes, with its cable.
    pub fn packet(&mut self, p: [u8; 4]) -> Option<(u8, Vec<u8>)> {
        let cable = p[0] >> 4;
        let sysex = &mut self.sysex[cable as usize];
        let take = |n: usize| Some((cable, p[1..1 + n].to_vec()));
        match p[0] & 0xF {
            // Reserved for future extensions.
            0x0 | 0x1 => None,
            0x2 | 0xC | 0xD => take(2),
            0x3 | 0x8..=0xB | 0xE => take(3),
            0x4 => {
                sysex.extend_from_slice(&p[1..4]);
                None
            }
            0x5 if sysex.is_empty() && p[1] != 0xF7 => take(1),
            n @ 0x5..=0x7 => {
                sysex.extend_from_slice(&p[1..1 + (n - 4) as usize]);
                Some((cable, core::mem::take(sysex)))
            }
            _ => take(1), // 0xF: a single byte (real time)
        }
    }
}

/// The event packets for one whole message on `cable`; nothing for bytes
/// that are not a message.
pub fn encode(cable: u8, msg: &[u8]) -> Vec<[u8; 4]> {
    let c = cable << 4;
    let Some(&status) = msg.first() else {
        return Vec::new();
    };
    let one = |cin: u8, n: usize| {
        if msg.len() < n {
            return Vec::new();
        }
        let mut p = [c | cin, 0, 0, 0];
        p[1..1 + n].copy_from_slice(&msg[..n]);
        alloc::vec![p]
    };
    match status {
        0xF0 => {
            let mut out = Vec::new();
            let mut chunks = msg.chunks(3).peekable();
            while let Some(ch) = chunks.next() {
                let last = chunks.peek().is_none() || ch.last() == Some(&0xF7);
                let cin = if last { 4 + ch.len() as u8 } else { 4 };
                let mut p = [c | cin, 0, 0, 0];
                p[1..1 + ch.len()].copy_from_slice(ch);
                out.push(p);
                if last {
                    break;
                }
            }
            out
        }
        0x80..=0xBF | 0xE0..=0xEF => one(status >> 4, 3),
        0xC0..=0xDF => one(status >> 4, 2),
        0xF1 | 0xF3 => one(0x2, 2),
        0xF2 => one(0x3, 3),
        0xF6 => one(0x5, 1),
        0xF8..=0xFF => one(0xF, 1),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(cable: u8, msg: &[u8]) {
        let mut d = Decoder::default();
        let out: Vec<_> = encode(cable, msg)
            .into_iter()
            .filter_map(|p| d.packet(p))
            .collect();
        assert_eq!(out, [(cable, msg.to_vec())], "{msg:02x?}");
    }

    #[test]
    fn channel_and_system_messages_round_trip() {
        round_trip(0, &[0x90, 60, 100]);
        round_trip(1, &[0xC3, 5]);
        round_trip(0, &[0xE0, 0, 0x40]);
        round_trip(15, &[0xF8]);
        round_trip(0, &[0xF6]);
        round_trip(0, &[0xF2, 1, 2]);
        assert_eq!(encode(2, &[0x90, 60, 100]), [[0x29, 0x90, 60, 100]]);
        assert!(encode(0, &[0x90, 60]).is_empty());
    }

    #[test]
    fn sysex_of_every_length_round_trips() {
        for n in 0..8 {
            let mut m = alloc::vec![0xF0];
            m.extend((0..n).map(|i| i as u8));
            m.push(0xF7);
            round_trip(0, &m);
        }
        // F0 7E F7: one packet that starts and ends it.
        assert_eq!(encode(0, &[0xF0, 0x7E, 0xF7]), [[0x07, 0xF0, 0x7E, 0xF7]]);
    }
}
