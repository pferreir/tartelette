//! The board files built into the emulator, and building a board from
//! board and shield files.
use pcb::Board;

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
/// top, with the RP2040's pin rules for buses.
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
    for s in shields {
        b.add_shield(&text(s)?, &registry)
            .map_err(|e| format!("{}:{e}", s.name))?;
    }
    Ok(b)
}
