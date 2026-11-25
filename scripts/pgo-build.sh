#!/usr/bin/env bash
# Build tartelette with profile-guided optimisation (PGO): opt-in, for
# release builds in CI. Ordinary `cargo build` and `cargo install` are not
# affected.
#
#   scripts/pgo-build.sh            # → target/pgo/tartelette
#
# 1. builds an instrumented tartelette (-Cprofile-generate),
# 2. runs it headless on a training corpus of firmware from this repository,
# 3. merges the profiles with llvm-profdata,
# 4. builds tartelette again with them (-Cprofile-use).
#
# The corpus is the bare-metal test firmware in firmware/tests (cpu-bench is
# the CPU-bound part), the USB composite device on both stacks
# (firmware/examples/pico-usb, pico-usb-hal; their crates are downloaded the
# first time) and, when CYW43_FIRMWARE_DIR names the radio firmware blobs,
# the Pico W Wi-Fi example. Extra firmware (a MicroPython UF2, ...) can join
# with PGO_FIRMWARE="a.uf2 b.elf": each runs headless for 5 emulated seconds.
#
# Needs llvm-profdata from the same LLVM as rustc: `rustup component add
# llvm-tools` (for the toolchain in rust-toolchain.toml), or point
# LLVM_PROFDATA at one.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root" # rust-toolchain.toml picks the toolchain from here on
out=$root/target/pgo
host=$(rustc -vV | sed -n 's/^host: //p')
profdata=${LLVM_PROFDATA:-$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-profdata}
# Without llvm-tools, a system llvm-profdata of the same LLVM major version
# reads rustc's profiles too.
if [[ ! -x $profdata && -z ${LLVM_PROFDATA:-} ]]; then
    want=$(rustc -vV | sed -n 's/^LLVM version: \([0-9]*\).*/\1/p')
    system=$(command -v llvm-profdata || true)
    if [[ -n $system ]] &&
        [[ $("$system" --version | sed -n 's/.*LLVM version \([0-9]*\).*/\1/p') == "$want" ]]; then
        profdata=$system
    fi
fi
if [[ ! -x $profdata ]]; then
    echo "pgo-build: no llvm-profdata at $profdata" >&2
    echo "  rustup component add llvm-tools   (or set LLVM_PROFDATA)" >&2
    exit 1
fi

step() { echo "pgo-build: $*" >&2; }

# ---- the training corpus ----

firmware() { # NAME (firmware/tests/NAME or firmware/examples/NAME)
    local dir=firmware/tests/$1
    [[ -d $dir ]] || dir=firmware/examples/$1
    env -u RUSTFLAGS cargo build --release --quiet --target thumbv6m-none-eabi \
        --manifest-path "$dir/Cargo.toml" --target-dir "target/firmware/$1"
}

step "building the training firmware"
tests=(cpu-bench uart-hello irq-timer io-test multicore-test batch-timer button-led i2c-test spi-dma-test pio-test sd-test)
for t in "${tests[@]}"; do firmware "$t"; done
for e in pico-usb pico-usb-hal; do firmware "$e"; done
if [[ -n ${CYW43_FIRMWARE_DIR:-} ]]; then
    firmware pico-w-wifi
fi

boards=$out/boards
mkdir -p "$boards"
cat >"$boards/button.toml" <<'EOF'
parts.status = { kind = "led", gpio = 25 }
parts.user = { kind = "button", gpio = 2 }
EOF
cat >"$boards/i2c.toml" <<'EOF'
buses.i2c1 = { sda = 2, scl = 3 }
parts.expander = { kind = "mcp23008", bus = "i2c1", addr = 0x20, int = 16 }
EOF
cat >"$boards/screen.toml" <<'EOF'
buses.spi0 = { sck = 18, mosi = 19, miso = 16 }
parts.screen = { kind = "st7789", bus = "spi0", cs = 9, dc = 13, rst = 17 }
EOF
cat >"$boards/pio.toml" <<'EOF'
parts.dac = { kind = "dac8165", sdi = 4, cs = 5, clk = 6, rst = 7, vref_mv = 1250 }
parts.leds = { kind = "ws2812", gpio = 14, count = 8 }
parts.knob = { kind = "encoder", clk = 22, dt = 21 }
EOF
cat >"$boards/sd.toml" <<'EOF'
buses.spi1 = { sck = 10, mosi = 11, miso = 12 }
parts.sd = { kind = "sdcard", bus = "spi1", cs = 8, size_mb = 1 }
EOF

train() { # BINARY: run the corpus on it
    local bin=$1 elf
    run() { "$bin" "$@" --headless --max-cycles 625000000 </dev/null >/dev/null 2>&1 || true; }
    for t in "${tests[@]}"; do
        elf=target/firmware/$t/thumbv6m-none-eabi/release/$t
        case $t in
        button-led) run "$elf" --board "$boards/button.toml" --press user@2000+500 ;;
        i2c-test) run "$elf" --board "$boards/i2c.toml" ;;
        spi-dma-test) run "$elf" --board "$boards/screen.toml" ;;
        pio-test) run "$elf" --board "$boards/pio.toml" --input knob:turn=3@1000 ;;
        sd-test) run "$elf" --board "$boards/sd.toml" ;;
        *) run "$elf" ;;
        esac
    done
    for e in pico-usb pico-usb-hal; do
        # Serial traffic both ways (stdin), HID reports and enumeration.
        yes 'the quick brown fox jumps over the lazy dog!' | head -c 20000 |
            "$bin" "target/firmware/$e/thumbv6m-none-eabi/release/usb_composite" \
                --headless --max-cycles 375000000 >/dev/null 2>&1 || true
    done
    if [[ -n ${CYW43_FIRMWARE_DIR:-} ]]; then
        run target/firmware/pico-w-wifi/thumbv6m-none-eabi/release/wifi_join --board pico-w --net loopback
    fi
    for f in ${PGO_FIRMWARE:-}; do
        run "$f"
    done
}

# ---- instrument, train, merge, rebuild ----

profiles=$out/profiles
rm -rf "$profiles"
mkdir -p "$profiles"

step "building the instrumented tartelette"
RUSTFLAGS="${RUSTFLAGS:-} -Cprofile-generate=$profiles" \
    cargo build --release --quiet -p tartelette --target "$host" --target-dir "$out/instrumented"

step "training"
train "$out/instrumented/$host/release/tartelette"

step "merging $(ls "$profiles" | wc -l) profiles"
"$profdata" merge -o "$out/merged.profdata" "$profiles"

step "building the optimised tartelette"
RUSTFLAGS="${RUSTFLAGS:-} -Cprofile-use=$out/merged.profdata" \
    cargo build --release --quiet -p tartelette --target "$host" --target-dir "$out/optimised"

cp "$out/optimised/$host/release/tartelette" "$out/tartelette"
step "done: $out/tartelette"
