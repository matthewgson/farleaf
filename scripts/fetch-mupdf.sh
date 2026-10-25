#!/usr/bin/env bash
# Download the MuPDF source farleaf is built with, and check it.
#
#   scripts/fetch-mupdf.sh [DEST_DIR]
#
# Puts mupdf-<version>-source/ in vendor/ (or DEST_DIR); build.rs compiles it
# into farleaf. Pinned to one release, with its sha256 from mupdf.com/releases.
# To move to a newer one, change VERSION and SHA256 (and MUPDF_VERSION in
# build.rs).
set -euo pipefail

VERSION="1.28.5"
SHA256="98a5c10cda20c3992cdf76ff6b2a1149c32bd79cc796d3f703230b1185b7e934"

repo=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-"$repo/vendor"}
if [[ -f "$dest/mupdf-$VERSION-source/Makefile" ]]; then
    echo "MuPDF $VERSION already in $dest"
    exit 0
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/mupdf.tgz" \
    "https://casper.mupdf.com/downloads/archive/mupdf-$VERSION-source.tar.gz"
got=$(shasum -a 256 "$tmp/mupdf.tgz" 2>/dev/null || sha256sum "$tmp/mupdf.tgz")
got=${got%% *}
if [[ "$got" != "$SHA256" ]]; then
    echo "checksum mismatch for mupdf-$VERSION-source.tar.gz: got $got, want $SHA256" >&2
    exit 1
fi
mkdir -p "$dest"
tar -xzf "$tmp/mupdf.tgz" -C "$dest"
echo "MuPDF $VERSION -> $dest/mupdf-$VERSION-source"
