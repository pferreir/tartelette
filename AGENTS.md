# AGENTS.md

tartelette is an RP2040 + board emulator in Rust (nightly, edition 2024).
Read `docs/development.md` first: it maps the crates, the tests and the
scripts. User-facing docs are in `README.md` and `docs/`.

## Ground rules

- `armv6m`, `pcb`, `pcb-devices`, `sdimage`, `usbhost` and `rp2040-core` are
  `no_std` + `alloc`, deterministic and do no I/O. Keep them that way: I/O
  belongs in `tartelette-runtime`, `tartelette` or the bridge crates.
- The core must keep building for `wasm32-unknown-unknown`.
- Each directory under `firmware/` is its own Cargo workspace (target
  `thumbv6m-none-eabi`), outside the main one.
- Don't commit generated or local files (`target/`, `card.img`, firmware
  blobs). `boards/board.schema.json` is generated but checked in: regenerate
  it when a device's `KIND` changes (see `docs/development.md`).

## Before committing

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p armv6m -p pcb -p pcb-devices -p sdimage -p rp2040-core \
    -p tartelette-runtime -p tartelette --target wasm32-unknown-unknown
```

- Firmware you touched: `cargo fmt` and `cargo clippy` with
  `--manifest-path firmware/.../Cargo.toml`.
- Shell scripts you touched: `shellcheck scripts/*.sh`.
- Web UI changes: `scripts/web-check.sh`. Performance-sensitive changes
  (CPU, bus, DMA, PIO): compare `firmware/tests/cpu-bench` before and after.
- If a check can't run here (missing blobs, browser, hardware), say so
  instead of skipping it silently.

## Commits

One line, imperative, prefixed with the area:

```
[cpu] Fold flag updates into the threaded dispatch
```

| Area         | Scope |
|--------------|-------|
| `[cpu]`      | `armv6m`, `cpu-difftest` |
| `[core]`     | `rp2040-core`: peripherals, bus, ELF/UF2 loading |
| `[devices]`  | `pcb`, `pcb-devices`, `boards/` |
| `[usb]`      | `usbhost`, `usbbridge` |
| `[net]`      | `netstack`, `hcibridge`, CYW43439 Wi-Fi/BLE |
| `[sd]`       | `sdimage` |
| `[rtt]`      | `rtt` |
| `[runtime]`  | `tartelette-runtime` |
| `[ui]`       | front panel (native and web) |
| `[cli]`      | headless runner, `mkdisk`, command line |
| `[firmware]` | `firmware/` |
| `[site]`     | `sitedocs`, `site/`, demo site build |
| `[docs]`     | `README.md`, `docs/`, this file |
| `[ci]`       | `.github/`, `scripts/`, release tooling |
| `[build]`    | workspace `Cargo.toml`, toolchain, dependencies |

Use two prefixes (`[core][devices]`) only when a change really spans them;
otherwise split the commit.

## Code

- Match the surrounding style; `rustfmt.toml` sets import grouping.
- Comment only what the code doesn't make obvious: hardware quirks, datasheet
  references, invariants, why a non-obvious approach was chosen.
- Comments describe the code as it is, not how it changed. No "now uses X
  instead of Y", "fixed", "new" or notes addressed to the reviewer; that
  belongs in the commit message.
- Prefer tests that run firmware headless (`crates/tartelette/tests`) for
  emulated behaviour, and unit tests for pure logic.

## Documentation

Update docs in the same commit as the change they describe:

- New or changed device: its page in `docs/devices/`, the list in
  `README.md`, the schema.
- New board keys, CLI flags or front-panel behaviour: `docs/boards.md`,
  `docs/running.md`.
- Crate layout, tests, scripts or CI: `docs/development.md`.
- What the emulator does or doesn't model: `docs/rp2040.md`.
- `docs/` is also rendered for the website: keep links relative to the file,
  as on GitHub.

## Asking for help

Ask the maintainer when:

- requirements are ambiguous, or there are several reasonable designs with
  real trade-offs;
- a change would break determinism, the web build, the board file format or
  CLI compatibility;
- emulated behaviour doesn't match the datasheet and you can't tell which
  one the firmware relies on;
- you're stuck after a couple of honest attempts.

A short question is cheaper than a wrong guess. Never commit, push or tag
unless asked.

## Pull requests and issues

Follow [CONTRIBUTING.md](CONTRIBUTING.md): keep changes small and focused.
Don't write pull request or issue descriptions, comments or review replies:
those must be written by a human. Hand the human a summary of the change and
let them write the text.
