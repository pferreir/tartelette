//! The emulated device's functions as Linux devices.
//!
//! * [`Pty`]: its serial port as a pseudo-terminal (`/dev/pts/N`, optionally
//!   behind a symlink), which terminal programs, `mpremote` and pyserial
//!   open like `/dev/ttyACM0`. DTR follows whether a program has it open.
//! * [`Uhid`]: a HID interface as a kernel input device through `/dev/uhid`,
//!   with the device's own report descriptor. The kernel's HID drivers take
//!   it from there: an emulated keyboard types into the desktop session.
//!   Opening `/dev/uhid` usually needs root or a udev rule.
//!
//! Both are non-blocking and are serviced by polling from the emulator's
//! loop; there are no threads.
#[cfg(target_os = "linux")]
mod pty;
#[cfg(target_os = "linux")]
mod uhid;

#[cfg(target_os = "linux")]
pub use pty::Pty;
#[cfg(target_os = "linux")]
pub use uhid::{Uhid, UhidDevice};
