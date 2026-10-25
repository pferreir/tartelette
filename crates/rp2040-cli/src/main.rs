//! Native runner: `rp2040-cli firmware.elf [--max-cycles N] [--board FILE]
//! [--press NAME@US[+HOLD_US]]... [--input NAME:ACTION[=VALUE]@US]...
//! [--dump-screen NAME=FILE.ppm]... [--sd NAME=FILE.img]... [--log]`
//!
//! UART0 output goes to stdout, UART1 output to stderr. With a board file,
//! events from its devices (LEDs, buttons, ...) are logged to stderr with
//! their time in microseconds. `--press` schedules a button press (and, with
//! `+HOLD`, the release) at the given time; `--input` schedules any device
//! action, e.g. `--input expander:gp3=0@500` pulls expander pin 3 low.
//! `--dump-screen` writes a display part's picture (as a binary PPM) when the
//! run ends. `--sd` loads a disk image into an SD card part and, if the
//! firmware wrote to the card, saves the image back to the same file at the
//! end (the card needs at least 512 KiB, so a smaller image file grows).
//! `--log` prints the firmware's defmt-over-RTT log to stderr, stamped with
//! the emulated time at which it was read out (to within one chunk of
//! about a millisecond).
//!
//! `rp2040-cli mkdisk FILE.img [--size-mb N] [--dir NAME]... [--label NAME]
//! [--force]` makes a blank SD card image for `--sd`: an MBR with one empty
//! FAT16 partition, as the BTL-01 OS mounts it (16 MiB by default, 4 to 2000;
//! the file is sparse), with an empty top-level directory for each `--dir`
//! (the OS lists `data`, so its Load menu needs `--dir data`). It will not
//! replace an existing file without `--force`. A
//! `--label` shows up in the OS's file list as an empty file, so it is best
//! left off unless the disk is also meant for a PC.
use pcb::Board;
use pcb_devices::sdcard::SdCard;
use pcb_devices::st7789::St7789;
use rp2040_core::system::{CYCLES_PER_US, StopReason, System};
use std::io::Write;
use std::process::ExitCode;

const CHUNK: u64 = 100_000;

