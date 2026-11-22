//! One widget per device kind, drawn in its item's body.
use egui::{
    Align2, Color32, FontId, Key, Pos2, Rect, Sense, Stroke, StrokeKind, TextureOptions, Ui, Vec2,
    pos2, vec2,
};
#[cfg(test)]
use pcb::BoardEvent;
use pcb_devices::{
    button::Button,
    cyw43439::{Cyw43439, Link},
    dac8165::Dac8165,
    led::Led,
    mcp23008::{Mcp23008, Wire},
    pot::Pot,
    sdcard::SdCard,
    st7789::St7789,
    wav::Wav,
    ws2812::Ws2812,
};
use tartelette_runtime::Session;

use super::{Ctx, Expander, Keycaps, PanelState, PinMode, Request, WifiNotes};
#[cfg(test)]
use super::{
    layout::{Layout, Widget},
    test_board,
};

mod button;
mod dac;
mod encoder;
mod expander;
mod led;
mod pot;
mod screen;
mod sdcard;
mod wav;
mod wifi;

pub(super) use button::button;
pub(super) use dac::dac;
pub(super) use encoder::encoder;
pub(super) use expander::expander;
use led::{circle_grid, lamp};
pub(super) use led::{led, leds};
pub(super) use pot::pot;
pub(super) use screen::screen;
pub(super) use sdcard::sdcard;
pub(super) use wav::wav;
pub(super) use wifi::wifi;
