//! An SDHC memory card in SPI mode, backed by an in-memory disk image.
//!
//! The card starts in native SD mode and ignores everything until `CMD0`
//! with chip select low switches it to SPI. Implemented commands: `CMD0`,
//! `CMD8` (v2 interface condition), `CMD9` (CSD v2) and `CMD10` (CID),
//! `CMD12` (stop a multi-block read), `CMD13`, `CMD16`, `CMD17`/`CMD18`
//! (single/multiple block read), `CMD24`/`CMD25` (single/multiple block
//! write), `CMD55` + `ACMD41` (initialisation, which takes a few polls) and
//! `ACMD23`, `CMD58` (OCR, CCS set: block addressing) and `CMD59` (CRC on/off).
//! Command CRC7 is always checked for `CMD0`/`CMD8` and for everything once
//! CRC is switched on; data blocks carry and are checked against CRC16 the
//! same way. Anything else answers "illegal command".
//!
//! Timing is per byte, not per clock: responses start one byte after the
//! command, data blocks one byte after the response, and a write keeps the
//! card busy (data line low) for [`WRITE_BUSY_BYTES`] bytes. Not modelled:
//! SD (non-HC) byte addressing and CSD v1, erase commands, write protect,
//! and the card's native-mode behaviour.
//!
//! The host loads or inspects the disk with [`SdCard::load_image`] and
//! [`SdCard::image`]; [`SdCard::dirty`] says whether the firmware wrote to it.
use alloc::{boxed::Box, collections::VecDeque, vec, vec::Vec};

use pcb::{BusType, Device, Io, Kind, Param, SpiDevice};

pub const BLOCK: usize = 512;
/// Bytes of "busy" (data line held low) after each programmed block.
pub const WRITE_BUSY_BYTES: u32 = 6;
/// The smallest capacity a CSD v2 can describe (512 KiB).
const MIN_BLOCKS: usize = 1024;
/// `ACMD41` polls answered "still initialising" before the card is ready.
const INIT_POLLS: u8 = 3;

// R1 bits
const R1_IDLE: u8 = 0x01;
const R1_ILLEGAL: u8 = 0x04;
const R1_CRC_ERROR: u8 = 0x08;
const R1_PARAMETER: u8 = 0x40;

const TOKEN_SINGLE: u8 = 0xFE;
const TOKEN_MULTI: u8 = 0xFC;
const TOKEN_STOP_TRAN: u8 = 0xFD;
const RESP_ACCEPTED: u8 = 0x05;
const RESP_CRC_ERROR: u8 = 0x0B;
const RESP_WRITE_ERROR: u8 = 0x0D;

/// CRC-7 of a command's first five bytes (polynomial x^7 + x^3 + 1).
pub fn crc7(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in data {
        for i in (0..8).rev() {
            let top = crc >> 6 & 1;
            crc = (crc << 1) & 0x7F;
            if top ^ (byte >> i & 1) != 0 {
                crc ^= 0x09;
            }
        }
    }
    crc
}

/// CRC-16/XMODEM (polynomial 0x1021, initial value 0) as used on data blocks.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                crc << 1 ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// What the card is waiting for from the host, besides the next command.
enum Rx {
    Command,
    /// A write was accepted; waiting for the data token (`multi` = CMD25).
    WriteToken {
        multi: bool,
    },
    /// Collecting a data block and its CRC16.
    WriteData {
        multi: bool,
        buf: Vec<u8>,
    },
}

pub struct SdCard {
    image: Vec<u8>,
    /// Native mode until CMD0 with CS low.
    spi_mode: bool,
    idle: bool,
    acmd41_polls: u8,
    app_cmd: bool,
    crc_on: bool,
    cmd: [u8; 6],
    cmd_len: usize,
    rx: Rx,
    /// Bytes the card drives next, then busy bytes, then idle-high.
    out: VecDeque<u8>,
    busy: u32,
    /// Next block of an open CMD18 stream / CMD25 write.
    next_block: u32,
    streaming: bool,
    /// Blocks transferred and whether the image was written.
    pub blocks_read: u32,
    pub blocks_written: u32,
    dirty: bool,
}

