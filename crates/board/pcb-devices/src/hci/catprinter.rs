//! The MXW01 "cat printer" protocol: a 384-dot thermal printer driven over
//! three characteristics of service `AE30`. [`CatPrinter`] is the printer's
//! side of it, for the [mock peer](super::peers) that stands in for one; what
//! it prints piles up on a roll of [paper](CatPrinter::paper).
//!
//! * `AE01` (write): commands, each framed `22 21 cmd 00 len_lo len_hi
//!   payload.. crc8(payload) FF`.
//! * `AE02` (notify): replies, framed the same way.
//! * `AE03` (write without response): raster data, a plain byte stream of
//!   48-byte rows, 1 bit per dot, leftmost dot in bit 0, 1 = black.
//!
//! A print job: `A2` intensity, `A1` status, `A9` print request (row count,
//! `0x30`, mode), the rows on `AE03`, then `AD` flush. The printer answers
//! `A9` with a status byte and sends `AA` once the paper is out.
use alloc::{vec, vec::Vec};

/// Dots per row.
pub const WIDTH: usize = 384;
/// Bytes per row in 1-bit mode.
pub const ROW_BYTES: usize = WIDTH / 8;

pub const GET_STATUS: u8 = 0xA1;
pub const SET_INTENSITY: u8 = 0xA2;
pub const FEED: u8 = 0xA3;
pub const RETRACT: u8 = 0xA4;
pub const PRINT: u8 = 0xA9;
pub const PRINT_COMPLETE: u8 = 0xAA;
pub const BATTERY: u8 = 0xAB;
pub const CANCEL: u8 = 0xAC;
pub const FLUSH: u8 = 0xAD;
pub const GET_VERSION: u8 = 0xB1;

/// Print mode in the `A9` request: 4 bits per dot, darkest = 15.
const MODE_GRAY: u8 = 2;
/// How long the head takes per row, in µs: 30 mm/s at 8 rows per mm.
const ROW_US: u64 = 4_000;

/// CRC-8, polynomial 0x07, initial value 0, as the printer checks it.
pub fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |crc, &b| {
        let mut c = crc ^ b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 { c << 1 ^ 0x07 } else { c << 1 };
        }
        c
    })
}

/// A command or reply frame.
pub fn frame(cmd: u8, payload: &[u8]) -> Vec<u8> {
    let len = (payload.len() as u16).to_le_bytes();
    let mut f = vec![0x22, 0x21, cmd, 0x00, len[0], len[1]];
    f.extend_from_slice(payload);
    f.push(crc8(payload));
    f.push(0xFF);
    f
}

struct Job {
    rows: usize,
    gray: bool,
    data: Vec<u8>,
    /// Flushed; the rows come out at this time (µs), once known.
    flushed: Option<Option<u64>>,
}

/// The printer: protocol state, battery, and the paper printed so far.
pub struct CatPrinter {
    /// Command bytes not yet framed.
    ctl: Vec<u8>,
    job: Option<Job>,
    pub intensity: u8,
    pub battery: u8,
    /// Every row printed since power-on, [`ROW_BYTES`] each.
    pub paper: Vec<u8>,
    /// Commands that failed their CRC (for tests).
    pub bad_frames: usize,
}

impl Default for CatPrinter {
    fn default() -> Self {
        Self::new()
    }
}

impl CatPrinter {
    pub fn new() -> Self {
        Self {
            ctl: Vec::new(),
            job: None,
            intensity: 0x5D,
            battery: 87,
            paper: Vec::new(),
            bad_frames: 0,
        }
    }

    /// The link went down: a half-sent job is lost.
    pub fn disconnected(&mut self) {
        self.ctl.clear();
        self.job = None;
    }

    pub fn rows(&self) -> usize {
        self.paper.len() / ROW_BYTES
    }

