//! The SD card model against the real `embedded-sdmmc` driver.
//!
//! The driver (upstream's version of the code the BTL-01 firmware uses) talks
//! to the card through a `SpiDevice` that sits on a `pcb::Board`, so every byte
//! goes through the board's SPI routing. The driver makes one SPI call per
//! byte and leaves chip select to the caller ("use an SpiDevice with a dummy
//! chip-select"), so here CS is simply held low for the whole session, as if
//! the card's CS were wired to ground. That checks
//! the card against an independent implementation of the SD SPI protocol:
//! initialisation (with CRC checking on and off), the CSD capacity, single and
//! multi-block reads and writes, and a real FAT16 file system on top.
use core::convert::Infallible;
use embedded_hal::delay::DelayNs;
use embedded_hal::spi::{ErrorType, Operation, SpiDevice};
use embedded_sdmmc::embedded_sdmmc_types::sdcard::CardType;
use embedded_sdmmc::sdcard::spi::AcquireOpts;
use embedded_sdmmc::{Mode, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use pcb::{Board, Pins, SpiTarget};
use pcb_devices::sdcard::{BLOCK, SdCard as SdModel};
use std::cell::RefCell;
use std::rc::Rc;

const CS: usize = 8;
const IMAGE_MB: usize = 20;

/// Pad levels for the test: the chip side drives CS, devices may drive the rest.
#[derive(Default)]
struct TestPins {
    chip: [bool; 30],
    ext: [Option<bool>; 30],
    seen: u32,
}

impl Pins for TestPins {
    fn level(&self, pin: usize) -> bool {
        self.ext[pin].unwrap_or(self.chip[pin])
    }
    fn drive(&mut self, pin: usize, level: Option<bool>) {
        self.ext[pin] = level;
    }
}

impl TestPins {
    fn take_changes(&mut self) -> u32 {
        let now = (0..30).fold(0, |m, p| m | (self.level(p) as u32) << p);
        std::mem::replace(&mut self.seen, now) ^ now
    }
}

struct Bench {
    board: Board,
    pins: TestPins,
}

impl Bench {
    fn set_cs(&mut self, high: bool) {
        self.pins.chip[CS] = high;
        let changed = self.pins.take_changes();
        self.board.service(&mut self.pins, changed, 0);
    }
    fn byte(&mut self, out: u8) -> u8 {
        self.board.spi_bus(1, &mut self.pins, 0).transfer(out as u16, 8) as u8
    }
}

/// An `SpiDevice` over the bench, like `embedded_hal_bus::spi::ExclusiveDevice`
/// (which the BTL-01 OS uses): CS goes low for each transaction and high
/// again after it, so the driver's single-byte reads each pulse CS.
#[derive(Clone)]
struct SdSpi(Rc<RefCell<Bench>>);

impl ErrorType for SdSpi {
    type Error = Infallible;
}

impl SpiDevice<u8> for SdSpi {
    fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Infallible> {
        let mut b = self.0.borrow_mut();
        b.set_cs(false);
        for op in operations {
            match op {
                Operation::Read(buf) => buf.iter_mut().for_each(|x| *x = b.byte(0xFF)),
                Operation::Write(buf) => buf.iter().for_each(|&x| {
                    b.byte(x);
                }),
                Operation::Transfer(read, write) => {
                    for i in 0..read.len().max(write.len()) {
                        let got = b.byte(write.get(i).copied().unwrap_or(0xFF));
                        if let Some(slot) = read.get_mut(i) {
                            *slot = got;
                        }
                    }
                }
                Operation::TransferInPlace(buf) => buf.iter_mut().for_each(|x| *x = b.byte(*x)),
                Operation::DelayNs(_) => {}
            }
        }
        b.set_cs(true);
        Ok(())
    }
}

struct NoDelay;

impl DelayNs for NoDelay {
    fn delay_ns(&mut self, _ns: u32) {}
}

struct FixedTime;

impl TimeSource for FixedTime {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp { year_since_1970: 55, zero_indexed_month: 5, zero_indexed_day: 14, hours: 12, minutes: 30, seconds: 0 }
    }
}

fn bench_with_image(image: Vec<u8>) -> Rc<RefCell<Bench>> {
    let text = format!("sdcard sd bus=spi1 cs={CS} size_mb={IMAGE_MB}");
    let mut board = Board::parse_with(&text, &pcb_devices::registry()).unwrap();
    let mut pins = TestPins::default();
    pins.chip[CS] = true;
    board.attach(&mut pins);
    pins.take_changes();
    board.device_mut::<SdModel>("sd").unwrap().load_image(image);
    Rc::new(RefCell::new(Bench { board, pins }))
}

fn new_card(bench: &Rc<RefCell<Bench>>, use_crc: bool) -> SdCard<SdSpi, NoDelay> {
    let opts = AcquireOpts { use_crc, ..AcquireOpts::default() };
    SdCard::new_with_options(SdSpi(bench.clone()), NoDelay, opts)
}

