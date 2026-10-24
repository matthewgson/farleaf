//! The stress test: farleaf in fake terminals of many sizes, directly and
//! inside herdr.
//!
//!   cargo dev stress [--pdf FILE --word WORD] [--scanned FILE] [--farleaf BIN]
//!                    [--only PATTERN ...] [--quick] [--out DIR]
//!
//! Each case starts farleaf in the fake terminal (directly, or in a throwaway
//! herdr session running inside it) and walks through a reading session: page
//! turns, scrolling, jumps, zoom and pan, fit modes, inversion, search, window
//! resizes, the key list popup, the sidebar (thumbnails and outline),
//! key-repeat bursts, and a reload while the file changes. After every step it
//! checks that the screen shows exactly what farleaf drew, image for image and
//! pixel for pixel, and it times how long the screen took to stop changing.
//! Results go to OUT/results.json and OUT/summary.md.
//!
//! Without --pdf it reads a generated sample. --word is a word in the PDF, for
//! the search step; --scanned adds cases with a scanned PDF. A PATTERN is a
//! substring of a case name; ^ and $ anchor it.
//!
//! Modes:
//!   local       Ghostty on this machine (shared memory, no link)
//!   ssh         Ghostty over SSH: PNG on a 9 MB/s, 50 ms link with SSH compression
//!   herdr-ssh   herdr on the remote machine, Ghostty over that same link
//!   herdr-local herdr and Ghostty both local (herdr hands images over in files)
//!   herdr-noenv herdr-ssh, but farleaf cannot see herdr's environment variables
//!               (as when farleaf runs over SSH from a herdr pane)
//!
//! herdr cases use sessions named flstress-*, never yours, and remove them.

use crate::session::{self, keybytes, Opts, Step};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// (name, cols, rows, cell): sizes of the windows this is used in.
const GEOMS: &[(&str, u32, u32, (u32, u32))] = &[
    ("mbp-half", 90, 53, (19, 42)),   // a 16" MacBook Pro (3456x2234), half the screen
    ("mbp-full", 181, 53, (19, 42)),  //
    ("wide-half", 150, 56, (19, 42)), // a 5K2K display (5710x2390), half
    ("wide-full", 300, 56, (19, 42)), // ... the whole of it
    ("small-font", 320, 84, (8, 17)), // a 2560x1440 non-retina monitor, small font
    ("big-font", 118, 44, (24, 54)),
    ("tiny", 40, 12, (19, 42)),
];
const HERDR_ENV_VARS: &[&str] = &[
    "HERDR_ENV",
    "HERDR_PANE_ID",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
    "HERDR_SOCKET_PATH",
    "HERDR_BIN_PATH",
    "HERDR_STARTUP_CWD",
];

fn keys(steps: &mut Vec<Step>, ks: &[&str]) {
    steps.extend(ks.iter().map(|k| Step::Keys(keybytes(k))));
}

fn session_steps(search: &str, reload: Box<dyn FnMut()>, cols: u32, rows: u32) -> Vec<Step> {
    let small = ((cols * 3 / 5).max(20), (rows * 3 / 5).max(8));
    let mut st = Vec::new();
    keys(&mut st, &["l", "l", "l", "h"]);
    keys(&mut st, &["j", "j", "j", "k", "^D", "^U", "\\s", "^B"]);
    keys(&mut st, &["10G", "30G", "gg", "G", "gg"]);
    keys(
        &mut st,
        &["+", "+", "+", "L", "L", "L", "H", "+", "+", "L", "j", "l", "?", "?"],
    );
    keys(&mut st, &["-", "-", "-", "-", "-", "Z"]);
    keys(&mut st, &["z", "-", "-", "-", "j", "l", "Z"]);
    keys(&mut st, &["i", "l", "i"]);
    keys(&mut st, &[&format!("/{search}\\r"), "n", "n", "N", "\\e"]);
    keys(&mut st, &["?", "j", "G", "?"]);
    // The sidebar: thumbnails (preview two pages on, go there), again (to the
    // end, then back), the outline instead, fit modes beside it, then hide it.
    keys(&mut st, &["t", "j", "j", "\\r"]);
    keys(&mut st, &["t", "G", "\\e"]);
    keys(&mut st, &["o", "j", "j", "\\r"]);
    keys(&mut st, &["z", "Z", "?", "?"]);
    keys(&mut st, &["o", "o"]);
    st.push(Step::Resize(small.0, small.1));
    keys(&mut st, &["j"]);
    st.push(Step::Resize(cols, rows));
    let rep = |k: &str, n| vec![keybytes(k); n];
    st.push(Step::Burst(rep("j", 40), 0.03));
    st.push(Step::Burst(rep("k", 40), 0.03));
    st.push(Step::Burst(rep("l", 12), 0.1));
    st.push(Step::Burst(rep("h", 12), 0.1));
    st.push(Step::Burst([rep("+", 3), rep("-", 3)].concat(), 0.05));
    // farleaf polls the file twice a second and reloads once it stops
    // changing: time from the write, and wait past one quiet poll.
    st.push(Step::Call(reload));
    st.push(Step::Wait("reload".into(), 1.5));
    keys(&mut st, &["l", "q"]);
    st
}