/// A scheduled device action: (cycle, part name, action, value).
type Input = (u64, String, String, i64);

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("mkdisk") {
        return mkdisk(std::env::args().skip(2));
    }
    let mut path = None;
    let mut max_cycles = u64::MAX;
    let mut board_path = None;
    let mut inputs: Vec<Input> = Vec::new();
    let mut dumps: Vec<(String, String)> = Vec::new();
    let mut sd_images: Vec<(String, String)> = Vec::new();
    let mut log = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--max-cycles" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) => max_cycles = n,
                None => return usage(),
            },
            "--board" => match args.next() {
                Some(p) => board_path = Some(p),
                None => return usage(),
            },
            "--press" => match args.next().as_deref().and_then(parse_press) {
                Some(mut v) => inputs.append(&mut v),
                None => return usage(),
            },
            "--input" => match args.next().as_deref().and_then(parse_input) {
                Some(i) => inputs.push(i),
                None => return usage(),
            },
            "--dump-screen" => match args.next().as_deref().and_then(|v| v.split_once('=')) {
                Some((name, file)) => dumps.push((name.into(), file.into())),
                None => return usage(),
            },
            "--sd" => match args.next().as_deref().and_then(|v| v.split_once('=')) {
                Some((name, file)) => sd_images.push((name.into(), file.into())),
                None => return usage(),
            },
            "--log" => log = true,
            _ if path.is_none() && !a.starts_with("--") => path = Some(a),
            _ => return usage(),
        }
    }
    let Some(path) = path else { return usage() };
    inputs.sort_by_key(|i| i.0); // stable: a press and release at the same time keep their order

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut sys = System::new();
    if let Err(e) = sys.load_elf(&bytes) {
        eprintln!("{path}: cannot load ELF: {e:?}");
        return ExitCode::FAILURE;
    }
    if let Some(p) = board_path {
        let board = match std::fs::read_to_string(&p) {
            Ok(t) => Board::parse_with(&t, &pcb_devices::registry()).map_err(|e| format!("{p}:{}: {}", e.line, e.msg)),
            Err(e) => Err(format!("{p}: {e}")),
        };
        match board {
            Ok(b) => sys.set_board(b),
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
    }
    // Reject inputs aimed at parts that don't exist before running anything.
    for (_, name, _, _) in &inputs {
        if sys.board.part_index(name).is_none() {
            eprintln!("no part named {name:?} on the board");
            return ExitCode::FAILURE;
        }
    }

    for (name, file) in &sd_images {
        let Some(card) = sys.board.device_mut::<SdCard>(name) else {
            eprintln!("--sd: no SD card named {name:?} on the board");
            return ExitCode::FAILURE;
        };
        match std::fs::read(file) {
            Ok(bytes) => card.load_image(bytes),
            Err(e) => {
                eprintln!("{file}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    for (name, _) in &dumps {
        if sys.board.device::<St7789>(name).is_none() {
            eprintln!("--dump-screen: no display named {name:?} on the board");
            return ExitCode::FAILURE;
        }
    }

    let mut rtt = None;
    if log {
        match rtt::Rtt::new(&bytes) {
            Ok(Some(r)) => rtt = Some(r),
            Ok(None) => eprintln!("--log: the firmware has no defmt-over-RTT log"),
            Err(e) => eprintln!("--log: {e}"),
        }
    }
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let end = max_cycles; // the core starts at cycle 0
    let mut pending = inputs.into_iter().peekable();
    let reason = loop {
        while let Some((_, name, action, value)) = pending.next_if(|i| i.0 <= sys.cycles()) {
            if !sys.input(&name, &action, value) {
                eprintln!("part {name:?} does not understand {action:?}");
                return ExitCode::FAILURE;
            }
        }
        let mut n = (end - sys.cycles().min(end)).min(CHUNK);
        if let Some(next) = pending.peek() {
            n = n.min(next.0 - sys.cycles());
        }
        let reason = if n == 0 { StopReason::CycleLimit } else { sys.run(n) };

        let _ = out.write_all(&std::mem::take(&mut sys.bus.uart[0].tx));
        let _ = err.write_all(&std::mem::take(&mut sys.bus.uart[1].tx));
        for e in std::mem::take(&mut sys.board.events) {
            let name = &sys.board.parts[e.part].name;
            let t = e.cycles as f64 / CYCLES_PER_US as f64;
            // Switch-like events say it all in the label; others carry a value.
            let _ = match e.label {
                "on" | "off" | "pressed" | "released" => writeln!(err, "[{t:>10.1} us] {name} {}", e.label),
                _ => writeln!(err, "[{t:>10.1} us] {name} {}={}", e.label, e.value),
            };
        }
        if let Some(r) = rtt.as_mut() {
            let mut lines = Vec::new();
            r.poll(&mut sys, &mut lines);
            let t = sys.cycles() as f64 / CYCLES_PER_US as f64;
            for l in lines {
                let level = l.level.map_or("", rtt::Level::name);
                let _ = writeln!(err, "[{t:>10.1} us] {level:<5} {}", l.text);
            }
        }
        let _ = out.flush();

        match reason {
            StopReason::CycleLimit if sys.cycles() < end => {}
            // Asleep with nothing to wake it, but a button press is still coming.
            StopReason::Sleeping if pending.peek().is_some_and(|i| i.0 < end) => {
                sys.idle_until(pending.peek().map_or(end, |i| i.0));
            }
            r => break r,
        }
    };
    for (name, file) in &dumps {
        if let Some(lcd) = sys.board.device::<St7789>(name)
            && let Err(e) = std::fs::write(file, ppm(lcd))
        {
            eprintln!("{file}: {e}");
            return ExitCode::FAILURE;
        }
    }
    for (name, file) in &sd_images {
        if let Some(card) = sys.board.device::<SdCard>(name)
            && card.dirty()
            && let Err(e) = std::fs::write(file, card.image())
        {
            eprintln!("{file}: {e}");
            return ExitCode::FAILURE;
        }
    }
    eprintln!("[stopped: {reason:?} after {} cycles, pc={:#010x}]", sys.cycles(), sys.cores[0].pc);
    ExitCode::SUCCESS
}

/// The display as a binary PPM (P6) image.
fn ppm(lcd: &St7789) -> Vec<u8> {
    let (w, h) = lcd.size();
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    out.extend(lcd.rgba().as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]));
    out
}

fn us_to_cycles(s: &str) -> Option<u64> {
    s.parse::<f64>().ok().filter(|v| *v >= 0.0).map(|v| (v * CYCLES_PER_US as f64) as u64)
}

/// `NAME@US` or `NAME@US+HOLD_US` -> press (and release) actions.
fn parse_press(spec: &str) -> Option<Vec<Input>> {
    let (name, when) = spec.split_once('@')?;
    let (at, hold) = match when.split_once('+') {
        Some((a, h)) => (a, Some(h)),
        None => (when, None),
    };
    let press = us_to_cycles(at)?;
    let mut v = vec![(press, name.to_string(), "press".to_string(), 0)];
    if let Some(h) = hold {
        v.push((press + us_to_cycles(h)?, name.to_string(), "release".to_string(), 0));
    }
    Some(v)
}

/// `NAME:ACTION@US` or `NAME:ACTION=VALUE@US` -> one device action.
fn parse_input(spec: &str) -> Option<Input> {
    let (target, when) = spec.rsplit_once('@')?;
    let (name, action) = target.split_once(':')?;
    let (action, value) = match action.split_once('=') {
        Some((a, v)) => (a, v.parse().ok()?),
        None => (action, 0),
    };
    Some((us_to_cycles(when)?, name.to_string(), action.to_string(), value))
}

/// `mkdisk FILE [--size-mb N] [--dir NAME]... [--label NAME] [--force]`.
fn mkdisk(mut args: impl Iterator<Item = String>) -> ExitCode {
    use std::io::{Seek, SeekFrom};
    let (mut file, mut size_mb, mut label, mut force) = (None, 16, "NO NAME".to_string(), false);
    let mut dirs: Vec<String> = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--size-mb" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) => size_mb = n,
                None => return usage(),
            },
            "--dir" => match args.next() {
                Some(d) => dirs.push(d),
                None => return usage(),
            },
            "--label" => match args.next() {
                Some(l) => label = l,
                None => return usage(),
            },
            "--force" => force = true,
            _ if file.is_none() && !a.starts_with("--") => file = Some(a),
            _ => return usage(),
        }
    }
    let Some(file) = file else { return usage() };
    let regions = match pcb_devices::fat::format_fat16(size_mb, &label, &dirs.iter().map(String::as_str).collect::<Vec<_>>()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mkdisk: {e}");
            return ExitCode::FAILURE;
        }
    };
    let write = || -> std::io::Result<()> {
        let mut f = if force { std::fs::File::create(&file)? } else { std::fs::File::create_new(&file)? };
        f.set_len(size_mb as u64 * 1024 * 1024)?;
        for (at, bytes) in regions {
            f.seek(SeekFrom::Start(at as u64))?;
            f.write_all(&bytes)?;
        }
        Ok(())
    };
    match write() {
        Ok(()) => {
            eprintln!("{file}: {size_mb} MiB, FAT16 \"{label}\"{}", dirs.iter().map(|d| format!(", {d}/")).collect::<String>());
            ExitCode::SUCCESS
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            eprintln!("{file}: already exists (use --force to replace it)");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("{file}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: rp2040-cli <firmware.elf> [--max-cycles N] [--board FILE] [--press NAME@US[+HOLD_US]]... [--input NAME:ACTION[=VALUE]@US]... [--dump-screen NAME=FILE.ppm]... [--sd NAME=FILE.img]... [--log]");
    eprintln!("       rp2040-cli mkdisk <FILE.img> [--size-mb N] [--dir NAME]... [--label NAME] [--force]");
    ExitCode::from(2)
}
