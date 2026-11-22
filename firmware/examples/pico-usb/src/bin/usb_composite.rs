//! A USB composite device: a CDC-ACM serial port, a boot keyboard and a
//! USB-MIDI interface, on embassy-usb.
//!
//! * Serial: echoes what it receives in upper case; a `!` in it also types
//!   an `a` on the keyboard.
//! * MIDI: echoes every event packet, note on and off an octave up.
#![no_std]
#![no_main]

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_futures::join::join4;
use embassy_rp::{
    bind_interrupts,
    peripherals::USB,
    usb::{Driver, InterruptHandler},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_usb::{
    Builder, Config,
    class::{
        cdc_acm::device::{self as cdc_acm, CdcAcmClass},
        hid::{
            HidBootProtocol, HidSubclass,
            device::{self as hid, HidWriter},
        },
        midi::device::{MidiClass, MidiClassConfig},
    },
};
use panic_probe as _;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

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

static TYPE_A: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let driver = Driver::new(p.USB, Irqs);

    // pid.codes' test VID/PID.
    let mut config = Config::new(0x1209, 0x0001);
    config.manufacturer = Some("Tartelette");
    config.product = Some("USB gadgets (embassy)");
    config.serial_number = Some("E0001");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    // Declared before the builder, which borrows them.
    let mut cdc_state = cdc_acm::State::new();
    let mut hid_state = hid::State::new();
    let mut config_descriptor = [0; 256];
    let mut bos_descriptor = [0; 256];
    let mut control_buf = [0; 64];
    let mut builder = Builder::new(
        driver,
        config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut [],
        &mut control_buf,
    );
    let mut serial = CdcAcmClass::new(&mut builder, &mut cdc_state, cdc_acm::Config::new());
    let mut keyboard = HidWriter::<_, 8>::new(
        &mut builder,
        &mut hid_state,
        hid::Config {
            report_descriptor: KEYBOARD,
            request_handler: None,
            poll_ms: 10,
            max_packet_size: 8,
            hid_subclass: HidSubclass::Boot,
            hid_boot_protocol: HidBootProtocol::Keyboard,
            interface_name: None,
        },
    );
    // One jack each way, 64-byte packets.
    let mut midi = MidiClass::new(&mut builder, MidiClassConfig::default());
    let mut usb = builder.build();

    let serial_loop = async {
        let mut buf = [0; 64];
        loop {
            serial.wait_connection().await;
            info!("serial: connected");
            while let Ok(n) = serial.read_packet(&mut buf).await {
                let data = &mut buf[..n];
                if data.contains(&b'!') {
                    TYPE_A.signal(());
                }
                data.make_ascii_uppercase();
                if serial.write_packet(data).await.is_err() {
                    break;
                }
            }
            info!("serial: disconnected");
        }
    };
    let keyboard_loop = async {
        loop {
            TYPE_A.wait().await;
            for key in [KEY_A, 0] {
                let _ = keyboard.write(&[0, 0, key, 0, 0, 0, 0, 0]).await;
            }
        }
    };
    let midi_loop = async {
        let mut buf = [0; 64];
        loop {
            midi.wait_connection().await;
            info!("midi: connected");
            while let Ok(n) = midi.read_packet(&mut buf).await {
                for packet in buf[..n].as_chunks_mut::<4>().0 {
                    transpose(packet);
                }
                if midi.write_packet(&buf[..n]).await.is_err() {
                    break;
                }
            }
        }
    };
    join4(usb.run(), serial_loop, keyboard_loop, midi_loop).await;
}

/// Note on and off (code index numbers 9 and 8) an octave up.
fn transpose(packet: &mut [u8]) {
    if matches!(packet[0] & 0xF, 0x8 | 0x9) && packet[2] < 116 {
        packet[2] += 12;
    }
}