fn kind(name: &str) -> &'static str {
    let n = name.split(" (").next().unwrap_or(name);
    if n.starts_with("resize") {
        return "resize";
    }
    if let Some((a, b)) = n.rsplit_once('x') {
        if !a.is_empty() && !b.is_empty() && b.bytes().all(|c| c.is_ascii_digit()) {
            return "burst";
        }
    }
    match n {
        "open" => "open",
        "reload" => "reload",
        "l" | "h" => "page",
        "j" | "k" | "\x04" | "\x15" | " " | "\x02" => "scroll",
        _ if n.ends_with('G') || n == "gg" => "jump",
        "+" | "-" | "z" | "Z" => "view",
        "L" | "H" => "pan",
        "i" => "invert",
        _ if n.starts_with('/') || matches!(n, "n" | "N" | "\x1b") => "search",
        "?" => "help",
        "t" | "o" | "\r" => "sidebar",
        _ => "other",
    }
}

/// Runs herdr's command line, against `sock` if given, never the user's own
/// herdr, giving up after ten seconds.
fn herdr(args: &[&str], sock: Option<&Path>) -> String {
    let mut c = Command::new("herdr");
    c.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for v in HERDR_ENV_VARS {
        c.env_remove(v);
    }
    if let Some(s) = sock {
        c.env("HERDR_SOCKET_PATH", s);
    }
    let Ok(mut child) = c.spawn() else { return String::new() };
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    child
        .wait_with_output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

struct Case {
    name: String,
    mode: &'static str,
    geom: usize,
    pdf: &'static str,
}

struct Outcome {
    case: Case,
    checks: usize,
    failures: Vec<(String, String)>,
    never_settled: usize,
    /// kind -> (count, median, max) seconds
    latency: BTreeMap<&'static str, (usize, f64, f64)>,
    probe: Option<String>,
    stats: crate::screen::Stats,
    wire_mb: f64,
    elapsed: f64,
}

fn run_case(
    case: Case,
    pdfs: &BTreeMap<&str, (PathBuf, String)>,
    farleaf: &Path,
    out: &Path,
    scratch: &Path,
) -> Outcome {
    let (_, cols, rows, cell) = GEOMS[case.geom];
    let d = out.join(&case.name);
    let _ = std::fs::create_dir_all(&d);
    let flog = d.join("farleaf.log");
    let _ = std::fs::write(&flog, "");
    // A copy of the PDF, so the reload step can rewrite it.
    let doc = scratch.join(format!("{}.pdf", case.name));
    let (src, word) = pdfs[case.pdf].clone();
    let _ = std::fs::copy(&src, &doc);
    let (doc2, src2) = (doc.clone(), src.clone());
    let reload = Box::new(move || {
        let tmp = doc2.with_extension("pdf.tmp");
        // Bytes, not fs::copy, which keeps the modification time on macOS:
        // farleaf has to see a new file.
        let _ = std::fs::read(&src2).and_then(|b| std::fs::write(&tmp, b));
        let _ = std::fs::rename(&tmp, &doc2);
    });
    let mut steps = session_steps(&word, reload, cols, rows);
    let mut o = Opts {
        cols,
        rows,
        cell,
        timeout: 600.0,
        out: d.clone(),
        flog: Some(flog.clone()),
        step_timeout: 40.0,
        verbose: false,
        // herdr's environment variables would point the child at the user's
        // own herdr: never pass them on.
        unset: HERDR_ENV_VARS.iter().map(|v| v.to_string()).collect(),
        env: vec![
            ("TERM".into(), "xterm-ghostty".into()),
            ("TERM_PROGRAM".into(), "ghostty".into()),
        ],
        ..Opts::default()
    };
    if matches!(case.mode, "ssh" | "herdr-ssh" | "herdr-noenv") {
        (o.mbps, o.rtt, o.compress) = (9.0, 50.0, true);
    }
    let farleaf_s = farleaf.to_string_lossy().into_owned();
    let doc_s = doc.to_string_lossy().into_owned();
    let mut session = None;
    let cmd: Vec<String> = if matches!(case.mode, "local" | "ssh") {
        o.env.push(("FARLEAF_LOG".into(), flog.to_string_lossy().into_owned()));
        o.shm = case.mode == "local";
        steps.insert(0, Step::Wait("open".into(), 1.0));
        vec![farleaf_s, doc_s]
    } else {
        let name: String = format!("flstress-{}", case.name).chars().take(40).collect();
        let home = std::env::var("HOME").unwrap_or_default();
        let sock = PathBuf::from(format!("{home}/.config/herdr/sessions/{name}/herdr.sock"));
        herdr(&["session", "stop", &name], None);
        herdr(&["session", "delete", &name], None);
        let launcher = scratch.join(format!("run-{}.sh", case.name));
        let unset = if case.mode == "herdr-noenv" {
            "env -u HERDR_ENV -u TERM_PROGRAM -u HERDR_PANE_ID "
        } else {
            ""
        };
        let _ = std::fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nexport FARLEAF_LOG='{}'\nexec {unset}'{farleaf_s}' '{doc_s}'\n",
                flog.display()
            ),
        );
        let _ = std::fs::set_permissions(&launcher, std::os::unix::fs::PermissionsExt::from_mode(0o755));
        // herdr redraws its own text (borders, status) now and then: time
        // only the images.
        o.text_counts = false;
        if case.mode != "herdr-local" {
            o.env
                .push(("SSH_CONNECTION".into(), "10.0.0.1 50000 10.0.0.2 22".into()));
            o.env.push(("SSH_TTY".into(), "/dev/ttys999".into()));
        }
        let launch = Box::new(move || {
            for _ in 0..80 {
                if sock.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let panes = herdr(&["pane", "list"], Some(&sock));
            let pane = panes
                .split("\"pane_id\"")
                .nth(1)
                .and_then(|s| s.split('"').nth(1))
                .unwrap_or_default()
                .to_string();
            herdr(&["pane", "run", &pane, &launcher.to_string_lossy()], Some(&sock));
        });
        steps.splice(
            0..0,
            [Step::Sleep(2.5), Step::Call(launch), Step::Wait("open".into(), 1.0)],
        );
        session = Some(name.clone());
        vec!["herdr".into(), "--session".into(), name]
    };
    let res = session::run(&cmd, &o, Vec::new(), Some(steps));
    if let Some(s) = &session {
        herdr(&["session", "stop", s], None);
        herdr(&["session", "delete", s], None);
    }
    for p in [doc.clone(), doc.with_extension("pdf.tmp")] {
        let _ = std::fs::remove_file(p);
    }
    let res = match res {
        Ok(r) => r,
        Err(e) => {
            return Outcome {
                case,
                checks: 1,
                failures: vec![("start".into(), e)],
                never_settled: 0,
                latency: BTreeMap::new(),
                probe: None,
                stats: Default::default(),
                wire_mb: 0.0,
                elapsed: 0.0,
            }
        }
    };
    let probe = res
        .log
        .iter()
        .find_map(|l| l.split_once("probe: ").map(|(_, p)| p.to_string()));
    let mut lat: BTreeMap<&'static str, Vec<f64>> = BTreeMap::new();
    for s in &res.settles {
        if let Some(l) = s.latency {
            lat.entry(kind(&s.name)).or_default().push(l);
        }
    }
    let latency = lat
        .into_iter()
        .map(|(k, mut v)| {
            v.sort_by(f64::total_cmp);
            let n = v.len();
            let median = if n % 2 == 1 {
                v[n / 2]
            } else {
                (v[n / 2 - 1] + v[n / 2]) / 2.0
            };
            (k, (n, median, v[n - 1]))
        })
        .collect();
    Outcome {
        case,
        checks: res.checks.len(),
        failures: res
            .checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| (c.name.clone(), c.why.clone()))
            .collect(),
        never_settled: res.settles.iter().filter(|s| s.latency.is_none()).count(),
        latency,
        probe,
        stats: res.stats,
        wire_mb: res.wire as f64 / 1e6,
        elapsed: res.elapsed,
    }
}

