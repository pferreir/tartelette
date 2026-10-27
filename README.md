# tartelette

An RP2040 emulator that runs a firmware ELF and emulates the PCB around the
chip (display, port expander, SD card, ...). One app (egui) with a front panel
that runs natively and in the browser, and natively also headless: the library crates are
`no_std` + `alloc`, deterministic and free of I/O.
The target board is the BTL-01 (`boards/btl-01.toml`).

## Crates

```
crates/
  armv6m/        Cpu, Bus trait, Ram, exceptions, Ppb (NVIC/SysTick/SCB)   deps: none
  pcb/           device framework + Board (no chip knowledge)              deps: toml
  pcb-devices/   stock device models + `registry()`                        deps: pcb
  rp2040-core/   peripherals, ELF loading, SystemBus, System               deps: armv6m, pcb
  netstack/      user-mode network (slirp-style NAT) for the Wi-Fi chip    deps: pcb-devices, smoltcp (std)
  tartelette/    front panel (native + web, eframe), headless runner and   deps: rp2040-core, pcb, pcb-devices, netstack (native)
                 `mkdisk` (native, clap); end-to-end tests
  cpu-difftest/  differential tests of the CPU against Unicorn             dev-deps: armv6m
firmware/
  examples/      Pico W firmware that runs on real boards and in the emulator
  tests/         bare-metal firmware for the emulator's tests (emulator only)
boards/          board files (TOML; also built into the CLI), shields/ and the schema
```

`pcb` talks to the chip only through the `Pins` trait, which `rp2040-core`
implements for its GPIO block. Peripherals stay inside `rp2040-core`: they are
coupled to reset gating, IRQ lines and (later) DMA/PIO.

## Installing

```sh
cargo install --locked --path crates/tartelette
# or, without a checkout:
cargo install --locked --git ssh://git@git.turbofish.cc:22122/pedro/tartelette-rp2040.git tartelette
```

The boards in `boards/` and shields in `boards/shields/` are built in, so
`--board pico-w --shield pico-display-2` works anywhere; paths to files work too.

## Running

```sh
tartelette firmware.elf --board btl-01 [--sd sd=card.img]
tartelette firmware.elf --board btl-01 --headless [--max-cycles N] [--press NAME@US[+HOLD]] [--log] ...
tartelette mkdisk card.img --dir data   # a blank FAT16 SD card image
cd crates/tartelette && trunk serve --release   # web: open the page, then open/drop the files
```

The firmware can be an ELF or a UF2 (as dragged onto a Pico in BOOTSEL mode).
An ELF is better when you have one: a UF2 has no symbols, so there is no defmt
log.

From the checkout, `cargo run --release -p tartelette --` does the same as
`tartelette`. See `tartelette --help` for every option. Headless, UART0 goes to stdout and
UART1, board events and the firmware log to stderr.

## Board files

A board is a TOML file (see `pcb::board` for the format):

```toml
#:schema ./board.schema.json
name = "my-board"

[buses]                          # bus controllers in use, and their pins
spi0 = { sck = 18, mosi = 19 }

[parts.screen]                   # one device: its kind and that kind's keys
kind = "st7789"
bus = "spi0"
cs = 9
dc = 13

[parts.status]
kind = "led"
gpio = 25

[panel]                          # optional: the front panel grid
screen = { at = [0, 0], span = [4, 3], rotate = 90 }
status = { at = [4, 0], color = "green" }
```

`tartelette parts` lists the device kinds and `tartelette parts KIND` their
keys, types and defaults; mistakes are reported with their line and column,
and a near-miss key gets a "did you mean". Every GPIO a board names belongs
to one user (two parts on one pin is an error, unless both may share it, like
an LED watching a line); bus pins must be ones the RP2040 can route that
signal to; and when the firmware first uses a bus whose declared pins it
didn't route to it, a warning says where it put them instead.

`boards/board.schema.json` (from `tartelette schema`) gives editors that read
the `#:schema` line (VS Code's Even Better TOML, taplo) completion, hover docs
and checking as you type. A test fails when it is out of date.

A shield (`--shield`, repeatable) is an add-on board in the same format: its
parts, buses and panel items go on top of the board's, and board panel items
it covers move out of its way.

Panel widget options such as `rotate = 90` or `key = "Space"` are listed in
`crates/tartelette/src/panel.rs`. Without a `[panel]` every part is laid out
automatically. Natively, the firmware, board and shields reload when their
files change.

## Pico W Wi-Fi

`boards/pico-w.toml` has the CYW43439 (`pcb-devices/src/cyw43439`), modelled
at the pins, so unmodified Embassy `cyw43` + `cyw43-pio` firmware runs: it
downloads the radio firmware over the emulated gSPI link, scans, joins one of
the board file's mock access points (`aps = [{ ssid = "...", security = "wpa2", passphrase = "..." }]`),
gets a DHCP lease and talks to the network. The Wi-Fi LED toggles as a `led`
board event.

What is behind the radio is a `NetBackend`: natively `netstack`'s user-mode NAT
(guest `10.0.2.15`, gateway `10.0.2.2` = the host's `127.0.0.1`, DNS `10.0.2.3`),
the deterministic `LoopbackBackend` in tests; the browser has none, so scans find
nothing and the panel says "Wi-Fi unavailable in the browser".

```sh
tartelette fw.elf --board boards/pico-w.toml [--net user|loopback|none] [--forward 8080:80] [--headless --log --wifi-trace wifi]
```

`firmware/examples` has Pico W firmware (Wi-Fi, Bluetooth, and two apps for
the Pico Display Pack) whose `cargo run` runs it in the emulator or flashes a
real board; see `firmware/README.md`. The radio firmware blobs are Infineon's
and not in the repository (`CYW43_FIRMWARE_DIR`).

## Testing

```sh
cargo test --workspace   # cpu-difftest is slow (~1 min)
cargo build -p armv6m -p pcb -p pcb-devices -p rp2040-core -p tartelette --target wasm32-unknown-unknown
```