    /// Bytes written to `AE01`. Returns the replies to notify on `AE02`, and
    /// notes for the board's event queue.
    pub fn control(&mut self, data: &[u8], notes: &mut Vec<(&'static str, i64)>) -> Vec<Vec<u8>> {
        self.ctl.extend_from_slice(data);
        let mut replies = Vec::new();
        loop {
            // Resynchronise on the header.
            match self.ctl.windows(2).position(|w| w == [0x22, 0x21]) {
                Some(0) => {}
                Some(n) => drop(self.ctl.drain(..n)),
                None => {
                    self.ctl.clear();
                    break;
                }
            }
            if self.ctl.len() < 6 {
                break;
            }
            let len = u16::from_le_bytes([self.ctl[4], self.ctl[5]]) as usize;
            if self.ctl.len() < 8 + len {
                break;
            }
            let f: Vec<u8> = self.ctl.drain(..8 + len).collect();
            let (cmd, payload) = (f[2], &f[6..6 + len]);
            if f[6 + len] != crc8(payload) || f[7 + len] != 0xFF {
                self.bad_frames += 1;
                continue;
            }
            if let Some(r) = self.command(cmd, payload, notes) {
                replies.push(r);
            }
        }
        replies
    }

    fn command(
        &mut self,
        cmd: u8,
        p: &[u8],
        notes: &mut Vec<(&'static str, i64)>,
    ) -> Option<Vec<u8>> {
        let arg16 = || {
            u16::from_le_bytes([
                p.first().copied().unwrap_or(0),
                p.get(1).copied().unwrap_or(0),
            ]) as usize
        };
        match cmd {
            GET_STATUS => {
                // State, ?, ?, battery %, temperature °C, ?, OK (0), error
                // code, then padding (decoders read as far as byte 13).
                let printing = self.job.is_some() as u8;
                let mut s = vec![printing, 0, 0, self.battery, 31, 0, 0, 0];
                s.resize(14, 0);
                Some(frame(GET_STATUS, &s))
            }
            SET_INTENSITY => {
                self.intensity = p.first().copied().unwrap_or(self.intensity);
                None
            }
            FEED => {
                self.paper.resize(self.paper.len() + arg16() * ROW_BYTES, 0);
                notes.push(("printer_feed", arg16() as i64));
                Some(frame(FEED, &[0]))
            }
            RETRACT => Some(frame(RETRACT, &[0])),
            PRINT => {
                let rows = arg16();
                let ok = self.job.is_none() && rows > 0;
                if ok {
                    self.job = Some(Job {
                        rows,
                        gray: p.get(3) == Some(&MODE_GRAY),
                        data: Vec::new(),
                        flushed: None,
                    });
                }
                Some(frame(PRINT, &[if ok { 0 } else { 1 }]))
            }
            FLUSH => {
                if let Some(j) = &mut self.job {
                    j.flushed.get_or_insert(None);
                }
                None
            }
            CANCEL => {
                self.job = None;
                None
            }
            BATTERY => Some(frame(BATTERY, &[self.battery])),
            GET_VERSION => Some(frame(GET_VERSION, b"MOCK-1.0.0\x001")),
            _ => None,
        }
    }

    /// Bytes written to `AE03`.
    pub fn data(&mut self, data: &[u8]) {
        if let Some(j) = &mut self.job
            && j.flushed.is_none()
        {
            j.data.extend_from_slice(data);
        }
    }

    /// Time passes (`now` in µs): a flushed job comes out once the head is
    /// done with it, and the printer says so.
    pub fn poll(&mut self, now: u64, notes: &mut Vec<(&'static str, i64)>) -> Option<Vec<u8>> {
        let j = self.job.as_mut()?;
        let due = *j
            .flushed
            .as_mut()?
            .get_or_insert(now + j.rows as u64 * ROW_US);
        if now < due {
            return None;
        }
        let j = self.job.take()?;
        let row_bytes = if j.gray { WIDTH / 2 } else { ROW_BYTES };
        for r in 0..j.rows {
            let src = j.data.get(r * row_bytes..).unwrap_or(&[]);
            let mut row = [0u8; ROW_BYTES];
            for (x, dot) in row_dots(src, j.gray).enumerate().take(WIDTH) {
                row[x / 8] |= (dot as u8) << (x % 8);
            }
            self.paper.extend_from_slice(&row);
        }
        notes.push(("printed", j.rows as i64));
        Some(frame(PRINT_COMPLETE, &[0]))
    }
}

/// The dots of one row, black = true. Gray rows carry two dots per byte,
/// the left one in the high nibble; half intensity and up burns.
fn row_dots(src: &[u8], gray: bool) -> impl Iterator<Item = bool> + '_ {
    (0..WIDTH).map(move |x| {
        if gray {
            src.get(x / 2)
                .is_some_and(|b| (if x % 2 == 0 { b >> 4 } else { b & 15 }) >= 8)
        } else {
            src.get(x / 8).is_some_and(|b| b >> (x % 8) & 1 != 0)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_and_framing_match_the_printer() {
        // Entries of the vendor app's CRC table.
        assert_eq!(crc8(&[1]), 0x07);
        assert_eq!(crc8(&[0x5D]), 0x94);
        assert_eq!(
            frame(GET_STATUS, &[0]),
            [0x22, 0x21, 0xA1, 0x00, 1, 0, 0, 0, 0xFF]
        );
    }

    #[test]
    fn a_job_prints_its_rows_after_the_flush() {
        let (mut p, mut notes) = (CatPrinter::new(), Vec::new());
        // A command split across two writes still parses.
        let req = frame(PRINT, &[2, 0, 0x30, 0]);
        assert!(p.control(&req[..5], &mut notes).is_empty());
        assert_eq!(p.control(&req[5..], &mut notes), [frame(PRINT, &[0])]);
        let mut rows = vec![0u8; 2 * ROW_BYTES];
        rows[0] = 0b0000_0101; // dots 0 and 2 of row 0
        rows[ROW_BYTES + 47] = 0x80; // the last dot of row 1
        p.data(&rows[..20]);
        p.data(&rows[20..]);
        assert!(p.poll(0, &mut notes).is_none(), "not flushed");
        assert!(p.control(&frame(FLUSH, &[0]), &mut notes).is_empty());
        assert!(
            p.poll(1000, &mut notes).is_none(),
            "the head takes its time"
        );
        assert_eq!(
            p.poll(1000 + 2 * ROW_US, &mut notes),
            Some(frame(PRINT_COMPLETE, &[0]))
        );
        assert_eq!(p.paper, rows);
        assert_eq!(notes, [("printed", 2)]);
    }

    #[test]
    fn bad_crcs_are_dropped_and_status_reports_the_battery() {
        let (mut p, mut notes) = (CatPrinter::new(), Vec::new());
        let mut bad = frame(GET_STATUS, &[0]);
        bad[6] = 1;
        assert!(p.control(&bad, &mut notes).is_empty());
        assert_eq!(p.bad_frames, 1);
        let st = p.control(&frame(GET_STATUS, &[0]), &mut notes);
        assert_eq!((st[0][6 + 3], st[0][6 + 6]), (87, 0), "battery, OK");
    }

    #[test]
    fn gray_rows_burn_from_half_intensity() {
        let (mut p, mut notes) = (CatPrinter::new(), Vec::new());
        p.control(&frame(PRINT, &[1, 0, 0x30, MODE_GRAY]), &mut notes);
        let mut row = vec![0u8; WIDTH / 2];
        row[0] = 0x7F; // dot 0 light, dot 1 dark
        p.data(&row);
        p.control(&frame(FLUSH, &[0]), &mut notes);
        p.poll(0, &mut notes);
        p.poll(ROW_US, &mut notes);
        assert_eq!(p.paper[0], 0b10);
    }
}
