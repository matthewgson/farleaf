# Development

farleaf is a few Rust files, two dependencies (libc, flate2) and MuPDF,
compiled in:

| File | What |
|---|---|
| `src/main.rs` | the app: probe, event loop, keys, search, frames |
| `src/layout.rs` | continuous page layout, fit modes, strips and tiles |
| `src/worker.rs` | the render worker, the render and encode pool, the writer thread |
| `src/encode.rs` | PNG, zlib and shared memory, Kitty commands |
| `src/term.rs` | raw mode, input and reply parsing, signals |
| `src/sidebar.rs` | the outline and thumbnail sidebar |
| `src/mupdf.rs`, `src/mupdf.c` | the engine: Rust side, and a C shim around the MuPDF calls used |
| `build.rs` | builds MuPDF (from `vendor/`) with its makefile, and the shim |
| `src/bench.rs` | `farleaf --bench`: where the time goes |

```sh
scripts/fetch-mupdf.sh       # MuPDF 1.28.5's source into vendor/, checksummed
cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings
```

build.rs compiles only what a PDF viewer needs (no other formats, no
JavaScript, the base 14 fonts and one CJK fallback) into static libraries in
cargo's build directory; the first build takes about a minute, later ones only
relink. It needs `make` and a C/C++ compiler. `cargo zigbuild --target
x86_64-unknown-linux-gnu.2.28` cross-compiles for Linux from a Mac.

The test and demo tools are a second crate in `dev/` (also Rust, with a font
rasterizer and a GIF encoder as its own dependencies; farleaf itself does not
use them). `cargo dev` runs them, in release mode.

## Measuring

- `farleaf --bench paper.pdf` times each stage on the first pages, as farleaf
  does them for half a 5K display (`FARLEAF_BENCH_GEOM=COLSxROWSxCWxCH` for
  another window): interpreting the page, rendering it strip by strip, in
  parallel and whole (each with MuPDF's caches emptied first, and checking
  that the parallel strips come out the same), and encoding each strip for PNG
  and zlib. `FARLEAF_BENCH_THREADS` sets the threads, `FARLEAF_BENCH_LEVELS=1`
  compares zlib levels, `FARLEAF_BENCH_OUTLINE=1` prints the outline, and
  `FARLEAF_BENCH_PNG=out.png` saves page 2.
- `FARLEAF_LOG=/tmp/fl.log farleaf paper.pdf` writes a timed trace: what was
  rendered and encoded and how long each took, each frame's placements, and
  how long after a key the screen was complete.
- `cargo dev sample-pdf out.pdf [PAGES]` writes a small test PDF with an outline.

## A terminal without a window

`cargo dev term` is a fake Kitty-graphics terminal: it runs farleaf (or
herdr) in a pty, answers its queries, types scripted keys, and saves what the
screen would show (images and text) as PNGs. `--mbps`, `--rtt` and
`--compress` simulate an SSH link; `--shm` a terminal on the same machine.

```sh
cargo dev term --mbps 9 --keys "2:snap:open 2.5:l 3.5:snap:next 4:q" \
    -- target/release/farleaf paper.pdf
```

## The demo GIF

`cargo dev demo` records `docs/demo.gif`: it runs farleaf on
`docs/demo/demo.pdf` (built from `demo.tex` with pdflatex) in the fake
terminal, checks every screen against what farleaf drew, and saves one frame
per key, with the key in a badge.

## Stress test

`cargo dev stress` walks through a reading session (page turns, scrolling,
jumps, zoom and pan, fit modes, inversion, search, the key list, the sidebar,
window resizes, key-repeat bursts, a reload while the file changes) in windows
from 40x12 cells to a whole 5K display, in Ghostty locally and over SSH, and
in herdr. After every step it checks that the screen shows exactly what
farleaf drew, image for image and pixel for pixel.

```sh
cargo dev stress --quick                         # a few cases, a generated PDF
cargo dev stress --pdf paper.pdf --word method   # all cases (~40 min), your PDF
cargo dev stress --only ssh --farleaf old/farleaf  # some cases, another build
```

Results go to `dev/out/stress/summary.md`. The herdr cases need herdr installed;
they run in throwaway sessions named `flstress-*` and remove them after.

## Releases

Push a tag such as `v0.4.1` (after bumping the version in `Cargo.toml`).
`.github/workflows/release.yml` builds for macOS (arm64, x86_64) and Linux
(x86_64, arm64, glibc 2.28 and later), each with MuPDF compiled in and the
licenses of what is inside, and publishes the archives, `SHA256SUMS` (which
`install.sh` checks) and MuPDF's source tarball.
