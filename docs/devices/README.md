# Devices

The parts a board file can use. Each page has the part's board-file keys, an
example, what its front-panel widget does, the events it logs, the actions
`--input` can send it, and what the model leaves out.

| Kind | Part | Connects over |
|------|------|---------------|
| [`led`](led.md) | an LED | a GPIO |
| [`button`](button.md) | a push button | a GPIO |
| [`encoder`](encoder.md) | a rotary encoder | two GPIOs |
| [`pot`](pot.md) | a potentiometer | a GPIO, read by the ADC |
| [`wav`](wav.md) | a WAV file player | a GPIO, read by the ADC |
| [`mcp23008`](mcp23008.md) | Microchip MCP23008 8-bit port expander | I2C |
| [`st7789`](st7789.md) | Sitronix ST7789 TFT controller | SPI |
| [`ws2812`](ws2812.md) | WS2812 / SK6812 LED chain | one GPIO (usually PIO) |
| [`dac8165`](dac8165.md) | TI DAC8165 quad 14-bit DAC | bit-banged serial (usually PIO) |
| [`sdcard`](sdcard.md) | SDHC card in SPI mode | SPI |
| [`cyw43439`](cyw43439.md) | Infineon CYW43439 Wi-Fi + Bluetooth (Pico W) | gSPI over PIO |

`tartelette parts KIND` prints the same key list as each page, straight from
the code.

## Common ideas

**Pins** are GPIO numbers, 0 to 29. A pin belongs to one part, except where a
page says it can be shared.

**Buses**: parts with a `bus` key sit on an SPI or I2C controller declared
under `[buses]`. A part with no `bus` is on the board but connected to
nothing, which is occasionally useful while you are wiring up a board file.

**Events** are what a part reports as the firmware drives it: an LED turning
on, a frame latched into an LED chain, a DAC output changing. Headless runs
print them on stderr with the emulated time:

```text
[   14600.0 us] dac dac=147455
```

**Actions** are what the outside world does to a part: press a button, turn a
knob. The panel sends them when you click; headless, `--input
NAME:ACTION=VALUE@US` sends one at a given time, and `--press NAME@US+HOLD` is
a shortcut for buttons.

**`mhz`** appears on parts that measure time on their pins. It is the core
clock, used to turn microseconds into cycles. Leave it out: the part then
follows the chip's clk_sys, 125 MHz unless the firmware overclocks (see
[rp2040.md](../rp2040.md#time)). Set, it fixes the rate whatever the chip
does.

## Writing a new one

See [development.md](../development.md#adding-a-device).
