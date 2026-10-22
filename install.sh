#!/bin/sh
# Install farleaf and the pdfium library it renders with, from GitHub releases.
#
#   curl -fsSL https://raw.githubusercontent.com/matthewgson/farleaf/main/install.sh | sh
#
# Puts farleaf in $PREFIX/bin and pdfium in $PREFIX/lib/farleaf (PREFIX is
# ~/.local unless FARLEAF_PREFIX says otherwise). FARLEAF_VERSION=v0.1.0 picks
# a release; the default is the latest. No root needed. To uninstall:
#   rm ~/.local/bin/farleaf && rm -r ~/.local/lib/farleaf
set -eu

repo=matthewgson/farleaf
prefix=${FARLEAF_PREFIX:-$HOME/.local}
version=${FARLEAF_VERSION:-latest}

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) platform=mac-arm64 ;;
    Darwin-x86_64) platform=mac-x64 ;;
    Linux-x86_64) platform=linux-x64 ;;
    Linux-aarch64 | Linux-arm64) platform=linux-arm64 ;;
    *)
        echo "farleaf: no prebuilt release for $(uname -s) $(uname -m); build it from source:" >&2
        echo "  https://github.com/$repo#from-source" >&2
        exit 1
        ;;
esac

if [ "$version" = latest ]; then
    base=https://github.com/$repo/releases/latest/download
else
    base=https://github.com/$repo/releases/download/$version
fi
name=farleaf-$platform.tar.gz

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "downloading $name ($version)"
curl -fsSL -o "$tmp/$name" "$base/$name"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"

want=$(grep " $name\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then
    got=$(sha256sum "$tmp/$name" | cut -d' ' -f1)
else
    got=$(shasum -a 256 "$tmp/$name" | cut -d' ' -f1)
fi
if [ -z "$want" ] || [ "$got" != "$want" ]; then
    echo "farleaf: checksum mismatch for $name (got $got, want ${want:-nothing})" >&2
    exit 1
fi

tar -xzf "$tmp/$name" -C "$tmp"
src=$tmp/farleaf-$platform
mkdir -p "$prefix/bin" "$prefix/lib/farleaf"
cp -R "$src/pdfium-licenses" "$prefix/lib/farleaf/"
# Each through a temporary name, so a farleaf that is running keeps its copy.
put() {
    cp "$1" "$2/.$(basename "$1").new"
    mv -f "$2/.$(basename "$1").new" "$2/$(basename "$1")"
}
for lib in "$src"/libpdfium.*; do put "$lib" "$prefix/lib/farleaf"; done
put "$src/farleaf" "$prefix/bin"

echo "installed $("$prefix/bin/farleaf" --version) in $prefix/bin"
case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "note: $prefix/bin is not on your PATH; add it, e.g. in ~/.zshrc or ~/.bashrc:
  export PATH=\"$prefix/bin:\$PATH\"" ;;
esac
