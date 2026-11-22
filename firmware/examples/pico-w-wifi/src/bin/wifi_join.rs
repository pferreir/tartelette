//! Joins a network (SSID and password from `WIFI_SSID` / `WIFI_PASS` at
//! build time, or open if the password is empty), gets a DHCP lease, and
//! echoes a UDP datagram off the emulator's gateway (10.0.2.2:7).
#![no_std]
#![no_main]

use cyw43::aligned_bytes;
use cyw43_pio::{DEFAULT_CLOCK_DIVIDER, PioSpi};
use defmt::*;
use embassy_executor::Spawner;
use cyw43::JoinOptions;
use embassy_net::udp::UdpSocket;
use embassy_net::wire::{Ipv4Addr, SocketAddr};
use embassy_net::{Stack, StackStorage, StaticPool};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::{bind_interrupts, dma};
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
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
async fn cyw43_task(runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>, cyw43::Cyw43439>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static>) -> ! {
    runner.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let fw = aligned_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/43439A0.bin"));
    let clm = aligned_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/43439A0_clm.bin"));
    let nvram = aligned_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/nvram_rp2040.bin"));

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
        dma::Channel::new(p.DMA_CH1, Irqs),
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw, nvram).await;
    spawner.spawn(unwrap!(cyw43_task(runner)));

    control.init(clm, cyw43::Config::default()).await;
    control.set_power_management(cyw43::PowerManagementMode::PowerSave).await;

    static STORAGE: StaticCell<StackStorage<'static>> = StaticCell::new();
    // cyw43 receives each frame, bus headers included, straight into these.
    static POOL: StaticPool<{ cyw43::RX_BUF_SIZE }, 8, { cyw43::RX_BUF_ALIGN }> = StaticPool::new();
    let (stack, net_runner) = Stack::new(STORAGE.init(StackStorage::new()), &POOL, 0x0123_4567_89ab_cdef);
    static DEVICE: StaticCell<cyw43::NetDriver<'static>> = StaticCell::new();
    let iface = unwrap!(stack.add_iface_borrowed(DEVICE.init(net_device)).ok());
    unwrap!(iface.set_dhcpv4(Some(Default::default())).ok());
    spawner.spawn(unwrap!(net_task(net_runner)));

    loop {
        let opts = if PASS.is_empty() { JoinOptions::new_open() } else { JoinOptions::new(PASS.as_bytes()) };
        match control.join(SSID, opts).await {
            Ok(()) => break,
            Err(e) => {
                info!("join failed: {}", e);
                Timer::after(Duration::from_secs(1)).await;
            }
        }
    }
    info!("joined {}", SSID);
    info!("waiting for DHCP...");
    iface.wait_config_up().await;
    let lease = unwrap!(iface.dhcpv4_lease());
    info!("DHCP up: {}", lease.address.address().octets());

    let mut socket = unwrap!(UdpSocket::new(stack).ok());
    unwrap!(socket.bind(5000, SocketAddr::UNSPECIFIED).ok());
    let gateway = SocketAddr::new(Ipv4Addr::new(10, 0, 2, 2).into(), 7);
    unwrap!(socket.send_to(b"hello from pico", gateway).await.ok());
    let mut buf = [0; 64];
    let (n, from) = unwrap!(socket.recv_from(&mut buf).await.ok());
    info!("echo from {}: {}", Display2Format(&from), core::str::from_utf8(&buf[..n]).unwrap_or("?"));
    control.gpio_set(0, true).await;
    loop {
        Timer::after_secs(1).await;
    }
}
