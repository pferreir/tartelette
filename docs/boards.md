# Board files

A board file describes what is soldered around the RP2040: which buses are in
use and on which pins, and which parts hang off them. tartelette builds the
emulated board from it, and lays out the front panel from its optional
`[panel]` section.

The built-in boards are in [`boards/`](../boards) and are a good place to
start. [`btl-01.toml`](../boards/btl-01.toml) uses most device kinds.

## An example

```toml
#:schema ./board.schema.json
name = "my-board"

[buses]                          # bus controllers in use, and their pins
spi0 = { sck = 18, mosi = 19 }
i2c1 = { sda = 2, scl = 3 }

[parts.screen]                   # one part: its kind, and that kind's keys
kind = "st7789"
bus = "spi0"
cs = 9
dc = 13

[parts.expander]
kind = "mcp23008"
bus = "i2c1"
addr = 0x20
int = 16

[parts.status]
kind = "led"
gpio = 25

[panel]                          # optional: the front panel grid
screen = { at = [0, 0], span = [4, 3], rotate = 90 }
status = { at = [4, 0], color = "green" }
expander = { at = [0, 3], span = [4, 1], pins = [4, 5, 6, 7], keys = ["1", "2", "3", "4"] }
```

## `[buses]`

The SPI and I2C controllers the board uses, and the pins each signal is on:

```toml
[buses]
spi1 = { sck = 10, mosi = 11, miso = 12 }
i2c0 = { sda = 4, scl = 5 }
```

The pins must be ones the RP2040 can route that signal to. Chip selects are
not part of the bus: on almost every board they are plain GPIOs, and each part
names its own `cs`.

If the firmware routes a bus to pins other than the declared ones, it still
works, and tartelette prints a warning saying where the firmware put them.

Parts that are bit-banged or driven from PIO (`ws2812`, `dac8165`, `encoder`,
`cyw43439`) don't sit on a bus; they name their pins directly.

## `[parts.NAME]`

Each part has a name (used on the command line and in event logs), a `kind`,
and the keys that kind takes. `tartelette parts` lists the kinds, and
`tartelette parts KIND` its keys:

```
$ tartelette parts st7789
st7789: SPI TFT display controller with frame RAM
  bus     spi0 | spi1  optional  the bus it sits on (declared under [buses])
  cs      GPIO         optional  chip select, active low (none: always selected)
  dc      GPIO         required  data/command select
  rst     GPIO         optional  reset, active low
  bl      GPIO         optional  backlight enable
  width   1..480       = 240     panel width in pixels, in its native orientation
  height  1..480       = 320     panel height in pixels
```

The same information, with examples, is on each device's page in
[devices/](devices/).

Each GPIO belongs to one part. Two parts on the same pin is an error, unless
both can share it: an LED watching a line, or open-drain interrupt outputs.

## `[panel]`

The front panel is a grid. Each line places one part's widget:

```toml
[panel]
screen = { at = [0, 0], span = [4, 3], rotate = 90 }
knob = { at = [4, 0], span = [2, 2], keys = ["Left", "Right"] }
enc_sw = { at = [4, 2], label = "Push", key = "Space" }
```

- `at = [COL, ROW]` is the top-left cell;
- `span = [W, H]` is the size in cells (default `[1, 1]`);
- `label` replaces the part's name as the widget's title.

Parts that the panel leaves out are not shown. Without a `[panel]` section at
all, every part is placed automatically.

Some widgets take options of their own:

| Kind | Widget | Options |
|------|--------|---------|
| `st7789` | the screen | `rotate = 0 \| 90 \| 180 \| 270` |
| `led` | a lamp | `color = "red" \| "green" \| "blue" \| "yellow" \| "orange" \| "white"` or a hex `"rrggbb"` |
| `ws2812` | a row of RGB lamps | |
| `button` | a push button, held while the mouse is down | `key = "Space"` |
| `encoder` | a knob: drag, scroll or use keys | `keys = ["Left", "Right"]` |
| `mcp23008` | a cell per pin | see below |
| `dac8165` | four voltmeters, showing the voltage at the jacks | |
| `sdcard` | card status, Load / Save / Browse | |
| `cyw43439` | Wi-Fi LED, network, address and errors | |

Key names are egui's (`A`, `1`, `Space`, `Enter`, `ArrowLeft` or `Left`, ...).

An `mcp23008` can appear several times, so its inputs and outputs can sit
apart with different settings. Pass a list:

```toml
expander = [
  { at = [0, 3], span = [2, 1], label = "Soft keys", pins = [4, 5, 6, 7], keys = ["1", "2", "3", "4"] },
  { at = [2, 3], span = [2, 1], label = "Gates", pins = [0, 1, 2, 3], labels = ["A", "B", "C", "D"] },
]
```

Each cell follows what the part's `pins` wire to its pin (see
[the device page](devices/mcp23008.md#pins)): an LED is shown lit or dark, a
switch is pressed while the cell is held (a toggle flips on each click), and
a bare pin is pulled low while held.

- `pins`: which pins, in that order (default all eight);
- `mode = "hold" | "toggle" | "view"`: ignore the wiring and treat every pin
  as a bare one that a cell drives while held, flips on each click, or only
  shows;
- `active = "low" | "high"`: the level a cell drives a bare pin to (default
  low, a switch to ground); released, the pin floats;
- `keys`: a keyboard key per pin;
- `labels`: text per cell instead of the pin's name or number.

## `[[flash]]`

What the flash chip holds before the firmware goes on: factory settings that
the firmware expects to find, such as calibration. An emulated chip starts
erased (all `0xFF`), which firmware rarely copes with. Each entry puts values
at an address in the XIP window (`0x10000000..0x11000000`), little endian, in
one of `u8`, `u16` or `u32`:

```toml
[[flash]]
addr = 0x101ff000
u16 = [63985, 57689, 51389]
```

The firmware goes on top, so it wins where they overlap. Headless, a
`--flash` image replaces these entries (it already has what the firmware
made of them).

## Shields

A shield is an add-on board, in the same format. With `--shield`, its buses,
parts and panel items go on top of the board's. Board panel items that a
shield item covers move out of its way.

```sh
tartelette fw.elf --board pico-w --shield pico-display-2
```

[`boards/shields/pico-display-2.toml`](../boards/shields/pico-display-2.toml)
is a complete example: the Pimoroni Pico Display Pack 2.0, a 320x240 ST7789
and four buttons.

## Editor support

`boards/board.schema.json` is a JSON schema for board files, generated by
`tartelette schema`. Editors that follow the `#:schema` comment on the first
line (VS Code with Even Better TOML, anything using taplo) then give you
completion, hover docs and errors as you type. A test fails if the checked-in
schema is out of date.

tartelette's own error messages give the line and column, and suggest the
right key when you misspell one.

## Reloading

With a window open, tartelette watches the board and shield files and
reloads when you save them, so you can lay out a panel with the emulator
running.