pub const KIND: Kind = Kind {
    name: "sdcard",
    summary: "SDHC card in SPI mode over a disk image",
    bus: Some(BusType::Spi),
    params: &[Param::int(
        "size_mb",
        1,
        2048,
        "size of the blank card in MiB (a loaded image sets its own)",
    )
    .default_int(16)],
    build: |p| {
        Ok(Box::new(SdCard::new(
            p.int("size_mb") as usize * 1024 * 1024 / BLOCK,
        )))
    },
};

impl SdCard {
    /// A blank card of `blocks` 512-byte blocks.
    pub fn new(blocks: usize) -> Self {
        let mut card = Self {
            image: Vec::new(),
            spi_mode: false,
            idle: true,
            acmd41_polls: 0,
            app_cmd: false,
            crc_on: false,
            cmd: [0; 6],
            cmd_len: 0,
            rx: Rx::Command,
            out: VecDeque::new(),
            busy: 0,
            next_block: 0,
            streaming: false,
            blocks_read: 0,
            blocks_written: 0,
            dirty: false,
        };
        card.load_image(vec![0; blocks * BLOCK]);
        card.dirty = false;
        card
    }

    /// Replace the disk contents. The image is padded to whole blocks and to
    /// the 512 KiB minimum a CSD can describe. Does not reset the card.
    pub fn load_image(&mut self, mut bytes: Vec<u8>) {
        let len = bytes.len().div_ceil(BLOCK).max(MIN_BLOCKS) * BLOCK;
        bytes.resize(len, 0);
        self.image = bytes;
        self.dirty = false;
    }

    /// The whole disk image.
    pub fn image(&self) -> &[u8] {
        &self.image
    }

    /// Capacity in 512-byte blocks.
    pub fn blocks(&self) -> u32 {
        (self.image.len() / BLOCK) as u32
    }

