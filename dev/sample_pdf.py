#!/usr/bin/env python3
"""Write a small sample PDF (text, a color figure per page) for tests.

  dev/sample_pdf.py OUT.pdf [PAGES]

Every page has the word "farleaf" in it, for the search tests.
"""
import sys

WORDS = ("the reader renders each page in strips and sends them to the terminal as images "
         "while the next page is already being prepared so that turning a page with farleaf "
         "only moves pictures that are there").split()


def page_stream(n, pages):
    out = [f"BT /F1 20 Tf 72 720 Td (Sample page {n} of {pages}) Tj ET"]
    y = 690
    for line in range(30):
        if 8 <= line < 18:
            continue  # room for the figure
        words = [WORDS[(n * 7 + line * 5 + i) % len(WORDS)] for i in range(13)]
        out.append(f"BT /F1 11 Tf 72 {y} Td ({' '.join(words)}) Tj ET")
        y -= 16 if line != 7 else 16 * 11
    # A bar chart, so pages are not all gray.
    for i in range(6):
        r, g, b = (0.2 + 0.1 * i, 0.4, 0.8 - 0.1 * i)
        h = 20 + ((n + i * 3) % 7) * 18
        out.append(f"{r:.2f} {g:.2f} {b:.2f} rg {100 + i * 70} {420} 50 {h} re f")
    out.append("0 0 0 RG 1 w 90 420 m 530 420 l S")
    return "\n".join(out).encode()


def write(path, pages=12):
    objs = [b"<< /Type /Catalog /Pages 2 0 R >>", None,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"]
    kids = []
    for n in range(1, pages + 1):
        content = page_stream(n, pages)
        objs.append(b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream")
        objs.append(b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
                    b"/Resources << /Font << /F1 3 0 R >> >> /Contents %d 0 R >>" % (len(objs)))
        kids.append(len(objs))
    objs[1] = b"<< /Type /Pages /Kids [%s] /Count %d >>" % (
        b" ".join(b"%d 0 R" % k for k in kids), pages)
    data = bytearray(b"%PDF-1.4\n")
    offsets = []
    for i, o in enumerate(objs, 1):
        offsets.append(len(data))
        data += b"%d 0 obj\n" % i + o + b"\nendobj\n"
    xref = len(data)
    data += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1)
    data += b"".join(b"%010d 00000 n \n" % o for o in offsets)
    data += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, xref)
    with open(path, "wb") as f:
        f.write(data)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    write(sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 12)
