# Development

## Crates

```
crates/
  core/                  the chip, no_std and I/O-free
    armv6m/              Cpu, Bus trait, Ram, exceptions, Ppb (NVIC/SysTick/SCB)  deps: none
    rp2040-core/         peripherals, ELF/UF2 loading, SystemBus, System          deps: armv6m, pcb, usbhost
    usbhost/             USB host for the emulated device: enumeration, CDC-ACM,  deps: none
                         HID, USB-MIDI, traffic log
  board/                 what is around the chip, no_std and I/O-free
    pcb/                 device framework + Board (no chip knowledge)             deps: toml
    pcb-devices/         stock device models + `registry()`                       deps: pcb
    sdimage/             SD card images: FAT16/32 formatter and reader            deps: none
  host/                  the host's side: bridges to the OS, log reader
    bridge-net/          user-mode network (slirp-style NAT) for the Wi-Fi chip   deps: pcb-devices, smoltcp (std)
    bridge-hci/          a real Bluetooth adapter through an HCI user channel     deps: pcb-devices, libc (Linux)
    bridge-usb/          the USB device's serial port as a pty, HID as UHID       deps: usbhost, libc (Linux)
    rtt/                 defmt-over-RTT reader                                    deps: defmt-decoder, object
  app/
    tartelette-runtime/  Session (firmware + board, stepped), pacing, boards,     deps: rp2040-core, pcb-devices,
                         the host's side of USB/UART/radios, test synth           rtt, the bridges, midir
    tartelette/          front panel (native + web, eframe), headless runner,     deps: tartelette-runtime,
                         `mkdisk`; end-to-end tests                               sdimage, eframe, cpal
tools/
  cpu-difftest/          differential tests of the CPU against Unicorn            dev-deps: armv6m
  sitedocs/              renders docs/ as HTML for the demo site                  deps: pulldown-cmark
  web-bench/             the core as a bare wasm module, timed in browsers        deps: rp2040-core
```

`armv6m`, `pcb`, `pcb-devices`, `sdimage`, `usbhost` and `rp2040-core` are `no_std` +
`alloc`, deterministic, and do no I/O. The same core runs natively and in the
browser, and two runs with the same inputs are identical. Everything that
touches the outside world (files, sockets, sound, MIDI, the pty) is in
`tartelette-runtime`, `tartelette` or one of the bridge crates.
`tartelette-runtime` has no UI: what the front panel, the headless runner and
tests share (a `Session` and its clock, board loading, the far ends of the
USB cable and UART lines) lives there, and `tartelette` adds the command
line, the window and the sound card. Front ends drive a `Session` with
`step` (up to a number of cycles, in `System::run` chunks of 250 000, until
a wall-clock deadline: the window paces it with `run::Clock`, from a thread
of its own natively and from tasks between frames in a browser,
`tartelette/src/pump.rs`) or `advance` (one `System::run`, as headless
does). A `Session` hands what the firmware produced (serial bytes, board
events, the defmt log, warnings) to its front end as an `Output` after each
step: the front panel keeps a capped history of it for the dock, headless
prints it. Anything that must hear board events as they happen, like the
live synth, registers a listener on the session instead.

`pcb` talks to the chip only through the `Pins` trait, which `rp2040-core`
implements for its GPIO block. Peripherals stay inside `rp2040-core`: they are
tied to reset gating, IRQ lines, DMA and PIO.

`rp2040-core` also owns what is on the other end of the USB cable: a
`usbhost::Host`, which `System` polls from its own scheduler so that every
transaction lands on the cycle it would on a real bus, as the board's devices
do. Frontends reach it through `System::usb_host` and `System::with_usb_host`.

### Adding a device

Each device in `pcb-devices` is one file that exposes a `KIND`: its name, a
one-line summary, the bus it sits on, and its board-file keys with types,
defaults and help text. Register it in `register_builtin` in
`pcb-devices/src/lib.rs` and the board parser, `tartelette parts`, the JSON
schema and the error messages all pick it up. Regenerate the schema with:

```sh
cargo run -p tartelette -- schema > boards/board.schema.json
```

For it to show on the front panel, give it a widget: a `Widget` variant,
picked for its kind in `crates/app/tartelette/src/panel/layout.rs`, and a
file in `panel/widgets/` that draws it.

## How a run advances