    /// The firmware has written to the card since the image was loaded.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    fn r1(&self, bits: u8) -> u8 {
        bits | if self.idle { R1_IDLE } else { 0 }
    }

    /// Answer a command: one byte of gap, R1, then `extra`.
    fn respond(&mut self, r1: u8, extra: &[u8]) {
        self.out.clear();
        self.out.push_back(0xFF);
        self.out.push_back(r1);
        self.out.extend(extra);
    }

    /// Queue a data block: token, data, CRC16.
    fn queue_block(&mut self, data: &[u8]) {
        self.out.push_back(TOKEN_SINGLE);
        self.out.extend(data);
        self.out.extend(crc16(data).to_be_bytes());
    }

    /// The 16-byte CSD register, version 2.
    fn csd(&self) -> [u8; 16] {
        let c_size = (self.blocks() / 1024).saturating_sub(1);
        let mut csd = [
            0x40, // CSD_STRUCTURE = 1
            0x0E,
            0x00,
            0x32, // TAAC, NSAC, TRAN_SPEED (25 MHz)
            0x5B,
            0x59, // CCC, READ_BL_LEN = 9
            0x00,
            (c_size >> 16) as u8 & 0x3F,
            (c_size >> 8) as u8,
            c_size as u8,
            0x7F, // ERASE_BLK_EN, SECTOR_SIZE = 127
            0x80, // SECTOR_SIZE low bit, WP_GRP_SIZE = 0
            0x0A, // R2W_FACTOR = 2, WRITE_BL_LEN high bits
            0x40, // WRITE_BL_LEN low bits
            0x00,
            0x00,
        ];
        csd[15] = crc7(&csd[..15]) << 1 | 1;
        csd
    }

    fn cid(&self) -> [u8; 16] {
        let mut cid = *b"\x00SDemu\x00\x10\x00\x00\x00\x01\x00\x01\x00\x00";
        cid[15] = crc7(&cid[..15]) << 1 | 1;
        cid
    }

    fn block(&self, n: u32) -> Option<&[u8]> {
        let at = n as usize * BLOCK;
        self.image.get(at..at + BLOCK)
    }

    /// A complete command has arrived.
    fn execute(&mut self) {
        let cmd = self.cmd[0] & 0x3F;
        let arg = u32::from_be_bytes([self.cmd[1], self.cmd[2], self.cmd[3], self.cmd[4]]);
        let acmd = core::mem::take(&mut self.app_cmd);
        if !self.spi_mode {
            // Native mode: CMD0 with CS low enters SPI mode; the rest is not for us.
            if cmd == 0 {
                self.spi_mode = true;
            } else {
                return;
            }
        }
        if (cmd == 0 || cmd == 8 || self.crc_on) && self.cmd[5] != (crc7(&self.cmd[..5]) << 1 | 1) {
            return self.respond(self.r1(R1_CRC_ERROR), &[]);
        }
        // Until initialisation completes the card only takes the init commands.
        let idle_ok = matches!((acmd, cmd), (false, 0 | 8 | 13 | 55 | 58 | 59) | (true, 41));
        if self.idle && !idle_ok {
            return self.respond(self.r1(R1_ILLEGAL), &[]);
        }
        if cmd != 12 {
            self.streaming = false;
        }
        match (acmd, cmd) {
            (false, 0) => {
                self.idle = true;
                self.acmd41_polls = 0;
                self.crc_on = false;
                self.rx = Rx::Command;
                self.busy = 0;
                self.respond(R1_IDLE, &[]);
            }
            (false, 8) if self.idle => {
                self.respond(R1_IDLE, &[0x00, 0x00, (arg >> 8) as u8 & 0x0F, arg as u8])
            }
            (false, 9) | (false, 10) => {
                let reg = if cmd == 9 { self.csd() } else { self.cid() };
                self.respond(self.r1(0), &[0xFF]);
                self.queue_block(&reg);
            }
            (false, 12) => {
                self.streaming = false;
                // A stuff byte, then R1, then the card is briefly busy.
                self.out.clear();
                self.out.extend([0xFF, 0x00]);
                self.busy = 2;
            }
            (false, 13) => self.respond(self.r1(0), &[0x00]),
            (false, 16) => self.respond(self.r1(0), &[]),
            (false, 17) | (false, 18) => {
                if self.block(arg).is_none() {
                    return self.respond(self.r1(R1_PARAMETER), &[]);
                }
                self.respond(self.r1(0), &[0xFF]);
                self.next_block = arg;
                self.streaming = cmd == 18;
                self.queue_next_read_block();
            }
            (false, 24) | (false, 25) => {
                if self.block(arg).is_none() {
                    return self.respond(self.r1(R1_PARAMETER), &[]);
                }
                self.next_block = arg;
                self.rx = Rx::WriteToken { multi: cmd == 25 };
                self.respond(self.r1(0), &[]);
            }
            (false, 55) => {
                self.app_cmd = true;
                self.respond(self.r1(0), &[]);
            }
            (true, 23) => self.respond(self.r1(0), &[]),
            (true, 41) => {
                if self.idle {
                    self.acmd41_polls += 1;
                    if self.acmd41_polls >= INIT_POLLS {
                        self.idle = false;
                    }
                }
                self.respond(self.r1(0), &[]);
            }
            (false, 58) => {
                let ocr: u32 = if self.idle { 0x00FF_8000 } else { 0xC0FF_8000 };
                self.respond(self.r1(0), &ocr.to_be_bytes());
            }
            (false, 59) => {
                self.crc_on = arg & 1 != 0;
                self.respond(self.r1(0), &[]);
            }
            _ => self.respond(self.r1(R1_ILLEGAL), &[]),
        }
    }

    /// Put the next block of the current read onto the line.
    fn queue_next_read_block(&mut self) {
        if let Some(data) = self.block(self.next_block).map(|b| b.to_vec()) {
            self.queue_block(&data);
            self.blocks_read += 1;
            self.next_block += 1;
        } else {
            self.streaming = false;
        }
    }

    /// Process one byte from the host.
    fn receive(&mut self, byte: u8) {
        match &mut self.rx {
            Rx::Command => {
                if self.cmd_len == 0 {
                    if byte & 0xC0 == 0x40 {
                        self.cmd[0] = byte;
                        self.cmd_len = 1;
                    }
                } else {
                    self.cmd[self.cmd_len] = byte;
                    self.cmd_len += 1;
                    if self.cmd_len == 6 {
                        self.cmd_len = 0;
                        self.execute();
                    }
                }
            }
            Rx::WriteToken { multi } => match byte {
                TOKEN_SINGLE if !*multi => {
                    self.rx = Rx::WriteData {
                        multi: false,
                        buf: Vec::with_capacity(BLOCK + 2),
                    }
                }
                TOKEN_MULTI if *multi => {
                    self.rx = Rx::WriteData {
                        multi: true,
                        buf: Vec::with_capacity(BLOCK + 2),
                    }
                }
                TOKEN_STOP_TRAN if *multi => {
                    self.rx = Rx::Command;
                    self.busy = WRITE_BUSY_BYTES;
                }
                _ => {}
            },
            Rx::WriteData { multi, buf } => {
                buf.push(byte);
                if buf.len() == BLOCK + 2 {
                    let multi = *multi;
                    let data = core::mem::take(buf);
                    self.finish_write(multi, &data);
                }
            }
        }
    }

    fn finish_write(&mut self, multi: bool, data: &[u8]) {
        let (block, crc) = data.split_at(BLOCK);
        let response = if self.crc_on && crc16(block).to_be_bytes() != [crc[0], crc[1]] {
            RESP_CRC_ERROR
        } else {
            let at = self.next_block as usize * BLOCK;
            match self.image.get_mut(at..at + BLOCK) {
                Some(slot) => {
                    slot.copy_from_slice(block);
                    self.dirty = true;
                    self.blocks_written += 1;
                    self.next_block += 1;
                    RESP_ACCEPTED
                }
                None => RESP_WRITE_ERROR,
            }
        };
        self.out.clear();
        self.out.push_back(response);
        self.busy = WRITE_BUSY_BYTES;
        self.rx = if multi && response == RESP_ACCEPTED {
            Rx::WriteToken { multi: true }
        } else {
            Rx::Command
        };
    }
}

