//! The traffic log: what went over the bus, at transfer level for the
//! control pipe and packet level for the others. NAKs are not logged (the
//! host only retries once the device has armed a buffer, so there are few).
use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::fmt::Write;

use crate::{
    Handshake,
    descriptor::{self, Setup},
};

/// Bytes shown per packet before the rest is elided.
const SHOWN: usize = 24;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// When it happened, in the host's clock (emulated cycles).
    pub at: u64,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The device pulled D+ up.
    Attached,
    /// It let go (or was never there).
    Detached,
    Reset,
    /// A whole control transfer: its data stage (read or written) and how it ended.
    Control {
        setup: Setup,
        data: Vec<u8>,
        result: Result<(), Handshake>,
    },
    /// A packet from (IN, address with 0x80) or to an endpoint.
    Data {
        ep: u8,
        data: Vec<u8>,
    },
    /// An endpoint stopped answering or stalled; the host stops using it.
    Halted {
        ep: u8,
        why: Handshake,
    },
    /// A packet with the wrong data toggle: the host takes it for a resend
    /// and drops it.
    Toggle {
        ep: u8,
    },
    /// Milestones and problems in words.
    Note(String),
}

impl Event {
    /// The event without its time, one line.
    pub fn describe(&self) -> String {
        match &self.kind {
            Kind::Attached => "device attached".into(),
            Kind::Detached => "device detached".into(),
            Kind::Reset => "bus reset".into(),
            Kind::Control {
                setup,
                data,
                result,
            } => {
                let mut s = format!(
                    "SETUP {:02x} {:02x} {:04x} {:04x} {:04x}",
                    setup.request_type, setup.request, setup.value, setup.index, setup.length
                );
                if let Some(name) = descriptor::request_name(setup) {
                    s.push_str("  ");
                    s.push_str(name);
                    if setup.request_type & 0x60 == 0 && setup.request == descriptor::GET_DESCRIPTOR
                    {
                        let _ = write!(
                            s,
                            " {} {}",
                            descriptor::descriptor_name((setup.value >> 8) as u8),
                            setup.value & 0xFF
                        );
                    }
                }
                match result {
                    Ok(()) if setup.is_in() => {
                        let _ = write!(s, " → {} bytes {}", data.len(), hex(data));
                    }
                    Ok(()) if !data.is_empty() => {
                        let _ = write!(s, " ← {}", hex(data));
                    }
                    Ok(()) => s.push_str(" ok"),
                    Err(e) => {
                        let _ = write!(s, " failed: {}", e.name());
                    }
                }
                s
            }
            Kind::Data { ep, data } => {
                let dir = if ep & 0x80 != 0 { "IN " } else { "OUT" };
                format!("{dir} ep{} {:3} bytes {}", ep & 0xF, data.len(), hex(data))
            }
            Kind::Halted { ep, why } => format!(
                "ep{}{} halted: {}",
                ep & 0xF,
                if ep & 0x80 != 0 { " IN" } else { " OUT" },
                why.name()
            ),
            Kind::Toggle { ep } => format!(
                "ep{}{}: data toggle mismatch, packet dropped",
                ep & 0xF,
                if ep & 0x80 != 0 { " IN" } else { " OUT" }
            ),
            Kind::Note(s) => s.to_string(),
        }
    }
}

fn hex(data: &[u8]) -> String {
    let mut s = String::new();
    for (i, b) in data.iter().take(SHOWN).enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{b:02x}");
    }
    if data.len() > SHOWN {
        s.push_str(" …");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_transfers_read_well() {
        let e = |result, data: &[u8]| Event {
            at: 0,
            kind: Kind::Control {
                setup: Setup::get_descriptor(1, 0, 0, 18),
                data: data.to_vec(),
                result,
            },
        };
        assert_eq!(
            e(Ok(()), &[0x12, 1]).describe(),
            "SETUP 80 06 0100 0000 0012  GET_DESCRIPTOR device 0 → 2 bytes 12 01"
        );
        assert!(
            e(Err(Handshake::Stall), &[])
                .describe()
                .ends_with("failed: STALL")
        );
        let long = Event {
            at: 0,
            kind: Kind::Data {
                ep: 0x81,
                data: alloc::vec![0; 30],
            },
        };
        assert!(long.describe().starts_with("IN  ep1  30 bytes 00"));
        assert!(long.describe().ends_with(" …"));
    }
}
