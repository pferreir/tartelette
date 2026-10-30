#!/usr/bin/env bash
# MicroPython's own test suite (tests/run-tests.py from a MicroPython checkout)
# against a MicroPython firmware running in the emulator, over the USB serial
# port (headless, on the emulator's stdin and stdout).
#
#   MICROPYTHON_UF2=RPI_PICO_W-20260824-v1.29.0.uf2 \
#   MICROPYTHON_DIR=~/micropython \
#   scripts/micropython-tests.sh [run-tests.py options and tests]
#
# MICROPYTHON_DIR is a checkout of MicroPython at the firmware's version (the
# tests and their expected output go with it), with lib/micropython-lib for
# unittest, which many tests need on the board:
#
#   git clone --depth 1 --branch v1.29.0 https://github.com/micropython/micropython.git
#   git -C micropython submodule update --init --depth 1 lib/micropython-lib
#
# Optional: BOARD (default pico-w), TARTELETTE (default: a release build of
# this checkout), RESULTS (default target/micropython-tests),
# MICROPY_TEST_TIMEOUT (seconds a test may go without output, default 120:
# run-tests.py's 30 is wall-clock time, and the emulator can run slower than
# the board, e.g. ~6x with I2S keeping a PIO and DMA busy). Arguments go to
# run-tests.py, e.g. `-d basics` or `-i 'extmod/vfs_.*'` or test files
# (relative to MICROPYTHON_DIR/tests).
#
# scripts/micropython-known-failures.txt lists the tests known to fail, each
# with why. The script fails if any other test does, and lists known failures
# that now pass so they can come off the list.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
: "${MICROPYTHON_UF2:?set MICROPYTHON_UF2 to a MicroPython firmware (.uf2)}"
: "${MICROPYTHON_DIR:?set MICROPYTHON_DIR to a MicroPython checkout at the version of the firmware}"
board=${BOARD:-pico-w}
export MICROPY_TEST_TIMEOUT=${MICROPY_TEST_TIMEOUT:-120}
results=${RESULTS:-$root/target/micropython-tests}
known=$root/scripts/micropython-known-failures.txt
unittest=$MICROPYTHON_DIR/lib/micropython-lib/python-stdlib/unittest/unittest/__init__.py

[[ -f $MICROPYTHON_UF2 ]] || { echo "MICROPYTHON_UF2=$MICROPYTHON_UF2: no such file" >&2; exit 2; }
[[ -f $MICROPYTHON_DIR/tests/run-tests.py ]] ||
    { echo "MICROPYTHON_DIR=$MICROPYTHON_DIR: no tests/run-tests.py" >&2; exit 2; }
[[ -f $unittest ]] || {
    echo "no $unittest: run git -C $MICROPYTHON_DIR submodule update --init --depth 1 lib/micropython-lib" >&2
    exit 2
}
# Tests from another version expect fixes (or behaviour) the firmware doesn't
# have. Compare the version in the UF2's name (BOARD-DATE-vX.Y.Z.uf2, previews
# vX.Y.Z-preview.N.gHASH.uf2) with the checkout's, dots and dashes alike.
uf2_version=$(basename "$MICROPYTHON_UF2" .uf2 | grep -o 'v[0-9].*' || true)
dir_version=$(git -C "$MICROPYTHON_DIR" describe --tags 2>/dev/null || true)
if [[ -n $uf2_version && -n $dir_version && ${uf2_version//-/.} != "${dir_version//-/.}" ]]; then
    echo "micropython-tests: warning: the firmware is $uf2_version but MICROPYTHON_DIR is at" \
        "$dir_version; tests changed since may fail (git -C $MICROPYTHON_DIR checkout $uf2_version)" >&2
fi
for arg in "$@"; do
    [[ $arg == -h || $arg == --help ]] && exec "$MICROPYTHON_DIR/tests/run-tests.py" --help
done
if [[ -z ${TARTELETTE:-} ]]; then
    cargo build --release --quiet -p tartelette --manifest-path "$root/Cargo.toml"
    TARTELETTE=$root/target/release/tartelette
fi

mkdir -p "$results"
uf2=$(realpath "$MICROPYTHON_UF2")
# UART0 to a file of its own: on stdout it would be mixed into the REPL.
run=$(printf '%q ' "$TARTELETTE" "$uf2" --board "$board" --net loopback --headless \
    --uart0 "$results/uart0.log")

# A flash image with unittest on the board's filesystem. The test run starts
# from a copy, so every run starts from the same filesystem.
echo "micropython-tests: installing unittest" >&2
rm -f "$results/flash.img"
python3 "$root/scripts/micropython-flash.py" "$MICROPYTHON_DIR" "$results/flash.img" "$run" \
    "$unittest:/lib/unittest/__init__.py" >/dev/null
cp "$results/flash.img" "$results/flash-run.img"

echo "micropython-tests: running" >&2
flash=$(printf '%q' "$results/flash-run.img")
status=0
cd "$MICROPYTHON_DIR/tests"
./run-tests.py -t "exec:$run --flash $flash" -r "$results" "$@" || status=$?
cd "$root"

# Compare the failures with the known ones.
python3 - "$results/_results.json" "$known" "$status" <<'EOF'
import json, sys

results, known_file, status = sys.argv[1], sys.argv[2], int(sys.argv[3])
known = {}
for line in open(known_file):
    test, _, why = line.partition("#")
    if test.strip():
        known[test.strip()] = why.strip()
runs = {r[0]: r[1] for r in json.load(open(results))["results"]}
failed = {t for t, r in runs.items() if r == "fail"}
new = sorted(failed - known.keys())
fixed = sorted(t for t in known if runs.get(t) == "pass")
counts = {k: sum(1 for r in runs.values() if r == k) for k in ("pass", "fail", "skip")}
print(f"micropython-tests: {counts['pass']} passed, {counts['fail']} failed "
      f"({len(failed) - len(new)} known), {counts['skip']} skipped", file=sys.stderr)
for t in fixed:
    print(f"  now passes, take it off the known failures: {t}", file=sys.stderr)
for t in new:
    print(f"  NEW FAILURE: {t}", file=sys.stderr)
if new:
    print("  (run-tests.py --print-failures -r RESULTS shows the diffs)", file=sys.stderr)
# run-tests.py also fails when it stops early (a test that hangs, say).
sys.exit(1 if new or (status and not failed) else 0)
EOF
