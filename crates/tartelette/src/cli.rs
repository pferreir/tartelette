//! Command line (native only): one set of options for the front panel and for
//! headless runs, plus the `mkdisk` tool.
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use rp2040_core::system::CYCLES_PER_US;

use crate::session::{BtMode, NetMode, Network, Source};

/// Tartelette, an RP2040 emulator: runs a firmware ELF or UF2 on an emulated board, with a front
/// panel window or headless.
///
/// Headless, UART0 output goes to stdout and UART1 output to stderr (see
/// `--uart0` and `--uart1`), with the board's events (LEDs, buttons, ...) logged to stderr at their time in
/// microseconds. With a window, the firmware and board reload when their
/// files change.
#[derive(Parser, Debug)]
#[command(
    version,
    args_conflicts_with_subcommands = true,
    args_override_self = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[command(flatten)]
    pub run: RunArgs,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    Mkdisk(MkdiskArgs),
    Parts(PartsArgs),
    /// Print a JSON Schema for board files, for editors (the repository keeps
    /// it as `boards/board.schema.json`; board files point at it with a
    /// `#:schema` comment, which VS Code's Even Better TOML and taplo follow)
    Schema,
}

/// List the device kinds a board file can use, or describe one: its keys,
/// their types and defaults
#[derive(Args, Debug)]
pub struct PartsArgs {
    /// The kind to describe (all of them if left out)
    pub kind: Option<String>,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Firmware ELF or UF2 to run (with a window, it can also be opened later)
    #[arg(required_if_eq("headless", "true"))]
    pub firmware: Option<PathBuf>,
    /// Board description file (TOML), or the name of a built-in board: pico
    /// (the default), pico-w or btl-01 (as in the repository's `boards/`);
    /// `tartelette parts` lists what a board can have on it
    #[arg(long, value_name = "FILE|NAME")]
    pub board: Option<PathBuf>,
    /// Put a shield (an add-on board) on the board: a file in the board format,
    /// or a built-in one: pico-display-2 (as in `boards/shields/`). May be
    /// given several times, stacking in order
    #[arg(long, value_name = "FILE|NAME")]
    pub shield: Vec<PathBuf>,
    /// Load a disk image into the SD card part NAME; headless, it is saved
    /// back at the end if the firmware wrote to it (the card needs at least
    /// 512 KiB, so a smaller image file grows)
    #[arg(long, value_name = "NAME=FILE", value_parser = name_and_path)]
    pub sd: Vec<(String, PathBuf)>,
    /// Write each block the firmware writes to an --sd card to its file
    /// straight away, so host tools (mtools, fsck.fat) see the card as it
    /// runs; with a window, the file otherwise changes only on Save…
    #[arg(long, requires = "sd")]
    pub sd_live: bool,
    /// What the board's Wi-Fi chips are connected to
    #[arg(long, value_enum, default_value = "user")]
    pub net: NetMode,
    /// Forward the host's 127.0.0.1:HOST to TCP port GUEST on the guest (with --net user)
    #[arg(long, value_name = "HOST:GUEST", value_parser = port_pair)]
    pub forward: Vec<(u16, u16)>,
    /// What the board's Bluetooth radios talk to: `none` (no radio, as in the
    /// browser), `mock` (software devices: a central, a BLE MIDI peripheral,
    /// a battery and an MXW01 cat printer), or a real adapter `hciN` (Linux; takes the adapter
    /// from the system while running and needs CAP_NET_ADMIN)
    #[arg(long, value_name = "none|mock|hciN", default_value = "none", value_parser = bt_mode)]
    pub bt: BtMode,
    /// Hand the emulated USB device's functions to this computer: `serial`
    /// (its CDC-ACM port as a pseudo-terminal; Linux), `hid` (its HID
    /// interfaces as input devices through /dev/uhid, so an emulated
    /// keyboard types for real; Linux, needs access to /dev/uhid), `midi`
    /// (its USB-MIDI interface as virtual MIDI ports; Linux and macOS).
    /// Comma-separated. Without a bridge the serial port is in the console
    /// (headless: stdin and stdout) and the rest in the USB log
    #[arg(long, value_name = "serial,hid,midi", value_delimiter = ',', value_parser = ["serial", "hid", "midi"])]
    pub usb_bridge: Vec<String>,
    /// Also make a symlink at PATH to the serial port's pseudo-terminal (with
    /// `--usb-bridge serial`), e.g. /tmp/ttyACM
    #[arg(long, value_name = "PATH", requires = "usb_bridge")]
    pub usb_tty_link: Option<PathBuf>,
    /// Patch a test voice into the board's outputs, played in the window's
    /// Synth panel or into `--wav`. An oscillator (WAVE is sine, triangle,
    /// saw or square) follows CV, a DAC channel (`dac.A`), at 1 V/octave
    /// from C4 at 0 V while GATE is high: an expander pin (`expander.0`), an
    /// LED part, or `open`. A drum (kick, snare, hat, openhat, clap) plays a
    /// hit on each rising edge of GATE. Left out, CV and GATE are the board's
    /// first. Repeat for more voices
    #[arg(long, value_name = "WAVE[:CV[:GATE]]|DRUM[:GATE]", value_parser = clap::value_parser!(crate::synth::Patch))]
    pub osc: Vec<crate::synth::Patch>,
    /// Patch a drum kit into the board's first four gates (kick, snare,
    /// closed and open hat; on the BTL-01 gates A..D), with any `--osc`
    /// voices, to hear the firmware as a drum machine
    #[arg(long)]
    pub drums: bool,
    /// Run without a window
    #[arg(long)]
    pub headless: bool,
    #[command(flatten)]
    pub headless_opts: Headless,
}

