//! What the board's radios talk to: the host's network or a fake one for
//! Wi-Fi, a mock neighbourhood or a real adapter for Bluetooth.
use pcb::Board;

/// What the board's Wi-Fi chips are connected to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum NetMode {
    /// The host's network, through a user-mode NAT (the host's 127.0.0.1 is 10.0.2.2)
    #[default]
    User,
    /// A deterministic fake network with only a gateway
    Loopback,
    /// No network, as in the browser
    None,
}

/// What the board's Bluetooth radios are connected to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum BtMode {
    /// No radio, as in the browser: the firmware finds and reaches nobody.
    #[default]
    None,
    /// Mock devices in software: a central, a BLE MIDI peripheral, a battery.
    Mock,
    /// A real adapter (`hciN`), through a Linux HCI user channel.
    Adapter(u16),
}

#[derive(Clone, Debug, Default)]
pub struct Network {
    pub mode: NetMode,
    pub bt: BtMode,
    /// (host port, guest port): the host's `127.0.0.1:HOST` forwarded to a
    /// TCP port on the guest (user mode only).
    pub forwards: Vec<(u16, u16)>,
    /// Which of the USB device's functions reach the host OS.
    pub usb: crate::usb::UsbBridges,
    /// Which UARTs reach the host OS.
    pub uart: crate::uart::UartBridges,
}

/// Give every Wi-Fi chip on the board a network to talk to. The browser has no
/// user-mode NAT, so there the chip finds no networks and the panel says why.
pub(crate) fn connect(board: &mut Board, net: &Network) -> Result<(), String> {
    let mut chips = board
        .devices_mut::<pcb_devices::cyw43439::Cyw43439>()
        .peekable();
    if chips.peek().is_none() && !net.forwards.is_empty() {
        return Err("--forward: no Wi-Fi chip on the board".into());
    }
    for (_, chip) in chips {
        match net.mode {
            #[cfg(not(target_arch = "wasm32"))]
            NetMode::User => {
                let mut slirp = bridge_net::Slirp::new();
                for &(host, guest) in &net.forwards {
                    let addr = slirp
                        .forward(host, guest)
                        .map_err(|e| format!("--forward {host}:{guest}: {e}"))?;
                    eprintln!("forwarding {addr} to the guest's port {guest}");
                }
                chip.set_backend(Box::new(slirp));
            }
            NetMode::Loopback => chip.set_backend(pcb_devices::net::LoopbackBackend::boxed()),
            _ => {}
        }
    }
    connect_bluetooth(board, net.bt)
}

/// Give every Bluetooth radio on the board its controller. A real adapter
/// can serve only one.
fn connect_bluetooth(board: &mut Board, bt: BtMode) -> Result<(), String> {
    use pcb_devices::{
        cyw43439::bt::bt_address,
        hci::{peers, virt::VirtualController},
    };
    let mut chips = board.devices_mut::<pcb_devices::cyw43439::Cyw43439>();
    while let Some((name, chip)) = chips.next() {
        match bt {
            BtMode::None => {}
            BtMode::Mock => chip.set_bt_backend(Box::new(VirtualController::new(
                bt_address(chip.mac()),
                peers::neighbourhood(),
            ))),
            BtMode::Adapter(dev) => {
                if chips.next().is_some() {
                    return Err(format!(
                        "--bt hci{dev}: the board has more than one Bluetooth radio"
                    ));
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    chip.set_bt_backend(bridge_hci::open(dev)?);
                    eprintln!("{name}: Bluetooth bridged to hci{dev}");
                }
                #[cfg(target_arch = "wasm32")]
                let _ = (name, dev);
                return Ok(());
            }
        }
        let _ = name;
    }
    Ok(())
}
