#!/usr/bin/env bash
# Download the prebuilt pdfium library farleaf renders with, and check it.
#
#   scripts/fetch-pdfium.sh [mac-arm64|mac-x64|linux-x64|linux-arm64] [DEST_DIR]
#
# Defaults: this machine's platform, and vendor/pdfium-<platform>/ in the repo.
# farleaf finds the library at runtime (see src/pdfium.rs for the search
# order); install it with `cp DEST_DIR/lib/libpdfium.* ~/.local/lib/farleaf/`.
#
# Pinned to one bblanchon/pdfium-binaries release, with its sha256. To move to a
# newer one, change TAG and the sums (gh release view <tag> -R
# bblanchon/pdfium-binaries --json assets lists each asset's digest).
set -euo pipefail

TAG="chromium/8066"
sha_for() {
    case "$1" in
        mac-arm64) echo 336219e80580b93c6523f44db7dc1de59cc497b13a7390ddac84223f68ca162b ;;
        mac-x64) echo 841ecac278cdd46288dd065873522cf72f3996560d8978f473d336f01d59942c ;;
        linux-x64) echo 0b43f405477cf2cfc4dbff06905093c3309756c6bca1fb9da99234a2ca97fed2 ;;
        linux-arm64) echo 0e6f90dccbc6b81fd5d7106abaf164c4222178f024c204d00d526b60fd2ad535 ;;
        *) echo "unknown platform: $1 (want mac-arm64, mac-x64, linux-x64 or linux-arm64)" >&2; exit 2 ;;
    esac
}

if [[ $# -ge 1 ]]; then
    platform=$1
else
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64) platform=mac-arm64 ;;
        Darwin-x86_64) platform=mac-x64 ;;
        Linux-x86_64) platform=linux-x64 ;;
        Linux-aarch64 | Linux-arm64) platform=linux-arm64 ;;
        *) echo "no pinned pdfium for $(uname -s)-$(uname -m)" >&2; exit 2 ;;
    esac
fi
want=$(sha_for "$platform")
repo=$(cd "$(dirname "$0")/.." && pwd)
dest=${2:-"$repo/vendor/pdfium-$platform"}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/pdfium.tgz" \
    "https://github.com/bblanchon/pdfium-binaries/releases/download/$TAG/pdfium-$platform.tgz"
got=$(shasum -a 256 "$tmp/pdfium.tgz" 2>/dev/null || sha256sum "$tmp/pdfium.tgz")
got=${got%% *}
if [[ "$got" != "$want" ]]; then
    echo "checksum mismatch for pdfium-$platform.tgz: got $got, want $want" >&2
    exit 1
fi
mkdir -p "$dest"
tar -xzf "$tmp/pdfium.tgz" -C "$dest" lib LICENSE VERSION licenses 2>/dev/null || tar -xzf "$tmp/pdfium.tgz" -C "$dest"
echo "pdfium $TAG ($platform) -> $dest/lib"
