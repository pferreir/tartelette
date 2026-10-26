# tartelette

An RP2040 emulator that runs a firmware ELF and emulates the PCB around the
chip (display, port expander, SD card, ...). One app (egui) with a front panel
that runs natively and in the browser, and natively also headless: the library crates are
`no_std` + `alloc`, deterministic and free of I/O.
The target board is the BTL-01 (`boards/btl-01.board`).

## Crates

```
crates/
  armv6m/        Cpu, Bus trait, Ram, exceptions, Ppb (NVIC/SysTick/SCB)   deps: none
  pcb/           device framework + Board (no chip knowledge)              deps: none
  pcb-devices/   stock device models + `registry()`                        deps: pcb
  rp2040-core/   peripherals, ELF loading, SystemBus, System               deps: armv6m, pcb
  netstack/      user-mode network (slirp-style NAT) for the Wi-Fi chip    deps: pcb-devices, smoltcp (std)
  tartelette/    front panel (native + web, eframe), headless runner and   deps: rp2040-core, pcb, pcb-devices, netstack (native)
                 `mkdisk` (native, clap); end-to-end tests
  cpu-difftest/  differential tests of the CPU against Unicorn             dev-deps: armv6m
firmware/        standalone bare-metal test firmwares (built by the tests)
boards/          board description files
```

`pcb` talks to the chip only through the `Pins` trait, which `rp2040-core`
implements for its GPIO block. Peripherals stay inside `rp2040-core`: they are
coupled to reset gating, IRQ lines and (later) DMA/PIO.

## Running

```sh
cargo run --release -p tartelette -- firmware.elf --board boards/btl-01.board [--sd sd=card.img]
cargo run --release -p tartelette -- firmware.elf --board boards/btl-01.board --headless [--max-cycles N] [--press NAME@US[+HOLD]] [--log] ...
cargo run --release -p tartelette -- mkdisk card.img --dir data   # a blank FAT16 SD card image
cd crates/tartelette && trunk serve --release   # web: open the page, then open/drop the files
```

See `tartelette --help` for every option. Headless, UART0 goes to stdout and
UART1, board events and the firmware log to stderr.

The board file's `panel <part> at=COL,ROW [span=W,H] [label=...]` lines place
each part's control on a grid (see `pcb::board::panel`); widget options such
as `rotate=90` or `key=Space` are listed in `crates/tartelette/src/panel.rs`.
Without `panel` lines every part is laid out automatically. Natively, the
firmware and board reload when their files change.

## Pico W Wi-Fi

`boards/pico-w.board` has the CYW43439 (`pcb-devices/src/cyw43439`), modelled
at the pins, so unmodified Embassy `cyw43` + `cyw43-pio` firmware runs: it
downloads the radio firmware over the emulated gSPI link, scans, joins one of
the board file's mock access points (`apN=SSID,open` / `apN=SSID,wpa2,PASS`),
gets a DHCP lease and talks to the network. The Wi-Fi LED toggles as a `led`
board event.

What is behind the radio is a `NetBackend`: natively `netstack`'s user-mode NAT
(guest `10.0.2.15`, gateway `10.0.2.2` = the host's `127.0.0.1`, DNS `10.0.2.3`),
the deterministic `LoopbackBackend` in tests; the browser has none, so scans find
nothing and the panel says "Wi-Fi unavailable in the browser".

```sh
tartelette fw.elf --board boards/pico-w.board [--net user|loopback|none] [--forward 8080:80] [--headless --log --wifi-trace wifi]
```

`firmware/wifi-test` has Pico W test firmware (blinky, scan, join); see
`crates/tartelette/tests/wifi_firmware.rs` for how it is built and run. The radio
firmware blobs are Infineon's and not in the repository (`CYW43_FIRMWARE_DIR`).

## Testing

```sh
cargo test --workspace   # cpu-difftest is slow (~1 min)
cargo build -p armv6m -p pcb -p pcb-devices -p rp2040-core -p tartelette --target wasm32-unknown-unknown
```
