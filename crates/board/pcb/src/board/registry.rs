//! Device kinds by name (what `kind = "..."` in a board file can mean), and
//! the chip's rules for which pins each bus signal may use.
use alloc::{string::String, vec::Vec};

use super::spec::Kind;

/// Says whether bus signal `signal` (`"sck"`, `"sda"`, ...) of controller
/// `bus` (`"spi0"`, ...) can be routed to GPIO `pin`; `Err` explains where
/// it can go instead.
pub type PinCheck = fn(bus: &str, signal: &str, pin: usize) -> Result<(), String>;

pub struct Registry {
    kinds: Vec<Kind>,
    pub(crate) pin_check: Option<PinCheck>,
}

impl Registry {
    /// No kinds at all, and no pin rules.
    pub fn empty() -> Self {
        Self {
            kinds: Vec::new(),
            pin_check: None,
        }
    }

    /// Make a kind available to board files. Re-registering a name replaces
    /// it, so a host can override a built-in model.
    pub fn register(&mut self, kind: Kind) -> &mut Self {
        match self.kinds.iter_mut().find(|k| k.name == kind.name) {
            Some(slot) => *slot = kind,
            None => self.kinds.push(kind),
        }
        self
    }

    /// Check the pins of every `[buses]` entry with the chip's pin mux rules.
    pub fn set_pin_check(&mut self, check: PinCheck) -> &mut Self {
        self.pin_check = Some(check);
        self
    }

    pub fn get(&self, name: &str) -> Option<&Kind> {
        self.kinds.iter().find(|k| k.name == name)
    }

    pub fn kinds(&self) -> impl Iterator<Item = &Kind> + '_ {
        self.kinds.iter()
    }
}