fn matches(name: &str, pattern: &str) -> bool {
    match (pattern.strip_prefix('^'), pattern.strip_suffix('$')) {
        (Some(p), _) if p.ends_with('$') => name == &p[..p.len() - 1],
        (Some(p), _) => name.starts_with(p),
        (None, Some(p)) => name.ends_with(p),
        (None, None) => name.contains(pattern),
    }
}

pub fn main(args: &[String]) -> Result<(), String> {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let (mut pdf, mut word, mut scanned) = (None, "farleaf".to_string(), None);
    let mut farleaf = repo.join("target/release/farleaf");
    let (mut only, mut quick, mut out) = (Vec::new(), false, repo.join("dev/out/stress"));
    let mut i = 0;
    while i < args.len() {
        let next = |i: usize| args.get(i + 1).cloned().ok_or(format!("{} needs a value", args[i]));
        match args[i].as_str() {
            "--pdf" => pdf = Some(PathBuf::from(next(i)?)),
            "--word" => word = next(i)?,
            "--scanned" => scanned = Some(PathBuf::from(next(i)?)),
            "--farleaf" => farleaf = PathBuf::from(next(i)?),
            "--out" => out = PathBuf::from(next(i)?),
            "--quick" => {
                quick = true;
                i += 1;
                continue;
            }
            "--only" => {
                while args.get(i + 1).is_some_and(|a| !a.starts_with("--")) {
                    i += 1;
                    only.push(args[i].clone());
                }
                i += 1;
                continue;
            }
            a => return Err(format!("unknown option {a}")),
        }
        i += 2;
    }
    let farleaf = std::fs::canonicalize(&farleaf).map_err(|e| format!("{}: {e}", farleaf.display()))?;
    let scratch = out.join("tmp");
    std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let mut pdfs: BTreeMap<&str, (PathBuf, String)> = BTreeMap::new();
    match pdf {
        Some(p) => pdfs.insert("text", (std::fs::canonicalize(&p).map_err(|e| e.to_string())?, word)),
        None => {
            let p = scratch.join("sample.pdf");
            crate::sample_pdf::write(&p, 40).map_err(|e| e.to_string())?;
            pdfs.insert("text", (p, "farleaf".into()))
        }
    };
    if let Some(s) = scanned {
        pdfs.insert(
            "scanned",
            (std::fs::canonicalize(&s).map_err(|e| e.to_string())?, "the".into()),
        );
    }
    let mut cases = Vec::new();
    for mode in ["local", "ssh", "herdr-ssh", "herdr-local"] {
        for (g, geom) in GEOMS.iter().enumerate() {
            if quick && !matches!(geom.0, "wide-full" | "mbp-half") {
                continue;
            }
            cases.push(Case {
                name: format!("{mode}-{}", geom.0),
                mode,
                geom: g,
                pdf: "text",
            });
        }
    }
    if pdfs.contains_key("scanned") {
        for mode in ["local", "herdr-ssh"] {
            cases.push(Case {
                name: format!("{mode}-wide-half-scanned"),
                mode,
                geom: 2,
                pdf: "scanned",
            });
        }
    }
    cases.push(Case {
        name: "herdr-noenv-wide-half".into(),
        mode: "herdr-noenv",
        geom: 2,
        pdf: "text",
    });
    if !only.is_empty() {
        cases.retain(|c| only.iter().any(|p| matches(&c.name, p)));
    }
    let mut results = Vec::new();
    for case in cases {
        let t = Instant::now();
        let r = run_case(case, &pdfs, &farleaf, &out, &scratch);
        let worst = r
            .latency
            .values()
            .map(|v| v.2)
            .fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b))));
        println!(
            "{:<30} {:>3}/{:<3} ok  never-settled {}  worst {} s  refused {} evicted {} missing {}  ({:.0} s)",
            r.case.name,
            r.checks - r.failures.len(),
            r.checks,
            r.never_settled,
            worst.map_or("-".into(), |w| format!("{w:.3}")),
            r.stats.refused,
            r.stats.evicted,
            r.stats.missing_places,
            t.elapsed().as_secs_f64()
        );
        for (n, why) in r.failures.iter().take(3) {
            println!("    {n:?}: {}", why.chars().take(150).collect::<String>());
        }
        results.push(r);
        let _ = std::fs::write(out.join("results.json"), json(&results));
    }
    let path = out.join("summary.md");
    let _ = std::fs::write(&path, summary(&results, &farleaf));
    println!("\nwrote {}", path.display());
    Ok(())
}

