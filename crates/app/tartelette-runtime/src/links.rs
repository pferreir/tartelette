//! Everything a run connects to on the host, as the command line or the
//! page asks for it: the radios' world, and which of the board's serial
//! links and USB functions reach the host OS.
use crate::{BtMode, NetMode, uart::UartBridges, usb::UsbBridges};

#[derive(Clone, Debug, Default)]
pub struct Links {
    /// What the Wi-Fi chips are connected to.
    pub wifi: NetMode,
    /// What the Bluetooth radios are connected to.
    pub bt: BtMode,
    /// (host port, guest port): the host's `127.0.0.1:HOST` forwarded to a
    /// TCP port on the guest (user mode only).
    pub forwards: Vec<(u16, u16)>,
    /// Which of the USB device's functions reach the host OS.
    pub usb: UsbBridges,
    /// Which UARTs reach the host OS.
    pub uart: UartBridges,
}
