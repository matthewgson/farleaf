#!/usr/bin/env python3
"""Write a small sample PDF (text, a color figure per page) for tests.

  dev/sample_pdf.py OUT.pdf [PAGES]

Every page has the word "farleaf" in it, for the search tests. It has an
outline: a section every four pages with two subsections, some pointing at
explicit destinations and some through GoTo actions, as LaTeX writes them.
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


def outline(objs, kids, pages):
    """Appends outline objects for `pages` pages; returns the root's number."""
    def dest(page, y):
        return b"[%d 0 R /XYZ 0 %d null]" % (kids[page], y)
    root = len(objs) + 1
    objs.append(None)
    sections = []
    for i, page in enumerate(range(0, pages, 4)):
        sec = len(objs) + 1
        objs.append(None)
        subs = []
        for j, (p, y) in enumerate([(page, 600), (min(page + 2, pages - 1), 400)]):
            subs.append(len(objs) + 1)
            # The first subsection by a GoTo action, the second by /Dest.
            target = b"/A << /S /GoTo /D %s >>" % dest(p, y) if j == 0 else b"/Dest %s" % dest(p, y)
            objs.append((b"(%d.%d Part %s)" % (i + 1, j + 1, b"ab"[j:j + 1]), sec, target))
        objs[sec - 1] = (b"(%d Section on page %d)" % (i + 1, page + 1), root, b"/Dest %s" % dest(page, 740), subs)
        sections.append(sec)

    def link(items):
        for n, num in enumerate(items):
            o = objs[num - 1]
            title, parent, target = o[0], o[1], o[2]
            d = b"<< /Title %s /Parent %d 0 R %s" % (title, parent, target)
            if n > 0:
                d += b" /Prev %d 0 R" % items[n - 1]
            if n + 1 < len(items):
                d += b" /Next %d 0 R" % items[n + 1]
            if len(o) > 3:
                link(o[3])
                d += b" /First %d 0 R /Last %d 0 R /Count %d" % (o[3][0], o[3][-1], len(o[3]))
            objs[num - 1] = d + b" >>"
    link(sections)
    objs[root - 1] = b"<< /Type /Outlines /First %d 0 R /Last %d 0 R /Count %d >>" % (
        sections[0], sections[-1], len(sections))
    return root


def write(path, pages=12):
    objs = [None, None, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"]
    kids = []
    for n in range(1, pages + 1):
        content = page_stream(n, pages)
        objs.append(b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream")
        objs.append(b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
                    b"/Resources << /Font << /F1 3 0 R >> >> /Contents %d 0 R >>" % (len(objs)))
        kids.append(len(objs))
    objs[1] = b"<< /Type /Pages /Kids [%s] /Count %d >>" % (
        b" ".join(b"%d 0 R" % k for k in kids), pages)
    objs[0] = b"<< /Type /Catalog /Pages 2 0 R /Outlines %d 0 R /PageMode /UseOutlines >>" % outline(
        objs, kids, pages)
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