impl Device for SdCard {
    fn spi(&mut self) -> Option<&mut dyn SpiDevice> {
        Some(self)
    }
}

impl SpiDevice for SdCard {
    fn select(&mut self, _io: &mut Io, active: bool) {
        if !active && matches!(self.rx, Rx::Command) {
            // Releasing CS drops a half-received command. Everything else
            // just waits for CS to come back: the response, a data block
            // being read or written, the busy time. Hosts like
            // `embedded_hal_bus::spi::ExclusiveDevice` pulse CS around every
            // single byte they read, and real cards cope with that.
            self.cmd_len = 0;
        }
    }

    fn transfer(&mut self, _io: &mut Io, mosi: u16, _bits: u8) -> u16 {
        if self.streaming && self.out.is_empty() {
            self.queue_next_read_block();
        }
        let miso = match self.out.pop_front() {
            Some(b) => b,
            None if self.busy > 0 => {
                self.busy -= 1;
                0x00
            }
            None => 0xFF,
        };
        self.receive(mosi as u8);
        miso as u16
    }
}

#[cfg(test)]
mod tests {
    use pcb::Board;

    use super::*;
    use crate::testutil::FakePins;

    const CS: usize = 8;

    /// A host driving the card through a board, the way a firmware would.
    struct Host {
        board: Board,
        pins: FakePins,
    }

