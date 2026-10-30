//! End to end: build the bare-metal firmware, run it in the emulator, check
//! what comes out of UART0.
use std::{path::PathBuf, process::Command};

use pcb_devices::is_lit;
use rp2040_core::system::{StopReason, System};

fn build_firmware() -> PathBuf {
    build_named("uart-hello")
}

fn build_named(name: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target_dir = root.join("target/firmware");
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "--release",
            "--target",
            "thumbv6m-none-eabi",
            "--manifest-path",
        ])
        .arg(root.join(format!("firmware/tests/{name}/Cargo.toml")))
        .arg("--target-dir")
        .arg(target_dir.join(name))
        .status()
        .expect("run cargo");
    assert!(status.success(), "firmware build failed");
    target_dir
        .join(name)
        .join(format!("thumbv6m-none-eabi/release/{name}"))
}

const EXPECTED: &str = "Hello from RP2040!\r\n0123456789\nromcpy!\n";

#[test]
fn library_runs_firmware() {
    let elf = std::fs::read(build_firmware()).unwrap();
    let mut sys = System::new();
    sys.load_elf(&elf).unwrap();
    assert_eq!(sys.run(10_000_000), StopReason::Breakpoint(1));
    assert_eq!(String::from_utf8_lossy(&sys.bus.uart[0].tx), EXPECTED);
    assert!(sys.bus.uart[1].tx.is_empty());
}

#[test]
fn cli_prints_uart0_to_stdout() {
    let elf = build_firmware();
    let out = Command::new(env!("CARGO_BIN_EXE_tartelette"))
        .arg(&elf)
        .arg("--headless")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), EXPECTED);
}

#[test]
fn cli_sends_uart0_where_asked() {
    let elf = build_firmware();
    let run = |to: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_tartelette"))
            .arg(&elf)
            .args(["--headless", "--uart0", to])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        (text(&out.stdout), text(&out.stderr))
    };
    let (stdout, stderr) = run("stderr");
    assert!(stdout.is_empty());
    assert!(stderr.starts_with(EXPECTED), "{stderr}");

    let (stdout, stderr) = run("none");
    assert!(stdout.is_empty());
    assert!(!stderr.contains("Hello"), "{stderr}");

    let file = std::env::temp_dir().join(format!("tartelette-uart0-{}.txt", std::process::id()));
    std::fs::write(&file, "old contents").unwrap();
    let (stdout, _) = run(file.to_str().unwrap());
    assert!(stdout.is_empty());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), EXPECTED);
    std::fs::remove_file(&file).unwrap();
}

#[test]
fn timer_alarm_systick_and_pendsv_interrupts() {
    let elf = std::fs::read(build_named("irq-timer")).unwrap();
    let mut sys = System::new();
    sys.load_elf(&elf).unwrap();
    assert_eq!(sys.run(10_000_000), StopReason::Breakpoint(1));
    assert_eq!(
        String::from_utf8_lossy(&sys.bus.uart[0].tx),
        "start\nA ok\nTTT\nP\nirq done\n"
    );
    // ~1000 us alarm + 300 us of SysTick, mostly spent asleep.
    assert!(
        (162_000..170_000).contains(&sys.cycles()),
        "{}",
        sys.cycles()
    );
}

#[test]
fn gpio_output_pulls_and_edge_interrupt() {
    let elf = std::fs::read(build_named("io-test")).unwrap();
    let mut sys = System::new();
    sys.load_elf(&elf).unwrap();
    assert_eq!(sys.run(10_000_000), StopReason::Breakpoint(1));
    assert_eq!(
        String::from_utf8_lossy(&sys.bus.uart[0].tx),
        "start\nfuncsel ok\nout ok\npull ok\nedge ok\nio done\n"
    );
}

fn board(text: &str) -> pcb::Board {
    pcb::Board::parse_with(text, &pcb_devices::registry()).unwrap()
}

const BOARD: &str = "name = \"test\"\nparts.status = { kind = \"led\", gpio = 25 }\nparts.user = { kind = \"button\", gpio = 2 }";