const KINDS: &[&str] = &[
    "open", "page", "scroll", "jump", "view", "pan", "search", "help", "sidebar", "resize", "burst", "reload",
];

fn summary(results: &[Outcome], farleaf: &Path) -> String {
    let mut out = vec![
        format!("# farleaf stress test\n\n`{}`\n", farleaf.display()),
        "Checks: after every step, the screen is exactly what farleaf drew. \
         Times: median / worst seconds from the key until the screen stopped changing.\n"
            .to_string(),
        format!("| case | cells | checks | {} |", KINDS.join(" | ")),
        format!("|---|---|---|{}", "---|".repeat(KINDS.len())),
    ];
    for r in results {
        let (_, cols, rows, cell) = GEOMS[r.case.geom];
        let cells: Vec<String> = KINDS
            .iter()
            .map(|k| {
                r.latency
                    .get(k)
                    .map_or(String::new(), |v| format!("{:.2} / {:.2}", v.1, v.2))
            })
            .collect();
        let mut ok = format!("{}/{}", r.checks - r.failures.len(), r.checks);
        if r.never_settled > 0 {
            ok += &format!(", {} stuck", r.never_settled);
        }
        out.push(format!(
            "| {} | {cols}x{rows} @{}x{} | {ok} | {} |",
            r.case.name,
            cell.0,
            cell.1,
            cells.join(" | ")
        ));
    }
    let fails: Vec<&Outcome> = results
        .iter()
        .filter(|r| !r.failures.is_empty() || r.never_settled > 0)
        .collect();
    if !fails.is_empty() {
        out.push("\n## Failures\n".into());
        for r in fails {
            let list: Vec<String> = r
                .failures
                .iter()
                .take(4)
                .map(|(n, w)| format!("`{n:?}`: {w}"))
                .collect();
            out.push(format!("- **{}**: {}", r.case.name, list.join("; ")));
        }
    }
    out.join("\n") + "\n"
}

