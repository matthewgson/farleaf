#!/usr/bin/env python3
"""Records the demo GIF: farleaf in dev/fake_kitty.py, one frame per step.

  dev/demo.py [--pdf docs/demo/demo.pdf] [--out docs/demo.gif] [--farleaf BIN]

Each frame is what farleaf sent, as the fake terminal draws it: the images,
and the text in its colors. A badge shows the key just pressed. Needs Pillow
(pip install pillow) and a monospace font (FAKE_KITTY_FONT, else JetBrains
Mono or Menlo).
"""
import argparse, os, shutil, sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
sys.path.insert(0, HERE)
import fake_kitty  # noqa: E402

try:
    from PIL import Image, ImageDraw
except ImportError:
    sys.exit("dev/demo.py needs Pillow: pip install pillow")

# (keys, badge, seconds on screen)
SCRIPT = [
    (None, "farleaf demo.pdf", 2.2),
    ("j", "j  scroll", 0.6),
    ("j", "j  scroll", 0.6),
    ("l", "l  next page", 1.4),
    ("t", "t  page thumbnails", 1.6),
    ("j", "j", 0.9),
    ("j", "j", 0.9),
    ("j", "j", 1.0),
    ("\r", "Enter  go there", 1.5),
    ("o", "o  outline", 1.6),
    ("k", "k", 0.8),
    ("k", "k", 0.8),
    ("k", "k", 0.8),
    ("k", "k", 1.0),
    ("\r", "Enter  go there", 1.5),
    ("o", "o", 0.6),
    ("o", "o  hide", 1.3),
    ("/pieces", "/  search", 1.0),
    ("\r", "Enter", 1.6),
    ("n", "n  next match", 1.4),
    ("z", "z  fit page", 1.5),
    ("Z", "Z  fit width", 1.2),
    ("?", "?  every key", 2.6),
    ("?", "", 1.2),
]


def badge(img, text, font):
    if not text:
        return img
    d = ImageDraw.Draw(img)
    pad, x1, y1 = 14, img.width - 24, img.height - 56
    w = d.textlength(text, font=font)
    box = [x1 - w - 2 * pad, y1 - font.size - 2 * pad, x1, y1]
    d.rounded_rectangle(box, radius=10, fill=(20, 24, 30), outline=(135, 175, 215), width=2)
    d.text((box[0] + pad, box[1] + pad + font.size // 2), text, font=font, fill=(235, 235, 235), anchor="lm")
    return img


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pdf", default=os.path.join(REPO, "docs/demo/demo.pdf"))
    ap.add_argument("--out", default=os.path.join(REPO, "docs/demo.gif"))
    ap.add_argument("--farleaf", default=os.path.join(REPO, "target/release/farleaf"))
    ap.add_argument("--cols", type=int, default=128)
    ap.add_argument("--rows", type=int, default=36)
    ap.add_argument("--cell", default="10x22")
    a = ap.parse_args()
    cw, ch = map(int, a.cell.split("x"))
    # The frames and the log stay in dev/out/demo, for a look afterwards.
    tmp = os.path.join(REPO, "dev/out/demo")
    os.makedirs(tmp, exist_ok=True)
    doc = os.path.join(tmp, os.path.basename(a.pdf))
    shutil.copyfile(a.pdf, doc)
    flog = os.path.join(tmp, "farleaf.log")
    open(flog, "w").close()
    steps = [("wait", "open")]
    for i, (keys, _, _) in enumerate(SCRIPT):
        if keys is not None:
            steps.append(("keys", fake_kitty.keybytes(keys)))
        steps.append(("snap", f"{i:02d}"))
    env = {"TERM": "xterm-ghostty", "TERM_PROGRAM": "ghostty", "FARLEAF_LOG": flog}
    for k in ("HERDR_ENV", "HERDR_PANE_ID", "HERDR_TAB_ID", "HERDR_WORKSPACE_ID", "HERDR_SOCKET_PATH"):
        os.environ.pop(k, None)
    res = fake_kitty.run([a.farleaf, doc], cols=a.cols, rows=a.rows, cell=(cw, ch), shm=True, steps=steps,
                         timeout=120, env=env, out=tmp, flog=flog, verbose=False)
    bad = [c for c in res["checks"] if not c["ok"]]
    if bad:
        sys.exit(f"the screen did not match what farleaf drew: {bad[0]}")
    font = fake_kitty._font(20, True)
    frames, durations = [], []
    for i, (_, label, secs) in enumerate(SCRIPT):
        img = Image.open(os.path.join(tmp, f"{i:02d}.png")).convert("RGB")
        frames.append(badge(img, label, font).quantize(colors=256, dither=Image.Dither.NONE))
        durations.append(int(secs * 1000))
    frames[0].save(a.out, save_all=True, append_images=frames[1:], duration=durations, loop=0, optimize=True)
    print(f"wrote {a.out}: {len(frames)} frames, {os.path.getsize(a.out) / 1e6:.1f} MB, "
          f"{sum(durations) / 1000:.0f} s; {len(res['checks'])} screens checked")


if __name__ == "__main__":
    main()