/// The boards in the repository's `boards/`, built in so that an installed
/// `tartelette` has them too.
const BOARDS: &[(&str, &str)] = &[
    ("pico", crate::session::PICO_BOARD),
    ("btl-01", include_str!("../../../boards/btl-01.toml")),
    ("pico-w", include_str!("../../../boards/pico-w.toml")),
];

/// The shields in `boards/shields/`.
const SHIELDS: &[(&str, &str)] = &[(
    "pico-display-2",
    include_str!("../../../boards/shields/pico-display-2.toml"),
)];

/// A file, or else the built-in one of that name.
fn file_or_builtin(
    path: &std::path::Path,
    builtin: &[(&str, &str)],
    option: &str,
) -> Result<Source, String> {
    if path.exists() {
        return Source::from_file(path);
    }
    let name = path.to_string_lossy();
    match builtin.iter().find(|(n, _)| *n == name) {
        Some((n, text)) => Ok(Source::new(format!("{n}.toml"), text.as_bytes().to_vec())),
        None => {
            let names: Vec<_> = builtin.iter().map(|(n, _)| *n).collect();
            Err(format!(
                "{option} {name}: no such file, nor a built-in one ({})",
                names.join(", ")
            ))
        }
    }
}

impl RunArgs {
    /// The voices `--osc` and `--drums` ask for, or else the board's default.
    pub fn patches(&self, board: &pcb::Board) -> Vec<crate::synth::Patch> {
        let mut v = self.osc.clone();
        if self.drums {
            v.extend(crate::synth::drum_kit(board));
        }
        if v.is_empty() {
            v = crate::synth::default_patches(board);
        }
        v
    }

    /// The `--board` file, or else the built-in board of that name.
    pub fn board(&self) -> Result<Option<Source>, String> {
        self.board
            .as_deref()
            .map(|p| file_or_builtin(p, BOARDS, "--board"))
            .transpose()
    }

    /// The `--shield` files, or built-in shields, in order.
    pub fn shields(&self) -> Result<Vec<Source>, String> {
        self.shield
            .iter()
            .map(|p| file_or_builtin(p, SHIELDS, "--shield"))
            .collect()
    }

    pub fn network(&self) -> Network {
        Network {
            mode: self.net,
            bt: self.bt,
            forwards: self.forward.clone(),
            usb: crate::usb::UsbBridges {
                serial: self.usb_bridge.iter().any(|b| b == "serial"),
                serial_link: self.usb_tty_link.clone(),
                hid: self.usb_bridge.iter().any(|b| b == "hid"),
                midi: self.usb_bridge.iter().any(|b| b == "midi"),
            },
        }
    }
}

