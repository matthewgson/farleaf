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

## herdr over SSH

herdr decodes every image it is sent and re-sends it to the outer terminal
uncompressed, 20-40 MB per page depending on the pane size. A viewer cannot
change that, but SSH compression can: rendered text compresses 30-50x. On a
simulated 9 MB/s, 50 ms link with a half-width pane, a page turn took 4.1-4.5 s
without compression and 1.1-1.5 s with it (88.9 MB vs 3.1 MB on the link).

## Measured

`dev/stress.py`, 31 window sizes and setups, 1907 checks that the screen showed
exactly what farleaf drew, none failed. Median seconds until the screen was
complete ("over SSH" is a simulated 9 MB/s, 50 ms link with compression):

| | next page | jump | scroll |
|---|---|---|---|
| Ghostty, same machine, any size | 0.02-0.10 | 0.03-0.13 | 0.01 |
| Ghostty over SSH | 0.08-0.34 | 0.09-0.74 | 0.05 |
| herdr, same machine | 0.02-0.12 | 0.04-0.13 | 0.01-0.04 |
| herdr over SSH: 1710x2226 / 2850x2352 / 5700x2352 px pane | 0.30 / 0.76 / 2.0 | 0.36 / 0.72 / 2.0 | 0.06-0.29 |

In herdr over SSH the time grows with the pane's pixels because herdr re-sends
each new image raw; farleaf's own part is about 35 ms.

## Lessons learned

- zlib-rs level 1 writes fixed-Huffman blocks that crash Ghostty 1.3.1 when such
  an image is placed again: never use level 1 for `o=z`.
- herdr answers the shared memory probe with OK, then shows nothing sent that
  way, so it never gets shared memory.
- herdr stores PNG as RGBA and re-sends it at 4 bytes a pixel, so it gets RGB.
- macOS `poll()` does not work on `/dev/tty`; farleaf uses `select()`.