/// An MBR with one FAT16 partition and a freshly formatted, empty file system in it.
fn formatted_image(megabytes: usize) -> Vec<u8> {
    pcb_devices::fat::blank_fat16_image(megabytes, "", &[]).unwrap()
}

/// A pattern that makes misplaced bytes obvious.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add((i >> 8) as u8).wrapping_add(seed)).collect()
}

#[test]
fn driver_initialises_the_card_and_reads_its_size_with_crc_on_and_off() {
    for use_crc in [true, false] {
        let bench = bench_with_image(formatted_image(IMAGE_MB));
        let card = new_card(&bench, use_crc);
        assert_eq!(card.num_bytes().unwrap(), (IMAGE_MB * 1024 * 1024) as u64, "crc={use_crc}");
        assert_eq!(card.get_card_type(), Some(CardType::SdhcSdxc));
        assert!(card.erase_single_block_enabled().unwrap());
    }
}

#[test]
fn files_round_trip_through_a_fat_filesystem() {
    let bench = bench_with_image(formatted_image(IMAGE_MB));
    let small = b"Hello from the emulated BTL-01!\n".to_vec();
    let big = pattern(10_000, 7); // several clusters: multi-block reads and writes

    {
        let volume_mgr = VolumeManager::new(new_card(&bench, true), FixedTime);
        let volume = volume_mgr.open_volume(VolumeIdx(0)).expect("open volume 0");
        let root = volume.open_root_dir().expect("root dir");
        let file = root.open_file_in_dir("HELLO.TXT", Mode::ReadWriteCreate).expect("create file");
        file.write(&small).expect("write small");
        drop(file);
        let file = root.open_file_in_dir("BIG.BIN", Mode::ReadWriteCreate).expect("create big file");
        file.write(&big).expect("write big");
    }
    assert!(bench.borrow().board.device::<SdModel>("sd").unwrap().dirty());

    // A fresh driver instance (the card is re-initialised) sees the same files.
    let volume_mgr = VolumeManager::new(new_card(&bench, true), FixedTime);
    let volume = volume_mgr.open_volume(VolumeIdx(0)).expect("reopen volume");
    let root = volume.open_root_dir().expect("root dir");
    let mut names = Vec::new();
    root.iterate_dir(|entry| {
        names.push((entry.name.to_string(), entry.size));
        std::ops::ControlFlow::Continue(())
    })
    .expect("list root");
    names.sort();
    assert_eq!(names, [("BIG.BIN".to_string(), 10_000), ("HELLO.TXT".to_string(), small.len() as u32)]);

    let read_all = |name: &str| {
        let file = root.open_file_in_dir(name, Mode::ReadOnly).expect("open for read");
        let mut data = vec![0u8; file.length() as usize];
        let n = file.read(&mut data).expect("read");
        assert_eq!(n, data.len());
        assert!(file.is_eof());
        data
    };
    assert_eq!(read_all("HELLO.TXT"), small);
    assert_eq!(read_all("BIG.BIN"), big);
}

#[test]
fn a_volume_label_shows_up_as_an_empty_directory_entry() {
    let bench = bench_with_image(pcb_devices::fat::blank_fat16_image(IMAGE_MB, "BTL01", &[]).unwrap());
    let volume_mgr = VolumeManager::new(new_card(&bench, true), FixedTime);
    let volume = volume_mgr.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let mut names = Vec::new();
    root.iterate_dir(|entry| {
        names.push((entry.name.to_string(), entry.size));
        std::ops::ControlFlow::Continue(())
    })
    .unwrap();
    assert_eq!(names, [("BTL01".to_string(), 0)]);
}

#[test]
fn a_formatted_directory_can_be_entered_and_written_to() {
    let bench = bench_with_image(pcb_devices::fat::blank_fat16_image(IMAGE_MB, "", &["data", "songs"]).unwrap());
    let payload = pattern(3000, 3);
    {
        let volume_mgr = VolumeManager::new(new_card(&bench, true), FixedTime);
        let volume = volume_mgr.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let data = root.open_dir("data").expect("the data directory exists");
        let file = data.open_file_in_dir("PATCH.BIN", Mode::ReadWriteCreate).unwrap();
        file.write(&payload).unwrap();
    }
    let volume_mgr = VolumeManager::new(new_card(&bench, true), FixedTime);
    let volume = volume_mgr.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let list = |dir: &embedded_sdmmc::Directory<'_, _, _, 4, 4, 1>| {
        let mut names = Vec::new();
        dir.iterate_dir(|e| {
            names.push(e.name.to_string());
            std::ops::ControlFlow::Continue(())
        })
        .unwrap();
        names.sort();
        names
    };
    assert_eq!(list(&root), ["DATA", "SONGS"]);
    let data = root.open_dir("data").unwrap();
    assert_eq!(list(&data), [".", "..", "PATCH.BIN"]);
    let file = data.open_file_in_dir("PATCH.BIN", Mode::ReadOnly).unwrap();
    let mut back = vec![0u8; file.length() as usize];
    file.read(&mut back).unwrap();
    assert_eq!(back, payload);
    assert!(root.open_dir("songs").is_ok());
}