#[derive(Args, Debug)]
#[command(next_help_heading = "Headless")]
pub struct Headless {
    /// Pace the run against the wall clock: 1 is real time. By default it
    /// runs as fast as it can, which can make the firmware's timeouts expire
    /// before real network replies arrive (with `--net user`)
    #[arg(long, value_name = "X", requires = "headless")]
    pub speed: Option<f64>,
    /// Stop after N cycles (125 per microsecond)
    #[arg(long, value_name = "N", requires = "headless")]
    pub max_cycles: Option<u64>,
    /// Where UART0's output goes: stdout, stderr, nowhere (none), or FILE
    /// (made anew; `./stdout` for a file of that name)
    #[arg(long, value_name = "stdout|stderr|none|FILE", default_value = "stdout", value_parser = output, requires = "headless")]
    pub uart0: Output,
    /// Where UART1's output goes, as for `--uart0`
    #[arg(long, value_name = "stdout|stderr|none|FILE", default_value = "stderr", value_parser = output, requires = "headless")]
    pub uart1: Output,
    /// Keep the flash chip in FILE: it starts from FILE's contents (if it
    /// exists) with the firmware on top, and is saved back at the end if the
    /// firmware wrote to it. What the firmware keeps in flash (MicroPython's
    /// filesystem, say) then lasts from one run to the next
    #[arg(long, value_name = "FILE", requires = "headless")]
    pub flash: Option<PathBuf>,
    /// Stop when an instruction faults, before the firmware's HardFault
    /// handler runs (which may hide the fault: a reset, a loop). Without it,
    /// each fault is a warning and the handler runs
    #[arg(long, requires = "headless")]
    pub stop_on_fault: bool,
    /// Press button NAME at US microseconds, and release it HOLD microseconds later
    #[arg(long, value_name = "NAME@US[+HOLD]", value_parser = press, requires = "headless")]
    pub press: Vec<Press>,
    /// Do ACTION to part NAME at US microseconds, e.g. `expander:gp3=0@500`
    /// pulls expander pin 3 low
    #[arg(long, value_name = "NAME:ACTION[=VALUE]@US", value_parser = input, requires = "headless")]
    pub input: Vec<Input>,
    /// Save the picture on display NAME at the end, as PNG or binary PPM (by extension)
    #[arg(long, value_name = "NAME=FILE", value_parser = name_and_path, requires = "headless")]
    pub dump_screen: Vec<(String, PathBuf)>,
    /// Save what the cat printer behind Wi-Fi chip NAME's Bluetooth printed (with `--bt
    /// mock`) at the end, as PNG or PBM (by extension)
    #[arg(long, value_name = "NAME=FILE", value_parser = name_and_path, requires = "headless")]
    pub dump_paper: Vec<(String, PathBuf)>,
    /// Print the last 1024 bus transactions with Wi-Fi chip NAME at the end
    #[arg(long, value_name = "NAME", requires = "headless")]
    pub wifi_trace: Vec<String>,
    /// Print the firmware's defmt-over-RTT log to stderr, stamped with the
    /// emulated time it was read out at (to within about a millisecond)
    #[arg(long, requires = "headless")]
    pub log: bool,
    /// Print the USB traffic to stderr: enumeration, every control transfer
    /// and every packet on the other endpoints
    #[arg(long, requires = "headless")]
    pub usb_log: bool,
    /// Record the `--osc` and `--drums` voices (by default a saw on the
    /// board's first CV output and gate) to FILE, a 48 kHz mono WAV, in
    /// emulated time
    #[arg(long, value_name = "FILE", requires = "headless")]
    pub wav: Option<PathBuf>,
}

impl Headless {
    /// Every scheduled action, in time order (a press and release at the same
    /// time keep their order).
    pub fn inputs(&self) -> Vec<Input> {
        let mut v = self.input.clone();
        for p in &self.press {
            let at = |cycle| Input {
                cycle,
                part: p.part.clone(),
                action: "press".into(),
                value: 0,
            };
            v.push(at(p.cycle));
            if let Some(hold) = p.hold {
                v.push(Input {
                    action: "release".into(),
                    ..at(p.cycle + hold)
                });
            }
        }
        v.sort_by_key(|i| i.cycle);
        v
    }
}

/// `tartelette parts [KIND]`.
pub fn parts(args: &PartsArgs) -> Result<(), String> {
    let registry = pcb_devices::registry();
    match &args.kind {
        Some(name) => {
            let kind = registry.get(name).ok_or_else(|| {
                let names: Vec<_> = registry.kinds().map(|k| k.name).collect();
                format!("no device kind {name:?} (there are {})", names.join(", "))
            })?;
            print!("{}", pcb::board::spec::describe(kind));
        }
        None => {
            let width = registry.kinds().map(|k| k.name.len()).max().unwrap_or(0);
            for k in registry.kinds() {
                println!("{:width$}  {}", k.name, k.summary);
            }
            println!(
                "\n`tartelette parts KIND` describes one; a part is `[parts.NAME]` with `kind = \"KIND\"` and its keys."
            );
        }
    }
    Ok(())
}

