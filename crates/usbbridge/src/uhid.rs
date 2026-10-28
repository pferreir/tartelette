use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

use usbhost::function::{Hid, HidRequest, ReportType};

/// `sizeof(struct uhid_event)` (linux/uhid.h): a u32 type and a packed union.
const EVENT_SIZE: usize = 4380;
const DESTROY: u32 = 1;
#[cfg(test)]
const START: u32 = 2;
const OUTPUT: u32 = 6;
const GET_REPORT: u32 = 9;
const GET_REPORT_REPLY: u32 = 10;
const CREATE2: u32 = 11;
const INPUT2: u32 = 12;
const SET_REPORT: u32 = 13;
const SET_REPORT_REPLY: u32 = 14;
/// `rtype` values.
const FEATURE_REPORT: u8 = 0;
const OUTPUT_REPORT: u8 = 1;
const BUS_USB: u16 = 3;
const DATA_MAX: usize = 4096;

/// What the kernel is told about the device.
pub struct UhidDevice<'a> {
    pub name: &'a str,
    pub vendor: u16,
    pub product: u16,
    pub country: u8,
    pub report_descriptor: &'a [u8],
}

/// One HID interface as a kernel HID device; it goes away when dropped.
pub struct Uhid {
    fd: OwnedFd,
    /// Kernel requests waiting for a reply: (id, was a GET_REPORT).
    asked: Vec<(u32, bool)>,
}

