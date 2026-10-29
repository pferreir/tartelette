//! Headless runs (native only): the firmware runs as fast as it can on a
//! schedule of inputs, and its output streams to the terminal. UART0 and the
//! USB serial port (unless bridged) go to stdout, and stdin goes to the USB
//! serial port; UART1, board events, the firmware's log and the USB log go
//! to stderr.
use std::{io::Write, path::Path};

use pcb_devices::{cyw43439::Cyw43439, sdcard::SdCard, st7789::St7789};
use rp2040_core::system::{CYCLES_PER_US, StopReason};

use crate::{
    cli::RunArgs,
    session::{Session, Source},
};

const CHUNK: u64 = 100_000;

fn us(cycles: u64) -> f64 {
    cycles as f64 / CYCLES_PER_US as f64
}

pub fn run(args: &RunArgs) -> Result<(), String> {
    let h = &args.headless_opts;
    let fw = Source::from_file(
        args.firmware
            .as_deref()
            .expect("clap requires a firmware with --headless"),
    )?;
    let board = args.board()?;
    let mut s = Session::new(&fw, board.as_ref(), &args.shields()?, &args.network())?;
    s.sys.stop_on_fault = h.stop_on_fault;
    let board = &mut s.sys.board;

    // Reject anything aimed at parts that don't exist before running anything.
    let inputs = h.inputs();
    if let Some(i) = inputs.iter().find(|i| board.part_index(&i.part).is_none()) {
        return Err(format!("no part named {:?} on the board", i.part));
    }
    for (name, file) in &args.sd {
        let card = board
            .device_mut::<SdCard>(name)
            .ok_or_else(|| format!("--sd: no SD card named {name:?} on the board"))?;
        card.load_image(std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?);
    }
    if let Some((name, _)) = h
        .dump_screen
        .iter()
        .find(|(name, _)| board.device::<St7789>(name).is_none())
    {
        return Err(format!(
            "--dump-screen: no display named {name:?} on the board"
        ));
    }
    if let Some((name, _)) = h
        .dump_paper
        .iter()
        .find(|(name, _)| board.device::<Cyw43439>(name).is_none())
    {
        return Err(format!(
            "--dump-paper: no Wi-Fi chip named {name:?} on the board"
        ));
    }
    if h.log
        && let Some(note) = &s.log_note
    {
        eprintln!("--log: {note}");
    }

    if !s.usb.serial_bridged() {
        s.usb.keep_fresh();
    }
    let stdin = read_stdin();
    let mut stdin_open = true;
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let end = h.max_cycles.unwrap_or(u64::MAX); // the core starts at cycle 0
    let mut pending = inputs.into_iter().peekable();
    let started = std::time::Instant::now();
    let reason = loop {
        if let Some(speed) = h.speed {
            let due = std::time::Duration::from_secs_f64(us(s.sys.cycles()) / 1e6 / speed);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        let sys = &mut s.sys;
        while let Some(i) = pending.next_if(|i| i.cycle <= sys.cycles()) {
            if !sys.input(&i.part, &i.action, i.value) {
                return Err(format!(
                    "part {:?} does not understand {:?}",
                    i.part, i.action
                ));
            }
        }
        let mut n = (end - sys.cycles().min(end)).min(CHUNK);
        if let Some(next) = pending.peek() {
            n = n.min(next.cycle - sys.cycles());
        }
        let reason = if n == 0 {
            StopReason::CycleLimit
        } else {
            sys.run(n)
        };
        if matches!(reason, StopReason::Breakpoint(_)) {
            sys.drain_uarts();
        }

        let _ = out.write_all(&std::mem::take(&mut sys.bus.uart[0].tx));
        let _ = err.write_all(&std::mem::take(&mut sys.bus.uart[1].tx));
        for e in std::mem::take(&mut sys.board.events) {
            let (name, t) = (&sys.board.parts[e.part].name, us(e.cycles));
            // Switch-like events say it all in the label; others carry a value.
            let _ = match e.label {
                "on" | "off" | "pressed" | "released" => {
                    writeln!(err, "[{t:>10.1} us] {name} {}", e.label)
                }
                _ => writeln!(err, "[{t:>10.1} us] {name} {}={}", e.label, e.value),
            };
        }
        for w in std::mem::take(&mut sys.warnings) {
            let _ = writeln!(err, "[{:>10.1} us] warning: {w}", us(sys.cycles()));
        }
        loop {
            match stdin.try_recv() {
                Ok(chunk) => s.usb.input.extend(chunk),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    stdin_open = false;
                    break;
                }
            }
        }
        s.exchange_usb();
        let _ = out.write_all(&s.usb.take_fresh());
        for e in s.usb.log.drain(..) {
            if h.usb_log {
                let _ = writeln!(err, "[{:>10.1} us] usb {}", e.at * 1e6, e.event.describe());
            }
        }
        for w in std::mem::take(&mut s.warnings) {
            let _ = writeln!(err, "[{:>10.1} us] {w}", us(s.sys.cycles()));
        }
        if h.log {
            let t = us(s.sys.cycles());
            for l in s.poll_log() {
                let level = l.level.map_or("", rtt::Level::name);
                let _ = writeln!(err, "[{t:>10.1} us] {level:<5} {}", l.text);
            }
        }
        let _ = out.flush();

        match reason {
            StopReason::CycleLimit if s.sys.cycles() < end => {}
            // Asleep with nothing to wake it, but an input is still coming.
            StopReason::Sleeping if pending.peek().is_some_and(|i| i.cycle < end) => {
                s.sys.idle_until(pending.peek().map_or(end, |i| i.cycle));
            }
            // A USB device waits for its host: for stdin, or for programs
            // on the far side of a bridge. Time passes as in real life.
            StopReason::Sleeping
                if s.sys.usb_host().device().is_some() && (stdin_open || s.usb.bridged()) =>
            {
                const MS: u64 = 1000 * CYCLES_PER_US;
                if h.speed.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                s.sys.idle_until((s.sys.cycles() + MS).min(end));
            }
            r => break r,
        }
    };

    let board = &mut s.sys.board;
    for (name, file) in &h.dump_screen {
        if let Some(lcd) = board.device::<St7789>(name) {
            save_screen(lcd, file)?;
        }
    }
    for (name, file) in &h.dump_paper {
        match board
            .device::<Cyw43439>(name)
            .and_then(|c| c.bt_backend().paper())
        {
            Some([]) => eprintln!("--dump-paper: {name:?}'s printer has printed nothing yet"),
            Some(paper) => save_paper(paper, file)?,
            None => {
                eprintln!("--dump-paper: no cat printer in range of {name:?} (it needs --bt mock)")
            }
        }
    }
    for (name, file) in &args.sd {
        if let Some(card) = board.device::<SdCard>(name)
            && card.dirty()
        {
            std::fs::write(file, card.image()).map_err(|e| format!("{}: {e}", file.display()))?;
        }
    }
    for name in &h.wifi_trace {
        let Some(chip) = board.device::<Cyw43439>(name) else {
            eprintln!("--wifi-trace: no Wi-Fi chip named {name:?} on the board");
            continue;
        };
        for t in chip.transactions() {
            let dir = if t.write { "write" } else { "read " };
            let _ = writeln!(
                err,
                "[{:>10.1} us] wifi {dir} f{} {:#07x} len={} {:02x?}",
                us(t.cycle),
                t.func,
                t.addr,
                t.len,
                t.head
            );
        }
    }
    let (at, core) = match (&reason, s.sys.bkpt_at) {
        (StopReason::Breakpoint(_), Some((core, pc))) => {
            (format!(" at {pc:#010x} on core {core}"), Some(core))
        }
        (StopReason::Fault { core, .. }, _) => (String::new(), Some(*core)),
        _ => (String::new(), None),
    };
    eprintln!("[stopped: {reason}{at} after {} cycles]", s.sys.cycles());
    // A fault, or a bkpt in a handler (a fault handler's, as in pico-sdk):
    // what the core was doing. A bkpt in thread mode is the firmware's own.
    if let Some(core) = core
        && (s.sys.cores[core].ipsr != 0 || matches!(reason, StopReason::Fault { .. }))
    {
        eprint!("{}", s.sys.core_report(core));
    }
    Ok(())
}

