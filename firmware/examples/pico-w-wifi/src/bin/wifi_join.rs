//! Joins a network (SSID and password from `WIFI_SSID` / `WIFI_PASS` at
//! build time, or open if the password is empty), gets a DHCP lease, and
//! echoes a UDP datagram off the emulator's gateway (10.0.2.2:7).
#![no_std]
#![no_main]

use cyw43_pio::PioSpi;
use defmt::*;
use embassy_executor::Spawner;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Config, IpEndpoint, Ipv4Address, Stack, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "TarteletteNet",
};
const PASS: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "hunter22",
};

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    stack.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let fw = include_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/43439A0.bin"));
    let clm = include_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/43439A0_clm.bin"));

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(&mut pio.common, pio.sm0, pio.irq0, cs, p.PIN_24, p.PIN_29, p.DMA_CH0);

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    unwrap!(spawner.spawn(cyw43_task(runner)));

    control.init(clm).await;
    control.set_power_management(cyw43::PowerManagementMode::PowerSave).await;

    static RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();
    static STACK: StaticCell<Stack<cyw43::NetDriver<'static>>> = StaticCell::new();
    let stack = &*STACK.init(Stack::new(net_device, Config::dhcpv4(Default::default()), RESOURCES.init(StackResources::new()), 0x0123_4567_89ab_cdef));
    unwrap!(spawner.spawn(net_task(stack)));

    loop {
        let r = if PASS.is_empty() { control.join_open(SSID).await } else { control.join_wpa2(SSID, PASS).await };
        match r {
            Ok(()) => break,
            Err(e) => {
                info!("join failed with status={}", e.status);
                Timer::after(Duration::from_secs(1)).await;
            }
        }
    }
    info!("joined {}", SSID);
    info!("waiting for DHCP...");
    while !stack.is_config_up() {
        Timer::after_millis(100).await;
    }
    let cfg = stack.config_v4().unwrap();
    info!("DHCP up: {}", cfg.address.address().0);

    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0; 128];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_buf = [0; 128];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    unwrap!(socket.bind(5000));
    let gateway = IpEndpoint::new(Ipv4Address::new(10, 0, 2, 2).into(), 7);
    unwrap!(socket.send_to(b"hello from pico", gateway).await);
    let mut buf = [0; 64];
    let (n, from) = unwrap!(socket.recv_from(&mut buf).await);
    info!("echo from {}: {}", from, core::str::from_utf8(&buf[..n]).unwrap_or("?"));
    control.gpio_set(0, true).await;
    loop {
        Timer::after_secs(1).await;
    }
}
