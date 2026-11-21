# Development

## Crates

```
crates/
  armv6m/        Cpu, Bus trait, Ram, exceptions, Ppb (NVIC/SysTick/SCB)   deps: none
  pcb/           device framework + Board (no chip knowledge)              deps: toml
  pcb-devices/   stock device models + `registry()`                        deps: pcb
  usbhost/       USB host for the emulated device: enumeration, CDC-ACM,   deps: none
                 HID, USB-MIDI, traffic log
  rp2040-core/   peripherals, ELF/UF2 loading, SystemBus, System           deps: armv6m, pcb, usbhost
  rtt/           defmt-over-RTT reader                                     deps: rp2040-core, defmt-decoder
  netstack/      user-mode network (slirp-style NAT) for the Wi-Fi chip    deps: pcb-devices, smoltcp (std)
  hcibridge/     a real Bluetooth adapter through an HCI user channel      deps: pcb-devices, libc (Linux)
  usbbridge/     the USB device's serial port as a pty, HID as UHID        deps: usbhost, libc (Linux)
  tartelette/    front panel (native + web, eframe), headless runner,      deps: all of the above
                 `mkdisk`; end-to-end tests
  cpu-difftest/  differential tests of the CPU against Unicorn             dev-deps: armv6m
```

`armv6m`, `pcb`, `pcb-devices`, `usbhost` and `rp2040-core` are `no_std` +
`alloc`, deterministic, and do no I/O. The same core runs natively and in the
browser, and two runs with the same inputs are identical. Everything that
touches the outside world (files, sockets, sound, MIDI, the pty) is in
`tartelette` or one of the bridge crates.

`pcb` talks to the chip only through the `Pins` trait, which `rp2040-core`
implements for its GPIO block. Peripherals stay inside `rp2040-core`: they are
tied to reset gating, IRQ lines, DMA and PIO.

### Adding a device

Each device in `pcb-devices` is one file that exposes a `KIND`: its name, a
one-line summary, the bus it sits on, and its board-file keys with types,
defaults and help text. Register it in `register_builtin` in
`pcb-devices/src/lib.rs` and the board parser, `tartelette parts`, the JSON
schema and the error messages all pick it up. Regenerate the schema with:

```sh
cargo run -p tartelette -- schema > boards/board.schema.json
```

For it to show on the front panel, give it a widget in
`crates/tartelette/src/panel.rs`.

## Tests

```sh
cargo test --workspace
```

`cpu-difftest` is the slow one, at around a minute. The end-to-end tests in
`crates/tartelette/tests` build the firmware in `firmware/tests` and
`firmware/examples` and run it headless; `firmware/README.md` lists what each
one checks. The Wi-Fi and Bluetooth tests need the CYW43439 firmware blobs,
from `CYW43_FIRMWARE_DIR` or the newest Embassy checkout in Cargo's git cache.

To check that the core still builds for the browser:

```sh
cargo build -p armv6m -p pcb -p pcb-devices -p rp2040-core -p tartelette --target wasm32-unknown-unknown
```

### In a browser

The web build is checked in a headless Chromium-family browser
(`thorium-browser`, `chromium` or `google-chrome`, or set `CHROME`), driven
over the DevTools protocol by `bench/web/cdp.mjs` (Node 22+, no packages):

```sh
scripts/web-check.sh [FIRMWARE] [SCREENSHOT]   # build with trunk, drop a firmware on the page, fail on exceptions, screenshot
scripts/web-bench.sh [ROUNDS]                  # time the core in the browser, with and without wasm tail calls
```

`web-check.sh` runs `firmware/tests/cpu-bench` by default. `WAIT_MS` sets how
long it runs, and `CLICK="X,Y"` clicks somewhere first (e.g. `283,632`, the
USB serial tab). `web-bench.sh` takes `MICROPYTHON_UF2=...` to time
MicroPython too.

The web build uses WebAssembly tail calls (set in `.cargo/config.toml`) so the
CPU interpreter can use threaded dispatch, as it does natively. Without the
flag it still works, about 20% slower.

### MicroPython's test suite

`scripts/micropython-tests.sh` runs MicroPython's own `tests/run-tests.py`
against a MicroPython firmware in the emulator, over the USB serial port.
It needs the firmware (`MICROPYTHON_UF2`) and a MicroPython checkout at the
same version with the `lib/micropython-lib` submodule (`MICROPYTHON_DIR`):

```sh
git clone --depth 1 --branch v1.29.0 https://github.com/micropython/micropython.git
git -C micropython submodule update --init --depth 1 lib/micropython-lib
MICROPYTHON_UF2=RPI_PICO_W-20260824-v1.29.0.uf2 MICROPYTHON_DIR=micropython scripts/micropython-tests.sh
```

Arguments go to `run-tests.py` (`-d basics`, `-i REGEX`, test files). The
script fails on any failure that isn't listed, with a reason, in
`scripts/micropython-known-failures.txt`, and tells you when a known failure
starts passing.

`scripts/micropython-flash.py` copies files onto MicroPython's filesystem in a
`--flash` image, using MicroPython's `pyboard.py`.

## Release builds with PGO

`scripts/pgo-build.sh` builds a profile-guided binary at
`target/pgo/tartelette`, 1.1 to 1.4 times faster than a plain release build.
It builds an instrumented tartelette, runs it on the repository's firmware,
and rebuilds with the profile, which takes a few minutes. It's meant for
release builds in CI.

```sh
rustup component add llvm-tools
scripts/pgo-build.sh
```

It needs `llvm-profdata` from rustc's LLVM (`llvm-tools`, or set
`LLVM_PROFDATA`). `CYW43_FIRMWARE_DIR` adds the Pico W Wi-Fi example to the
training run, and `PGO_FIRMWARE="a.uf2 b.elf"` any other firmware.