/// Stdin, a chunk at a time, for the USB serial port. The reading thread
/// ends with stdin (or with the process).
fn read_stdin() -> std::sync::mpsc::Receiver<Vec<u8>> {
    use std::io::Read;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin().lock();
        while let Ok(n @ 1..) = stdin.read(&mut buf) {
            if tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    rx
}

/// The display's picture, in the format the file's extension names.
fn save_screen(lcd: &St7789, file: &Path) -> Result<(), String> {
    let (w, h) = lcd.size();
    let rgb: Vec<u8> = lcd
        .rgba()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    // `image` writes a PAM (P7) for `.ppm`, so binary PPM is done by hand.
    if file
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("ppm"))
    {
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        ppm.extend(rgb);
        return std::fs::write(file, ppm).map_err(|e| format!("{}: {e}", file.display()));
    }
    image::save_buffer(file, &rgb, w as u32, h as u32, image::ColorType::Rgb8)
        .map_err(|e| format!("{}: {e}", file.display()))
}

/// A roll of thermal paper (1 bit per dot, bit 0 leftmost), black on white,
/// in the format the file's extension names.
fn save_paper(paper: &[u8], file: &Path) -> Result<(), String> {
    use pcb_devices::hci::catprinter::{ROW_BYTES, WIDTH};
    let rows = paper.len() / ROW_BYTES;
    if file
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pbm"))
    {
        // PBM is MSB-first with 1 = black.
        let mut pbm = format!("P4\n{WIDTH} {rows}\n").into_bytes();
        pbm.extend(paper.iter().map(|b| b.reverse_bits()));
        return std::fs::write(file, pbm).map_err(|e| format!("{}: {e}", file.display()));
    }
    let gray: Vec<u8> = (0..rows * WIDTH)
        .map(|i| {
            if paper[i / 8] >> (i % 8) & 1 != 0 {
                0x20
            } else {
                0xF4
            }
        })
        .collect();
    image::save_buffer(file, &gray, WIDTH as u32, rows as u32, image::ColorType::L8)
        .map_err(|e| format!("{}: {e}", file.display()))
}
