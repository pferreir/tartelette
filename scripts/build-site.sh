#!/usr/bin/env bash
# Build the showcase web site: the landing page (site/index.md), the web app
# (crates/app/tartelette, under emu/), the demos it opens (firmware/showcase,
# under demos/) and the documentation (docs/, rendered by tools/sitedocs,
# under docs/), as one static directory for GitHub Pages or any web server.
#
#   scripts/build-site.sh [OUT]        # OUT defaults to target/site
#
# Then, to look at it:
#
#   python3 -m http.server -d target/site
#
# MICROPYTHON_UF2 adds the MicroPython demo from that file; MICROPYTHON=fetch
# downloads it (RPI_PICO, the version below) instead. Without either, the
# page leaves the MicroPython card out. REPO_URL is where the page's source
# links go (in GitHub Actions, the repository the workflow runs in).
#
# Needs: the nightly toolchain with thumbv6m-none-eabi and
# wasm32-unknown-unknown, trunk, and llvm-objcopy (or a versioned
# llvm-objcopy-N) to strip the firmware.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out=$(realpath -m "${1:-$root/target/site}")
MICROPYTHON_VERSION=${MICROPYTHON_VERSION:-20260824-v1.29.0}

if [[ -z ${REPO_URL:-} && -n ${GITHUB_REPOSITORY:-} ]]; then
    REPO_URL=${GITHUB_SERVER_URL:-https://github.com}/$GITHUB_REPOSITORY
fi
REPO_URL=${REPO_URL:-https://github.com/}

objcopy=$(command -v llvm-objcopy || compgen -c llvm-objcopy- | sort -V | tail -1 || true)
[[ -n $objcopy ]] || { echo "build-site: needs llvm-objcopy" >&2; exit 1; }

rm -rf "$out"
mkdir -p "$out/emu" "$out/demos"

echo "build-site: the web app" >&2
(cd "$root/crates/app/tartelette" && trunk build --release --public-url ./ --dist "$out/emu")
rm -f "$out/emu/.trunk-lock"

echo "build-site: the demo firmware" >&2
(cd "$root/firmware/showcase" && env -u RUSTFLAGS cargo build --release --bins)
bins=$root/target/firmware/showcase/thumbv6m-none-eabi/release
for demo in picosh lumen groovebox stardust spectra; do
    # Debug info off, the defmt symbols kept: the log still decodes.
    "$objcopy" --strip-debug "$bins/$demo" "$out/demos/$demo.elf"
done
cp "$root"/firmware/showcase/boards/*.{toml,wav} "$out/demos/"
# The boards point their editors at the schema; it doesn't matter on the web.
sed -i '1{/^#:schema/d}' "$out"/demos/*.toml

micropython=
if [[ -n ${MICROPYTHON_UF2:-} ]]; then
    cp "$MICROPYTHON_UF2" "$out/demos/micropython.uf2"
    micropython=1
elif [[ ${MICROPYTHON:-} == fetch ]]; then
    echo "build-site: MicroPython $MICROPYTHON_VERSION" >&2
    curl -fsSL -o "$out/demos/micropython.uf2" \
        "https://micropython.org/resources/firmware/RPI_PICO-$MICROPYTHON_VERSION.uf2"
    micropython=1
fi

echo "build-site: the landing page and the documentation" >&2
# site/index.md and docs/ become HTML (tools/sitedocs); the rest of site/
# (the screenshots) is copied as is.
cp -r "$root/site/." "$out/"
rm -f "$out"/*.md
cp "$root/assets/tartelette.png" "$out/img/logo.png"
sitedocs=(--var "MICROPYTHON_VERSION=${MICROPYTHON_VERSION#*-}")
# Without MicroPython, drop its entry from the page.
[[ -n $micropython ]] || sitedocs+=(--drop micropython)
(cd "$root" && env -u RUSTFLAGS cargo run -q -p sitedocs -- . "$out" "${REPO_URL%/}" "${sitedocs[@]}")

touch "$out/.nojekyll"

du -sh "$out" >&2
echo "build-site: done, in $out" >&2
