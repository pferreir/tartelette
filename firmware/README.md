# Firmware

The firmware here is outside the emulator's workspace and targets
`thumbv6m-none-eabi` (each `.cargo/config.toml` says so). Every example and
`showcase/` is a standalone crate, with its own `[workspace]` and lock file;
`tests/` is one workspace for all the test firmware. They build into the
repository's `target/firmware/` (`target/firmware/NAME`, `tests` for the test
firmware), where the emulator's tests also build the examples they run.

```
examples/   real firmware for real boards; runs in the emulator too
showcase/   the demos on the web page: plain RP2040 boards, no blobs needed
tests/      bare-metal firmware for the emulator's own tests, emulator only
```

The showcase demos (a drum machine, a game, an LED matrix and a shell) have
their own [README](showcase/README.md).

## Examples

Embassy firmware for the Raspberry Pi Pico W, unmodified for the emulator:
the same ELF runs in tartelette and on the board. The Embassy examples and
`showcase/` all use one Embassy revision from git, set by the `[patch]` at
the end of each `Cargo.toml`: Embassy's released crates lack the cyw43
Bluetooth fixes the Bluetooth examples need. Moving to another revision means
changing it in all of them.

| Example        | Board            | What it does |
|----------------|------------------|--------------|
| `pico-w-wifi`  | `pico-w`         | `wifi_blinky` blinks the LED (on the Wi-Fi chip), `wifi_scan` logs the networks around, `wifi_join` joins one, gets a DHCP lease and echoes a UDP datagram off 10.0.2.2:7 (the emulator's gateway) |
| `pico-w-ble`   | `pico-w`         | trouble's example apps: `ble_scanner`, `ble_bas_peripheral` (a battery service) and `ble_bas_central` (reads one) |
| `netscope`     | `pico-w` + `pico-display-2` | NETSCOPE, an internet sonar: probes a few hosts (DNS, NTP, HTTP, TCP) and plots their round trips on a radar scope. A retest, B/X page, Y ms or km of light |
| `pico-usb`     | `pico-w`         | `usb_composite`: a USB serial port that echoes in upper case (a `!` types an `a`), a boot keyboard and a MIDI interface that echoes notes an octave up, on embassy-usb |
| `pico-usb-hal` | `pico-w`         | the same `usb_composite` on rp2040-hal and usb-device (usbd-serial, usbd-hid, usbd-midi) |
| `catprint`     | `pico-w` + `pico-display-2` | THE DAILY BIT: fetches the time, place, weather and ISS crew over Wi-Fi and prints a receipt on an MXW01 "cat printer" over Bluetooth LE. A new edition, B reprint, X/Y scroll |

The boards are in `../boards/`. `pico-display-2` is a shield
(`../boards/shields/`): a Pimoroni Pico Display Pack 2.0 (320x240 ST7789,
buttons A/B/X/Y) on top of the Pico W.

### Building

The CYW43439's firmware blobs are Infineon's and not in this repository: point
`CYW43_FIRMWARE_DIR` at a directory with `43439A0.bin`, `43439A0_clm.bin` and
`nvram_rp2040.bin`, plus `43439A0_btfw.bin` for the Bluetooth examples
(`pico-w-ble`, `catprint`). Embassy has them in its `cyw43-firmware/`.

The examples that join a network take its name and password at build time,
from `WIFI_SSID` and `WIFI_PASS`. They default to `TarteletteNet` /
`hunter22`, one of the mock access points in the emulator's Pico W boards.
`catprint` also reads `INTENSITY` (print head energy, 0-100, default 100) and
`ROW_GAP_MS` (a pause after each printed row, default none).

```sh
cd firmware/examples/netscope
export CYW43_FIRMWARE_DIR=~/embassy/cyw43-firmware
cargo build --release
```

### Running: emulator or real board

`cargo run` goes through `examples/run` (each example's cargo runner), and
`TARTELETTE_RUNNER` decides where the firmware runs:

| `TARTELETTE_RUNNER`  | Runs the firmware |
|----------------------|-------------------|
| unset or `emulator`  | in tartelette, on the example's board, with a front panel window |
| `probe-rs`           | on a real Pico W over a debug probe (SWD): `probe-rs run --chip RP2040`, which flashes it and shows the defmt log |
| `picotool`           | on a real Pico W held in BOOTSEL mode over USB: `picotool load -v -x -t elf` (no log: the examples log over RTT) |

```sh
cd firmware/examples/pico-w-wifi
cargo run --release --bin wifi_scan                          # emulator, window
cargo run --release --bin wifi_scan -- --headless --log      # emulator, log to the terminal
WIFI_SSID=home WIFI_PASS=secret TARTELETTE_RUNNER=probe-rs \
    cargo run --release --bin wifi_join                      # a Pico W on a debug probe

cd ../catprint
cargo run --release                                          # prints on the mock cat printer
cargo run --release -- --headless --speed 1 --max-cycles 7500000000 \
    --dump-paper wifi=receipt.png                            # what it printed in 60 s, as a picture
cargo run --release -- --bt hci0                             # a real printer, through the host's adapter
```

With the emulator, what follows `cargo run --` goes to tartelette (see
`tartelette --help`) and overrides the example's own options: `catprint`
and `pico-w-ble` use `--bt mock` (the emulator's mock Bluetooth devices, cat
printer included). With `probe-rs` or `picotool`, it goes to that tool.

The emulator is the repository's own, built (release) the first time; set
`TARTELETTE` to use another binary, e.g. `TARTELETTE=tartelette` for an
installed one.

## Tests

Bare-metal firmware (no HAL, a zeroed boot2, registers poked by hand), each
exercising one part of the emulated chip and printing what it found on
UART0. They are emulator only: some stop on `bkpt`, and most need the test
to act as the board around them (press a button, turn a knob, pull a pin, put
data on an SD card) at the right moment.

The tests in `crates/app/tartelette` and `crates/app/tartelette-runtime` build them
as needed (`cargo test -p tartelette -p tartelette-runtime`), and
`cpu-bench` is the benchmark the scripts run (`scripts/web-check.sh`,
`scripts/web-bench.sh`, `scripts/pgo-build.sh`).

Each test firmware is a crate of the `tests/` workspace with only its
`src/main.rs`: its vector table, its `reset` and the test. What they all
share is in `tests/testrt` (the boot2, the panic handler, the handler for
unexpected exceptions, register and UART0 helpers), `tests/link.ld` and
`tests/build.rs`. To build one by hand: `cd firmware/tests && cargo build
--release -p uart-hello`.

| Firmware          | Checks | Run by |
|-------------------|--------|--------|
| `uart-hello`      | UART0, `.data`/`.bss` init, the bootrom's function table | `tests/uart_hello.rs`, `tests/batch_equivalence.rs` |
| `uart-echo`       | UART0 receive, RX and receive-timeout interrupts waking WFI; the headless runner holding stdin back until the receiver is on, or bridging the UART to a pseudo-terminal | `tests/uart_hello.rs` |
| `button-led`      | a GPIO input driving a GPIO output through the board | `tests/uart_hello.rs`, `tartelette-runtime/src/session.rs` |
| `io-test`         | SIO outputs, pad pulls, input loopback, edge interrupts | `tests/uart_hello.rs`, `tests/batch_equivalence.rs` |
| `irq-timer`       | TIMER alarms, SysTick, PendSV, WFI | `tests/uart_hello.rs`, `tests/batch_equivalence.rs` |
| `batch-timer`     | interrupts landing on the exact cycle while the core spins | `tests/batch_equivalence.rs` |
| `i2c-test`        | I2C1 against an MCP23008 and its interrupt line | `tests/uart_hello.rs` |
| `spi-dma-test`    | SPI0 and DMA against an ST7789 | `tests/uart_hello.rs`, `tests/batch_equivalence.rs` |
| `sd-test`         | an SD card on SPI1 with DMA and CRCs | `tests/uart_hello.rs` |
| `pio-test`        | PIO driving a DAC8165 and WS2812s and reading an encoder | `tests/uart_hello.rs`, `tests/batch_equivalence.rs` |
| `multicore-test`  | core 1 launch, spinlocks, dividers, the inter-core FIFOs | `tests/uart_hello.rs`, `tests/batch_equivalence.rs` |
| `cpu-bench`       | nothing: CPU-bound work (CRC, sort, multiplies, copies, a bytecode loop, 64-bit division) ending in a checksum, to time the core | `scripts/web-check.sh`, `scripts/web-bench.sh`, `scripts/pgo-build.sh` |

Each one's `src/main.rs` starts with what it does and the exact output
expected. `examples/pico-w-wifi` and `examples/pico-w-ble` also serve as tests
(`tests/wifi_firmware.rs`, `tests/ble_firmware.rs`), with the blobs from
`CYW43_FIRMWARE_DIR` or, without it, from the newest Embassy checkout in
Cargo's git cache (`~/.cargo/git/checkouts/embassy-*/*/cyw43-firmware`).
