//! Command line (native only): one set of options for the front panel and for
//! headless runs, plus the `mkdisk` tool.
use crate::session::{BtMode, NetMode, Network};
use clap::{Args, Parser, Subcommand};
use rp2040_core::system::CYCLES_PER_US;
use std::path::PathBuf;

/// Tartelette, an RP2040 emulator: runs a firmware ELF on an emulated board, with a front
/// panel window or headless.
///
/// Headless, UART0 output goes to stdout and UART1 output to stderr, with the
/// board's events (LEDs, buttons, ...) logged to stderr at their time in
/// microseconds. With a window, the firmware and board reload when their
/// files change.
#[derive(Parser, Debug)]
#[command(version, args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[command(flatten)]
    pub run: RunArgs,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    Mkdisk(MkdiskArgs),
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Firmware ELF to run (with a window, it can also be opened later)
    #[arg(required_if_eq("headless", "true"))]
    pub firmware: Option<PathBuf>,
    /// Board description file
    #[arg(long, value_name = "FILE")]
    pub board: Option<PathBuf>,
    /// Load a disk image into the SD card part NAME; headless, it is saved
    /// back at the end if the firmware wrote to it (the card needs at least
    /// 512 KiB, so a smaller image file grows)
    #[arg(long, value_name = "NAME=FILE", value_parser = name_and_path)]
    pub sd: Vec<(String, PathBuf)>,
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
    /// Run without a window
    #[arg(long)]
    pub headless: bool,
    #[command(flatten)]
    pub headless_opts: Headless,
}

impl RunArgs {
    pub fn network(&self) -> Network {
        Network { mode: self.net, bt: self.bt, forwards: self.forward.clone() }
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
}

impl Headless {
    /// Every scheduled action, in time order (a press and release at the same
    /// time keep their order).
    pub fn inputs(&self) -> Vec<Input> {
        let mut v = self.input.clone();
        for p in &self.press {
            let at = |cycle| Input { cycle, part: p.part.clone(), action: "press".into(), value: 0 };
            v.push(at(p.cycle));
            if let Some(hold) = p.hold {
                v.push(Input { action: "release".into(), ..at(p.cycle + hold) });
            }
        }
        v.sort_by_key(|i| i.cycle);
        v
    }
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
    s.parse::<f64>().ok().filter(|v| *v >= 0.0).map(|v| (v * CYCLES_PER_US as f64) as u64).ok_or_else(|| format!("{s:?} is not a time in microseconds"))
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
    Ok((h.parse().map_err(|e| format!("{h:?}: {e}"))?, g.parse().map_err(|e| format!("{g:?}: {e}"))?))
}

/// `NAME@US` or `NAME@US+HOLD_US`.
fn press(s: &str) -> Result<Press, String> {
    let (part, when) = s.split_once('@').ok_or("expected NAME@US[+HOLD]")?;
    let (at, hold) = match when.split_once('+') {
        Some((a, h)) => (a, Some(us_to_cycles(h)?)),
        None => (when, None),
    };
    Ok(Press { part: part.into(), cycle: us_to_cycles(at)?, hold })
}

/// `NAME:ACTION@US` or `NAME:ACTION=VALUE@US`.
fn input(s: &str) -> Result<Input, String> {
    let (target, when) = s.rsplit_once('@').ok_or("expected NAME:ACTION[=VALUE]@US")?;
    let (part, action) = target.split_once(':').ok_or("expected NAME:ACTION[=VALUE]@US")?;
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
    Ok(Input { cycle: us_to_cycles(when)?, part: part.into(), action: action.into(), value })
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
        let cli = Cli::try_parse_from(["x", "fw", "--headless", "--press", "b@100+50", "--input", "k:turn=-1@120"]).unwrap();
        let v: Vec<_> = cli.run.headless_opts.inputs().into_iter().map(|i| (i.cycle, i.action, i.value)).collect();
        assert_eq!(v, [(12_500, "press".into(), 0), (15_000, "turn".into(), -1), (18_750, "release".into(), 0)]);
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
        assert!(matches!(Cli::try_parse_from(["x", "mkdisk", "a.img", "--dir", "data"]).unwrap().command, Some(Command::Mkdisk(_))));
    }
}
