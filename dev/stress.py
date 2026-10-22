#!/usr/bin/env python3
"""Stress test: farleaf in fake terminals of many sizes, directly and inside herdr.

  dev/stress.py [--pdf FILE --word WORD] [--scanned FILE] [--farleaf BIN]
                [--only REGEX ...] [--quick] [--out DIR]

Without --pdf it reads a generated sample (dev/sample_pdf.py). --word is a
word in the PDF, for the search step; --scanned adds cases with a scanned PDF.

Each case starts farleaf in dev/fake_kitty.py (directly, or in a throwaway
herdr session running inside it) and walks through a reading session: page
turns, scrolling, jumps, zoom and pan, fit modes, inversion, search, window
resizes, the key list popup, key-repeat bursts, and a reload while the
file changes. After every
step it checks that the screen shows exactly what farleaf drew, image for
image and pixel for pixel; and it times how long the screen took to stop
changing. Results go to OUT/results.json and OUT/summary.md.

Modes:
  local       Ghostty on this machine (shared memory, no link)
  ssh         Ghostty over SSH: PNG on a 9 MB/s, 50 ms link with SSH compression
  herdr-ssh   herdr on the remote machine, Ghostty over that same link
  herdr-local herdr and Ghostty both local (herdr hands images over in files)
  herdr-noenv herdr-ssh, but farleaf cannot see herdr's environment variables
              (as when farleaf runs over SSH from a herdr pane)

herdr cases use sessions named flstress-*, never yours, and remove them.
"""
import argparse, json, os, re, shutil, statistics, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
sys.path.insert(0, HERE)
import fake_kitty  # noqa: E402
import sample_pdf  # noqa: E402

# name -> (path, search word), set in main()
PDFS = {}
# (cols, rows, cell): window sizes, in cells and cell pixels.
GEOMS = {
    "mbp-half": (90, 53, (19, 42)),     # a 16" MacBook Pro (3456x2234), half the screen
    "mbp-full": (181, 53, (19, 42)),
    "wide-half": (150, 56, (19, 42)),   # a 5K2K display (5710x2390), half
    "wide-full": (300, 56, (19, 42)),   # ... the whole of it
    "small-font": (320, 84, (8, 17)),   # a 2560x1440 non-retina monitor, small font
    "big-font": (118, 44, (24, 54)),
    "tiny": (40, 12, (19, 42)),
}
LINK = dict(mbps=9, rtt=50, compress=True)
HERDR_ENV_VARS = ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_TAB_ID", "HERDR_WORKSPACE_ID", "HERDR_SOCKET_PATH",
                  "HERDR_BIN_PATH", "HERDR_STARTUP_CWD"]


def k(s):
    return fake_kitty.keybytes(s)