#[test]
fn button_drives_led_through_the_board() {
    let elf = std::fs::read(build_named("button-led")).unwrap();
    let mut sys = System::new();
    sys.load_elf(&elf).unwrap();
    sys.set_board(board(BOARD));

    // Released button reads high through the pull-up: LED stays off.
    assert_eq!(sys.run(125 * 100), StopReason::CycleLimit);
    assert_eq!(is_lit(&sys.board, "status"), Some(false));
    assert!(sys.board.events.is_empty());

    assert!(sys.set_button("user", true));
    sys.run(1000);
    assert_eq!(is_lit(&sys.board, "status"), Some(true));
    assert!(sys.set_button("user", false));
    assert_eq!(sys.run(10_000_000), StopReason::Breakpoint(1));
    assert_eq!(is_lit(&sys.board, "status"), Some(false));
    assert_eq!(String::from_utf8_lossy(&sys.bus.uart[0].tx), "done\n");

    let log: Vec<(usize, bool)> = sys
        .board
        .events
        .iter()
        .map(|e| (e.part, e.value != 0))
        .collect();
    assert_eq!(log, [(1, true), (0, true), (1, false), (0, false)]); // press, led on, release, led off
    assert!(
        sys.board
            .events
            .windows(2)
            .all(|w| w[0].cycles <= w[1].cycles)
    );
}

