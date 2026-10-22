#!/usr/bin/env python3
"""A fake Kitty-graphics terminal for testing farleaf without a real window.

Runs a command in a pty sized like a real terminal, answers the queries
farleaf (or herdr) sends, keeps the images and placements it is sent, types
scripted keys, and saves what the screen would show as PNGs.

  dev/fake_kitty.py [--cols 134 --rows 50 --cell 19x42] [--shm] [--out DIR]
                    [--keys "1.0:j 2.0:snap:after-j 3.0:q"] [--flog LOG] -- CMD...

--keys is a list of TIME:KEYS (seconds since start; KEYS may use \\e, \\r, \\s (space),
^X for Ctrl+X), TIME:snap:NAME (save DIR/NAME.png) and TIME:resize:COLSxROWS. --shm makes the
terminal read POSIX shared memory (t=s), like a local Ghostty.

It enforces what Ghostty enforces, so a test fails where Ghostty would show
nothing: images over 10000 px on a side are refused, and stored images are
evicted beyond 320 MB (unplaced ones first, oldest first). Errors are
answered unless the command said q=2.

With --flog (the FARLEAF_LOG of the farleaf it runs, directly or inside
herdr), every time the screen settles after a key it checks that what is on
screen is exactly what farleaf last drew: the same images, pixel for pixel
(CRC-32), in the same cells, cut the same way.

dev/stress.py imports this file and drives run() directly.
"""
import argparse, base64, fcntl, json, os, pty, re, select, signal, struct, sys, termios, time, zlib


def png_info(data):
    """(width, height) from a PNG's IHDR, or None."""
    if data[:8] != b"\x89PNG\r\n\x1a\n" or data[12:16] != b"IHDR":
        return None
    return struct.unpack(">II", data[16:24])


def png_decode(data):
    """Packed RGB from the PNGs farleaf writes (8-bit gray, RGB or RGBA, filters none/sub/up)."""
    assert data[:8] == b"\x89PNG\r\n\x1a\n"
    i, idat, w = 8, b"", 0
    while i < len(data):
        n = struct.unpack(">I", data[i:i + 4])[0]
        kind, body = data[i + 4:i + 8], data[i + 8:i + 8 + n]
        if kind == b"IHDR":
            w, h, depth, ctype = struct.unpack(">IIBB", body[:10])
            assert depth == 8 and ctype in (0, 2, 6), (depth, ctype)
            bpp = {0: 1, 2: 3, 6: 4}[ctype]
        elif kind == b"IDAT":
            idat += body
        i += 12 + n
    raw, stride = zlib.decompress(idat), w * bpp
    px, prev = bytearray(), bytearray(stride)
    for y in range(h):
        f, row = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        if f == 1:
            for x in range(bpp, stride): row[x] = (row[x] + row[x - bpp]) & 255
        elif f == 2:
            for x in range(stride): row[x] = (row[x] + prev[x]) & 255
        elif f != 0:
            raise ValueError(f"filter {f} not implemented")
        prev = row
        px += row
    return w, h, to_rgb(bytes(px), bpp)


def to_rgb(px, bpp):
    if bpp == 3:
        return px
    n = len(px) // bpp
    out = bytearray(n * 3)
    for c in range(3):
        out[c::3] = px[0::bpp] if bpp == 1 else px[c::bpp]
    return bytes(out)


def png_encode(w, h, rgb):
    raw = b"".join(b"\0" + rgb[y * w * 3:(y + 1) * w * 3] for y in range(h))
    def chunk(k, d):
        return struct.pack(">I", len(d)) + k + d + struct.pack(">I", zlib.crc32(k + d) & 0xFFFFFFFF)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + \
        chunk(b"IDAT", zlib.compress(raw, 1)) + chunk(b"IEND", b"")


