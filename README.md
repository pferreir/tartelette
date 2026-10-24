# rp2040_emu

An RP2040 emulator that runs a firmware ELF and emulates the PCB around the
chip (display, port expander, SD card, ...). A headless CLI, and a front-panel
app (egui) that runs natively and in the browser: the library crates are
`no_std` + `alloc`, deterministic and free of I/O.
The target board is the BTL-01 (`boards/btl-01.board`).

## Crates

```
crates/
  armv6m/        Cpu, Bus trait, Ram, exceptions, Ppb (NVIC/SysTick/SCB)   deps: none
  pcb/           device framework + Board (no chip knowledge)              deps: none
  pcb-devices/   stock device models + `registry()`                        deps: pcb
  rp2040-core/   peripherals, ELF loading, SystemBus, System               deps: armv6m, pcb
  rp2040-cli/    native runner and end-to-end tests                        deps: rp2040-core, pcb, pcb-devices
  rp2040-app/    front-panel GUI, native + web (eframe)                    deps: rp2040-core, pcb, pcb-devices
  cpu-difftest/  differential tests of the CPU against Unicorn             dev-deps: armv6m
firmware/        standalone bare-metal test firmwares (built by the tests)
boards/          board description files
```

`pcb` talks to the chip only through the `Pins` trait, which `rp2040-core`
implements for its GPIO block. Peripherals stay inside `rp2040-core`: they are
coupled to reset gating, IRQ lines and (later) DMA/PIO.

## Front-panel app

```sh
cargo run --release -p rp2040-app -- firmware.elf --board boards/btl-01.board [--sd sd=card.img]
cd crates/rp2040-app && trunk serve --release   # web: open the page, then open/drop the files
```

The board file's `panel <part> at=COL,ROW [span=W,H] [label=...]` lines place
each part's control on a grid (see `pcb::board::panel`); widget options such
as `rotate=90` or `key=Space` are listed in `crates/rp2040-app/src/panel.rs`.
Without `panel` lines every part is laid out automatically. Natively, the
firmware and board reload when their files change.

## Testing

```sh
cargo test --workspace   # cpu-difftest is slow (~1 min)
cargo build -p armv6m -p pcb -p pcb-devices -p rp2040-core -p rp2040-app --target wasm32-unknown-unknown
```
