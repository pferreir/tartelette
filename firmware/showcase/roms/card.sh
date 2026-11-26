#!/usr/bin/env bash
# Makes an SD card image for the varvara demo (see ../README.md), with ROMs
# to pick from its menu: fourtette, donsol and catclock at the top, and uxn's
# examples in directories by kind. The ROMs are assembled here from sources
# downloaded at pinned commits: uxn's (MIT: the examples, and the assembler,
# uxnasm, built with cc) and Hundred Rabbits' donsol and catclock (MIT).
# fourtette comes from fetch.sh. Their licences go in licences/ on the card.
# Nothing downloaded or built is kept in the repository.
#
#   roms/card.sh [IMAGE]      default roms/card.img
#
# The workspace's `mkdisk` writes the image (`tartelette mkdisk` on its own,
# which builds without the emulator's system libraries), built as needed
# unless MKDISK names a binary.
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../../.." && pwd)
out=${1:-$here/card.img}
work=$here/build
mkdir -p "$work/src"

"$here/fetch.sh"

# fetch NAME URL SHA256: a source tarball, unpacked in build/src/NAME.
fetch() {
    local name=$1 url=$2 sha=$3
    local tar=$work/src/$name.tar.gz dir=$work/src/$name
    if [[ -d $dir ]] && echo "$sha  $tar" | sha256sum -c --status 2>/dev/null; then
        return
    fi
    curl -fsSL -o "$tar.part" "$url"
    if ! echo "$sha  $tar.part" | sha256sum -c --status; then
        echo "$name: checksum mismatch, from $url" >&2
        rm -f "$tar.part"
        exit 1
    fi
    mv "$tar.part" "$tar"
    rm -rf "$dir"
    mkdir -p "$dir"
    tar -xzf "$tar" -C "$dir" --strip-components=1
    echo "$name: fetched"
}

# The last uxn commit with the examples in it (December 2025).
fetch uxn \
    https://git.sr.ht/~rabbits/uxn/archive/ba28d2440304b7cc4ac2f5f53fcfcf8d7ff6b9a7.tar.gz \
    757a1d5e2d1573bdb1e17a58a2ff7fdd7888ca83dded0a8ffa76c60a807767fd
fetch donsol \
    https://git.sr.ht/~rabbits/donsol/archive/db3e23b8a4a079504d2aeb34f4e54104cde7bfda.tar.gz \
    b3312fbbe080d7ac74a4501e62f0044f12b4bfe6f0587aa55dbf9b65cb421413
fetch catclock \
    https://git.sr.ht/~rabbits/catclock/archive/93941854c1696a8ef5705a645b77f4bab7061ea3.tar.gz \
    32f3d3a8031a87ec31af3f50cc0bde080be597284c67104b0dc4e27ac07c5037

uxnasm=$work/uxnasm
if [[ ! -x $uxnasm || $work/src/uxn/src/uxnasm.c -nt $uxnasm ]]; then
    "${CC:-cc}" -std=c89 -O2 -Wno-unknown-pragmas -o "$uxnasm" "$work/src/uxn/src/uxnasm.c"
fi

card=$work/card
rm -rf "$card"
mkdir -p "$card/licences"

# asm PROJECT SOURCE ROM: assembles SOURCE (relative to the project, from
# which it includes its other files) to ROM on the card.
asm() {
    local rom=$card/$3
    mkdir -p "$(dirname "$rom")"
    (cd "$work/src/$1" && "$uxnasm" "$2" "$rom" >/dev/null)
    # And the symbols it writes beside it, which the card doesn't need.
    rm -f "$rom.sym"
}

cp "$here/fourtette.rom" "$card/"
cp "$here/fourtette.LICENSE.txt" "$card/licences/fourtette.txt"
asm donsol src/donsol.tal donsol.rom
cp "$work/src/donsol/LICENSE" "$card/licences/donsol.txt"
asm catclock src/catclock.tal catclock.rom
cp "$work/src/catclock/LICENSE" "$card/licences/catclock.txt"
cp "$work/src/uxn/LICENSE" "$card/licences/uxn.txt"

# uxn's examples, by directory. Those that need a mouse are left out.
examples() {
    local dir=$1
    shift
    for name in "$@"; do
        asm uxn "projects/$dir/$name.tal" "${dir#examples/}/$name.rom"
    done
}
examples examples/demos amiga bunnymark circles cube3d drool dvd hilbert life \
    mandelbrot move snake
examples examples/devices audio audio.channels audio-tests console \
    console.read console.write controller datetime file file.cat screen \
    screen.auto screen.blending screen.pixel system.expansion
examples examples/exercises ack brainfuck fib fizzbuzz primes quine \
    sierpinski subleq tak
examples examples/gui label monospace picture shapes terminal timer
examples software clock piano
# file.cat prints the file called cat.tal: its own source.
cp "$work/src/uxn/projects/examples/devices/file.cat.tal" "$card/cat.tal"

if [[ -z ${MKDISK:-} ]]; then
    (cd "$root" && env -u DEFMT_LOG cargo build --release -p mkdisk)
    MKDISK=$root/target/release/mkdisk
fi
add=()
for f in "$card"/*; do
    add+=(--add "$f")
done
# FAT16 and small, as the ROMs come to about 130 KiB: the image is quick to
# load, in the browser too.
"$MKDISK" "$out" --force --fat 16 --size-mb 8 "${add[@]}"
