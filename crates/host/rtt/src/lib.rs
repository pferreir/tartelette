//! The host side of defmt over RTT, for firmware running in the emulator.
//!
//! A defmt-rtt firmware writes encoded log frames into a ring buffer inside
//! its RAM (the SEGGER RTT control block, found by its `_SEGGER_RTT` symbol)
//! and relies on a debug probe to read them out. This does what the probe
//! does: between emulator steps it empties up-channel 0 from RAM, bumping the
//! read offset, and decodes the frames with the string table in the ELF.
//! Reading RAM directly (not through the bus) leaves the emulation untouched.
use defmt_decoder::{DecodeError, StreamDecoder, Table};
use object::{Object, ObjectSymbol};

/// Offsets inside the control block (`SEGGER_RTT_CB`) and its first up buffer.
const ID: &[u8] = b"SEGGER RTT";
const MAX_UP: u32 = 16;
const UP0: u32 = 24;
const BUF: u32 = UP0 + 4;
const SIZE: u32 = UP0 + 8;
const WRITE: u32 = UP0 + 12;
const READ: u32 = UP0 + 16;
/// A control block is the header plus one up and one down buffer descriptor at least.
const MIN_BLOCK: u32 = UP0 + 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

/// One decoded log message.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    /// None for `println!`-style messages without a level.
    pub level: Option<Level>,
    /// The firmware's own timestamp, if it defines one.
    pub timestamp: Option<String>,
    pub text: String,
}

pub struct Rtt {
    control: u32,
    decoder: Box<dyn StreamDecoder + Send + Sync>,
    /// Frames the decoder could not make sense of.
    pub malformed: u64,
}

impl Rtt {
    /// Prepare to read the log of the firmware in `elf`. `Ok(None)` if it has
    /// no RTT block or no defmt strings; `Err` if the strings cannot be used
    /// (for instance, a defmt version this decoder does not speak).
    pub fn new(elf: &[u8]) -> Result<Option<Self>, String> {
        let file = object::File::parse(elf).map_err(|e| format!("cannot read symbols: {e}"))?;
        let Some(control) = file
            .symbols()
            .find(|s| s.name() == Ok("_SEGGER_RTT"))
            .map(|s| s.address() as u32)
        else {
            return Ok(None);
        };
        let Some(table) = Table::parse(elf).map_err(|e| format!("defmt: {e}"))? else {
            return Ok(None);
        };
        // The stream decoder borrows the table for as long as it lives. A
        // table is small (one string per log call) and made once per run of
        // the firmware, so it is leaked rather than made self-referential.
        let table: &'static Table = Box::leak(Box::new(table));
        Ok(Some(Self {
            control,
            decoder: table.new_stream_decoder(),
            malformed: 0,
        }))
    }

    /// Empty the firmware's RTT buffer and add the messages in it to `out`.
    /// `ram` is the RAM holding the control block, mapped at address `base`.
    pub fn poll(&mut self, ram: &mut [u8], base: u32, out: &mut Vec<Line>) {
        let Some(start) = self.control.checked_sub(base) else {
            return;
        };
        let bytes = drain(ram, base, start as usize);
        if bytes.is_empty() {
            return;
        }
        self.decoder.received(&bytes);
        loop {
            match self.decoder.decode() {
                Ok(frame) => out.push(Line {
                    level: frame.level().map(|l| match l.as_str() {
                        "trace" => Level::Trace,
                        "debug" => Level::Debug,
                        "info" => Level::Info,
                        "warn" => Level::Warn,
                        _ => Level::Error,
                    }),
                    timestamp: frame.display_timestamp().map(|t| t.to_string()),
                    text: frame.display_message().to_string(),
                }),
                Err(DecodeError::UnexpectedEof) => break,
                Err(DecodeError::Malformed) => self.malformed += 1,
            }
        }
    }
}

fn word(ram: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(ram.get(at..at + 4)?.try_into().ok()?))
}

/// Take the bytes waiting in up-channel 0 of the control block at `control`
/// (an offset into `ram`, which is mapped at `base`) and mark them read. Nothing if there is no valid
/// block yet (the firmware writes its id last, once the buffer is set up).
fn drain(ram: &mut [u8], base: u32, control: usize) -> Vec<u8> {
    let at = |off: u32| control + off as usize;
    if ram.get(control..control + ID.len()) != Some(ID) || ram.len() < at(MIN_BLOCK) {
        return Vec::new();
    }
    let (Some(max_up), Some(buf), Some(size), Some(write), Some(read)) = (
        word(ram, at(MAX_UP)),
        word(ram, at(BUF)),
        word(ram, at(SIZE)),
        word(ram, at(WRITE)),
        word(ram, at(READ)),
    ) else {
        return Vec::new();
    };
    let (size, write, read) = (size as usize, write as usize, read as usize);
    let Some(buf) = buf.checked_sub(base).map(|b| b as usize) else {
        return Vec::new();
    };
    if max_up == 0 || size == 0 || write >= size || read >= size || buf + size > ram.len() {
        return Vec::new();
    }
    let ring = &ram[buf..buf + size];
    let bytes = if write >= read {
        ring[read..write].to_vec()
    } else {
        [&ring[read..], &ring[..write]].concat()
    };
    ram[at(READ)..at(READ) + 4].copy_from_slice(&(write as u32).to_le_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where the RAM is mapped (the RP2040's SRAM).
    const BASE: u32 = 0x2000_0000;

    /// RAM holding a control block at offset 0x40 and a ring of `size` bytes at 0x100.
    fn ram(size: u32, write: u32, read: u32) -> Vec<u8> {
        let mut r = vec![0u8; 0x400];
        let put =
            |r: &mut Vec<u8>, off: usize, v: u32| r[off..off + 4].copy_from_slice(&v.to_le_bytes());
        r[0x40..0x40 + ID.len()].copy_from_slice(ID);
        put(&mut r, 0x40 + MAX_UP as usize, 1);
        put(&mut r, 0x40 + BUF as usize, BASE + 0x100);
        put(&mut r, 0x40 + SIZE as usize, size);
        put(&mut r, 0x40 + WRITE as usize, write);
        put(&mut r, 0x40 + READ as usize, read);
        for i in 0..size as usize {
            r[0x100 + i] = i as u8;
        }
        r
    }

    #[test]
    fn drains_what_was_written_and_marks_it_read() {
        let mut r = ram(16, 5, 2);
        assert_eq!(drain(&mut r, BASE, 0x40), [2, 3, 4]);
        assert_eq!(word(&r, 0x40 + READ as usize), Some(5));
        assert!(
            drain(&mut r, BASE, 0x40).is_empty(),
            "nothing new the second time"
        );
    }

    #[test]
    fn follows_the_ring_around_its_end() {
        let mut r = ram(16, 3, 14);
        assert_eq!(drain(&mut r, BASE, 0x40), [14, 15, 0, 1, 2]);
        assert_eq!(word(&r, 0x40 + READ as usize), Some(3));
    }

    #[test]
    fn ignores_a_block_that_is_not_set_up_yet_or_is_garbage() {
        let mut r = ram(16, 5, 2);
        r[0x40] = 0; // the id is written last by the firmware
        assert!(drain(&mut r, BASE, 0x40).is_empty());
        let mut r = ram(16, 99, 2); // write offset outside the buffer
        assert!(drain(&mut r, BASE, 0x40).is_empty());
        let mut r = ram(0, 0, 0);
        assert!(drain(&mut r, BASE, 0x40).is_empty());
        assert!(
            drain(&mut r, BASE, 0x3FF).is_empty(),
            "block runs off the end of RAM"
        );
    }
}