/// `tartelette schema`.
pub fn schema() -> String {
    pcb::board::spec::json_schema(&pcb_devices::registry())
}

/// Make a blank SD card image: an MBR with one empty FAT16 partition, as the
/// BTL-01 OS mounts it (the file is sparse).
#[derive(Args, Debug)]
pub struct MkdiskArgs {
    pub file: PathBuf,
    /// Size in MiB
    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u16).range(4..=2000))]
    pub size_mb: u16,
    /// Make an empty top-level directory (the OS lists `data`, so its Load menu needs `--dir data`)
    #[arg(long, value_name = "NAME")]
    pub dir: Vec<String>,
    /// Volume label; it shows up in the OS's file list as an empty file, so
    /// it is best left off unless the disk is also meant for a PC
    #[arg(long, default_value = "NO NAME")]
    pub label: String,
    /// Replace an existing file
    #[arg(long)]
    pub force: bool,
}

#[derive(Clone, Debug)]
pub struct Press {
    part: String,
    cycle: u64,
    hold: Option<u64>,
}

/// A scheduled device action.
#[derive(Clone, Debug)]
pub struct Input {
    pub cycle: u64,
    pub part: String,
    pub action: String,
    pub value: i64,
}

fn us_to_cycles(s: &str) -> Result<u64, String> {
    s.parse::<f64>()
        .ok()
        .filter(|v| *v >= 0.0)
        .map(|v| (v * CYCLES_PER_US as f64) as u64)
        .ok_or_else(|| format!("{s:?} is not a time in microseconds"))
}

/// Where a headless run sends a stream of output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    Stdout,
    Stderr,
    None,
    File(PathBuf),
}

fn output(s: &str) -> Result<Output, String> {
    Ok(match s {
        "stdout" => Output::Stdout,
        "stderr" => Output::Stderr,
        "none" => Output::None,
        "" => return Err("expected stdout, stderr, none or a file".into()),
        file => Output::File(file.into()),
    })
}

fn name_and_path(s: &str) -> Result<(String, PathBuf), String> {
    let (name, file) = s.split_once('=').ok_or("expected NAME=FILE")?;
    Ok((name.into(), file.into()))
}

fn bt_mode(s: &str) -> Result<BtMode, String> {
    match s {
        "none" => Ok(BtMode::None),
        "mock" => Ok(BtMode::Mock),
        _ => s
            .strip_prefix("hci")
            .and_then(|n| n.parse().ok())
            .map(BtMode::Adapter)
            .ok_or_else(|| format!("{s:?}: expected none, mock or hciN (e.g. hci0)")),
    }
}

fn port_pair(s: &str) -> Result<(u16, u16), String> {
    let (h, g) = s.split_once(':').ok_or("expected HOST:GUEST")?;
    Ok((
        h.parse().map_err(|e| format!("{h:?}: {e}"))?,
        g.parse().map_err(|e| format!("{g:?}: {e}"))?,
    ))
}

/// `NAME@US` or `NAME@US+HOLD_US`.
fn press(s: &str) -> Result<Press, String> {
    let (part, when) = s.split_once('@').ok_or("expected NAME@US[+HOLD]")?;
    let (at, hold) = match when.split_once('+') {
        Some((a, h)) => (a, Some(us_to_cycles(h)?)),
        None => (when, None),
    };
    Ok(Press {
        part: part.into(),
        cycle: us_to_cycles(at)?,
        hold,
    })
}

