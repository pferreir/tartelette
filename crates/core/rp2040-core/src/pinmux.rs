//! Which GPIOs can carry which bus signal (the RP2040's fixed pin mux for
//! SPI and I2C), so a board's `[buses]` wiring can be checked when it is
//! loaded ([`check_bus_pin`], a [`pcb::PinCheck`]) and against what the
//! firmware selects when it runs ([`wiring_warnings`]).
use alloc::{format, string::String, vec::Vec};

use pcb::{BusDecl, NUM_PINS};

use crate::periph::gpio::Gpio;

/// GPIO FUNCSEL of the SPI controllers.
pub const FUNCSEL_SPI: u32 = 1;
/// GPIO FUNCSEL of the I2C controllers.
pub const FUNCSEL_I2C: u32 = 3;

/// The bus signal GPIO `pin` carries with function `func`, as a board names
/// it: `("spi0", "sck")`. SPI's own chip select (`"csn"`) is not a board
/// signal (boards use a plain GPIO).
pub fn signal(func: u32, pin: usize) -> Option<(&'static str, &'static str)> {
    match func {
        FUNCSEL_SPI => Some((
            ["spi0", "spi1"][pin / 8 % 2],
            ["miso", "csn", "sck", "mosi"][pin % 4],
        )),
        FUNCSEL_I2C => Some((["i2c0", "i2c1"][pin / 2 % 2], ["sda", "scl"][pin % 2])),
        _ => None,
    }
}

/// The function select that routes controller `bus` to its pins.
fn funcsel_of(bus: &str) -> u32 {
    if bus.starts_with("spi") {
        FUNCSEL_SPI
    } else {
        FUNCSEL_I2C
    }
}

/// `SPI0 SCK`, for messages.
fn name(bus: &str, signal: &str) -> String {
    format!(
        "{} {}",
        bus.to_ascii_uppercase(),
        signal.to_ascii_uppercase()
    )
}

/// What a FUNCSEL value routes to a pin, for messages.
pub fn function_name(func: u32) -> &'static str {
    match func {
        1 => "SPI",
        2 => "UART",
        3 => "I2C",
        4 => "PWM",
        5 => "SIO",
        6 => "PIO0",
        7 => "PIO1",
        8 => "a clock",
        9 => "USB",
        _ => "no function",
    }
}

/// `"2, 6, 18 or 22"`.
fn list(pins: &[usize]) -> String {
    let words: Vec<String> = pins.iter().map(|p| format!("{p}")).collect();
    match words.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => words.concat(),
    }
}

/// Can `signal` of controller `bus` be on GPIO `pin`? (A [`pcb::PinCheck`].)
pub fn check_bus_pin(bus: &str, sig: &str, pin: usize) -> Result<(), String> {
    let func = funcsel_of(bus);
    if signal(func, pin) == Some((bus, sig)) {
        return Ok(());
    }
    let ok: Vec<usize> = (0..NUM_PINS)
        .filter(|&p| signal(func, p) == Some((bus, sig)))
        .collect();
    Err(format!(
        "{} can't be on GPIO {pin}: the RP2040 has it on GPIO {}",
        name(bus, sig),
        list(&ok)
    ))
}

/// Problems with the board's wiring of `bus` now that the firmware uses it:
/// a declared pin the firmware hasn't routed to the controller, and where
/// the firmware routed that signal instead.
pub fn wiring_warnings(decl: &BusDecl, gpio: &Gpio) -> Vec<String> {
    let func = funcsel_of(&decl.name);
    let mut out = Vec::new();
    for (sig, pin) in &decl.pins {
        let actual = gpio.funcsel(*pin);
        if actual == func {
            continue;
        }
        let elsewhere: Vec<usize> = (0..NUM_PINS)
            .filter(|&p| gpio.funcsel(p) == func && signal(func, p) == Some((&decl.name, sig)))
            .collect();
        let instead = if elsewhere.is_empty() {
            String::new()
        } else {
            format!(
                "; the firmware put {} on GPIO {}",
                name(&decl.name, sig),
                list(&elsewhere)
            )
        };
        out.push(format!(
            "{} is in use, but GPIO {pin} (the board's {}) is set to {}, not {}{instead}",
            decl.name.to_ascii_uppercase(),
            name(&decl.name, sig),
            function_name(actual),
            function_name(func),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_follow_the_datasheet_table() {
        assert_eq!(signal(FUNCSEL_SPI, 18), Some(("spi0", "sck")));
        assert_eq!(signal(FUNCSEL_SPI, 19), Some(("spi0", "mosi")));
        assert_eq!(signal(FUNCSEL_SPI, 12), Some(("spi1", "miso")));
        assert_eq!(signal(FUNCSEL_SPI, 29), Some(("spi1", "csn")));
        assert_eq!(signal(FUNCSEL_I2C, 2), Some(("i2c1", "sda")));
        assert_eq!(signal(FUNCSEL_I2C, 21), Some(("i2c0", "scl")));
        assert_eq!(signal(5, 2), None);
    }

    #[test]
    fn bus_pins_are_checked() {
        assert_eq!(check_bus_pin("spi0", "sck", 18), Ok(()));
        assert_eq!(check_bus_pin("spi1", "miso", 12), Ok(()));
        assert_eq!(check_bus_pin("i2c1", "scl", 3), Ok(()));
        assert_eq!(
            check_bus_pin("spi0", "sck", 10).unwrap_err(),
            "SPI0 SCK can't be on GPIO 10: the RP2040 has it on GPIO 2, 6, 18 or 22"
        );
        assert_eq!(
            check_bus_pin("i2c0", "sda", 2).unwrap_err(),
            "I2C0 SDA can't be on GPIO 2: the RP2040 has it on GPIO 0, 4, 8, 12, 16, 20, 24 or 28"
        );
    }
}
