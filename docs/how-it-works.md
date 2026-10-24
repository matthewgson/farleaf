# How farleaf works

farleaf renders pages with pdfium on a worker thread and sends them to the
terminal as Kitty graphics images. Everything else follows from what terminals
can hold and how fast the link is.

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
(pages 2489 px wide, 24 MB of pixels each), per page, on one core of an M2 Max:

| stage | ms | |
|---|---|---|
| load the page | 1 | pdfium parses it once, then the worker keeps it |
| render it, strip by strip | 7.8 | straight into RGB order: no reordering pass (it was 9.3) |
| PNG (over SSH) | 17 | 0.48 MB; on 8 encoder threads, while the next strips render |
| zlib (herdr) | 15 | level 2 (level 3 took 18 for 3% fewer bytes) |

- For a terminal on the same machine, strips are rendered straight into the
  shared memory the terminal reads: nothing is encoded or copied.
- PNG and zlib take twice as long as rendering, so a pool of encoder threads
  compresses strips in parallel while the worker renders the next ones.
- pdfium redoes part of the work for an image it scales up on every call, so a
  scanned page on a whole 5K display took 410 ms strip by strip against 100 ms
  in one call. farleaf times the first two calls on each page and, when two
  strips at once cost much less per pixel than one, renders the rest of the
  page in bands of several strips. Text pages cost the same either way and
  keep going strip by strip, which gets the first strips out sooner.
- The outline is read after the first screen is drawn.
- Over SSH the link, not the computer, sets the pace: a page is 0.3-1.3 MB of
  PNG, and a 9 MB/s link needs 30-140 ms for it. Row filters (Sub, Up, Paeth,
  adaptive) made rendered pages 5-20% larger, and palettes fit almost no
  colored strip (antialiased colored text has more than 256 colors).

## The PDF engine

pdfium (Chrome's) renders the paper above at 7 ms a page and the scanned one at
53 ms. Poppler's `pdftoppm`, at the same size, took about 75 ms and 370 ms (with
writing its files). MuPDF is often faster than pdfium, but it is AGPL, which
would make farleaf AGPL too, and it has no prebuilt shared library to load at
run time, so building farleaf would need a C toolchain. With rendering a few
milliseconds of a page turn and the rest prefetched, pdfium is not the
bottleneck.

## The sidebar

The outline comes from the PDF's bookmarks (pdfium resolves each one to a page
and a height on it), so it is there for any LaTeX paper built with hyperref.
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
| Ghostty, same machine: half / whole 5K display | 0.00 / 0.00 | 0.02 / 0.03 | 0.00 / 0.01 |
| Ghostty over SSH: half a MacBook / whole 5K display | 0.05 / 0.15 | 0.11 / 0.13 | 0.05 |
| herdr, same machine, half 5K display | 0.06 | 0.08 | 0.01 |
| herdr over SSH, half a MacBook | 0.24 | 0.27 | 0.06 |

A next page is usually prefetched, so it only moves images. In herdr over SSH
the time grows with the pane's pixels because herdr re-sends each new image
raw; farleaf's own part is a few tens of milliseconds. For a scanned paper on a
whole 5K display, a jump takes 0.09 s and a resize 0.06 s (0.17 and 0.12 before
the bands above).

## Lessons learned

- zlib-rs level 1 writes fixed-Huffman blocks that crash Ghostty 1.3.1 when such
  an image is placed again: never use level 1 for `o=z`.
- herdr answers the shared memory probe with OK, then shows nothing sent that
  way, so it never gets shared memory.
- herdr stores PNG as RGBA and re-sends it at 4 bytes a pixel, so it gets RGB.
- macOS `poll()` does not work on `/dev/tty`; farleaf uses `select()`.
