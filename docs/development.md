# Development

farleaf is a few Rust files and three dependencies (libc, libloading, flate2):

| File | What |
|---|---|
| `src/main.rs` | the app: probe, event loop, keys, search, frames |
| `src/layout.rs` | continuous page layout, fit modes, strips and tiles |
| `src/worker.rs` | the render worker and the writer thread |
| `src/encode.rs` | PNG, zlib and shared memory, Kitty commands |
| `src/term.rs` | raw mode, input and reply parsing, signals |
| `src/pdfium.rs` | runtime binding to the ~20 pdfium functions used |

```sh
scripts/fetch-pdfium.sh
cargo test && cargo clippy --all-targets -- -D warnings
```

## Debugging

- `FARLEAF_LOG=/tmp/fl.log farleaf paper.pdf` writes a timed trace: what was
  rendered and sent, and each frame's placements.
- `farleaf --bench paper.pdf` times rendering and encoding of the first pages.
- `dev/sample_pdf.py out.pdf [PAGES]` writes a small test PDF.

## A terminal without a window

`dev/fake_kitty.py` is a fake Kitty-graphics terminal: it runs farleaf (or herdr)
in a pty, answers its queries, types scripted keys, and saves what the screen
would show as PNGs. `--mbps`, `--rtt` and `--compress` simulate an SSH link.

```sh
python3 dev/fake_kitty.py --mbps 9 --keys "2:snap:open 2.5:l 3.5:snap:next 4:q" \
    -- target/release/farleaf paper.pdf
```

## Stress test

`dev/stress.py` walks through a reading session (page turns, scrolling, jumps,
zoom and pan, fit modes, inversion, search, the key list, window resizes,
key-repeat bursts, a reload while the file changes) in windows from 40x12 cells
to a whole 5K display, in Ghostty locally and over SSH, and in herdr. After every
step it checks that the screen shows exactly what farleaf drew, image for image
and pixel for pixel.

```sh
dev/stress.py --quick                         # a few cases, a generated PDF
dev/stress.py --pdf paper.pdf --word method   # all cases (~40 min), your PDF
```

Results go to `dev/out/stress/summary.md`. The herdr cases need herdr installed;
they run in throwaway sessions named `flstress-*` and remove them after.

## Releases

Push a tag such as `v0.2.0` (after bumping the version in `Cargo.toml`).
`.github/workflows/release.yml` builds for macOS (arm64, x86_64) and Linux
(x86_64, arm64, glibc 2.17 and later), bundles each with pdfium, and publishes
the archives and `SHA256SUMS` that `install.sh` downloads.
