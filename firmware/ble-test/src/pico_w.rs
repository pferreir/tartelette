//! Pico W bring-up shared by the binaries: power the CYW43439, load its
//! Wi-Fi and Bluetooth firmware, and hand back an HCI controller for
//! trouble. The blobs come from `CYW43_FIRMWARE_DIR` (see build.rs).
use cyw43::{aligned_bytes, Cyw43439};
use cyw43_pio::PioSpi;
use defmt::unwrap;
use embassy_executor::Spawner;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::{bind_interrupts, dma};
use static_cell::StaticCell;
use trouble_host::prelude::ExternalController;

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
});

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>, Cyw43439>) -> ! {
    runner.run().await
}

pub async fn controller(spawner: Spawner) -> ExternalController<cyw43::bluetooth::BtDriver<'static>, 10> {
    let p = embassy_rp::init(Default::default());
    let fw = aligned_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/43439A0.bin"));
    let clm = aligned_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/43439A0_clm.bin"));
    let btfw = aligned_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/43439A0_btfw.bin"));
    let nvram = aligned_bytes!(concat!(env!("CYW43_FIRMWARE_DIR"), "/nvram_rp2040.bin"));

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        cyw43_pio::DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
        dma::Channel::new(p.DMA_CH1, Irqs),
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (_net_device, bt_device, mut control, runner) = cyw43::new_with_bluetooth(state, pwr, spi, fw, btfw, nvram).await;
    spawner.spawn(unwrap!(cyw43_task(runner)));
    control.init(clm).await;
    ExternalController::new(bt_device)
}
