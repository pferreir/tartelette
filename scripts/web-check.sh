#!/usr/bin/env bash
# Build the web app (trunk, release) and check it in a headless browser:
# the page loads without exceptions and runs a firmware dropped on it; a
# screenshot shows the result.
#
#   scripts/web-check.sh [FIRMWARE] [SCREENSHOT]
#
# FIRMWARE defaults to firmware/tests/cpu-bench (it prints a checksum on
# UART0), SCREENSHOT to target/web-check.png. WAIT_MS (default 8000) is how
# long it runs before the screenshot; CLICK="X,Y;X,Y" clicks there first
# (e.g. CLICK=283,632 opens the console's USB serial tab). CHROME picks the
# browser, as for scripts/web-bench.sh. Needs trunk, Node 22+ and python3.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
work=$(mktemp -d)
server=
cleanup() {
    [[ -n $server ]] && kill "$server" 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT

firmware=${1:-}
if [[ -z $firmware ]]; then
    env -u RUSTFLAGS cargo build --release --quiet --target thumbv6m-none-eabi \
        --manifest-path firmware/tests/cpu-bench/Cargo.toml --target-dir target/firmware/cpu-bench
    firmware=target/firmware/cpu-bench/thumbv6m-none-eabi/release/cpu-bench
fi
firmware=$(realpath "$firmware")
shot=$(realpath -m "${2:-target/web-check.png}")

echo "web-check: building the web app" >&2
(cd crates/tartelette && trunk build --release --dist "$work" >/dev/null)

port=$((8000 + RANDOM % 1000))
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$work" >/dev/null 2>&1 &
server=$!
sleep 0.5

clicks=()
IFS=';' read -ra points <<<"${CLICK:-}"
for p in "${points[@]}"; do clicks+=(--click "$p"); done
echo "web-check: running $(basename "$firmware")" >&2
node bench/web/cdp.mjs "http://127.0.0.1:$port/index.html" \
    "document.querySelector('canvas')?.width > 0" \
    --drop "$firmware" --wait "${WAIT_MS:-8000}" "${clicks[@]}" --screenshot "$shot" |
    grep -v 'GL Driver\|integrity\|favicon\|404'
echo "web-check: screenshot in $shot" >&2