fn esc(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o += "\\\"",
            '\\' => o += "\\\\",
            c if (c as u32) < 0x20 => o += &format!("\\u{:04x}", c as u32),
            c => o.push(c),
        }
    }
    o + "\""
}

fn json(results: &[Outcome]) -> String {
    let items: Vec<String> = results
        .iter()
        .map(|r| {
            let (_, cols, rows, cell) = GEOMS[r.case.geom];
            let fails: Vec<String> = r
                .failures
                .iter()
                .take(8)
                .map(|(n, w)| format!("[{}, {}]", esc(n), esc(w)))
                .collect();
            let lat: Vec<String> = r
                .latency
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}: {{\"n\": {}, \"median\": {:.3}, \"max\": {:.3}}}",
                        esc(k),
                        v.0,
                        v.1,
                        v.2
                    )
                })
                .collect();
            let s = &r.stats;
            format!(
                "{{\"name\": {}, \"mode\": {}, \"cols\": {cols}, \"rows\": {rows}, \"cell\": \"{}x{}\", \
                 \"checks\": {}, \"failed\": {}, \"failures\": [{}], \"never_settled\": {}, \"latency\": {{{}}}, \
                 \"probe\": {}, \"stats\": {{\"bytes\": {}, \"uploads\": {}, \"refused\": {}, \"evicted\": {}, \
                 \"missing_places\": {}, \"peak_mem\": {}}}, \"wire_mb\": {:.1}, \"elapsed\": {:.1}}}",
                esc(&r.case.name),
                esc(r.case.mode),
                cell.0,
                cell.1,
                r.checks,
                r.failures.len(),
                fails.join(", "),
                r.never_settled,
                lat.join(", "),
                r.probe.as_deref().map_or("null".into(), esc),
                s.bytes,
                s.uploads,
                s.refused,
                s.evicted,
                s.missing_places,
                s.peak_mem,
                r.wire_mb,
                r.elapsed
            )
        })
        .collect();
    format!("[\n{}\n]\n", items.join(",\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_patterns() {
        assert_eq!(kind("jx40"), "burst");
        assert_eq!(kind("10G"), "jump");
        assert_eq!(kind("/farleaf\r"), "search");
        assert_eq!(kind("resize:20x8 (never settled)"), "resize");
        assert!(matches("local-tiny", "^local-tiny$"));
        assert!(!matches("local-tiny-x", "^local-tiny$"));
        assert!(matches("herdr-ssh-wide-full", "ssh"));
    }
}
