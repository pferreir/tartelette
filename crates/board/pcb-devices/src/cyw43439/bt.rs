//! The chip's Bluetooth side as the host driver sees it (Embassy `cyw43`
//! `bluetooth.rs`): a patch download into the Bluetooth core's address space,
//! a few control registers on the backplane, and two ring buffers in WLAN
//! RAM that carry HCI packets. Behind them is an [`HciBackend`]: a real
//! controller bridged by the host, or the [virtual one](VirtualController).
//!
//! Ring layout (`WLAN_RAM_BASE` points at it): host-to-BT data at +0, BT-to-
//! host data at +0x1000, then the four indices (H2B in/out, B2H in/out).
//! Each packet is a 4-byte header (24-bit length, H4 type) and the payload
//! padded to a multiple of four. The writer owns "in", the reader "out".
use alloc::{boxed::Box, collections::VecDeque, vec::Vec};

use super::backplane::Backplane;
use crate::hci::{HciBackend, virt::VirtualController};

pub const BT_CTRL: u32 = 0x1800_0C7C;
pub const WLAN_RAM_BASE: u32 = 0x1800_0D68;
pub const HOST_CTRL: u32 = 0x1800_0D6C;
pub const SDIO_INT_STATUS: u32 = 0x1800_2020;
pub const SDIO_INT_HOST_MASK: u32 = 0x1800_2024;
/// The Bluetooth core's own address space (patch RAM, power-up register).
const BT_SPACE: core::ops::Range<u32> = 0x1900_0000..0x1A00_0000;

/// Where the rings live in RAM: clear of the WLAN firmware and of the
/// shared-memory area at the top.
pub const RING: u32 = 0x6_8000;
const BUF: u32 = 0x1000;
const H2B_IN: u32 = 0x2000;
const H2B_OUT: u32 = 0x2004;
const B2H_IN: u32 = 0x2008;
const B2H_OUT: u32 = 0x200C;

const FW_READY: u32 = 1 << 24;
const BT_AWAKE: u32 = 1 << 8;
const HOST_READY: u32 = 1 << 24;
/// SDIO core interrupt: the Bluetooth rings changed.
const I_HMB_FC_CHANGE: u32 = 1 << 5;

pub struct Bt {
    cycles_per_us: u64,
    pub backend: Box<dyn HciBackend>,
    /// Last write into the Bluetooth core (the patch download), in cycles.
    patched_at: Option<u64>,
    int_status: u32,
    /// Packets from the backend waiting for room in the ring.
    pending: VecDeque<(u8, Vec<u8>)>,
    /// The host has set up the rings and said it is ready.
    pub up: bool,
    /// What the traffic says the radio is doing: scanning, advertising, and
    /// how many connections are open.
    pub scanning: bool,
    pub advertising: bool,
    pub links: u16,
    /// Observable happenings for the board's event queue.
    pub notes: Vec<(&'static str, i64)>,
}

fn round4(n: u32) -> u32 {
    (n + 3) & !3
}

impl Bt {
    pub fn new(backend: Box<dyn HciBackend>, cycles_per_us: u64) -> Self {
        Self {
            cycles_per_us,
            backend,
            patched_at: None,
            int_status: 0,
            pending: VecDeque::new(),
            up: false,
            scanning: false,
            advertising: false,
            links: 0,
            notes: Vec::new(),
        }
    }

    /// The controller to use with nothing better installed: the virtual
    /// one with no radio. `mac` is the WLAN MAC; Bluetooth gets the next one.
    pub fn no_radio(mac: [u8; 6]) -> Box<dyn HciBackend> {
        Box::new(VirtualController::no_radio(bt_address(mac)))
    }

    /// The patch has been in for a moment: the core is up.
    fn ready(&self, now: u64) -> bool {
        self.patched_at
            .is_some_and(|t| now >= t + 2000 * self.cycles_per_us)
    }

    fn us(&self, now: u64) -> u64 {
        now / self.cycles_per_us
    }

    /// The Bluetooth interrupt toward the host (F1_INTR), given the SDIO
    /// host interrupt mask.
    pub fn interrupt(&self, bp: &Backplane) -> bool {
        self.int_status & bp.read32(SDIO_INT_HOST_MASK) != 0
    }

    /// A 32-bit register read the Bluetooth side answers itself.
    pub fn read32(&self, now: u64, addr: u32) -> Option<u32> {
        let ready = self.ready(now);
        match addr {
            BT_CTRL => Some(if ready { FW_READY | BT_AWAKE } else { 0 }),
            WLAN_RAM_BASE => Some(if ready { RING } else { 0 }),
            SDIO_INT_STATUS => Some(self.int_status),
            _ => None,
        }
    }

