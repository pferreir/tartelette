#!/usr/bin/env bash
# Time the emulator core in a browser, with and without WebAssembly tail
# calls (the CPU interpreter's threaded dispatch, crates/core/armv6m/src/cpu/
# threaded.rs), on firmware/tests/cpu-bench.
#
#   scripts/web-bench.sh [ROUNDS]       # default 3 rounds of each
#
# MICROPYTHON_UF2=path/to/RPI_PICO.uf2 adds MicroPython running a compute
# loop typed into its REPL over the emulated USB. CHROME picks the browser
# (default: thorium-browser, chromium or google-chrome, whichever is
# installed). Needs Node 22+ and python3 (for a static file server).
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
rounds=${1:-3}
work=$(mktemp -d)
server=
cleanup() {
    [[ -n $server ]] && kill "$server" 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT

echo "web-bench: building" >&2
env -u RUSTFLAGS cargo build --release --quiet --target thumbv6m-none-eabi \
    --manifest-path firmware/tests/cpu-bench/Cargo.toml --target-dir target/firmware/cpu-bench
cp target/firmware/cpu-bench/thumbv6m-none-eabi/release/cpu-bench "$work/cpu-bench.elf"
for variant in tail loop; do
    flag=$([[ $variant == tail ]] && echo +tail-call || echo -- -tail-call)
    RUSTFLAGS="-C target-feature=${flag#-- }" cargo build --release --quiet \
        --target wasm32-unknown-unknown --manifest-path tools/web-bench/Cargo.toml \
        --target-dir "target/web-bench/$variant"
    cp "target/web-bench/$variant/wasm32-unknown-unknown/release/web_bench.wasm" "$work/$variant.wasm"
done
if command -v llvm-objdump >/dev/null; then
    tail_calls=$(llvm-objdump -d "$work/tail.wasm" | grep -c return_call_indirect || true)
    if [[ $tail_calls == 0 ]]; then
        echo "web-bench: the tail-call build has no tail calls" >&2
        exit 1
    fi
fi
cp tools/web-bench/index.html "$work/"
query="rounds=$rounds"
if [[ -n ${MICROPYTHON_UF2:-} ]]; then
    cp "$MICROPYTHON_UF2" "$work/micropython.uf2"
    query+="&mp=1"
fi

port=$((8000 + RANDOM % 1000))
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$work" >/dev/null 2>&1 &
server=$!
sleep 0.5

echo "web-bench: running in the browser" >&2
out=$(node tools/web-bench/cdp.mjs "http://127.0.0.1:$port/index.html?$query" "document.title !== 'running'")
grep -v '^RESULT ' <<<"$out" | grep -v 'favicon\|404' >&2 || true
node -e '
const results = JSON.parse(process.argv[1]);
if (!Array.isArray(results)) { console.error("web-bench:", results); process.exit(1); }
const median = (v) => v.sort((a, b) => a - b)[v.length >> 1];
console.log("firmware          loop (ms)  tail calls (ms)  change");
for (const fw of [...new Set(results.map((r) => r.firmware))]) {
  const of = (v) => results.filter((r) => r.firmware === fw && r.variant === v);
  const [loop, tail] = [of("loop"), of("tail")];
  const same = new Set(results.filter((r) => r.firmware === fw).map((r) => r.mcycles + r.out)).size === 1;
  const [l, t] = [median(loop.map((r) => r.ms)), median(tail.map((r) => r.ms))];
  console.log(`${fw.padEnd(17)} ${String(l).padStart(9)}  ${String(t).padStart(15)}  ${((t / l - 1) * 100).toFixed(1).padStart(5)}%` +
    (same ? "" : "   RESULTS DIFFER"));
  if (!same) process.exitCode = 1;
}
' "$(sed -n 's/^RESULT //p' <<<"$out")"