#[test]
fn every_formatted_size_mounts_and_keeps_a_file() {
    // Different sizes get different cluster and FAT sizes.
    for mb in [pcb_devices::fat::MIN_MB, 33, 100] {
        let bench = bench_with_image(formatted_image(mb));
        let data = pattern(70_000, mb as u8);
        {
            let volume_mgr = VolumeManager::new(new_card(&bench, true), FixedTime);
            let volume = volume_mgr.open_volume(VolumeIdx(0)).unwrap_or_else(|e| panic!("{mb} MiB: open volume: {e:?}"));
            let root = volume.open_root_dir().unwrap();
            let file = root.open_file_in_dir("DATA.BIN", Mode::ReadWriteCreate).unwrap();
            file.write(&data).unwrap();
        }
        let volume_mgr = VolumeManager::new(new_card(&bench, true), FixedTime);
        let volume = volume_mgr.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let file = root.open_file_in_dir("DATA.BIN", Mode::ReadOnly).unwrap();
        let mut back = vec![0u8; file.length() as usize];
        assert_eq!(file.read(&mut back).unwrap(), data.len(), "{mb} MiB");
        assert_eq!(back, data, "{mb} MiB");
    }
}

#[test]
fn appending_and_overwriting_preserve_the_rest_of_the_disk() {
    let bench = bench_with_image(formatted_image(IMAGE_MB));
    let untouched: Vec<u8> = bench.borrow().board.device::<SdModel>("sd").unwrap().image()[..2048 * BLOCK].to_vec();
    {
        let volume_mgr = VolumeManager::new(new_card(&bench, false), FixedTime);
        let volume = volume_mgr.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let log = root.open_file_in_dir("LOG.TXT", Mode::ReadWriteCreateOrAppend).unwrap();
        log.write(b"line one\n").unwrap();
    }
    {
        let volume_mgr = VolumeManager::new(new_card(&bench, false), FixedTime);
        let volume = volume_mgr.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let log = root.open_file_in_dir("LOG.TXT", Mode::ReadWriteCreateOrAppend).unwrap();
        log.write(b"line two\n").unwrap();
    }
    let volume_mgr = VolumeManager::new(new_card(&bench, false), FixedTime);
    let volume = volume_mgr.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let log = root.open_file_in_dir("LOG.TXT", Mode::ReadOnly).unwrap();
    let mut data = vec![0u8; 64];
    let n = log.read(&mut data).unwrap();
    assert_eq!(&data[..n], b"line one\nline two\n");
    // The MBR and the sectors before the partition were never touched.
    assert_eq!(bench.borrow().board.device::<SdModel>("sd").unwrap().image()[..2048 * BLOCK], untouched[..]);
}

/// Mount a real disk image made by the upstream project (set `UPSTREAM_DISK_IMG`
/// to its `tests/disk.img` after un-gzipping) and read the file its own tests
/// check by hash: a byte-exact check of reads against a filesystem we didn't build.
#[test]
#[ignore = "needs the upstream embedded-sdmmc tests/disk.img (see UPSTREAM_DISK_IMG)"]
fn reads_the_upstream_test_disk_image() {
    use sha2::{Digest, Sha256};
    const TEST_DAT_SHA256: &str = "59e3468e3bef8bfe37e60a8221a1896e105b80a61a23637612ac8cd24ca04a75";
    let path = std::env::var("UPSTREAM_DISK_IMG").expect("UPSTREAM_DISK_IMG");
    let bench = bench_with_image(std::fs::read(path).unwrap());
    let volume_mgr = VolumeManager::new(new_card(&bench, true), FixedTime);
    let volume = volume_mgr.open_volume(VolumeIdx(0)).expect("open the FAT16 volume");
    let root = volume.open_root_dir().expect("root dir");
    let dir = root.open_dir("TEST").expect("TEST dir");
    let file = dir.open_file_in_dir("TEST.DAT", Mode::ReadOnly).expect("TEST.DAT");
    let mut data = vec![0u8; 4096];
    let n = file.read(&mut data).expect("read");
    assert_eq!(n, 3500);
    let hash = Sha256::digest(&data[..n]);
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, TEST_DAT_SHA256);
    // The second partition is FAT32.
    drop((file, dir, root, volume));
    let volume1 = volume_mgr.open_volume(VolumeIdx(1)).expect("open the FAT32 volume");
    volume1.open_root_dir().expect("FAT32 root dir");
}