[rp2040.md](rp2040.md#fast-paths) says what the scheduler does from the
firmware's side; this is how it does it, in
`crates/core/rp2040-core/src/system.rs`.

`System::run(max_cycles)` loops until device time reaches the end:

1. `next_core` wakes sleeping cores that have an interrupt or event, and
   picks the running core whose clock is in the earliest turn slice
   (`turn_shift`, 128 cycles; core 0 when both are in the same one).
2. If `can_batch`, `run_batch` runs that core's instructions back to back
   (`Cpu::run_threaded`) until the next device event (`batch_limit`) or an
   access that needs the system. On the other core's turn, `run_turn`
   returns and the batch carries on with that core, without syncing the
   devices in between. Otherwise `step_core` runs one instruction, or takes
   a pending exception.
3. `advance_to` brings the devices up to the running core ahead,
   stopping at each board part's timer and each USB host poll on the way.
   `advance_segment` does the work between those: the bus transfers the
   firmware queued, one DMA item a cycle, the PIO blocks, then
   `advance_clocks` (timer, SysTick, UARTs, SPI wires, USB frames, ADC,
   PWM). Board parts hear of pin changes in `sync_board`, and the cores'
   NVIC inputs are recomputed in `refresh_core_views`.
4. With every core asleep, it skips to the next event (`cycles_to_event`,
   board timers, the USB host), in `DMA_SLICE` steps while the DMA can move
   something and to a running PIO block's next tick otherwise; it stops
   with `StopReason::Sleeping` when there is no next event.

```text
  System::run(max_cycles)
      |
      v
  next_core ---- no core running ----> skip ahead (advance_to) to the next
      |                                event, or stop with Sleeping
      | the running core in the
      | earliest turn slice
      v
  can_batch? --yes--> run_batch, then try_spin -----+
      |                                             |
      no                                            |
      v                                             |
  step_core: one instruction or exception ----------+
                                                    |
                                                    v
                          advance_to(the running core ahead)
                              | stops at board timers and USB polls
                              v
                          advance_segment
                              quiet: advance_clocks
                              else:  buses, DMA, PIO, advance_clocks,
                                     refresh_core_views, sync_board
                              |
      +-----------------------+
      v
  bkpt, fault, or the end reached? stop; else back to next_core
```

Three flags decide how much of that a step needs:

- `quiet` (`System`): only the timer, SysTick and PIO dividers move on
  their own. Recomputed by `is_quiet` after each full sync. A running PIO
  block keeps a segment on the quiet path only up to its next tick
  (`pio_cycles_to_tick`, which counts `pio_owed`).
- `touched` (`SystemBus`): an access may have started work in a device the
  quiet path doesn't advance (`needs_sync`: DMA, PIO, buses, GPIO,
  RESETS). It forces a full sync.
- `lines_dirty` (`SystemBus`): an interrupt line may have moved, so the
  NVIC inputs need recomputing. Every MMIO write sets it, and the reads
  with side effects (`read_moves_lines`).

During a batch, devices are behind the core: `SystemBus::now` is the cycle
the current instruction starts on, `time` the cycle the devices were
brought to. `sync_to_now` catches up the timer, SysTick, UARTs and USB
before an access that could see them, and PIO dividers build up in
`pio_owed` until a PIO, DMA or RESETS access (`flush_pio_for`) or a full
sync:

```text
  An instruction in a batch is about to access a register:

  devices   ----------|                         time
  core 0    ----------+-+-+-+-+-+-+-+-+-+-+-+-|  now: where it starts

  sync_to_now: the timer, SysTick, UARTs, SPI wires and USB catch up

  devices   ----------------------------------|  time = now
  PIO       ----------|<------ pio_owed ----->|  applied on a PIO, DMA or
                                                 RESETS access, or a sync
```

The other two fast paths sit on top: `Stream` (a DMA channel feeding a
timed SPI, which `find_stream` sets up for one batch, and which catches up
through `StreamBus` before any access that could see it), and spinning
(`try_spin`, `try_lap` on a `DryBus`, `materialize` to bring the core
back; the comment above `try_spin` has the rules).

### A peripheral that keeps time

A block that changes state on its own (a timed FIFO, a counter, a
conversion) has to take part in all of this, or batches will run past its
events:

- report its next event in `SystemBus::cycles_to_event`;
- catch up in `advance_clocks`, and in `sync_to_now` if the cores read it
  often;
- set `lines_dirty` when its interrupt changes, and `touched` when it
  raises a DMA request or needs the DMA or a board part to act;
- add its DREQs to `dreq_mask` and its interrupt to `irq_lines`;
- make sure `needs_sync` and `read_moves_lines` cover its registers;
- follow clk_sys changes in `SystemBus::set_sys_hz` if it counts in
  cycles;