#[test]
fn cli_logs_board_events_and_honours_press_schedule() {
    let elf = build_named("button-led");
    let dir = std::env::temp_dir().join(format!("rp2040-headless-board-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let board = dir.join("test.toml");
    std::fs::write(&board, BOARD).unwrap();

    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tartelette"))
            .arg(&elf)
            .arg("--headless")
            .arg("--board")
            .arg(&board)
            .args(["--max-cycles", "1000000"])
            .args(extra)
            .output()
            .unwrap()
    };
    let out = run(&["--press", "user@100+50"]);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(stdout, "done\n");
    let lines: Vec<&str> = stderr.lines().collect();
    assert!(lines[0].ends_with("100.0 us] user pressed"), "{stderr}");
    assert!(lines[1].contains("status on"), "{stderr}");
    assert!(lines[2].ends_with("150.0 us] user released"), "{stderr}");
    assert!(lines[3].contains("status off"), "{stderr}");
    assert!(lines[4].contains("bkpt #1 at 0x"), "{stderr}");
    assert_eq!(
        lines.len(),
        5,
        "no core report after a bkpt in thread mode: {stderr}"
    );

    // Without a press the firmware just polls until the cycle limit.
    let out = run(&[]);
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cycle limit"));

    // Unknown buttons and bad board files are rejected up front.
    assert!(!run(&["--press", "nope@1"]).status.success());
    std::fs::write(&board, "parts.x = { kind = \"led\", gpio = 99 }").unwrap();
    let bad = run(&[]);
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("test.toml:1:34: gpio must be a GPIO, 0..29")
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn i2c_controller_talks_to_expander_and_its_interrupt_reaches_the_firmware() {
    let elf = std::fs::read(build_named("i2c-test")).unwrap();
    let mut sys = System::new();
    sys.load_elf(&elf).unwrap();
    sys.set_board(board("buses.i2c1 = { sda = 2, scl = 3 }\nparts.expander = { kind = \"mcp23008\", bus = \"i2c1\", addr = 0x20, int = 16 }"));

    // Runs until the firmware sleeps waiting for the expander's interrupt.
    assert_eq!(sys.run(10_000_000), StopReason::Sleeping);
    assert_eq!(
        String::from_utf8_lossy(&sys.bus.uart[0].tx),
        "start\nwrite ok\nread A0\nstop ok\nrx irq ok\nnack ok\nwaiting\n"
    );
    // A switch on expander pin 0 closes; INT falls and wakes the core.
    assert!(sys.input("expander", "gp0", 0));
    assert_eq!(sys.run(10_000_000), StopReason::Breakpoint(1));
    assert!(String::from_utf8_lossy(&sys.bus.uart[0].tx).ends_with("waiting\nint AE\ni2c done\n"));
}

#[test]
fn btl01_board_file_parses() {
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../boards/btl-01.toml"),
    )
    .unwrap();
    let b = board(&text);
    assert_eq!(b.name, "BTL-01");
    assert_eq!(b.parts.len(), 8);
}

const SCREEN_BOARD: &str = "name = \"test\"\nbuses.spi0 = { sck = 18, mosi = 19, miso = 16 }\nparts.screen = { kind = \"st7789\", bus = \"spi0\", cs = 9, dc = 13, rst = 17 }";

#[test]
fn spi_and_dma_draw_on_the_display() {
    use pcb::Board;
    use pcb_devices::st7789::St7789;
    let elf = std::fs::read(build_named("spi-dma-test")).unwrap();
    let mut sys = System::new();
    sys.load_elf(&elf).unwrap();
    sys.set_board(Board::parse_with(SCREEN_BOARD, &pcb_devices::registry()).unwrap());

    assert_eq!(sys.run(20_000_000), StopReason::Breakpoint(1));
    assert_eq!(
        String::from_utf8_lossy(&sys.bus.uart[0].tx),
        "start\nloopback ok\ncpu draw sent\ndma draw ok\ndma rx ok\nmemcpy ok\nchain ok\nspi done\n"
    );
    let lcd = sys.board.device::<St7789>("screen").unwrap();
    // CPU-driven red block at (0,0)..(9,4), DMA-driven green block at (20,10)..(39,19).
    for y in 0..20 {
        for x in 0..40 {
            let want = match (x, y) {
                (0..=9, 0..=4) => 0xF800,
                (20..=39, 10..=19) => 0x07E0,
                _ => 0,
            };
            assert_eq!(lcd.pixel(x, y), want, "pixel ({x},{y})");
        }
    }
    assert_eq!(lcd.total_pixels, 50 + 200);
    assert!(lcd.is_showing());
    // Both bursts were reported when the firmware released chip select.
    let updates: Vec<_> = sys
        .board
        .events
        .iter()
        .filter(|e| e.label == "update")
        .map(|e| e.value)
        .collect();
    assert_eq!(updates, [50, 200]);
}

#[test]
fn cli_dumps_the_display_and_logs_updates() {
    let elf = build_named("spi-dma-test");
    let dir = std::env::temp_dir().join(format!("rp2040-headless-screen-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (board, shot) = (dir.join("lcd.board"), dir.join("shot.ppm"));
    std::fs::write(&board, SCREEN_BOARD).unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tartelette"))
            .arg(&elf)
            .arg("--headless")
            .arg("--board")
            .arg(&board)
            .args(extra)
            .output()
            .unwrap()
    };
    let out = run(&["--dump-screen", &format!("screen={}", shot.display())]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("screen update=50"), "{stderr}");
    assert!(stderr.contains("screen update=200"), "{stderr}");
    let img = std::fs::read(&shot).unwrap();
    let header = b"P6\n240 320\n255\n";
    assert!(img.starts_with(header));
    let px = |x: usize, y: usize| {
        let at = header.len() + 3 * (y * 240 + x);
        [img[at], img[at + 1], img[at + 2]]
    };
    assert_eq!(img.len(), header.len() + 240 * 320 * 3);
    assert_eq!(
        (px(0, 0), px(9, 4), px(10, 0)),
        ([255, 0, 0], [255, 0, 0], [0, 0, 0])
    );
    assert_eq!(
        (px(20, 10), px(39, 19), px(40, 10)),
        ([0, 255, 0], [0, 255, 0], [0, 0, 0])
    );
    // Unknown or non-display parts are rejected up front.
    assert!(!run(&["--dump-screen", "nope=/dev/null"]).status.success());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn second_core_launches_and_both_cores_run_together() {
    let elf = std::fs::read(build_named("multicore-test")).unwrap();
    let mut sys = System::new();
    sys.load_elf(&elf).unwrap();
    assert_eq!(sys.run(20_000_000), StopReason::Breakpoint(1));
    assert_eq!(
        String::from_utf8_lossy(&sys.bus.uart[0].tx),
        "start\ncore1 launched\ncore1 id 1\nlock ok\ndiv ok\nfifo ok\nfifo irq ok\nmulticore done\n"
    );
    // Core 1 ended up in the firmware's echo loop (asleep in WFE), not in the bootrom.
    assert!(
        sys.cores[1].pc >= 0x1000_0000,
        "pc = {:#x}",
        sys.cores[1].pc
    );
    assert!(sys.cores[1].sleeping && sys.cores[1].wfe);
}

const PIO_BOARD: &str = r#"
name = "test"
parts.dac = { kind = "dac8165", sdi = 4, cs = 5, clk = 6, rst = 7, vref_mv = 1250 }
parts.leds = { kind = "ws2812", gpio = 14, count = 8 }
parts.knob = { kind = "encoder", clk = 22, dt = 21 }
"#;

#[test]
fn pio_drives_the_dac_and_led_chain_and_reads_the_encoder() {
    use pcb::Board;
    use pcb_devices::{dac8165::Dac8165, ws2812::Ws2812};
    let elf = std::fs::read(build_named("pio-test")).unwrap();
    let mut sys = System::new();
    sys.load_elf(&elf).unwrap();
    sys.set_board(Board::parse_with(PIO_BOARD, &pcb_devices::registry()).unwrap());

    // Run in slices, acting on the firmware's progress: it asks for the knob
    // to be turned once clockwise and then once back.
    let text = |s: &System| String::from_utf8_lossy(&s.bus.uart[0].tx).into_owned();
    let (mut turned_cw, mut turned_back) = (false, false);
    let mut stop = StopReason::CycleLimit;
    for _ in 0..400 {
        stop = sys.run(50_000);
        if stop != StopReason::CycleLimit {
            break;
        }
        let t = text(&sys);
        if !turned_cw && t.ends_with("encoder ready\n") {
            assert!(sys.input("knob", "turn", 1));
            turned_cw = true;
        }
        if !turned_back && t.ends_with("enc+ 4\n") {
            assert!(sys.input("knob", "turn", -1));
            turned_back = true;
        }
    }
    assert_eq!(stop, StopReason::Breakpoint(1), "{}", text(&sys));
    assert_eq!(
        text(&sys),
        "start\ndac sent\nleds sent\nencoder ready\nenc+ 4\nenc- 0\npio done\n"
    );

    // The DAC received four frames over the bit-banged serial link.
    let dac = sys.board.device::<Dac8165>("dac").unwrap();
    assert_eq!((dac.frames, dac.unsupported), (4, 0));
    let codes: Vec<u16> = (0..4).map(|c| dac.code(c)).collect();
    assert_eq!(codes, [0x2000, 0x1000, 0x0400, 0x3FFF]);
    assert_eq!(dac.millivolts(0), 625);

    // The LED chain decoded 8 pixels from the WS2812 pulse train.
    let leds = sys.board.device::<Ws2812>("leds").unwrap();
    let expected: Vec<[u8; 3]> = (0..8u8).map(|i| [i * 16, 255 - i * 16, i]).collect();
    assert_eq!(leds.leds(), expected.as_slice());
    assert_eq!(leds.frames, 1);

    // One detent each way: the encoder's own events.
    let detents: Vec<i64> = sys
        .board
        .events
        .iter()
        .filter(|e| e.label == "detent")
        .map(|e| e.value)
        .collect();
    assert_eq!(detents, [1, -1]);
}

#[test]
fn spi_and_dma_talk_to_an_sd_card() {
    use pcb::Board;
    use pcb_devices::sdcard::{BLOCK, SdCard};
    let elf = std::fs::read(build_named("sd-test")).unwrap();
    let mut sys = System::new();
    sys.load_elf(&elf).unwrap();
    let board = "buses.spi1 = { sck = 10, mosi = 11, miso = 12 }\nparts.sd = { kind = \"sdcard\", bus = \"spi1\", cs = 8, size_mb = 1 }";
    sys.set_board(Board::parse_with(board, &pcb_devices::registry()).unwrap());
    // Block 0 holds the pattern the firmware expects.
    let mut image = vec![0u8; 2048 * BLOCK];
    for (i, b) in image[..BLOCK].iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(3);
    }
    sys.board
        .device_mut::<SdCard>("sd")
        .unwrap()
        .load_image(image);

    assert_eq!(
        sys.run(50_000_000),
        StopReason::Breakpoint(1),
        "{}",
        String::from_utf8_lossy(&sys.bus.uart[0].tx)
    );
    assert_eq!(
        String::from_utf8_lossy(&sys.bus.uart[0].tx),
        "start\ncard ready\nocr sdhc\nread ok\ncrc ok\nwrite ok\nverify ok\nsd done\n"
    );
    // What the firmware wrote: block 0's pattern XOR 0x5A, in block 5, and nothing else.
    let card = sys.board.device::<SdCard>("sd").unwrap();
    let expected: Vec<u8> = (0..BLOCK)
        .map(|i| (i as u8).wrapping_mul(3) ^ 0x5A)
        .collect();
    assert_eq!(&card.image()[5 * BLOCK..6 * BLOCK], expected.as_slice());
    assert_eq!(card.blocks_written, 1);
    assert_eq!(card.blocks_read, 2);
    assert!(card.dirty());
    assert!(card.image()[BLOCK..5 * BLOCK].iter().all(|&b| b == 0));
}

#[test]
fn cli_loads_an_sd_image_and_saves_what_the_firmware_wrote() {
    use pcb_devices::sdcard::BLOCK;
    let elf = build_named("sd-test");
    let dir = std::env::temp_dir().join(format!("rp2040-headless-sd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (board, img) = (dir.join("sd.toml"), dir.join("card.img"));
    std::fs::write(&board, "buses.spi1 = { sck = 10, mosi = 11, miso = 12 }\nparts.sd = { kind = \"sdcard\", bus = \"spi1\", cs = 8, size_mb = 1 }\n").unwrap();
    let mut image = vec![0u8; 1024 * BLOCK];
    for (i, b) in image[..BLOCK].iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(3);
    }
    std::fs::write(&img, &image).unwrap();
    let run = |sd: &str| {
        Command::new(env!("CARGO_BIN_EXE_tartelette"))
            .arg(&elf)
            .arg("--headless")
            .arg("--board")
            .arg(&board)
            .args(["--sd", sd])
            .output()
            .unwrap()
    };
    let out = run(&format!("sd={}", img.display()));
    assert!(out.status.success());
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("warning"),
        "the board's SPI1 wiring is what the firmware uses"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).ends_with("verify ok\nsd done\n"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let saved = std::fs::read(&img).unwrap();
    assert_eq!(saved.len(), image.len());
    assert!(
        saved[5 * BLOCK..6 * BLOCK]
            .iter()
            .enumerate()
            .all(|(i, &b)| b == (i as u8).wrapping_mul(3) ^ 0x5A)
    );
    // Written through as it runs: nothing is saved at the end with --sd-live,
    // so the block can only have come from there.
    std::fs::write(&img, &image).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tartelette"))
        .arg(&elf)
        .arg("--headless")
        .arg("--board")
        .arg(&board)
        .args(["--sd", &format!("sd={}", img.display()), "--sd-live"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(std::fs::read(&img).unwrap(), saved);
    // Unknown card and unreadable image are errors up front.
    assert!(!run("nope=/dev/null").status.success());
    assert!(
        !run(&format!("sd={}", dir.join("missing.img").display()))
            .status
            .success()
    );
    std::fs::remove_dir_all(&dir).ok();
}
