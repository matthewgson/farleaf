# farleaf

A fast, minimal PDF reader for the terminal, with Vim keys. It shows the pages
as real images through the Kitty graphics protocol, and it stays fast over SSH
and inside [herdr](https://herdr.dev).

![farleaf: page thumbnails, the outline, search and the key list](docs/demo.gif)

- Renders with [MuPDF](https://mupdf.com), compiled in: one binary, nothing
  else to install. Pages heavy with images (scans) render on several cores.
- Turning a page usually only moves images that are already in the terminal; a
  jump to a new page is on screen in 10-30 ms locally, about 0.1 s over SSH.
- Picks the fastest way to send images by asking the terminal at startup:
  shared memory locally, compressed images over SSH.
- A sidebar with the outline (sections and subsections) or page thumbnails,
  as in macOS Preview: `o` or `t`, then `j` `k` to look around and Enter to go.
- Reloads by itself when the file changes, for example after a LaTeX run.

## Install

macOS (Apple Silicon or Intel) and Linux (x86_64 or arm64):

```sh
curl -fsSL https://raw.githubusercontent.com/matthewgson/farleaf/main/install.sh | sh
```

This puts `farleaf` in `~/.local/bin` (checked against the release's
checksums) and its licenses in `~/.local/share/doc/farleaf`. No root needed.
Set `FARLEAF_PREFIX` to install elsewhere, or `FARLEAF_VERSION=v0.4.0` for a
specific release. Linux needs glibc 2.28 or later. To uninstall:
`rm ~/.local/bin/farleaf && rm -r ~/.local/share/doc/farleaf`.

### From source

Needs Rust, `make` and a C/C++ compiler (MuPDF is C, with a little C++).

```sh
git clone https://github.com/matthewgson/farleaf && cd farleaf
scripts/fetch-mupdf.sh       # MuPDF's source, pinned and checksummed, into vendor/
cargo build --release        # compiles MuPDF too, about a minute the first time
cp target/release/farleaf ~/.local/bin/
```

## Use

```sh
farleaf paper.pdf
farleaf -p 12 paper.pdf      # open at page 12
farleaf -p 12:340 paper.pdf  # ... with the spot 340 pt from its top a third of the way down
farleaf docs/demo/demo.pdf   # the paper in the GIF, in a clone of this repo
```

The `12:340` form is for editors: `synctex view` gives a source line's page
and height, so a LaTeX editor's forward search can open farleaf there, or type
`:12:340` and Enter into a farleaf that is already open.

Press `?` for the list of keys. A count works before most keys: `5j`, `3l`, `12G`.

| Keys | Action |
|---|---|
| `j` `k`, `↓` `↑` | scroll |
| `Ctrl-d` `Ctrl-u` | half a screen down / up |
| `Space` `Ctrl-f` `PgDn`, `Ctrl-b` `PgUp` | a screen down / up |
| `l` `h`, `J` `K` | next / previous page |
| `gg`, `G`, `12G` | first page, end, page 12 |
| `z`, `Z` | fit page, fit width |
| `+` `-` | zoom in / out |
| `H` `L`, `←` `→` | pan when zoomed wider than the screen |
| `i` | invert colors |
| `/`, `n` `N` | search (smartcase), next / previous match |
| `Esc` | clear search highlights |
| `m{a-z}`, `'{a-z}` | set a mark, go to a mark |
| `''`, `Ctrl-o`, `Tab` | jump back, back in the jump list, forward |
| `o` | the outline (contents) in a sidebar; again to hide it |
| `t` | page thumbnails in a sidebar; again to hide them |
| in the sidebar: `j` `k`, `Enter`, `Esc` | move (the page follows), stay there, go back to where you were |
| `:12` `:12:340` `:q` `:noh` `:e` | go to page 12 (at 340 pt from its top), quit, clear highlights, reload |
| `r` | reload |
| `?`, `:help` | the key list |
| `q`, `Ctrl-c` | quit |

## Terminals

| Terminal | Status |
|---|---|
| [Ghostty](https://ghostty.org) | tested, locally and over SSH |
| [herdr](https://herdr.dev) | tested, locally and over SSH; detected automatically |
| other terminals with the Kitty graphics protocol (kitty, ...) | should work, untested |
| tmux | not supported |

Over SSH, run farleaf on the remote machine. If you read PDFs inside herdr over
SSH, turn on SSH compression for that host; it makes page turns several times
faster:

```
# ~/.ssh/config
Host my-server
    Compression yes
```

## More

- [How it works](docs/how-it-works.md): why it is fast, and measurements.
- [Development](docs/development.md): tests, the fake terminal, making a release.

## License

farleaf is © 2026 Matthew G. Son, under the [GNU Affero General Public
License](LICENSE), version 3 or later (AGPL-3.0-or-later). It is built with
MuPDF, © Artifex Software, which is under the AGPL too; the libraries inside
MuPDF have their own licenses, in `licenses/` in each release archive. Each
release also carries MuPDF's exact source. Releases before 0.4.0, which
rendered with pdfium, were MIT.
