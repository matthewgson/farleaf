//! farleaf's development tools, in one program (`cargo dev ...` in the repo):
//!
//!   cargo dev term [--cols 134 --rows 50 --cell 19x42] [--shm] [--out DIR]
//!                  [--keys "1.0:j 2.0:snap:after-j 3.0:q"] [--timeout S]
//!                  [--mbps N [--rtt MS] [--compress]] [--flog LOG]
//!                  [--env NAME=VALUE ...] -- CMD ARGS...
//!       a fake Kitty-graphics terminal: runs CMD in a pty, answers its
//!       queries, types the keys, saves screenshots
//!   cargo dev stress [...]       the stress test (see src/stress.rs)
//!   cargo dev sample-pdf OUT.pdf [PAGES]
//!   cargo dev demo [--out docs/demo.gif]
//!
//! --keys is a list of TIME:KEYS (seconds from the start; KEYS may use \e,
//! \r, \s for space and ^X for Ctrl+X), TIME:snap:NAME (save OUT/NAME.png),
//! TIME:resize:COLSxROWS and TIME:check:NAME. --shm makes the terminal read
//! shared memory (t=s), like a Ghostty on the same machine. With --flog
//! (farleaf's FARLEAF_LOG), every time the screen settles it checks that what
//! is on screen is exactly what farleaf last drew.

mod demo;
mod draw;
mod farlog;
mod png;
mod sample_pdf;
mod screen;
mod session;
mod stress;

use session::{keybytes, Opts, Step};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("term") => term(&args[1..]),
        Some("stress") => stress::main(&args[1..]),
        Some("demo") => demo::main(&args[1..]),
        Some("sample-pdf") => match &args[1..] {
            [out, rest @ ..] => {
                let pages = rest.first().and_then(|n| n.parse().ok()).unwrap_or(12);
                sample_pdf::write(&PathBuf::from(out), pages).map_err(|e| e.to_string())
            }
            [] => Err("usage: cargo dev sample-pdf OUT.pdf [PAGES]".into()),
        },
        _ => Err("usage: cargo dev term|stress|demo|sample-pdf ... (see dev/src/main.rs)".into()),
    };
    if let Err(e) = result {
        eprintln!("farleaf-dev: {e}");
        std::process::exit(1);
    }
}

fn parse_keys(spec: &str) -> Result<Vec<(f64, Step)>, String> {
    let mut script = Vec::new();
    for item in spec.split_whitespace() {
        let (t, rest) = item.split_once(':').ok_or(format!("{item}: want TIME:KEYS"))?;
        let t: f64 = t.parse().map_err(|_| format!("{item}: bad time"))?;
        let step = if let Some(name) = rest.strip_prefix("snap:") {
            Step::Snap(name.into())
        } else if let Some(size) = rest.strip_prefix("resize:") {
            let (c, r) = size.split_once('x').ok_or(format!("{item}: want resize:COLSxROWS"))?;
            Step::Resize(c.parse().map_err(|_| "bad cols")?, r.parse().map_err(|_| "bad rows")?)
        } else if let Some(name) = rest.strip_prefix("check:") {
            Step::Check(name.into())
        } else {
            Step::Keys(keybytes(rest))
        };
        script.push((t, step));
    }
    Ok(script)
}

fn term(args: &[String]) -> Result<(), String> {
    let mut o = Opts::default();
    let mut keys = String::new();
    let mut i = 0;
    let mut cmd = Vec::new();
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            cmd = args[i + 1..].to_vec();
            break;
        }
        let flag = matches!(a, "--shm" | "--compress");
        let v = if flag {
            ""
        } else {
            args.get(i + 1)
                .map(String::as_str)
                .ok_or(format!("{a} needs a value"))?
        };
        let n = |v: &str| v.parse::<f64>().map_err(|_| format!("{a}: not a number"));
        match a {
            "--cols" => o.cols = n(v)? as u32,
            "--rows" => o.rows = n(v)? as u32,
            "--cell" => {
                let (w, h) = v.split_once('x').ok_or("--cell WxH")?;
                o.cell = (n(w)? as u32, n(h)? as u32);
            }
            "--shm" => o.shm = true,
            "--compress" => o.compress = true,
            "--out" => o.out = PathBuf::from(v),
            "--keys" => keys = v.to_string(),
            "--timeout" => o.timeout = n(v)?,
            "--mbps" => o.mbps = n(v)?,
            "--rtt" => o.rtt = n(v)?,
            "--flog" => o.flog = Some(PathBuf::from(v)),
            "--env" => {
                let (k, val) = v.split_once('=').ok_or("--env NAME=VALUE")?;
                o.env.push((k.into(), val.into()));
            }
            _ => {
                // The command, without "--".
                cmd = args[i..].to_vec();
                break;
            }
        }
        i += if flag { 1 } else { 2 };
    }
    if cmd.is_empty() {
        return Err("no command to run (cargo dev term [options] -- CMD ARGS...)".into());
    }
    let res = session::run(&cmd, &o, parse_keys(&keys)?, None)?;
    let s = &res.stats;
    if !res.settles.is_empty() {
        let list: Vec<String> = res
            .settles
            .iter()
            .map(|x| {
                format!(
                    "{}={}",
                    x.name.escape_debug(),
                    x.latency.map_or("-".into(), |l| format!("{l:.3}"))
                )
            })
            .collect();
        println!("settle after key (s): {}", list.join(", "));
    }
    for c in &res.checks {
        println!(
            "check {}: {} ({})",
            c.name.escape_debug(),
            if c.ok { "ok" } else { "FAILED" },
            c.why
        );
    }
    let text: Vec<&String> = res.screen.iter().filter(|l| !l.is_empty()).collect();
    if !text.is_empty() && s.uploads == 0 {
        println!("screen text:");
        for l in text {
            println!("  {l}");
        }
    }
    if o.mbps > 0.0 {
        println!(
            "on the modeled link: {:.2} MB ({}){}",
            res.wire as f64 / 1e6,
            if o.compress { "compressed" } else { "uncompressed" },
            if o.compress {
                format!("; compressing took {:.2} s", res.zlib_s)
            } else {
                String::new()
            }
        );
    }
    println!(
        "received {:.2} MB; {} uploads ({:.2} MB payload), {} placements, {} deletes, {} frames; refused {}, \
         evicted {}, placed-but-missing {}; first image placed at {}",
        s.bytes as f64 / 1e6,
        s.uploads,
        s.upload_bytes as f64 / 1e6,
        s.places,
        s.deletes,
        s.frames,
        s.refused,
        s.evicted,
        s.missing_places,
        res.first_image.map_or("-".into(), |t| format!("{t:.3} s"))
    );
    Ok(())
}