/// `NAME:ACTION@US` or `NAME:ACTION=VALUE@US`.
fn input(s: &str) -> Result<Input, String> {
    let (target, when) = s
        .rsplit_once('@')
        .ok_or("expected NAME:ACTION[=VALUE]@US")?;
    let (part, action) = target
        .split_once(':')
        .ok_or("expected NAME:ACTION[=VALUE]@US")?;
    let (action, value) = match action.split_once('=') {
        Some((a, v)) => {
            let n = match v.strip_prefix("0x") {
                Some(hex) => i64::from_str_radix(hex, 16),
                None => v.parse(),
            };
            (a, n.map_err(|e| format!("{v:?}: {e}"))?)
        }
        None => (action, 0),
    };
    Ok(Input {
        cycle: us_to_cycles(when)?,
        part: part.into(),
        action: action.into(),
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn presses_expand_into_sorted_actions() {
        let cli = Cli::try_parse_from([
            "x",
            "fw",
            "--headless",
            "--press",
            "b@100+50",
            "--input",
            "k:turn=-1@120",
        ])
        .unwrap();
        let v: Vec<_> = cli
            .run
            .headless_opts
            .inputs()
            .into_iter()
            .map(|i| (i.cycle, i.action, i.value))
            .collect();
        assert_eq!(
            v,
            [
                (12_500, "press".into(), 0),
                (15_000, "turn".into(), -1),
                (18_750, "release".into(), 0)
            ]
        );
    }

    #[test]
    fn headless_options_need_headless() {
        assert!(Cli::try_parse_from(["x", "fw", "--log"]).is_err());
        assert!(Cli::try_parse_from(["x", "--headless"]).is_err());
        assert!(Cli::try_parse_from(["x", "fw", "--board", "b", "--net", "loopback"]).is_ok());
        let bt = |v: &str| Cli::try_parse_from(["x", "fw", "--bt", v]).map(|c| c.run.bt);
        assert_eq!(bt("hci1").unwrap(), BtMode::Adapter(1));
        assert_eq!(bt("mock").unwrap(), BtMode::Mock);
        assert!(bt("hcix").is_err() && bt("usb").is_err());
        assert_eq!(input("wifi:midi=0x903C64@5").unwrap().value, 0x903C64);
        assert!(matches!(
            Cli::try_parse_from(["x", "mkdisk", "a.img", "--dir", "data"])
                .unwrap()
                .command,
            Some(Command::Mkdisk(_))
        ));
        // A later option wins, so `cargo run -- ...` can override an example's runner options.
        assert_eq!(
            Cli::try_parse_from(["x", "fw", "--bt", "mock", "--bt", "hci0"])
                .unwrap()
                .run
                .bt,
            BtMode::Adapter(0)
        );
    }

    #[test]
    fn boards_and_shields_are_files_or_built_in() {
        let board = |b: &str| {
            Cli::try_parse_from(["x", "fw", "--board", b])
                .unwrap()
                .run
                .board()
        };
        for (name, _) in BOARDS {
            let b = board(name).unwrap().unwrap();
            crate::session::parse_board(Some(&b), &[]).unwrap();
            assert!(b.path.is_none());
        }
        let file = board(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../boards/pico-w.toml"
        ))
        .unwrap()
        .unwrap();
        assert!(file.path.is_some());
        assert!(
            board("pico2")
                .err()
                .unwrap()
                .contains("pico, btl-01, pico-w")
        );
        assert!(
            Cli::try_parse_from(["x", "fw"])
                .unwrap()
                .run
                .board()
                .unwrap()
                .is_none()
        );

        // Every shield fits on every board, the default one included.
        let run = Cli::try_parse_from(["x", "fw", "--shield", "pico-display-2"])
            .unwrap()
            .run;
        let shields = run.shields().unwrap();
        assert_eq!(shields[0].name, "pico-display-2.toml");
        for (name, _) in BOARDS.iter().filter(|(n, _)| *n != "btl-01") {
            let b = crate::session::parse_board(Some(&board(name).unwrap().unwrap()), &shields)
                .unwrap();
            assert_eq!(b.shields, ["Pico Display Pack 2.0"]);
        }
        let b = crate::session::parse_board(None, &shields).unwrap();
        assert_eq!(b.name, "Pico");
        assert_eq!(b.shields, ["Pico Display Pack 2.0"]);
        assert!(
            Cli::try_parse_from(["x", "fw", "--shield", "hat"])
                .unwrap()
                .run
                .shields()
                .err()
                .unwrap()
                .contains("pico-display-2")
        );
    }

    #[test]
    fn the_committed_schema_is_up_to_date() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../boards/board.schema.json"
        );
        let committed = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            committed == schema(),
            "boards/board.schema.json is stale: run `cargo run -p tartelette -- schema > boards/board.schema.json`"
        );
    }
}
