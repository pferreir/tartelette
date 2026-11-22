//! The USB composite device of `pico-usb` on rp2040-hal and usb-device: a
//! CDC-ACM serial port (usbd-serial), a boot keyboard (usbd-hid) and a
//! USB-MIDI interface (usbd-midi), polled from the main loop.
//!
//! * Serial: echoes what it receives in upper case; a `!` in it also types
//!   an `a` on the keyboard.
//! * MIDI: echoes every event packet, note on and off an octave up.
#![no_std]
#![no_main]

use defmt::info;
use defmt_rtt as _;
use panic_probe as _;
use rp2040_hal::{self as hal, clocks::init_clocks_and_plls, pac, usb::UsbBus, watchdog::Watchdog};
use usb_device::{class_prelude::*, prelude::*};
use usbd_hid::hid_class::HIDClass;
use usbd_midi::UsbMidiClass;
use usbd_serial::SerialPort;

#[unsafe(link_section = ".boot2")]
#[used]
pub static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_GENERIC_03H;

const XTAL_FREQ_HZ: u32 = 12_000_000;

/// A boot keyboard: modifiers, a reserved byte and six keys in; five LEDs out.
#[rustfmt::skip]
const KEYBOARD: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7,
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01,
    0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01, 0x05, 0x08, 0x19, 0x01,
    0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65,
    0x81, 0x00, 0xC0,
];
/// The usage of the A key.
const KEY_A: u8 = 0x04;

#[hal::entry]
fn main() -> ! {
    let mut pac = pac::Peripherals::take().unwrap();
    let mut watchdog = Watchdog::new(pac.WATCHDOG);
    let clocks = init_clocks_and_plls(
        XTAL_FREQ_HZ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();
    let bus = UsbBusAllocator::new(UsbBus::new(
        pac.USBCTRL_REGS,
        pac.USBCTRL_DPRAM,
        clocks.usb_clock,
        true,
        &mut pac.RESETS,
    ));
    let mut serial = SerialPort::new(&bus);
    let mut keyboard = HIDClass::new(&bus, KEYBOARD, 10);
    let mut midi = UsbMidiClass::new(&bus, 1, 1).unwrap();
    // pid.codes' test VID/PID.
    let mut dev = UsbDeviceBuilder::new(&bus, UsbVidPid(0x1209, 0x0001))
        .strings(&[StringDescriptors::default()
            .manufacturer("Tartelette")
            .product("USB gadgets (rp2040-hal)")
            .serial_number("H0001")])
        .unwrap()
        .composite_with_iads()
        .max_packet_size_0(64)
        .unwrap()
        .build();
    info!("USB up");

    // Keyboard reports still to send: the press, then the release.
    let mut keys: &[u8] = &[];
    let mut buf = [0u8; 64];
    loop {
        if !dev.poll(&mut [&mut serial, &mut keyboard, &mut midi]) {
            continue;
        }
        if let Ok(n @ 1..) = serial.read(&mut buf) {
            let data = &mut buf[..n];
            if data.contains(&b'!') {
                keys = &[KEY_A, 0];
            }
            data.make_ascii_uppercase();
            let mut rest: &[u8] = data;
            while !rest.is_empty() {
                match serial.write(rest) {
                    Ok(k) => rest = &rest[k..],
                    Err(UsbError::WouldBlock) => {
                        dev.poll(&mut [&mut serial, &mut keyboard, &mut midi]);
                    }
                    Err(_) => break,
                }
            }
        }
        if let [key, more @ ..] = keys
            && keyboard
                .push_raw_input(&[0, 0, *key, 0, 0, 0, 0, 0])
                .is_ok()
        {
            keys = more;
        }
        if let Ok(n @ 1..) = midi.read(&mut buf) {
            for packet in buf[..n].as_chunks_mut::<4>().0 {
                transpose(packet);
                let p = [packet[0], packet[1], packet[2], packet[3]];
                while let Err(UsbError::WouldBlock) = midi.send_bytes(p) {
                    dev.poll(&mut [&mut serial, &mut keyboard, &mut midi]);
                }
            }
        }
    }
}

/// Note on and off (code index numbers 9 and 8) an octave up.
fn transpose(packet: &mut [u8]) {
    if matches!(packet[0] & 0xF, 0x8 | 0x9) && packet[2] < 116 {
        packet[2] += 12;
    }
}