    impl Host {
        fn new() -> Self {
            let text = alloc::format!(
                "buses.spi1 = {{ sck = 10, mosi = 11, miso = 12 }}\nparts.sd = {{ kind = \"sdcard\", bus = \"spi1\", cs = {CS}, size_mb = 1 }}"
            );
            let mut board = Board::parse_with(&text, &crate::registry()).unwrap();
            let mut pins = FakePins::default();
            pins.set_chip(CS, true);
            board.attach(&mut pins);
            Host { board, pins }
        }
        fn cs(&mut self, high: bool) {
            self.pins.set_chip(CS, high);
            let changed = self.pins.take_changes();
            self.board.service(&mut self.pins, changed, 0);
        }
        fn byte(&mut self, out: u8) -> u8 {
            use pcb::SpiTarget;
            self.board
                .spi_bus(1, &mut self.pins, 0)
                .transfer(out as u16, 8) as u8
        }
        fn card(&self) -> &SdCard {
            self.board.device::<SdCard>("sd").unwrap()
        }
        fn card_mut(&mut self) -> &mut SdCard {
            self.board.device_mut::<SdCard>("sd").unwrap()
        }
        /// Send a command and return R1 (the first byte with bit 7 clear).
        fn command(&mut self, cmd: u8, arg: u32) -> u8 {
            let mut frame = [
                0x40 | cmd,
                (arg >> 24) as u8,
                (arg >> 16) as u8,
                (arg >> 8) as u8,
                arg as u8,
                0,
            ];
            frame[5] = crc7(&frame[..5]) << 1 | 1;
            for b in frame {
                self.byte(b);
            }
            for _ in 0..10 {
                let r = self.byte(0xFF);
                if r & 0x80 == 0 {
                    return r;
                }
            }
            panic!("no response to CMD{cmd}");
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.byte(0xFF)).collect()
        }
        /// Read a data block: wait for the token, then 512 bytes and the CRC.
        fn data_block(&mut self, len: usize) -> (Vec<u8>, u16) {
            for _ in 0..16 {
                let t = self.byte(0xFF);
                if t == TOKEN_SINGLE {
                    let data = self.bytes(len);
                    let crc = u16::from_be_bytes([self.byte(0xFF), self.byte(0xFF)]);
                    return (data, crc);
                }
                assert_eq!(t, 0xFF, "unexpected byte before the data token");
            }
            panic!("no data token");
        }
        /// The standard initialisation sequence; leaves CS low.
        fn init(&mut self) {
            self.cs(true);
            self.bytes(10);
            self.cs(false);
            assert_eq!(self.command(0, 0), R1_IDLE);
            assert_eq!(self.command(59, 1), R1_IDLE);
            assert_eq!(self.command(8, 0x1AA), R1_IDLE);
            assert_eq!(self.bytes(4), [0, 0, 1, 0xAA]);
            let mut polls = 0;
            loop {
                assert_eq!(self.command(55, 0) & !R1_IDLE, 0);
                if self.command(41, 0x4000_0000) == 0 {
                    break;
                }
                polls += 1;
                assert!(polls < 10);
            }
            assert_eq!(self.command(58, 0), 0);
            assert_eq!(self.bytes(4)[0] & 0xC0, 0xC0, "CCS: block addressed");
        }
        fn write_block(&mut self, block: u32, data: &[u8]) {
            assert_eq!(self.command(24, block), 0);
            self.byte(TOKEN_SINGLE);
            for &b in data {
                self.byte(b);
            }
            for b in crc16(data).to_be_bytes() {
                self.byte(b);
            }
            assert_eq!(self.byte(0xFF) & 0x1F, RESP_ACCEPTED);
            while self.byte(0xFF) != 0xFF {}
        }
    }

    #[test]
    fn crc_known_answers() {
        // The command CRCs every SD init sequence uses, and CRC16 check values.
        assert_eq!(crc7(&[0x40, 0, 0, 0, 0]) << 1 | 1, 0x95);
        assert_eq!(crc7(&[0x48, 0, 0, 0x01, 0xAA]) << 1 | 1, 0x87);
        assert_eq!(crc7(&[0x51, 0, 0, 0, 0]) << 1 | 1, 0x55);
        assert_eq!(crc16(b"123456789"), 0x31C3);
        assert_eq!(crc16(&[0xFF; 512]), 0x7FA1);
        assert_eq!(crc16(&[0; 512]), 0);
    }

    #[test]
    fn ignores_everything_until_cmd0_in_spi_mode() {
        let mut h = Host::new();
        h.cs(false);
        for b in [0x48, 0, 0, 1, 0xAA, 0x87] {
            h.byte(b);
        }
        assert_eq!(h.bytes(8), [0xFF; 8], "no response before CMD0");
        assert_eq!(h.command(0, 0), R1_IDLE);
        assert_eq!(h.command(8, 0x1AA), R1_IDLE);
    }

    #[test]
    fn init_sequence_takes_a_few_acmd41_polls_and_reports_sdhc() {
        let mut h = Host::new();
        h.cs(true);
        assert_eq!(h.bytes(10), [0xFF; 10], "CS high: the card doesn't respond");
        h.cs(false);
        assert_eq!(h.command(0, 0), R1_IDLE);
        assert_eq!(h.command(8, 0x1AA), R1_IDLE);
        assert_eq!(h.bytes(4), [0, 0, 1, 0xAA]);
        assert_eq!(h.command(58, 0), R1_IDLE, "OCR readable while idle");
        assert_eq!(h.bytes(4)[0] & 0x80, 0, "power-up not complete");
        let mut idle_polls = 0;
        loop {
            h.command(55, 0);
            if h.command(41, 0x4000_0000) == 0 {
                break;
            }
            idle_polls += 1;
        }
        assert_eq!(idle_polls, INIT_POLLS as usize - 1);
        assert_eq!(h.command(58, 0), 0);
        assert_eq!(h.bytes(4), [0xC0, 0xFF, 0x80, 0x00]);
    }

    #[test]
    fn bad_commands_and_crc_errors() {
        let mut h = Host::new();
        h.cs(false);
        h.command(0, 0);
        assert_eq!(
            h.command(1, 0),
            R1_IDLE | R1_ILLEGAL,
            "CMD1 is not an SD command"
        );
        assert_eq!(
            h.command(17, 0),
            R1_IDLE | R1_ILLEGAL,
            "no reads before initialisation"
        );
        // A wrong CRC on CMD8 is rejected even with CRC checking off.
        for b in [0x48, 0, 0, 1, 0xAA, 0x01] {
            h.byte(b);
        }
        assert_eq!(h.bytes(2)[1], R1_IDLE | R1_CRC_ERROR);
        // Reading past the end is a parameter error.
        h.init();
        let past = h.card().blocks();
        assert_eq!(h.command(17, past), R1_PARAMETER);
    }

    #[test]
    fn csd_describes_the_capacity_and_carries_valid_crcs() {
        let mut h = Host::new();
        h.init();
        assert_eq!(h.command(9, 0), 0);
        let (csd, crc) = h.data_block(16);
        assert_eq!(crc, crc16(&csd));
        assert_eq!(csd[0] >> 6, 1, "CSD version 2");
        assert_eq!(csd[15], crc7(&csd[..15]) << 1 | 1);
        let c_size = (csd[7] as u32 & 0x3F) << 16 | (csd[8] as u32) << 8 | csd[9] as u32;
        assert_eq!(
            (c_size + 1) as u64 * 512 * 1024,
            h.card().blocks() as u64 * 512
        );
        assert_eq!(csd[10] & 0x40, 0x40, "ERASE_BLK_EN");
    }

    #[test]
    fn single_block_read_returns_the_image_with_a_valid_crc() {
        let mut h = Host::new();
        let mut image = vec![0u8; 1024 * BLOCK];
        for (i, b) in image[BLOCK * 3..BLOCK * 4].iter_mut().enumerate() {
            *b = i as u8 ^ 0xA5;
        }
        h.card_mut().load_image(image);
        h.init();
        assert_eq!(h.command(17, 3), 0);
        let (data, crc) = h.data_block(BLOCK);
        assert_eq!(data[..4], [0xA5, 0xA4, 0xA7, 0xA6]);
        assert_eq!(crc, crc16(&data));
        assert_eq!(h.card().blocks_read, 1);
    }

    #[test]
    fn multi_block_read_streams_until_cmd12() {
        let mut h = Host::new();
        let mut image = vec![0u8; 1024 * BLOCK];
        for blk in 0..3 {
            image[blk * BLOCK..(blk + 1) * BLOCK].fill(0x10 + blk as u8);
        }
        h.card_mut().load_image(image);
        h.init();
        assert_eq!(h.command(18, 0), 0);
        for blk in 0..3u8 {
            let (data, crc) = h.data_block(BLOCK);
            assert!(data.iter().all(|&b| b == 0x10 + blk));
            assert_eq!(crc, crc16(&data));
        }
        // Stop: a stuff byte, then R1, then busy, then ready.
        for b in [0x4C, 0, 0, 0, 0, crc7(&[0x4C, 0, 0, 0, 0]) << 1 | 1] {
            h.byte(b);
        }
        h.byte(0xFF); // stuff byte
        assert_eq!(h.byte(0xFF), 0x00, "R1");
        let mut busy = 0;
        while h.byte(0xFF) != 0xFF {
            busy += 1;
            assert!(busy < 20);
        }
        // The card is usable again afterwards.
        assert_eq!(h.command(13, 0), 0);
    }

    #[test]
    fn single_block_write_checks_crc_and_persists() {
        let mut h = Host::new();
        h.init();
        let data: Vec<u8> = (0..BLOCK).map(|i| (i * 7) as u8).collect();
        h.write_block(5, &data);
        assert_eq!(h.command(13, 0), 0);
        assert_eq!(h.byte(0xFF), 0x00, "R2 status byte");
        assert_eq!(&h.card().image()[5 * BLOCK..6 * BLOCK], data.as_slice());
        assert!(h.card().dirty() && h.card().blocks_written == 1);
        // A corrupted CRC is refused with a CRC-error data response and nothing is written.
        assert_eq!(h.command(24, 6), 0);
        h.byte(TOKEN_SINGLE);
        for _ in 0..BLOCK {
            h.byte(0xEE);
        }
        h.byte(0x00);
        h.byte(0x00);
        assert_eq!(h.byte(0xFF) & 0x1F, RESP_CRC_ERROR);
        assert!(
            h.card().image()[6 * BLOCK..7 * BLOCK]
                .iter()
                .all(|&b| b == 0)
        );
        // Out-of-range writes are parameter errors.
        while h.byte(0xFF) != 0xFF {}
        let past = h.card().blocks();
        assert_eq!(h.command(24, past), R1_PARAMETER);
    }

    #[test]
    fn multi_block_write_with_stop_token() {
        let mut h = Host::new();
        h.init();
        assert_eq!(h.command(55, 0), 0);
        assert_eq!(h.command(23, 3), 0);
        assert_eq!(h.command(25, 10), 0);
        for blk in 0..3u8 {
            h.byte(TOKEN_MULTI);
            let data = vec![0x40 + blk; BLOCK];
            for &b in &data {
                h.byte(b);
            }
            for b in crc16(&data).to_be_bytes() {
                h.byte(b);
            }
            assert_eq!(h.byte(0xFF) & 0x1F, RESP_ACCEPTED);
            while h.byte(0xFF) != 0xFF {}
        }
        h.byte(TOKEN_STOP_TRAN);
        h.cs(true); // the driver raises CS straight away; the card is still busy
        h.cs(false);
        let mut busy = 0;
        while h.byte(0xFF) != 0xFF {
            busy += 1;
        }
        assert!(
            busy > 0 && busy <= WRITE_BUSY_BYTES,
            "busy persisted across CS: {busy}"
        );
        for blk in 0..3usize {
            assert!(
                h.card().image()[(10 + blk) * BLOCK..(11 + blk) * BLOCK]
                    .iter()
                    .all(|&b| b == 0x40 + blk as u8)
            );
        }
        assert_eq!(h.card().blocks_written, 3);
    }

    #[test]
    fn releasing_cs_aborts_a_partial_command_but_keeps_the_card_state() {
        let mut h = Host::new();
        h.init();
        h.byte(0x51); // half a CMD17
        h.byte(0x00);
        h.cs(true);
        h.cs(false);
        // The next command starts fresh and the card is still initialised.
        assert_eq!(h.command(13, 0), 0);
    }

    /// One byte read in its own CS-low transaction, like `ExclusiveDevice`.
    fn pulsed(h: &mut Host, out: u8) -> u8 {
        h.cs(false);
        let r = h.byte(out);
        h.cs(true);
        r
    }

    /// Send a command in one transaction, then poll for R1 a byte at a time.
    fn command_pulsed(h: &mut Host, cmd: u8, arg: u32) -> u8 {
        let mut frame = [
            0x40 | cmd,
            (arg >> 24) as u8,
            (arg >> 16) as u8,
            (arg >> 8) as u8,
            arg as u8,
            0,
        ];
        frame[5] = crc7(&frame[..5]) << 1 | 1;
        h.cs(false);
        for b in frame {
            h.byte(b);
        }
        h.cs(true);
        for _ in 0..10 {
            let r = pulsed(h, 0xFF);
            if r & 0x80 == 0 {
                return r;
            }
        }
        panic!("no response to CMD{cmd}");
    }

    #[test]
    fn a_response_and_read_data_survive_cs_pulsing_around_every_byte() {
        let mut h = Host::new();
        let mut image = vec![0u8; 1024 * BLOCK];
        for blk in 0..3 {
            for (i, b) in image[blk * BLOCK..(blk + 1) * BLOCK].iter_mut().enumerate() {
                *b = i as u8 ^ (0x30 + blk as u8);
            }
        }
        h.card_mut().load_image(image.clone());
        h.init();
        // A single block.
        assert_eq!(command_pulsed(&mut h, 17, 1), 0);
        while pulsed(&mut h, 0xFF) != TOKEN_SINGLE {}
        let data: Vec<u8> = (0..BLOCK).map(|_| pulsed(&mut h, 0xFF)).collect();
        assert_eq!(data, image[BLOCK..2 * BLOCK]);
        let crc = u16::from_be_bytes([pulsed(&mut h, 0xFF), pulsed(&mut h, 0xFF)]);
        assert_eq!(crc, crc16(&data));
        // A multiple block read keeps streaming until CMD12.
        assert_eq!(command_pulsed(&mut h, 18, 0), 0);
        for blk in 0..2 {
            while pulsed(&mut h, 0xFF) != TOKEN_SINGLE {}
            let data: Vec<u8> = (0..BLOCK).map(|_| pulsed(&mut h, 0xFF)).collect();
            assert_eq!(data, image[blk * BLOCK..(blk + 1) * BLOCK], "block {blk}");
            pulsed(&mut h, 0xFF);
            pulsed(&mut h, 0xFF);
        }
        assert_eq!(h.card().blocks_read, 3);
    }

    #[test]
    fn a_block_write_survives_cs_pulsing_around_every_byte() {
        let mut h = Host::new();
        h.init();
        assert_eq!(command_pulsed(&mut h, 24, 7), 0);
        let data: Vec<u8> = (0..BLOCK).map(|i| (i * 7) as u8).collect();
        pulsed(&mut h, TOKEN_SINGLE);
        for &b in &data {
            pulsed(&mut h, b);
        }
        for b in crc16(&data).to_be_bytes() {
            pulsed(&mut h, b);
        }
        assert_eq!(pulsed(&mut h, 0xFF) & 0x1F, RESP_ACCEPTED);
        while pulsed(&mut h, 0xFF) != 0xFF {}
        assert_eq!(h.card().image()[7 * BLOCK..8 * BLOCK], data[..]);
        assert_eq!(h.card().blocks_written, 1);
    }

    #[test]
    fn images_are_padded_and_factory_sizes_the_card() {
        let mut card = SdCard::new(8);
        assert_eq!(card.blocks(), 1024, "padded up to the 512 KiB minimum");
        card.load_image(vec![1; 1024 * BLOCK + 1]);
        assert_eq!(card.blocks(), 1025, "partial block rounded up");
        assert!(!card.dirty());
        let reg = crate::registry();
        let b = Board::parse_with("buses.spi0 = { sck = 18, mosi = 19 }\nparts.s = { kind = \"sdcard\", bus = \"spi0\", cs = 1, size_mb = 4 }", &reg).unwrap();
        assert_eq!(b.device::<SdCard>("s").unwrap().blocks(), 8192);
        assert!(Board::parse_with("buses.spi0 = { sck = 18, mosi = 19 }\nparts.s = { kind = \"sdcard\", bus = \"spi0\", size_mb = 4 }", &reg).is_ok(), "no CS: always selected");
        assert!(
            Board::parse_with("parts.s = { kind = \"sdcard\", cs = 1 }", &reg).is_err(),
            "a chip select only means something on a bus"
        );
        assert!(Board::parse_with("buses.spi0 = { sck = 18, mosi = 19 }\nparts.s = { kind = \"sdcard\", bus = \"spi0\", cs = 1, size_mb = 0 }", &reg).is_err());
    }
}
