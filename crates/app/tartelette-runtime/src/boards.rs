//! The board files built into the emulator, and building a board from
//! board and shield files.
use pcb::Board;
#[cfg(not(target_arch = "wasm32"))]
use pcb_devices::wav::Wav;

use crate::Source;

/// The board used without one: a plain Raspberry Pi Pico.
pub const PICO_BOARD: &str = include_str!("../../../../boards/pico.toml");

/// The boards in the repository's `boards/`, built in so that an installed
/// `tartelette` has them too.
pub const BOARDS: &[(&str, &str)] = &[
    ("pico", PICO_BOARD),
    ("btl-01", include_str!("../../../../boards/btl-01.toml")),
    ("pico-w", include_str!("../../../../boards/pico-w.toml")),
];

/// The shields in `boards/shields/`.
pub const SHIELDS: &[(&str, &str)] = &[(
    "pico-display-2",
    include_str!("../../../../boards/shields/pico-display-2.toml"),
)];

/// The built-in board or shield called `name` in `list`.
pub fn builtin(list: &[(&str, &str)], name: &str) -> Option<Source> {
    list.iter()
        .find(|(n, _)| *n == name)
        .map(|(n, text)| Source::new(format!("{n}.toml"), text.as_bytes().to_vec()))
}

/// Build the board from its file (a plain Pico without one) and the shields on
/// top, with the RP2040's pin rules for buses. Natively, the files parts
/// name (a `wav` part's `file`) are loaded too, relative to the board or
/// shield file that names them.
pub fn parse_board(board: Option<&Source>, shields: &[Source]) -> Result<Board, String> {
    let mut registry = pcb_devices::registry();
    registry.set_pin_check(rp2040_core::pinmux::check_bus_pin);
    let text = |s: &Source| {
        std::str::from_utf8(&s.bytes)
            .map(str::to_owned)
            .map_err(|_| format!("{}: not a text file", s.name))
    };
    let mut b = match board {
        Some(s) => {
            Board::parse_with(&text(s)?, &registry).map_err(|e| format!("{}:{e}", s.name))?
        }
        None => Board::parse_with(PICO_BOARD, &registry).map_err(|e| format!("pico.toml:{e}"))?,
    };
    load_files(&mut b, 0, board)?;
    for s in shields {
        let from = b.parts.len();
        b.add_shield(&text(s)?, &registry)
            .map_err(|e| format!("{}:{e}", s.name))?;
        load_files(&mut b, from, Some(s))?;
    }
    Ok(b)
}

/// Load the files that parts `from..` name, relative to `src`, the file
/// that declares them (the working directory for a built-in board). The
/// browser has no files to read: there the front panel loads them.
#[cfg_attr(target_arch = "wasm32", expect(unused_variables))]
fn load_files(b: &mut Board, from: usize, src: Option<&Source>) -> Result<(), String> {
    #[cfg(not(target_arch = "wasm32"))]
    for p in &mut b.parts[from..] {
        let any: &mut dyn std::any::Any = &mut *p.device;
        let Some(wav) = any.downcast_mut::<Wav>() else {
            continue;
        };
        let Some(file) = wav.file.clone() else {
            continue;
        };
        let dir = src
            .and_then(|s| s.path.as_deref())
            .and_then(std::path::Path::parent)
            .unwrap_or(std::path::Path::new(""));
        let what = format!("{}: {}.file", src.map_or("pico.toml", |s| &s.name), p.name);
        let file = Source::from_file(&dir.join(file)).map_err(|e| format!("{what}: {e}"))?;
        wav.load(&file.name, &file.bytes)
            .map_err(|e| format!("{what}: {e}"))?;
    }
    Ok(())
}
