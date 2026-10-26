//! The chip's end of the gSPI wire: command words, data words and the
//! status word, shifted one bit per clock edge.
//!
//! The host (Embassy's `cyw43-pio` program) changes DIO while CLK is low and
//! the chip samples it on the rising edge. In the other direction the chip
//! changes DIO on the *falling* edge, so the host (which samples on the
//! rising edge, in the same PIO instruction that raises CLK) always finds a
//! settled level.
//!
//! One transaction (CS low) is: a command word; for a write, the data words;
//! then, for a read, the data words (preceded by a padding word for the
//! backplane function) and finally one status word.
use super::chip::Chip;
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// CS is high.
    Idle,
    Command,
    /// Receiving the data words of a write.
    WriteData,
    /// Driving the response (read data + status, or just status).
    Output,
}

#[derive(Clone, Copy, Default, Debug)]
struct Command {
    func: u8,
    addr: u32,
    len: usize,
}

pub struct Gspi {
    state: State,
    shift: u32,
    nbits: u32,
    cmd: Command,
    words_in: Vec<u32>,
    need: usize,
    out: Vec<u32>,
    /// Index of the next response bit to drive.
    out_pos: usize,
}

impl Default for Gspi {
    fn default() -> Self {
        Self::new()
    }
}

impl Gspi {
    pub fn new() -> Self {
        Self { state: State::Idle, shift: 0, nbits: 0, cmd: Command::default(), words_in: Vec::new(), need: 0, out: Vec::new(), out_pos: 0 }
    }

    /// The chip is driving DIO (CS low, past the command).
    pub fn outputting(&self) -> bool {
        self.state == State::Output
    }

    pub fn cs_fall(&mut self) {
        *self = Self { state: State::Command, ..Self::new() };
    }

    pub fn cs_rise(&mut self) {
        *self = Self::new();
    }

    /// CLK rose while CS was low: the host's bit is on DIO.
    pub fn clk_rise(&mut self, bit: bool, chip: &mut Chip) {
        if !matches!(self.state, State::Command | State::WriteData) {
            return;
        }
        self.shift = self.shift << 1 | bit as u32;
        self.nbits += 1;
        if self.nbits == 32 {
            let word = core::mem::take(&mut self.shift);
            self.nbits = 0;
            self.word(word, chip);
        }
    }

    /// CLK fell while CS was low. Returns the level the chip puts on DIO.
    pub fn clk_fall(&mut self) -> Option<bool> {
        if self.state != State::Output {
            return None;
        }
        let pos = self.out_pos;
        self.out_pos += 1;
        // Past the end the line is parked low so a leftover 1 isn't mistaken
        // for the interrupt request.
        Some(self.out.get(pos / 32).is_some_and(|w| w >> (31 - pos % 32) & 1 != 0))
    }

    fn word(&mut self, mut w: u32, chip: &mut Chip) {
        let swapped = chip.swapped();
        if swapped {
            w = w.rotate_left(16);
        }
        match self.state {
            State::Command => {
                let write = w >> 31 != 0;
                self.cmd = Command { func: (w >> 28 & 3) as u8, addr: w >> 11 & 0x1_FFFF, len: (w & 0x7FF) as usize };
                if write {
                    self.need = self.cmd.len.div_ceil(4);
                    self.state = State::WriteData;
                    if self.need == 0 {
                        self.finish_write(chip);
                    }
                } else {
                    self.respond_read(chip);
                }
            }
            State::WriteData => {
                self.words_in.push(w);
                if self.words_in.len() == self.need {
                    self.finish_write(chip);
                }
            }
            _ => {}
        }
    }

    fn respond_read(&mut self, chip: &mut Chip) {
        let Command { func, addr, len } = self.cmd;
        let data = chip.read(func, addr, len);
        let mut words = alloc::vec![0u32; chip.pad_words(func)];
        words.extend(data.chunks(4).map(|c| {
            let mut b = [0u8; 4];
            b[..c.len()].copy_from_slice(c);
            u32::from_le_bytes(b)
        }));
        words.push(chip.status());
        self.start_output(words, chip.swapped());
    }

    fn finish_write(&mut self, chip: &mut Chip) {
        let swapped = chip.swapped();
        let mut bytes: Vec<u8> = self.words_in.iter().flat_map(|w| w.to_le_bytes()).collect();
        bytes.truncate(self.cmd.len);
        chip.write(self.cmd.func, self.cmd.addr, &bytes);
        // The status word of this transaction goes out in the framing the
        // host used for it (a write to the bus control register can change that).
        self.start_output(alloc::vec![chip.status()], swapped);
    }

    fn start_output(&mut self, mut words: Vec<u32>, swapped: bool) {
        if swapped {
            for w in &mut words {
                *w = w.rotate_left(16);
            }
        }
        self.out = words;
        self.out_pos = 0;
        self.state = State::Output;
    }
}