class Image:
    __slots__ = ("kv", "raw", "w", "h", "mem", "gen", "rgb", "crc")

    def __init__(self, kv, raw, w, h, mem, gen):
        self.kv, self.raw, self.w, self.h, self.mem, self.gen = kv, raw, w, h, mem, gen
        self.rgb = self.crc = None

    def decoded(self):
        if self.rgb is None:
            raw = zlib.decompress(self.raw) if self.kv.get("o") == "z" else self.raw
            f = self.kv.get("f", "32")
            if f == "100":
                _, _, rgb = png_decode(raw)
            else:
                rgb = to_rgb(raw, 3 if f == "24" else 4)
            assert len(rgb) == self.w * self.h * 3, (len(rgb), self.w, self.h)
            self.rgb, self.raw = rgb, None
        return self.rgb

    def checksum(self):
        if self.crc is None:
            self.crc = zlib.crc32(self.decoded()) & 0xFFFFFFFF
        return self.crc


class Term:
    def __init__(self, cols, rows, cw, ch, shm=False, storage_limit=320_000_000, max_dim=10000,
                 xtversion="ghostty 1.3.1"):
        self.cols, self.rows, self.cw, self.ch = cols, rows, cw, ch
        self.shm, self.storage_limit, self.max_dim, self.xtversion = shm, storage_limit, max_dim, xtversion
        self.images, self.places, self.loading = {}, {}, None
        self.cur = (0, 0)
        self.grid = [[" "] * cols for _ in range(rows)]
        self.stats = {"bytes": 0, "uploads": 0, "upload_bytes": 0, "places": 0, "deletes": 0, "frames": 0,
                      "refused": 0, "evicted": 0, "missing_places": 0, "peak_mem": 0}
        self.replies = []
        self.apc = None
        self.gen = 0
        self.mem = 0
        # Bumped whenever what the screen shows may have changed.
        self.version = 0

    def resize(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.grid = [[" "] * cols for _ in range(rows)]

    def reply(self, kv, msg):
        q = kv.get("q", "0")
        if "i" in kv and (q == "0" or (q == "1" and msg != "OK")):
            extra = f",p={kv['p']}" if "p" in kv else ""
            self.replies.append(f"\x1b_Gi={kv['i']}{extra};{msg}\x1b\\".encode())

    def graphics(self, ctl, payload):
        kv = dict(p.split("=", 1) for p in ctl.split(",") if "=" in p)
        if self.loading is not None:
            if set(kv) <= {"m", "q"}:
                self.loading["data"] += payload
                if kv.get("m", "0") == "0":
                    job, self.loading = self.loading, None
                    self.finish(job)
                return
            self.loading = None  # a new command cut the transfer off: it is lost
            self.stats["refused"] += 1
        act = kv.get("a", "t")
        if act == "q":
            t = kv.get("t", "d")
            ok = t == "d" or (t == "s" and self.shm and self.read_shm(kv, payload) is not None) or \
                (t in ("f", "t") and self.read_file(kv, payload) is not None)
            self.reply(kv, "OK" if ok else "ENOTSUP:no")
            return
        if act in ("t", "T"):
            job = {"kv": kv, "data": payload}
            if kv.get("t") in ("s", "f", "t"):
                job["raw"] = (self.read_shm if kv["t"] == "s" else self.read_file)(kv, payload)
                if job["raw"] is None:
                    return self.reply(kv, "EBADF:cannot read the medium")
                return self.finish(job)
            if kv.get("m", "0") == "1":
                self.loading = job
            else:
                self.finish(job)
        elif act == "p":
            self.stats["places"] += 1
            i = int(kv.get("i", 0))
            if i not in self.images:
                self.stats["missing_places"] += 1
                return self.reply(kv, "ENOENT:no such image")
            self.places[(i, int(kv.get("p", 0)))] = (self.cur, kv)
            self.version += 1
        elif act == "d":
            self.stats["deletes"] += 1
            d = kv.get("d", "a")
            if d in ("a", "A"):
                self.places.clear()
                if d == "A":
                    self.drop_images(list(self.images))
            elif d in ("i", "I"):
                i, p = int(kv["i"]), int(kv.get("p", 0))
                for key in [k for k in self.places if k[0] == i and (p == 0 or k[1] == p)]:
                    del self.places[key]
                if d == "I" and p == 0:
                    self.drop_images([i])
            self.version += 1

    def drop_images(self, ids):
        for i in ids:
            img = self.images.pop(i, None)
            if img is not None:
                self.mem -= img.mem

    def read_shm(self, kv, payload):
        from multiprocessing import shared_memory
        name = base64.b64decode(payload).decode()
        try:
            m = shared_memory.SharedMemory(name=name.lstrip("/"), create=False, track=False)
        except (FileNotFoundError, TypeError):
            return None
        data = bytes(m.buf[:int(kv.get("S", m.size))])
        m.close()
        m.unlink()
        return data

    def read_file(self, kv, payload):
        """t=f (a file) and t=t (a temporary file, deleted once read), as a local terminal does."""
        path = base64.b64decode(payload).decode()
        try:
            with open(path, "rb") as f:
                f.seek(int(kv.get("O", 0)))
                data = f.read(int(kv["S"])) if "S" in kv else f.read()
        except OSError:
            return None
        if kv.get("t") == "t" and "tty-graphics-protocol" in path:
            try:
                os.unlink(path)
            except OSError:
                pass
        return data

    def finish(self, job):
        # Keep the bytes; decode only when a check or snapshot needs the
        # pixels, so reading stays as fast as a real terminal's.
        kv, raw = job["kv"], job.get("raw")
        if raw is None:
            raw = base64.b64decode(job["data"])
        self.stats["uploads"] += 1
        self.stats["upload_bytes"] += len(job["data"])
        f = kv.get("f", "32")
        if f == "100":
            dims = png_info(raw)
            if dims is None:
                return self.reply(kv, "EBADPNG:not a PNG")
            (w, h), bpp = dims, 4  # Ghostty keeps decoded PNGs as RGBA
        else:
            w, h, bpp = int(kv.get("s", 0)), int(kv.get("v", 0)), 3 if f == "24" else 4
        if w <= 0 or h <= 0 or w > self.max_dim or h > self.max_dim:
            self.stats["refused"] += 1
            return self.reply(kv, "EINVAL:dimensions too large")
        mem = w * h * bpp
        i = int(kv.get("i", 0))
        self.drop_images([i])  # a re-sent id replaces the image and its placements
        for key in [k for k in self.places if k[0] == i]:
            del self.places[key]
        if mem > self.storage_limit:
            self.stats["refused"] += 1
            return self.reply(kv, "ENOMEM:image too big")
        self.evict(self.mem + mem - self.storage_limit)
        self.gen += 1
        self.images[i] = Image(kv, raw, w, h, mem, self.gen)
        self.mem += mem
        self.stats["peak_mem"] = max(self.stats["peak_mem"], self.mem)
        self.version += 1
        self.reply(kv, "OK")

    def evict(self, need):
        """Ghostty's policy: unplaced images first, then placed ones, oldest first."""
        if need <= 0:
            return
        placed = {i for i, _ in self.places}
        for img_id in sorted(self.images, key=lambda i: (i in placed, self.images[i].gen)):
            if need <= 0:
                break
            need -= self.images[img_id].mem
            self.stats["evicted"] += 1
            for key in [k for k in self.places if k[0] == img_id]:
                del self.places[key]
            self.drop_images([img_id])
        self.version += 1

    def shown(self):
        """What is on screen: one (crc, row, col, cols, rows, sx, sy, sw, sh) per placement."""
        out = []
        for (i, _), ((row, col), kv) in self.places.items():
            img = self.images.get(i)
            if img is None:
                continue
            sx, sy = int(kv.get("x", 0)), int(kv.get("y", 0))
            sw, sh = int(kv.get("w", 0)) or img.w - sx, int(kv.get("h", 0)) or img.h - sy
            c = int(kv.get("c", 0)) or -(-sw // self.cw)
            r = int(kv.get("r", 0)) or -(-sh // self.ch)
            out.append((img.checksum(), row, col, c, r, sx, sy, sw, sh))
        return out

    def snapshot(self, path):
        cw, ch = self.cw, self.ch
        W, H = self.cols * cw, self.rows * ch
        fb = bytearray(b"\x1e" * (W * H * 3))
        for (i, _), ((row, col), kv) in self.places.items():
            img = self.images.get(i)
            if img is None:
                continue
            w, rgb = img.w, img.decoded()
            sx, sy = int(kv.get("x", 0)), int(kv.get("y", 0))
            sw, sh = int(kv.get("w", 0)) or img.w - sx, int(kv.get("h", 0)) or img.h - sy
            dw, dh = int(kv.get("c", 0)) * cw or sw, int(kv.get("r", 0)) * ch or sh
            for dy in range(dh):
                Y = row * ch + dy
                if not 0 <= Y < H:
                    continue
                syy = sy + dy * sh // dh
                if sw == dw:
                    x0, x1 = max(0, col * cw), min(W, col * cw + dw)
                    if x1 <= x0:
                        continue
                    src = (syy * w + sx + (x0 - col * cw)) * 3
                    fb[(Y * W + x0) * 3:(Y * W + x1) * 3] = rgb[src:src + (x1 - x0) * 3]
                else:
                    for dx in range(dw):
                        X = col * cw + dx
                        if 0 <= X < W:
                            s = (syy * w + sx + dx * sw // dw) * 3
                            fb[(Y * W + X) * 3:(Y * W + X) * 3 + 3] = rgb[s:s + 3]
        with open(path, "wb") as f:
            f.write(png_encode(W, H, bytes(fb)))
        # The text, which the PNG leaves out.
        with open(path[:-4] + ".txt", "w") as f:
            f.write("\n".join("".join(r).rstrip() for r in self.grid) + "\n")

    def status(self):
        return "".join(self.grid[-1]).rstrip()

    def feed(self, data):
        # An unfinished APC (a big image arriving in small pty reads) is kept
        # in self.apc, and only the new bytes are searched for its end.
        if self.apc is not None:
            tail = self.apc[-1:] + data
            k = tail.find(b"\x1b\\")
            if k < 0:
                self.apc += data
                return b""
            data = bytes(self.apc[:-1]) + tail
            self.apc = None
        i, n = 0, len(data)
        while i < n:
            c = data[i]
            if c == 0x1b and i + 1 < n:
                nx = data[i + 1]
                if nx == ord("_"):
                    j = data.find(b"\x1b\\", i)
                    if j < 0:
                        self.apc = bytearray(data[i:])
                        return b""
                    body = data[i + 2:j]
                    if body.startswith(b"G"):
                        ctl, _, payload = body[1:].partition(b";")
                        self.graphics(ctl.decode(), payload)
                    i = j + 2
                    continue
                if nx == ord("["):
                    m = re.compile(rb"\x1b\[([0-9;?>=]*)([ -/]*)([@-~])").match(data, i)
                    if not m:
                        return data[i:]
                    self.csi(m.group(1).decode(), m.group(3).decode())
                    i = m.end()
                    continue
                if nx == ord("7") or nx == ord("8"):
                    if nx == ord("7"):
                        self.saved = self.cur
                    else:
                        self.cur = getattr(self, "saved", (0, 0))
                    i += 2
                    continue
                i += 2
                continue
            if c == 0x1b:
                return data[i:]
            if c >= 0x20:
                ln = 1 if c < 0x80 else 2 if c < 0xe0 else 3 if c < 0xf0 else 4
                ch_ = data[i:i + ln].decode("utf-8", "replace")
                r, cc = self.cur
                if r < self.rows and cc < self.cols:
                    self.grid[r][cc] = ch_
                self.cur = (r, cc + 1)
                i += ln
                continue
            i += 1
        return b""

    def csi(self, params, final):
        nums = [int(x) if x.isdigit() else 0 for x in params.lstrip("?>").split(";")] if params else []
        if final == "H":
            self.cur = ((nums[0] if nums else 1) - 1, (nums[1] if len(nums) > 1 else 1) - 1)
        elif final == "J" and nums[:1] == [2]:
            self.grid = [[" "] * self.cols for _ in range(self.rows)]
        elif final == "K":
            self.grid[min(self.cur[0], self.rows - 1)] = [" "] * self.cols
        elif final == "t" and nums[:1] == [16]:
            self.replies.append(f"\x1b[6;{self.ch};{self.cw}t".encode())
        elif final == "q" and params.startswith(">") and self.xtversion:
            self.replies.append(f"\x1bP>|{self.xtversion}\x1b\\".encode())
        elif final == "c" and not params.startswith(">"):
            self.replies.append(b"\x1b[?62;22c")
        elif final == "h" and params == "?2026":
            self.stats["frames"] += 1


def keybytes(s):
    s = s.replace("\\e", "\x1b").replace("\\r", "\r").replace("\\s", " ")
    return re.sub(r"\^([A-Z])", lambda m: chr(ord(m.group(1)) - 64), s).encode()


class FarleafLog:
    """Reads farleaf's FARLEAF_LOG as it grows: strip checksums and frames."""

    def __init__(self, path):
        self.path, self.pos, self.crc, self.frame, self.lines = path, 0, {}, None, []

    def poll(self):
        try:
            with open(self.path, "rb") as f:
                f.seek(self.pos)
                data = f.read()
        except FileNotFoundError:
            return
        end = data.rfind(b"\n") + 1
        self.pos += end
        for line in data[:end].decode("utf-8", "replace").splitlines():
            self.lines.append(line)
            m = re.search(r"strip id=(\d+) .*crc ([0-9a-f]{8})", line)
            if m:
                self.crc[int(m.group(1))] = int(m.group(2), 16)
                continue
            m = re.match(r"\s*[\d.]+ frame (\d+) missing=(\d) ?(.*)$", line)
            if m:
                places = [tuple(int(v) for v in p.split(":")) for p in m.group(3).split()]
                self.frame = (int(m.group(1)), m.group(2) == "1", places)
            elif re.match(r"\s*[\d.]+ exit: ", line):
                self.frame = (-1, False, [])  # it quit and cleared its images

    def expected(self):
        """farleaf's last frame as (crc, row, col, cols, rows, sx, sy, sw, sh), and whether it was complete."""
        if self.frame is None:
            return None, False
        _, missing, places = self.frame
        out = []
        for p in places:
            i, row, col, sx, sy, sw, sh, c, r = p
            out.append((self.crc.get(i, -1), row, col, c, r, sx, sy, sw, sh))
        return out, not missing


def compare(expected, shown):
    """Is `shown` exactly `expected`, give or take one offset for all (herdr's pane origin)?"""
    if expected is None:
        return False, "farleaf drew nothing"
    if len(expected) != len(shown):
        return False, f"{len(shown)} images on screen, farleaf drew {len(expected)}"
    if not expected:
        return True, "empty"
    def body(p):
        return (p[0],) + p[3:]
    have = {}
    for p in shown:
        have[(body(p), p[1], p[2])] = have.get((body(p), p[1], p[2]), 0) + 1
    # The offset is that of some image with the first expected one's content.
    first = expected[0]
    best = None
    for s in shown:
        if body(s) != body(first):
            continue
        dr, dc = s[1] - first[1], s[2] - first[2]
        left, bad = dict(have), None
        for e in expected:
            key = (body(e), e[1] + dr, e[2] + dc)
            if left.get(key, 0) == 0:
                bad = e
                break
            left[key] -= 1
        if bad is None:
            return True, f"{len(expected)} images match"
        best = best or bad
    if best is None:
        return False, f"missing or different: {first}"
    return False, f"missing or different: {best}"


def run(cmd, *, cols=134, rows=50, cell=(19, 42), shm=False, keys=(), steps=None, timeout=30, mbps=0.0, rtt=0.0,
        compress=False, env=None, out="dev/out", flog=None, storage_limit=320_000_000, xtversion="ghostty 1.3.1",
        settle_quiet=0.3, step_timeout=30.0, verbose=True):
    """Runs `cmd` in a fake terminal and returns a dict of results.

    `keys` is a timed script: (seconds from start, action). `steps` is run in
    order instead, each waiting for the screen to settle before the next.
    Actions: ("keys", bytes) | ("burst", [bytes, ...], interval_s) | ("snap", name)
    | ("resize", (cols, rows)) | ("call", fn) | ("wait", name) (wait for the
    screen to settle without typing, e.g. while the program starts) | ("sleep", s).
    Every settle is checked against `flog` when given."""
    cw, ch = cell
    os.makedirs(out, exist_ok=True)
    script = sorted(keys, key=lambda x: x[0])
    use_steps, steps = steps is not None, list(steps or [])
    log = FarleafLog(flog) if flog else None

    child_env = dict(os.environ)
    child_env.update(env or {})
    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe(cmd[0], cmd, child_env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, cols * cw, rows * ch))
    term, start, rest = Term(cols, rows, cw, ch, shm, storage_limit, xtversion=xtversion), time.time(), b""
    res = {"settles": [], "checks": [], "snaps": [], "first_image": None, "exit": None}
    # A key's settle: from sending it until output has been quiet for
    # `settle_quiet` s; its latency: until the last change to the screen.
    pending, last_out, last_change, seen_version = None, None, None, 0
    burst, next_step = [], start
    # The modeled link: bandwidth holds the sender back (we read from the pty
    # only once the link has sent everything read so far); latency only delays
    # delivery, by half the round trip each way.
    send_t = time.time()
    zc = zlib.compressobj(6)
    wire, zlib_s = 0, 0.0
    arriving, keys_out = [], []  # (due time, bytes) in order

    def deliver(data):
        nonlocal rest, last_out, last_change, seen_version
        term.stats["bytes"] += len(data)
        last_out = time.time()
        rest = term.feed(rest + data)
        if term.version != seen_version:
            seen_version, last_change = term.version, time.time()
        if res["first_image"] is None and term.places:
            res["first_image"] = round(time.time() - start, 3)
        if os.environ.get("FAKE_KITTY_TRACE"):
            sys.stderr.write(f"<- {data[:300]!r}\n-> {term.replies!r}\n")
        for rep in term.replies:
            keys_out.append((time.time() + rtt / 2000, rep))
        term.replies.clear()

    def check(name):
        if log is None:
            return None
        log.poll()
        exp, complete = log.expected()
        shown = term.shown()
        ok, why = compare(exp, shown)
        ok = ok and complete
        if not complete:
            why = "farleaf's last frame was still loading; " + why
        r = {"name": name, "ok": ok, "why": why, "images": len(shown), "term_mb": round(term.mem / 1e6, 1),
             "status": term.status()}
        res["checks"].append(r)
        if verbose and not ok:
            print(f"CHECK FAILED at {name}: {why}")
        return r

    def settle(p, quiet_at, timed_out=False):
        lat = None
        if last_change is not None:
            lat = round(max(0.0, last_change - p["t"]), 3)
        name = p["name"] if p.get("n", 1) == 1 else f"{p['name']}x{p['n']}"
        res["settles"].append({"name": name, "latency": None if timed_out else lat,
                               "settled": None if timed_out else round(quiet_at - p["t"], 3)})
        check(name + (" (never settled)" if timed_out else ""))

    def press(arg, name=None):
        nonlocal pending
        name = name or (arg.decode(errors="replace") if isinstance(arg, bytes) else str(arg))
        if pending is None:
            pending = {"t": time.time(), "name": name, "n": 1}
        else:  # a key before the last one settled: time the burst from its last key
            pending["t"], pending["n"] = time.time(), pending["n"] + 1
        keys_out.append((time.time() + rtt / 2000, arg))

    def act(kind, arg):
        nonlocal pending, next_step
        if kind == "snap":
            path = os.path.join(out, arg + ".png")
            term.snapshot(path)
            res["snaps"].append(path)
            if verbose:
                print(f"snap {arg}: {len(term.places)} placements, status: {term.status()!r}")
        elif kind == "resize":
            c2, r2 = arg
            term.resize(c2, r2)
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", r2, c2, c2 * cw, r2 * ch))
            if pending is None:
                pending = {"t": time.time(), "name": f"resize:{c2}x{r2}"}
        elif kind == "call":
            arg()
        elif kind == "check":
            check(arg)
        elif kind == "wait":
            name, quiet = (arg, 1.0) if isinstance(arg, str) else arg
            # Quiet is not enough while the program is still starting (a
            # shell in a herdr pane can take seconds): with a log, also wait
            # for a complete frame.
            pending = {"t": time.time(), "name": name, "n": 1, "quiet": quiet, "frame": log is not None}
        elif kind == "sleep":
            next_step = time.time() + arg
        elif kind == "burst":
            ks, interval = arg
            burst.extend((time.time() + i * interval, k) for i, k in enumerate(ks))
        else:
            press(arg)

    exited = False
    while time.time() - start < timeout:
        now = time.time()
        quiet_since = max(last_out or 0, pending["t"] if pending else 0) + rtt / 1000
        if pending is not None and not keys_out and not arriving and not burst:
            ready = True
            if pending.get("frame"):
                log.poll()
                ready = log.frame is not None and not log.frame[1]
            if ready and now - quiet_since > pending.get("quiet", settle_quiet):
                settle(pending, quiet_since)
                pending, next_step = None, now + 0.05
            elif now - pending["t"] > step_timeout:
                settle(pending, now, timed_out=True)
                pending, next_step = None, now + 0.05
        while script and script[0][0] <= now - start:
            act(*script.pop(0)[1])
        while steps and pending is None and not burst and now >= next_step:
            act(*steps.pop(0))
        if use_steps and not steps and pending is None and not burst:
            break
        while burst and burst[0][0] <= now:
            press(burst.pop(0)[1])
        while keys_out and keys_out[0][0] <= now:
            try:
                os.write(fd, keys_out.pop(0)[1])
            except OSError:
                break
        while arriving and arriving[0][0] <= time.time():
            deliver(arriving.pop(0)[1])
        can_read = not mbps or time.time() >= send_t
        wait = 0.003 if can_read else max(0.0, min(0.003, send_t - time.time()))
        r, _, _ = select.select([fd] if can_read else [], [], [], wait)
        if r:
            # macOS ptys hand over at most 1 KB per read: gather up to 64 KB of
            # what is already waiting before accounting for it on the link.
            data = b""
            try:
                while len(data) < 64 * 1024:
                    part = os.read(fd, 64 * 1024)
                    if not part:
                        break
                    data += part
                    if not select.select([fd], [], [], 0)[0]:
                        break
            except OSError:
                pass
            if not data:
                exited = True
                break
            if mbps:
                if compress:
                    t_c = time.time()
                    n = sum(len(zc.compress(data[i:i + 32768]) + zc.flush(zlib.Z_SYNC_FLUSH))
                            for i in range(0, len(data), 32768))
                    zlib_s += time.time() - t_c
                else:
                    n = len(data)
                wire += n
                send_t = max(send_t, time.time()) + n / (mbps * 1e6)
                arriving.append((send_t + rtt / 2000, data))
            else:
                deliver(data)
    for due, data in arriving:
        deliver(data)
    if pending is not None and not exited:
        settle(pending, time.time(), timed_out=True)
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        wpid, status = os.waitpid(pid, os.WNOHANG)
        if wpid == 0:
            time.sleep(0.2)
            wpid, status = os.waitpid(pid, os.WNOHANG)
        if wpid:
            res["exit"] = os.waitstatus_to_exitcode(status)
    except ChildProcessError:
        pass
    if log:
        log.poll()
    res.update(stats=term.stats, wire=wire, zlib_s=zlib_s, exited=exited, term_mem=term.mem,
               screen=[''.join(r).rstrip() for r in term.grid], log=log.lines if log else [],
               elapsed=round(time.time() - start, 1))
    return res


def parse_keys(spec):
    script = []
    for item in spec.split():
        t, _, rest = item.partition(":")
        t = float(t)
        if rest.startswith("snap:"):
            script.append((t, ("snap", rest[5:])))
        elif rest.startswith("resize:"):
            script.append((t, ("resize", tuple(map(int, rest[7:].split("x"))))))
        elif rest.startswith("check:"):
            script.append((t, ("check", rest[6:])))
        else:
            script.append((t, ("keys", keybytes(rest))))
    return script


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cols", type=int, default=134)
    ap.add_argument("--rows", type=int, default=50)
    ap.add_argument("--cell", default="19x42")
    ap.add_argument("--shm", action="store_true")
    ap.add_argument("--out", default="dev/out")
    ap.add_argument("--keys", default="")
    ap.add_argument("--timeout", type=float, default=30)
    ap.add_argument("--mbps", type=float, default=0, help="throttle to this many MB/s, like an SSH link")
    ap.add_argument("--rtt", type=float, default=0, help="with --mbps: round-trip time in ms (half each way)")
    ap.add_argument("--compress", action="store_true",
                    help="with --mbps: model SSH compression (zlib 6, flushed per 32 KB packet; time it costs included)")
    ap.add_argument("--flog", help="farleaf's FARLEAF_LOG: check the screen against it after every key")
    ap.add_argument("--env", action="append", default=[], help="NAME=VALUE for the child")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd
    env = dict(e.partition("=")[::2] for e in a.env)
    res = run(cmd, cols=a.cols, rows=a.rows, cell=tuple(map(int, a.cell.split("x"))), shm=a.shm,
              keys=parse_keys(a.keys), timeout=a.timeout, mbps=a.mbps, rtt=a.rtt, compress=a.compress,
              env=env, out=a.out, flog=a.flog)
    s = res["stats"]
    if res["settles"]:
        print("settle after key (s):", ", ".join(f"{x['name']}={x['latency']}" for x in res["settles"]))
    for c in res["checks"]:
        print(f"check {c['name']}: {'ok' if c['ok'] else 'FAILED'} ({c['why']})")
    text = [t for t in res["screen"] if t]
    if text and not s["uploads"]:
        print("screen text:", *text, sep="\n  ")
    if a.mbps:
        print(f"on the modeled link: {res['wire'] / 1e6:.2f} MB ({'compressed' if a.compress else 'uncompressed'})"
              + (f"; compressing took {res['zlib_s']:.2f} s" if a.compress else ""))
    print(f"received {s['bytes'] / 1e6:.2f} MB; {s['uploads']} uploads ({s['upload_bytes'] / 1e6:.2f} MB payload), "
          f"{s['places']} placements, {s['deletes']} deletes, {s['frames']} frames; "
          f"refused {s['refused']}, evicted {s['evicted']}, placed-but-missing {s['missing_places']}; "
          f"first image placed at {res['first_image']} s")


if __name__ == "__main__":
    main()
