//! Reads the log of a real defmt-rtt firmware. Set `DEFMT_FIRMWARE` to its ELF,
//! and `DEFMT_BOARD` to its board file if it needs more than a plain Pico;
//! skipped otherwise.
use rp2040_core::system::{SRAM_BASE, StopReason, System};

#[test]
fn decodes_the_log_of_a_defmt_firmware() {
    let Ok(path) = std::env::var("DEFMT_FIRMWARE") else {
        eprintln!("DEFMT_FIRMWARE not set: skipped");
        return;
    };
    let elf = std::fs::read(path).unwrap();
    let mut rtt = rtt::Rtt::new(&elf)
        .unwrap()
        .expect("firmware has RTT and defmt");
    let mut sys = System::new();
    sys.load_elf(&elf).unwrap();
    if let Ok(board) = std::env::var("DEFMT_BOARD") {
        let board = std::fs::read_to_string(board).unwrap();
        sys.set_board(pcb::Board::parse_with(&board, &pcb_devices::registry()).unwrap());
    }

    let mut lines = Vec::new();
    while sys.cycles() < 125_000_000 {
        match sys.run(250_000) {
            StopReason::Sleeping => sys.idle_until(sys.cycles() + 250_000),
            StopReason::CycleLimit => {}
            r => panic!("{r:?}"),
        }
        rtt.poll(&mut sys.bus.sram.data, SRAM_BASE, &mut lines);
    }
    for l in &lines {
        println!("{:?} {}", l.level, l.text);
    }
    assert!(!lines.is_empty(), "the firmware logs during boot");
    assert_eq!(rtt.malformed, 0);
}
