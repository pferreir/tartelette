//! Device kinds by name: what `kind` words in a board file can mean.
use super::params::Params;
use crate::device::Device;
use alloc::{boxed::Box, string::String, vec::Vec};

/// Builds a device from its board-file parameters. Take every parameter you
/// understand through the [`Params`] getters; the board rejects leftovers.
pub type Factory = fn(&mut Params) -> Result<Box<dyn Device>, String>;

pub struct Registry {
    kinds: Vec<(&'static str, Factory)>,
}

impl Registry {
    /// No kinds at all.
    pub fn empty() -> Self {
        Self { kinds: Vec::new() }
    }

    /// Make `kind` available to board files. Re-registering a kind replaces
    /// it, so a host can override a built-in model.
    pub fn register(&mut self, kind: &'static str, factory: Factory) -> &mut Self {
        match self.kinds.iter_mut().find(|(k, _)| *k == kind) {
            Some(slot) => slot.1 = factory,
            None => self.kinds.push((kind, factory)),
        }
        self
    }

    pub fn get(&self, kind: &str) -> Option<Factory> {
        self.kinds.iter().find(|(k, _)| *k == kind).map(|(_, f)| *f)
    }

    pub fn kinds(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.kinds.iter().map(|(k, _)| *k)
    }
}
