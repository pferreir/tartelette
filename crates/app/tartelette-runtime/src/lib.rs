//! Running firmware on an emulated RP2040 board, with no UI: what the
//! `tartelette` front panel, its headless mode and the tests share.
//!
//! * [`Session`]: one run, a firmware ELF or UF2 on a board, stepped
//!   against a wall-clock deadline. It moves what the firmware sends out of
//!   the emulator (UART text, board events, the defmt log, warnings) and
//!   connects the board to the host as its [`Links`] ask: its radios
//!   ([`network`]), the far end of the USB cable ([`usb`]) and of the UART
//!   lines ([`uart`]).
//! * [`run`]: a session paced against the wall clock, natively on a thread
//!   of its own.
//! * [`boards`]: the built-in board and shield files, and building a board
//!   from them.
//! * [`synth`]: a test synthesizer that turns the board's CV and gate
//!   outputs into sound samples.
//!
//! It builds natively and for the browser (`wasm32`), where there is no host
//! network, Bluetooth adapter or pseudo-terminal to bridge to.
pub mod boards;
mod links;
pub mod network;
pub mod run;
pub mod session;
mod source;
pub mod synth;
pub mod uart;
pub mod usb;

pub use links::Links;
pub use network::{BtMode, NetMode};
pub use session::{CYCLES_PER_SEC, LogEntry, Session};
pub use source::Source;
