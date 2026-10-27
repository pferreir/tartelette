//! Reads the log of a real defmt-rtt firmware. Set `BTL_FIRMWARE` to its ELF
//! (the BTL-01 OS built with its `log` feature); skipped otherwise.
use rp2040_core::system::{StopReason, System};

#[test]
fn decodes_the_log_of_the_btl_os() {
    let Ok(path) = std::env::var("BTL_FIRMWARE") else {
        eprintln!("BTL_FIRMWARE not set: skipped");
        return;
    };
    let elf = std::fs::read(path).unwrap();
    let mut rtt = rtt::Rtt::new(&elf)
        .unwrap()
        .expect("firmware has RTT and defmt");
    let mut sys = System::new();
    sys.load_elf(&elf).unwrap();
    let board = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../boards/btl-01.toml"
    ))
    .unwrap();
    sys.set_board(pcb::Board::parse_with(&board, &pcb_devices::registry()).unwrap());

    let mut lines = Vec::new();
    while sys.cycles() < 125_000_000 {
        match sys.run(250_000) {
            StopReason::Sleeping => sys.idle_until(sys.cycles() + 250_000),
            StopReason::CycleLimit => {}
            r => panic!("{r:?}"),
        }
        rtt.poll(&mut sys, &mut lines);
    }
    for l in &lines {
        println!("{:?} {}", l.level, l.text);
    }
    assert!(!lines.is_empty(), "the firmware logs during boot");
    assert!(
        lines
            .iter()
            .any(|l| l.text.contains("Started encoder task")),
        "{lines:?}"
    );
    assert_eq!(rtt.malformed, 0);
}
