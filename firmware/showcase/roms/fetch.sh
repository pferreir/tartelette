#!/usr/bin/env bash
# Downloads the ROMs the varvara demo can be built with (see ../README.md),
# and their licences: fourtette, a block game by nf (Apache-2.0), from its
# v0.1.1 release. Neither is kept in the repository.
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

fetch() {
    local name=$1 url=$2 sha=$3
    local out=$here/$name
    if [[ -f $out ]] && echo "$sha  $out" | sha256sum -c --status; then
        echo "$name: already there"
        return
    fi
    curl -fsSL -o "$out.part" "$url"
    if ! echo "$sha  $out.part" | sha256sum -c --status; then
        echo "$name: checksum mismatch, from $url" >&2
        rm -f "$out.part"
        exit 1
    fi
    mv "$out.part" "$out"
    echo "$name: fetched"
}

fetch fourtette.rom \
    https://github.com/nf/fourtette/releases/download/v0.1.1/tet.rom \
    908455fd908ba42bf0ebd3903a62d28d915fe756f7f92e1d34e6d51e9f5427d0
fetch fourtette.LICENSE.txt \
    https://raw.githubusercontent.com/nf/fourtette/v0.1.1/LICENSE \
    cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30
