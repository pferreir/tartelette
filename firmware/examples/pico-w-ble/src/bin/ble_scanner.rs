//! trouble's `ble_scanner` example app on the Pico W.
#![no_std]
#![no_main]

#[path = "../pico_w.rs"]
mod pico_w;

use embassy_executor::Spawner;
use {defmt_rtt as _, embassy_time as _, panic_probe as _};

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let controller = pico_w::controller(spawner).await;
    trouble_example_apps::ble_scanner::run(controller).await;
}
