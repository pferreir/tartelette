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
  usbhost/       USB host for the emulated device: enumeration, CDC-ACM,   deps: none
                 HID, USB-MIDI, traffic log
  rp2040-core/   peripherals, ELF loading, SystemBus, System               deps: armv6m, pcb, usbhost
  netstack/      user-mode network (slirp-style NAT) for the Wi-Fi chip    deps: pcb-devices, smoltcp (std)
  usbbridge/     the USB device's serial port as a pty, HID as UHID        deps: usbhost, libc (Linux)
  tartelette/    front panel (native + web, eframe), headless runner and   deps: rp2040-core, pcb, pcb-devices, netstack (native)
                 `mkdisk` (native, clap); end-to-end tests
  cpu-difftest/  differential tests of the CPU against Unicorn             dev-deps: armv6m
scripts/         pgo-build.sh (an opt-in PGO release build), web-check.sh and
                 web-bench.sh (the browser build in a headless browser)
bench/web/       the emulator core as a bare wasm module for web-bench.sh, and
                 cdp.mjs, the DevTools driver both browser scripts use
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
Without `--board` the board is `pico`, a plain Raspberry Pi Pico (its LED on
GPIO25 and nothing else).

### Release builds with PGO

`scripts/pgo-build.sh` builds a profile-guided (PGO) binary at
`target/pgo/tartelette`, 1.1–1.4× faster than a plain release build. It's
opt-in, meant for release builds in CI: it builds an instrumented
tartelette, runs it on a training corpus of the repository's firmware, and
rebuilds with the profile (a few minutes). It needs `llvm-profdata` from
rustc's LLVM (`rustup component add llvm-tools`, or `LLVM_PROFDATA=...`).
`CYW43_FIRMWARE_DIR` adds the Pico W Wi-Fi example to the corpus and
`PGO_FIRMWARE="a.uf2 b.elf"` any other firmware.

```sh
rustup component add llvm-tools
scripts/pgo-build.sh
```

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

The web build uses WebAssembly tail calls (`.cargo/config.toml`), which the
CPU interpreter's threaded dispatch needs to be fast in the browser: Chrome
and Edge 112+, Firefox 121+, Safari 18.2+. Without that flag the build still
works, about 20% slower.

From the checkout, `cargo run --release -p tartelette --` does the same as
`tartelette`. See `tartelette --help` for every option. Headless, UART0 goes to stdout and
UART1, board events and the firmware log to stderr.

A fault (an unaligned access, an undefined instruction, ...) is a warning that
names the instruction and why, and the firmware's HardFault handler runs.
`--stop-on-fault` stops the run there instead, before the handler can hide it
(by resetting, or spinning). Headless, a run that stops on a fault, or on a
`bkpt` in a handler (pico-sdk's fault handlers are `bkpt`s), ends with the
core's registers, the exception frame it stacked and the fault's cause.

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

## USB

The USB controller (`rp2040-core/src/periph/usb.rs`) is modelled in device
mode after the datasheet: registers, DPRAM buffer control words (AVAILABLE,
FULL, PID, STALL, double buffering), BUFF_STATUS and the SIE_STATUS
interrupts. On the other end of the cable is `usbhost`, a small deterministic
host that enumerates the device the way a PC does, one transaction at a time
at full-speed timing, and drives the functions it knows: CDC-ACM serial
ports, HID interfaces and USB-MIDI. It works with TinyUSB (MicroPython,
pico-sdk), embassy-usb and usb-device (rp2040-hal), unmodified.

Without anything else, the serial port is the console's **USB serial** tab
(headless: stdin and stdout), and the **USB** tab (headless: `--usb-log`)
shows the traffic: enumeration, every control transfer decoded, every packet.
That works everywhere, browser included.

`--usb-bridge` hands functions to this computer instead:

| Bridge   | Becomes                                   | Where |
|----------|-------------------------------------------|-------|
| `serial` | a pseudo-terminal (`--usb-tty-link PATH` adds a symlink); DTR follows whether a program has it open | Linux |
| `hid`    | a kernel input device through `/dev/uhid` (an emulated keyboard types for real) | Linux; needs access to `/dev/uhid` |
| `midi`   | virtual MIDI ports `tartelette: USB MIDI out` / `USB MIDI in` | Linux (ALSA), macOS (CoreMIDI) |

Elsewhere (Windows, the browser via WebMIDI) the USB tab connects the MIDI
interface to existing ports instead.

```sh
tartelette micropython.uf2 --headless                                     # REPL on stdin/stdout
tartelette micropython.uf2 --usb-bridge serial --usb-tty-link /tmp/ttyACM  # then: mpremote connect /tmp/ttyACM
tartelette synth.elf --usb-bridge midi --usb-log --headless --speed 1
```

`firmware/examples/pico-usb` (embassy-usb) and `pico-usb-hal` (rp2040-hal)
are the same composite device, a serial port, a keyboard and a MIDI
interface, used by the end-to-end tests.

## Testing

```sh
cargo test --workspace   # cpu-difftest is slow (~1 min)
cargo build -p armv6m -p pcb -p pcb-devices -p rp2040-core -p tartelette --target wasm32-unknown-unknown
```

The browser build is checked in a headless Chromium-family browser
(`thorium-browser`, `chromium` or `google-chrome`, or `CHROME=...`), driven
over the DevTools protocol by `bench/web/cdp.mjs` (Node 22+, no packages):

```sh
scripts/web-check.sh [FIRMWARE] [SCREENSHOT]   # trunk build, drop a firmware on it, fail on page exceptions, screenshot
scripts/web-bench.sh [ROUNDS]                  # time the core in the browser with and without wasm tail calls
```

`web-check.sh` runs `firmware/tests/cpu-bench` by default; `WAIT_MS` and
`CLICK="X,Y"` (e.g. `283,632`, the console's USB serial tab) help with
others. `web-bench.sh` takes `MICROPYTHON_UF2=...` to add MicroPython.