impl Uhid {
    pub fn create(dev: &UhidDevice) -> Result<Self, String> {
        let fd = unsafe {
            libc::open(
                c"/dev/uhid".as_ptr(),
                libc::O_RDWR | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            let e = io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(libc::EACCES) | Some(libc::EPERM) => "/dev/uhid: permission denied (USB HID bridging needs root, or a udev rule such as KERNEL==\"uhid\", GROUP=\"input\", MODE=\"0660\" and membership of that group)".into(),
                Some(libc::ENOENT) => "/dev/uhid: missing (load the uhid module: sudo modprobe uhid)".into(),
                _ => format!("/dev/uhid: {e}"),
            });
        }
        let u = Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            asked: Vec::new(),
        };
        let mut ev = event(CREATE2);
        let name = dev.name.as_bytes();
        let n = name.len().min(127);
        ev[4..4 + n].copy_from_slice(&name[..n]);
        let rd = &dev.report_descriptor[..dev.report_descriptor.len().min(DATA_MAX)];
        put16(&mut ev, 260, rd.len() as u16);
        put16(&mut ev, 262, BUS_USB);
        put32(&mut ev, 264, dev.vendor as u32);
        put32(&mut ev, 268, dev.product as u32);
        put32(&mut ev, 276, dev.country as u32);
        ev[280..280 + rd.len()].copy_from_slice(rd);
        u.send(&ev).map_err(|e| format!("/dev/uhid: create: {e}"))?;
        Ok(u)
    }

    fn send(&self, ev: &[u8]) -> io::Result<()> {
        let n = unsafe { libc::write(self.fd.as_raw_fd(), ev.as_ptr().cast(), ev.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Input reports to the kernel; output reports and report requests
    /// from it, and the device's replies back.
    pub fn exchange(&mut self, h: &mut Hid) {
        for report in h.input.drain(..) {
            let mut ev = event(INPUT2);
            let n = report.len().min(DATA_MAX);
            put16(&mut ev, 4, n as u16);
            ev[6..6 + n].copy_from_slice(&report[..n]);
            let _ = self.send(&ev);
        }
        let mut ev = vec![0u8; EVENT_SIZE];
        loop {
            let n = unsafe { libc::read(self.fd.as_raw_fd(), ev.as_mut_ptr().cast(), ev.len()) };
            if n < 4 {
                break;
            }
            match get32(&ev, 0) {
                OUTPUT if ev[4102] == OUTPUT_REPORT => {
                    let size = (get16(&ev, 4100) as usize).min(DATA_MAX);
                    h.output.push_back(ev[4..4 + size].to_vec());
                }
                GET_REPORT => {
                    let id = get32(&ev, 4);
                    self.asked.push((id, true));
                    h.requests.push_back(HidRequest {
                        id,
                        kind: report_type(ev[9]),
                        report_id: ev[8],
                        set: None,
                    });
                }
                SET_REPORT => {
                    let id = get32(&ev, 4);
                    let size = (get16(&ev, 10) as usize).min(DATA_MAX);
                    self.asked.push((id, false));
                    h.requests.push_back(HidRequest {
                        id,
                        kind: report_type(ev[9]),
                        report_id: ev[8],
                        set: Some(ev[12..12 + size].to_vec()),
                    });
                }
                // START, STOP, OPEN, CLOSE: nothing to do.
                _ => {}
            }
        }
        for reply in h.replies.drain(..) {
            let Some(k) = self.asked.iter().position(|(id, _)| *id == reply.id) else {
                continue;
            };
            let (id, get) = self.asked.remove(k);
            let err = if reply.data.is_some() {
                0
            } else {
                libc::EIO as u16
            };
            let mut ev = event(if get {
                GET_REPORT_REPLY
            } else {
                SET_REPORT_REPLY
            });
            put32(&mut ev, 4, id);
            put16(&mut ev, 8, err);
            if get && let Some(data) = &reply.data {
                let n = data.len().min(DATA_MAX);
                put16(&mut ev, 10, n as u16);
                ev[12..12 + n].copy_from_slice(&data[..n]);
            }
            let _ = self.send(&ev);
        }
    }
}

impl Drop for Uhid {
    fn drop(&mut self) {
        let _ = self.send(&event(DESTROY));
    }
}

fn report_type(rtype: u8) -> ReportType {
    match rtype {
        FEATURE_REPORT => ReportType::Feature,
        OUTPUT_REPORT => ReportType::Output,
        _ => ReportType::Input,
    }
}

fn event(kind: u32) -> Vec<u8> {
    let mut ev = vec![0u8; EVENT_SIZE];
    put32(&mut ev, 0, kind);
    ev
}

fn put16(ev: &mut [u8], at: usize, v: u16) {
    ev[at..at + 2].copy_from_slice(&v.to_ne_bytes());
}

fn put32(ev: &mut [u8], at: usize, v: u32) {
    ev[at..at + 4].copy_from_slice(&v.to_ne_bytes());
}

fn get16(ev: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([ev[at], ev[at + 1]])
}

fn get32(ev: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(ev[at..at + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs access to /dev/uhid (root, or a udev rule): skipped otherwise.
    #[test]
    fn the_kernel_takes_a_keyboard() {
        #[rustfmt::skip]
        const KEYBOARD: [u8; 45] = [
            0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7,
            0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01,
            0x75, 0x08, 0x81, 0x01, 0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65,
            0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
        ];
        let dev = UhidDevice {
            name: "tartelette test keyboard",
            vendor: 0x1209,
            product: 0x0001,
            country: 0,
            report_descriptor: &KEYBOARD,
        };
        let u = match Uhid::create(&dev) {
            Ok(u) => u,
            Err(e) => return eprintln!("skipped: {e}"),
        };
        // The kernel answers UHID_START once a HID driver has bound to it.
        let mut ev = vec![0u8; EVENT_SIZE];
        for _ in 0..200 {
            let n = unsafe { libc::read(u.fd.as_raw_fd(), ev.as_mut_ptr().cast(), ev.len()) };
            if n >= 4 && get32(&ev, 0) == START {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the kernel never started the device");
    }

    #[test]
    fn the_event_is_as_big_as_the_kernels() {
        // CREATE2 is the member with the most fields; it fits.
        const { assert!(280 + DATA_MAX <= EVENT_SIZE) };
    }
}
