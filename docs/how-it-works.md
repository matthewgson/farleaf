# How farleaf works

farleaf renders pages with MuPDF, compiled in, and sends them to the terminal
as Kitty graphics images. Everything else follows from what terminals can hold
and how fast the link is.

## Pages go out in pieces

A page is cut into strips of at most 4 MB and an eighth of the screen tall, and,
when zoomed wider than the screen, tiles half a screen wide. No piece is over
8192 px on a side. Ghostty and herdr refuse images over 10000 px, and herdr
silently drops any over 30 MiB, which a whole page at a retina font size
exceeds. Pieces also let a scroll or a pan *move* images instead of sending them
again.

## The transport is asked, not guessed

One round trip at startup asks the terminal whether it can read farleaf's shared
memory, and what it is (XTVERSION). herdr's panes answer "libghostty", which also
gets through SSH, where herdr's environment variables do not.

| Where | Transport | Bytes per page on the pty |
|---|---|---|
| Ghostty, same machine | shared memory | a few names |
| Ghostty over SSH | PNG (grayscale when the strip is) | ~0.3-1 MB |
| inside herdr, local or remote | zlib RGB (`o=z`) | ~0.5-1 MB |

`FARLEAF_TRANSPORT=shm|png|zlib` overrides the choice.

## It stays within the terminal's image memory

Ghostty keeps up to 320 MB of images per screen, a herdr pane only 64 MiB; past
that they evict images, which would leave blank strips. farleaf keeps under
192 MiB (56 MiB in herdr): prefetching stops at that budget, images nobody needs
are freed before a new one is sent, and if one screen alone would not fit (a
zoomed full-screen pane on a 5K display, in herdr) it renders slightly less
sharply instead. Every command asks the terminal to report errors, so an image
it refused or lost is sent again rather than left blank.

## Rendering runs ahead, output never blocks input

The worker renders the visible strips first, then the rest of the page, the next
two pages and the previous one. A writer thread owns the terminal: small frames
(placements, the status line) jump ahead of queued uploads, and uploads no longer
needed are dropped before they are sent. The key list popup is drawn as text in
a hole cut out of the page images, so opening it sends no images.

## Where the time goes

`farleaf --bench paper.pdf` times each stage. For a paper on half a 5K display
(pages 2489 px wide, 24 MB of pixels each), per page, on an M2 Max:

| stage | ms | |
|---|---|---|
| interpret the page | 1.3 | once, into a display list every strip renders from |
| render it, strip by strip | 3.8 (6.3 cold) | straight into the terminal's shared memory, locally |
| PNG (over SSH) | 15 | 0.33 MB; on the pool, while the next strips render |
| zlib (herdr) | 12 | level 2 (level 3 took a fifth longer for 3% fewer bytes) |

- Each page is interpreted once into a MuPDF display list, which any number of
  threads can render at once.
- Text renders fastest on one thread: each strip takes a fraction of a
  millisecond, and MuPDF's locks (its allocator, its glyph cache) make threads
  wait on each other, so 8 threads took twice as long as one. The worker
  renders text strips itself and hands PNG and zlib, which take longer than
  rendering, to the pool.
- Pages heavy with images (a scan) render on the pool, in parallel. farleaf
  times each page's first strip: text takes about 1 µs per thousand pixels, a
  scan over 10, and anything over 5 counts as heavy.
- MuPDF decodes only the part of an image a draw needs, by default, and caches
  it under that part, so each strip of a scan decoded the scan again. farleaf
  has it decode whole images (up to 64 million pixels) once, for every strip
  to share: a scanned page went from 257 to 107 ms on one thread.
- For a terminal on the same machine, strips are rendered straight into the
  shared memory the terminal reads: nothing is encoded or copied.
- The outline is read after the first screen is drawn.
- Over SSH the link, not the computer, sets the pace: a page is 0.3-1.3 MB of
  PNG, and a 9 MB/s link needs 30-140 ms for it. Row filters (Sub, Up, Paeth,
  adaptive) made rendered pages 5-20% larger, and palettes fit almost no
  colored strip (antialiased colored text has more than 256 colors).

## The PDF engine

MuPDF (since version 0.4.0) against pdfium, Chrome's engine, which farleaf used
before: milliseconds to render a page, caches empty, strip by strip as farleaf
draws it, on an M2 Max:

| | paper, half 5K | paper, whole 5K | scan, half 5K | scan, whole 5K |
|---|---|---|---|---|
| pdfium | 7.8 | 34 | 66 | 130 |
| MuPDF | **6.3** | **13** | 107 (one thread), **72** (8) | 157 (one thread), **65** (8) |

Poppler's `pdftoppm` took about 75 and 370 ms for the half-5K paper and scan.
The pictures look the same; MuPDF's PNGs of text are about 30% smaller (less
to send over SSH), and of scans about 9% larger. MuPDF is AGPL, so farleaf is
too; it is compiled in, which needs a C/C++ compiler to build farleaf but
leaves nothing else to install.

## The sidebar

The outline comes from the PDF's bookmarks (MuPDF resolves each one to a page
and a place on it), so it is there for any LaTeX paper built with hyperref.
Thumbnails are images like the strips: rendered by the same worker, counted in
the same memory budget, prefetched a few pages ahead, and painted on the
sidebar's background so they blend in. Opening the sidebar narrows the pages;
in fit-page mode they usually keep their size, and then their images are kept
and only moved, as they are on any resize that leaves the pieces the same.

## herdr over SSH

herdr decodes every image it is sent and re-sends it to the outer terminal
uncompressed, 20-40 MB per page depending on the pane size. A viewer cannot
change that, but SSH compression can: rendered text compresses 30-50x. On a
simulated 9 MB/s, 50 ms link with a half-width pane, a page turn took 4.1-4.5 s
without compression and 1.1-1.5 s with it (88.9 MB vs 3.1 MB on the link).

## Measured

`cargo dev stress` checks after every step that the screen shows exactly what
farleaf drew (none failed), and times how long until the screen stopped
changing. Median seconds for a paper ("over SSH" is a simulated 9 MB/s link
with 50 ms round trips and compression, so 0.05 s is its floor):

| | next page | jump | scroll |
|---|---|---|---|
| Ghostty, same machine: half / whole 5K display | 0.00 / 0.01 | 0.02 / 0.02 | 0.00 / 0.01 |
| Ghostty over SSH: half a MacBook / whole 5K display | 0.05 / 0.13 | 0.10 / 0.12 | 0.05 |
| herdr, same machine, half 5K display | 0.09 | 0.07 | 0.01 |
| herdr over SSH, half a MacBook | 0.23 | 0.26 | 0.07 |

A next page is usually prefetched, so it only moves images. In herdr over SSH
the time grows with the pane's pixels because herdr re-sends each new image
raw; farleaf's own part is a few tens of milliseconds. For a scanned paper on a
whole 5K display, a jump takes 0.02-0.03 s and a resize 0.02 s (0.07 each with
pdfium); over SSH, where the scan's slightly larger PNGs count, a jump takes
0.18 s (0.14 with pdfium).

## Lessons learned

- zlib-rs level 1 writes fixed-Huffman blocks that crash Ghostty 1.3.1 when such
  an image is placed again: never use level 1 for `o=z`.
- herdr answers the shared memory probe with OK, then shows nothing sent that
  way, so it never gets shared memory.
- herdr stores PNG as RGBA and re-sends it at 4 bytes a pixel, so it gets RGB.
- macOS `poll()` does not work on `/dev/tty`; farleaf uses `select()`.