- add firmware that uses it to
  `crates/app/tartelette/tests/batch_equivalence.rs`, which runs it with
  `batching` off (every device synced after every instruction) and on, and
  compares the two cycle by cycle.

For performance-sensitive changes there, compare `firmware/tests/cpu-bench`
before and after.

## Tests

```sh
cargo test --workspace
```

`cpu-difftest` is the slow one, at around a minute. The end-to-end tests in
`crates/app/tartelette/tests` build the firmware in `firmware/tests` and
`firmware/examples` (and the `groovebox-audio` and `varvara` showcase
demos) and run it headless; `firmware/README.md` lists what each
one checks. The Wi-Fi and Bluetooth tests need the CYW43439 firmware blobs,
from `CYW43_FIRMWARE_DIR` or the newest Embassy checkout in Cargo's git cache.

To check that the core still builds for the browser:

```sh
cargo build -p armv6m -p pcb -p pcb-devices -p sdimage -p rp2040-core \
    -p tartelette-runtime -p tartelette --target wasm32-unknown-unknown
```

`.github/workflows/ci.yml` runs both, along with `cargo fmt --check` and
`cargo clippy -D warnings`, on each push to `main` and on pull requests.

### In a browser

The web build is checked in a headless Chromium-family browser
(`thorium-browser`, `chromium` or `google-chrome`, or set `CHROME`), driven
over the DevTools protocol by `tools/web-bench/cdp.mjs` (Node 22+, no packages):

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

### The demo site

`scripts/build-site.sh [OUT]` builds the static site GitHub Pages serves
(`.github/workflows/pages.yml` runs it on each push to `main`): the
landing page, the web app under `emu/`, the `firmware/showcase` demos under
`demos/`, and the documentation under `docs/`. `MICROPYTHON=fetch` adds the
official MicroPython build for the Pico, `MICROPYTHON_UF2=FILE` a local
one; `PICODEXED=fetch` and `PICODEXED_UF2=FILE` do the same for
[picodexed](https://github.com/diyelectromusic/picodexed)'s prebuilt UF2
(on `firmware/showcase/boards/picodexed.toml`), and `VARVARA=fetch` the
ROM of the `varvara` demo and its SD card image
(`firmware/showcase/roms/card.sh`). Without them, the page leaves those
cards out. Look at it with `python3 -m http.server -d target/site`.

The landing page's text is `site/index.md`, and the documentation is the
Markdown in `docs/`. `tools/sitedocs` (pulldown-cmark) turns both into HTML,
with the layouts in `tools/sitedocs/src/landing.html` and `page.html`. The
comment at the top of `site/index.md` explains how its Markdown maps onto
the page: a `###` entry is a demo, its image the screenshot, a list of
`Label: value` items its details, its last paragraph of links the buttons.

Links are written as they would be on GitHub, relative to the Markdown file.
Links between `.md` files become links between pages, links into `site/`
(the screenshots, `emu/`) stay relative, and anything else in the
repository goes to GitHub. Heading ids follow GitHub's, so
`page.md#heading` links work in both places. Code blocks in `toml` and `sh`
are highlighted (`tools/sitedocs/src/highlight.rs`).

The page links each demo as `emu/?firmware=../demos/NAME.elf&board=...`.
Its screenshots (`site/img/`) are checked in: retake them when a demo's
look changes.

To render the pages on their own, into `target/site`:

```sh
cargo run -p sitedocs -- . target/site https://github.com/OWNER/REPO \
    --var MICROPYTHON_VERSION=v1.29.0 [--drop micropython]
```

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
release builds in CI, and for running heavy firmware natively: picodexed
(250 MHz, both cores busy) runs about 16% faster with it.

```sh
rustup component add llvm-tools
scripts/pgo-build.sh
```

It needs `llvm-profdata` from rustc's LLVM: `llvm-tools`, a system
`llvm-profdata` of the same major version (found on the `PATH`), or set
`LLVM_PROFDATA`. `CYW43_FIRMWARE_DIR` adds the Pico W Wi-Fi example to the
training run, and `PGO_FIRMWARE="a.uf2 b.elf"` any other firmware.

## Releases

Pushing a version tag publishes a release (`.github/workflows/release.yml`):

```sh
git tag v0.1.0 && git push origin v0.1.0
```

The tag has to match `version` in the workspace `Cargo.toml`. The tests run
first, then the release gets the `tartelette` binary for Linux (x86_64,
aarch64), macOS (Apple silicon) and Windows, the web app as a static
directory, and `SHA256SUMS`. A tag with a suffix (`v0.2.0-rc.1`) makes a
pre-release.