def session_steps(search, reload_fn, geom):
    cols, rows, _ = geom
    small = (max(20, cols * 3 // 5), max(8, rows * 3 // 5))
    st = []
    def key(s, *more):
        st.append(("keys", k(s)))
        for m in more:
            st.append(("keys", k(m)))
    key("l", "l", "l", "h")
    key("j", "j", "j", "k", "^D", "^U", "\\s", "^B")
    key("10G", "30G", "gg", "G", "gg")
    key("+", "+", "+", "L", "L", "L", "H", "+", "+", "L", "j", "l", "?", "?")
    key("-", "-", "-", "-", "-", "Z")
    key("z", "-", "-", "-", "j", "l", "Z")
    key("i", "l", "i")
    key(f"/{search}\\r", "n", "n", "N", "\\e")
    key("?", "j", "G", "?")
    st.append(("resize", small))
    key("j")
    st.append(("resize", (cols, rows)))
    st.append(("burst", ([k("j")] * 40, 0.03)))
    st.append(("burst", ([k("k")] * 40, 0.03)))
    st.append(("burst", ([k("l")] * 12, 0.1)))
    st.append(("burst", ([k("h")] * 12, 0.1)))
    st.append(("burst", ([k("+")] * 3 + [k("-")] * 3, 0.05)))
    # farleaf polls the file twice a second and reloads once it stops
    # changing: time from the write, and wait past one quiet poll.
    st.append(("call", reload_fn))
    st.append(("wait", ("reload", 1.5)))
    key("l")
    st.append(("keys", k("q")))
    return st


def kind(name):
    n = name.split(" (")[0]
    if n.startswith("resize"):
        return "resize"
    if re.fullmatch(r".+x\d+", n, re.S):
        return "burst"
    if n in ("open", "reload"):
        return n
    if n in ("l", "h"):
        return "page"
    if n in ("j", "k", "\x04", "\x15", " ", "\x02"):
        return "scroll"
    if n.endswith("G") or n == "gg":
        return "jump"
    if n in ("+", "-", "z", "Z"):
        return "view"
    if n in ("L", "H"):
        return "pan"
    if n == "i":
        return "invert"
    if n.startswith("/") or n in ("n", "N", "\x1b"):
        return "search"
    if n == "?":
        return "help"
    if n.startswith("resize"):
        return "resize"
    return "other"


def herdr(args, sock=None, timeout=10):
    env = {k_: v for k_, v in os.environ.items() if k_ not in HERDR_ENV_VARS}
    if sock:
        env["HERDR_SOCKET_PATH"] = sock
    return subprocess.run(["herdr", *args], env=env, capture_output=True, text=True, timeout=timeout)


def run_case(name, mode, geom_name, pdf_name, farleaf, out, scratch):
    cols, rows, cell = GEOMS[geom_name]
    d = os.path.join(out, name)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    flog = os.path.join(d, "farleaf.log")
    # A copy of the PDF, so the reload step can rewrite it.
    doc = os.path.join(scratch, f"{name}.pdf")
    src, word = PDFS[pdf_name]
    shutil.copyfile(src, doc)

    def reload_fn():
        tmp = doc + ".tmp"
        shutil.copyfile(src, tmp)
        os.replace(tmp, doc)

    steps = session_steps(word, reload_fn, GEOMS[geom_name])
    link = LINK if mode in ("ssh", "herdr-ssh", "herdr-noenv") else {}
    base_env = {"TERM": "xterm-ghostty", "TERM_PROGRAM": "ghostty"}
    session = None
    if mode in ("local", "ssh"):
        cmd = [farleaf, doc]
        env = dict(base_env, FARLEAF_LOG=flog)
        steps = [("wait", "open")] + steps
        shm = mode == "local"
    else:
        session = f"flstress-{name}"[:40]
        sock = os.path.expanduser(f"~/.config/herdr/sessions/{session}/herdr.sock")
        herdr(["session", "stop", session])
        herdr(["session", "delete", session])
        launcher = os.path.join(scratch, f"run-{name}.sh")
        unset = "env -u HERDR_ENV -u TERM_PROGRAM -u HERDR_PANE_ID " if mode == "herdr-noenv" else ""
        with open(launcher, "w") as f:
            f.write(f"#!/bin/sh\nexport FARLEAF_LOG='{flog}'\nexec {unset}'{farleaf}' '{doc}'\n")
        os.chmod(launcher, 0o755)
        cmd = ["herdr", "--session", session]
        env = dict(base_env)
        for v in HERDR_ENV_VARS:
            env[v] = ""
        if mode in ("herdr-ssh", "herdr-noenv"):
            env.update(SSH_CONNECTION="10.0.0.1 50000 10.0.0.2 22", SSH_TTY="/dev/ttys999")

        def launch():
            for _ in range(80):
                if os.path.exists(sock):
                    break
                time.sleep(0.1)
            panes = json.loads(herdr(["pane", "list"], sock).stdout or "{}")
            pane = panes["result"]["panes"][0]["pane_id"]
            herdr(["pane", "run", pane, launcher], sock)
        steps = [("sleep", 2.5), ("call", launch), ("wait", "open")] + steps
        shm = False
    # herdr's environment variables would point fake_kitty's child at the
    # user's own herdr: remove them from what the child inherits.
    saved = {v: os.environ.pop(v) for v in HERDR_ENV_VARS if v in os.environ}
    try:
        res = fake_kitty.run(cmd, cols=cols, rows=rows, cell=cell, shm=shm, steps=steps, timeout=600,
                             env={k_: v for k_, v in env.items() if v != ""}, out=d, flog=flog,
                             step_timeout=40, verbose=False, **link)
    finally:
        os.environ.update(saved)
        if session:
            herdr(["session", "stop", session])
            herdr(["session", "delete", session])
        for p in (doc, doc + ".tmp"):
            if os.path.exists(p):
                os.remove(p)
    lines = res["log"]
    probe = next((ln.split("probe: ", 1)[1] for ln in lines if "probe: " in ln), None)
    exit_line = next((ln.split("exit: ", 1)[1] for ln in lines if " exit: " in ln), None)
    lat = {}
    for s in res["settles"]:
        if s["latency"] is not None:
            lat.setdefault(kind(s["name"]), []).append(s["latency"])
    fails = [c for c in res["checks"] if not c["ok"]]
    return {
        "name": name, "mode": mode, "geom": geom_name, "pdf": pdf_name, "cols": cols, "rows": rows,
        "cell": f"{cell[0]}x{cell[1]}", "checks": len(res["checks"]), "failed": len(fails),
        "failures": [(c["name"], c["why"]) for c in fails[:8]],
        "never_settled": sum(1 for s in res["settles"] if s["latency"] is None),
        "latency": {kk: {"n": len(v), "median": round(statistics.median(v), 3), "max": round(max(v), 3)}
                    for kk, v in lat.items()},
        "probe": probe, "exit": exit_line if session else res["exit"], "stats": res["stats"],
        "wire_mb": round(res["wire"] / 1e6, 1), "elapsed": res["elapsed"],
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pdf", help="the PDF to read (default: a generated sample)")
    ap.add_argument("--word", default="farleaf", help="a word in --pdf, for the search step")
    ap.add_argument("--scanned", help="a scanned PDF, for a few more cases")
    ap.add_argument("--farleaf", default=os.path.join(REPO, "target/release/farleaf"))
    ap.add_argument("--only", nargs="*", default=[], help="run the cases whose name matches one of these regexes")
    ap.add_argument("--quick", action="store_true", help="one geometry per mode")
    ap.add_argument("--out", default=os.path.join(REPO, "dev/out/stress"))
    a = ap.parse_args()
    farleaf = os.path.abspath(a.farleaf)
    scratch = os.path.join(a.out, "tmp")
    os.makedirs(scratch, exist_ok=True)
    if a.pdf:
        PDFS["text"] = (os.path.abspath(a.pdf), a.word)
    else:
        PDFS["text"] = (os.path.join(scratch, "sample.pdf"), "farleaf")
        sample_pdf.write(PDFS["text"][0], 40)
    if a.scanned:
        PDFS["scanned"] = (os.path.abspath(a.scanned), "the")

    cases = []
    for mode in ("local", "ssh", "herdr-ssh", "herdr-local"):
        for g in GEOMS:
            if a.quick and g not in ("wide-full", "mbp-half"):
                continue
            cases.append((f"{mode}-{g}", mode, g, "text"))
    if "scanned" in PDFS:
        for mode in ("local", "herdr-ssh"):
            cases.append((f"{mode}-wide-half-scanned", mode, "wide-half", "scanned"))
    cases.append(("herdr-noenv-wide-half", "herdr-noenv", "wide-half", "text"))
    if a.only:
        cases = [c for c in cases if any(re.search(o, c[0]) for o in a.only)]

    results = []
    for c in cases:
        t = time.time()
        r = run_case(*c, farleaf=farleaf, out=a.out, scratch=scratch)
        results.append(r)
        worst = max((v["max"] for v in r["latency"].values()), default=None)
        print(f"{r['name']:<30} {r['checks'] - r['failed']:>3}/{r['checks']:<3} ok"
              f"  never-settled {r['never_settled']}  worst {worst} s  "
              f"refused {r['stats']['refused']} evicted {r['stats']['evicted']} "
              f"missing {r['stats']['missing_places']}  ({time.time() - t:.0f} s)", flush=True)
        for n, why in r["failures"][:3]:
            print(f"    {n!r}: {why[:150]}")
        with open(os.path.join(a.out, "results.json"), "w") as f:
            json.dump(results, f, indent=1)
    write_summary(results, os.path.join(a.out, "summary.md"), farleaf)


def write_summary(results, path, farleaf):
    kinds = ["open", "page", "scroll", "jump", "view", "pan", "search", "help", "resize", "burst", "reload"]
    out = [f"# farleaf stress test\n\n`{farleaf}`, {time.strftime('%Y-%m-%d %H:%M')}\n",
           "Checks: after every step, the screen is exactly what farleaf drew. "
           "Times: median / worst seconds from the key until the screen stopped changing.\n",
           "| case | cells | checks | " + " | ".join(kinds) + " |",
           "|---|---|---|" + "---|" * len(kinds)]
    for r in results:
        cells = []
        for kk in kinds:
            v = r["latency"].get(kk)
            cells.append(f"{v['median']:.2f} / {v['max']:.2f}" if v else "")
        ok = f"{r['checks'] - r['failed']}/{r['checks']}"
        if r["never_settled"]:
            ok += f", {r['never_settled']} stuck"
        out.append(f"| {r['name']} | {r['cols']}x{r['rows']} @{r['cell']} | {ok} | " + " | ".join(cells) + " |")
    fails = [r for r in results if r["failed"] or r["never_settled"]]
    if fails:
        out.append("\n## Failures\n")
        for r in fails:
            out.append(f"- **{r['name']}**: " + "; ".join(f"`{n}`: {w}" for n, w in r["failures"][:4]))
    with open(path, "w") as f:
        f.write("\n".join(out) + "\n")
    print(f"\nwrote {path}")


if __name__ == "__main__":
    main()