    /// A backplane write at `addr`. Returns true if it was handled here
    /// (otherwise it is plain memory or register storage).
    pub fn write(&mut self, now: u64, addr: u32, data: &[u8], bp: &mut Backplane) -> bool {
        match addr {
            SDIO_INT_STATUS => {
                let v = u32::from_le_bytes(
                    data.get(..4)
                        .and_then(|d| d.try_into().ok())
                        .unwrap_or_default(),
                );
                self.int_status &= !v;
                true
            }
            HOST_CTRL => {
                bp.write(addr, data);
                // Each toggle of the data-valid bit says "look at the rings".
                if bp.read32(HOST_CTRL) & HOST_READY != 0 {
                    self.up = true;
                }
                self.pump(now, bp);
                true
            }
            a if BT_SPACE.contains(&a) => {
                self.patched_at = Some(now);
                bp.write(addr, data);
                true
            }
            _ => false,
        }
    }

    /// Move packets both ways: everything the host queued goes to the
    /// backend, then whatever the backend has goes into the host's ring.
    pub fn pump(&mut self, now: u64, bp: &mut Backplane) {
        if !self.up {
            return;
        }
        let us = self.us(now);
        let host_in = bp.read32(RING + H2B_IN) % BUF;
        let mut out = bp.read32(RING + H2B_OUT) % BUF;
        while out != host_in {
            let header = bp.read32(RING + out);
            let (len, kind) = (header & 0xFF_FFFF, (header >> 24) as u8);
            out = (out + 4) % BUF;
            let mut packet = Vec::with_capacity(len as usize);
            for i in 0..len {
                packet.push(bp.byte(RING + (out + i) % BUF));
            }
            out = (out + round4(len)) % BUF;
            self.watch_host(kind, &packet);
            self.backend.send(us, kind, &packet);
        }
        bp.write32(RING + H2B_OUT, out);
        while let Some(p) = self.backend.recv(us) {
            self.watch_controller(p.0, &p.1);
            self.pending.push_back(p);
        }
        self.deliver(bp);
        self.notes.append(&mut self.backend.take_notes());
    }

    /// Follow the host's commands that start and stop scanning and
    /// advertising (legacy and extended).
    fn watch_host(&mut self, kind: u8, p: &[u8]) {
        if kind != crate::hci::COMMAND || p.len() < 4 {
            return;
        }
        let on = p[3] == 1;
        match u16::from_le_bytes([p[0], p[1]]) {
            0x0C03 => (self.scanning, self.advertising) = (false, false),
            0x200A | 0x2039 => self.advertising = on,
            0x200C | 0x2042 => self.scanning = on,
            _ => {}
        }
    }

    /// Follow connections opening and closing.
    fn watch_controller(&mut self, kind: u8, p: &[u8]) {
        if kind != crate::hci::EVENT || p.len() < 3 {
            return;
        }
        match (p[0], p[2]) {
            // Disconnection Complete, status 0.
            (0x05, 0) => self.links = self.links.saturating_sub(1),
            // LE (Enhanced) Connection Complete, status 0: advertising stops.
            (0x3E, 0x01 | 0x0A) if p.get(3) == Some(&0) => {
                self.links += 1;
                self.advertising = false;
            }
            _ => {}
        }
    }

    /// Copy waiting packets into the BT-to-host ring while they fit.
    fn deliver(&mut self, bp: &mut Backplane) {
        let base = RING + BUF;
        while let Some((kind, packet)) = self.pending.front() {
            let mut at = bp.read32(RING + B2H_IN) % BUF;
            let used = at.wrapping_sub(bp.read32(RING + B2H_OUT)) % BUF;
            let len = packet.len() as u32;
            // Never fill it completely: equal indices mean empty.
            if 4 + round4(len) >= BUF - used {
                break;
            }
            bp.write32(base + at, len | (*kind as u32) << 24);
            at = (at + 4) % BUF;
            for (i, &b) in packet.iter().enumerate() {
                bp.write(base + (at + i as u32) % BUF, &[b]);
            }
            at = (at + round4(len)) % BUF;
            bp.write32(RING + B2H_IN, at);
            self.int_status |= I_HMB_FC_CHANGE;
            self.pending.pop_front();
        }
    }
}

/// The Bluetooth address that goes with a WLAN MAC (the next address), as
/// sent over HCI (little-endian).
pub fn bt_address(mac: [u8; 6]) -> [u8; 6] {
    let n = u64::from_be_bytes([0, 0, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]]) + 1;
    let b = n.to_be_bytes();
    [b[7], b[6], b[5], b[4], b[3], b[2]]
}
