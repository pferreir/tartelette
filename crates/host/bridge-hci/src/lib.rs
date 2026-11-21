//! A real Bluetooth controller behind the emulated chip: the firmware's HCI
//! packets go to a host adapter through a Linux HCI *user channel*, which
//! gives this process the adapter to itself (the kernel's Bluetooth stack
//! and `bluetoothd` let go of it while it is open). The firmware's own host
//! stack then drives the radio: it scans, connects and advertises for real.
//!
//! Opening needs `CAP_NET_ADMIN` (`sudo setcap cap_net_admin+ep tartelette`,
//! or root). The adapter is taken down first; when the channel closes the
//! kernel gets it back and `bluetoothd` brings it up again.
//!
//! The controller runs in real time while the firmware runs in emulated
//! time; a run much slower than real time can miss the controller's
//! supervision timeouts and drop connections.
use pcb_devices::hci::HciBackend;

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        io,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
    };

    use super::*;

    const BTPROTO_HCI: libc::c_int = 1;
    const HCI_CHANNEL_USER: u16 = 1;
    /// `_IOW('H', 202, int)`
    const HCIDEVDOWN: libc::c_ulong = 0x4004_48CA;

    #[repr(C)]
    struct SockaddrHci {
        family: libc::sa_family_t,
        dev: u16,
        channel: u16,
    }

    pub struct UserChannel {
        fd: OwnedFd,
    }

    fn explain(e: io::Error, what: &str, name: &str) -> String {
        match e.raw_os_error() {
            Some(libc::EPERM) | Some(libc::EACCES) => {
                format!(
                    "{name}: {what}: permission denied (needs CAP_NET_ADMIN: sudo setcap cap_net_admin+ep on the tartelette binary)"
                )
            }
            Some(libc::EBUSY) => {
                format!("{name}: adapter busy (something else holds its user channel, or it is up)")
            }
            Some(libc::ENODEV) => format!("{name}: no such Bluetooth adapter"),
            _ => format!("{name}: {what}: {e}"),
        }
    }

    impl UserChannel {
        pub fn open(dev: u16) -> Result<Self, String> {
            let name = format!("hci{dev}");
            let raw = unsafe {
                libc::socket(
                    libc::AF_BLUETOOTH,
                    libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    BTPROTO_HCI,
                )
            };
            if raw < 0 {
                return Err(explain(io::Error::last_os_error(), "socket", &name));
            }
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let addr = SockaddrHci {
                family: libc::AF_BLUETOOTH as _,
                dev,
                channel: HCI_CHANNEL_USER,
            };
            // The adapter must be down to be bound; bluetoothd may race to
            // power it up again, so try a few times.
            let mut last = None;
            for _ in 0..5 {
                if unsafe { libc::ioctl(fd.as_raw_fd(), HCIDEVDOWN as _, dev as libc::c_int) } < 0 {
                    let e = io::Error::last_os_error();
                    if e.raw_os_error() != Some(libc::EALREADY) {
                        return Err(explain(e, "taking the adapter down", &name));
                    }
                }
                let r = unsafe {
                    libc::bind(
                        fd.as_raw_fd(),
                        &addr as *const SockaddrHci as *const libc::sockaddr,
                        size_of::<SockaddrHci>() as libc::socklen_t,
                    )
                };
                if r == 0 {
                    return Ok(Self { fd });
                }
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EBUSY) {
                    return Err(explain(e, "binding the user channel", &name));
                }
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(explain(last.unwrap(), "binding the user channel", &name))
        }
    }

    impl HciBackend for UserChannel {
        fn available(&self) -> bool {
            true
        }

        fn send(&mut self, _now: u64, kind: u8, packet: &[u8]) {
            let mut buf = Vec::with_capacity(packet.len() + 1);
            buf.push(kind);
            buf.extend_from_slice(packet);
            // One packet per write; a full socket buffer drops it, as a
            // controller with no buffers would.
            unsafe { libc::write(self.fd.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
        }

        fn recv(&mut self, _now: u64) -> Option<(u8, Vec<u8>)> {
            let mut buf = [0u8; 2048];
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n < 2 {
                return None;
            }
            Some((buf[0], buf[1..n as usize].to_vec()))
        }
    }
}

/// Bridge to adapter `hci<dev>`.
#[cfg(target_os = "linux")]
pub fn open(dev: u16) -> Result<Box<dyn HciBackend>, String> {
    Ok(Box::new(linux::UserChannel::open(dev)?))
}

#[cfg(not(target_os = "linux"))]
pub fn open(_dev: u16) -> Result<Box<dyn HciBackend>, String> {
    Err("bridging to a Bluetooth adapter is only supported on Linux".into())
}
