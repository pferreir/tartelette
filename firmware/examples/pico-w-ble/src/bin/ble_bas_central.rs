//! trouble's `ble_bas_central` example app on the Pico W.
#![no_std]
#![no_main]

#[path = "../pico_w.rs"]
mod pico_w;

use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_time as _;
use panic_probe as _;

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let controller = pico_w::controller(spawner).await;
    trouble_example_apps::ble_bas_central::run(controller).await;
}
