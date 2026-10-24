//! Runs a program in a pseudo-terminal sized like a real terminal, as the
//! fake terminal: answers its queries, types scripted keys, models an SSH
//! link, and, given farleaf's FARLEAF_LOG, checks after every step that the
//! screen shows exactly what farleaf drew.

use crate::draw::Text;
use crate::farlog::{compare, FarleafLog};
use crate::screen::{Stats, Term};
use std::collections::VecDeque;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub enum Step {
    Keys(Vec<u8>),
    /// Keys typed `interval` seconds apart, like key repeat.
    Burst(Vec<Vec<u8>>, f64),
    /// Save the screen as NAME.png (and its text as NAME.txt).
    Snap(String),
    Resize(u32, u32),
    Call(Box<dyn FnMut()>),
    /// Wait until the screen has been quiet this long (and, with a log,
    /// farleaf drew a complete frame), e.g. while the program starts.
    Wait(String, f64),
    Sleep(f64),
    Check(String),
}

pub struct Opts {
    pub cols: u32,
    pub rows: u32,
    pub cell: (u32, u32),
    /// Read POSIX shared memory (t=s), like a terminal on the same machine.
    pub shm: bool,
    pub timeout: f64,
    /// The modeled link: MB/s (0: no link), round trip in ms, and whether
    /// SSH compresses (zlib 6, flushed per 32 KB packet).
    pub mbps: f64,
    pub rtt: f64,
    pub compress: bool,
    pub env: Vec<(String, String)>,
    pub unset: Vec<String>,
    pub out: PathBuf,
    pub flog: Option<PathBuf>,
    pub storage_limit: usize,
    pub xtversion: String,
    pub settle_quiet: f64,
    pub step_timeout: f64,
    pub verbose: bool,
    /// Text changes count as screen changes, for timing (see Term::text_counts).
    pub text_counts: bool,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            cols: 134,
            rows: 50,
            cell: (19, 42),
            shm: false,
            timeout: 30.0,
            mbps: 0.0,
            rtt: 0.0,
            compress: false,
            env: Vec::new(),
            unset: Vec::new(),
            out: PathBuf::from("dev/out"),
            flog: None,
            storage_limit: 320_000_000,
            xtversion: "ghostty 1.3.1".into(),
            settle_quiet: 0.3,
            step_timeout: 30.0,
            verbose: true,
            text_counts: true,
        }
    }
}

pub struct Settle {
    pub name: String,
    pub latency: Option<f64>,
}

pub struct Check {
    pub name: String,
    pub ok: bool,
    pub why: String,
}

pub struct Results {
    pub settles: Vec<Settle>,
    pub checks: Vec<Check>,
    pub first_image: Option<f64>,
    pub exit: Option<i32>,
    pub stats: Stats,
    pub wire: usize,
    pub zlib_s: f64,
    pub screen: Vec<String>,
    pub log: Vec<String>,
    pub elapsed: f64,
}

struct Pending {
    t: f64,
    name: String,
    n: u32,
    quiet: Option<f64>,
    frame: bool,
}

/// The typed form of keys: \e, \r, \s (space), and ^X for Ctrl+X.
pub fn keybytes(s: &str) -> Vec<u8> {
    let s = s.replace("\\e", "\x1b").replace("\\r", "\r").replace("\\s", " ");
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('^', Some(&n)) if n.is_ascii_uppercase() => {
                out.push(n as u8 - 64);
                chars.next();
            }
            _ => {
                let mut buf = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out
}

fn which(prog: &str) -> Option<PathBuf> {
    if prog.contains('/') {
        return Some(PathBuf::from(prog));
    }
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(prog)).find(|p| p.is_file()))
}

fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).expect("no NUL left")
}

fn winsize(cols: u32, rows: u32, cell: (u32, u32)) -> libc::winsize {
    libc::winsize {
        ws_row: rows as u16,
        ws_col: cols as u16,
        ws_xpixel: (cols * cell.0) as u16,
        ws_ypixel: (rows * cell.1) as u16,
    }
}

/// Starts `cmd` on a new pseudo-terminal; returns its pid and the master fd.
fn spawn(cmd: &[String], o: &Opts) -> Result<(libc::pid_t, i32), String> {
    let prog = which(&cmd[0]).ok_or_else(|| format!("{}: not found", cmd[0]))?;
    let prog = cstr(&prog.to_string_lossy());
    let argv: Vec<CString> = cmd.iter().map(|a| cstr(a)).collect();
    let mut argv_p: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let env: Vec<CString> = std::env::vars()
        .filter(|(k, _)| !o.unset.contains(k) && !o.env.iter().any(|(e, _)| e == k))
        .chain(o.env.iter().cloned())
        .map(|(k, v)| cstr(&format!("{k}={v}")))
        .collect();
    let mut env_p: Vec<*const libc::c_char> = env.iter().map(|e| e.as_ptr()).collect();
    env_p.push(std::ptr::null());
    let mut ws = winsize(o.cols, o.rows, o.cell);
    let mut master = 0;
    // A raw pointer: macOS takes the size mutable, Linux const.
    let pid = unsafe {
        libc::forkpty(
            &mut master,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(ws),
        )
    };
    if pid < 0 {
        return Err(format!("forkpty: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        unsafe {
            libc::execve(prog.as_ptr(), argv_p.as_ptr(), env_p.as_ptr());
            libc::_exit(127);
        }
    }
    Ok((pid, master))
}

fn readable(fd: i32, timeout: f64) -> bool {
    unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut set);
        libc::FD_SET(fd, &mut set);
        let mut tv = libc::timeval {
            tv_sec: timeout as libc::time_t,
            tv_usec: ((timeout.fract()) * 1e6) as libc::suseconds_t,
        };
        libc::select(fd + 1, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &mut tv) > 0
            && libc::FD_ISSET(fd, &set)
    }
}

/// Runs `cmd`: with `steps`, in order, each after the screen settled from
/// the one before; else the timed `script` (seconds from the start).
pub fn run(
    cmd: &[String],
    o: &Opts,
    mut script: Vec<(f64, Step)>,
    steps: Option<Vec<Step>>,
) -> Result<Results, String> {
    std::fs::create_dir_all(&o.out).map_err(|e| e.to_string())?;
    script.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut script: VecDeque<(f64, Step)> = script.into();
    let use_steps = steps.is_some();
    let mut steps: VecDeque<Step> = steps.unwrap_or_default().into();
    let mut log = o.flog.clone().map(FarleafLog::new);
    let (pid, fd) = spawn(cmd, o)?;
    let (cw, ch) = o.cell;
    let mut term = Term::new(o.cols, o.rows, cw, ch, o.shm, o.storage_limit, &o.xtversion);
    term.text_counts = o.text_counts;
    let mut text: Option<Text> = None;
    let start = Instant::now();
    let now = || start.elapsed().as_secs_f64();
    let mut r = Results {
        settles: Vec::new(),
        checks: Vec::new(),
        first_image: None,
        exit: None,
        stats: Stats::default(),
        wire: 0,
        zlib_s: 0.0,
        screen: Vec::new(),
        log: Vec::new(),
        elapsed: 0.0,
    };
    let mut rest: Vec<u8> = Vec::new();
    // A key's settle: from sending it until output has been quiet for
    // `settle_quiet` s; its latency: until the last change to the screen.
    let mut pending: Option<Pending> = None;
    let (mut last_out, mut last_change, mut seen_version) = (0.0f64, None::<f64>, 0u64);
    let mut burst: VecDeque<(f64, Vec<u8>)> = VecDeque::new();
    let mut next_step = 0.0;
    // The modeled link: bandwidth holds the sender back (we read from the pty
    // only once the link has sent everything read so far); latency only
    // delays delivery, by half the round trip each way.
    let mut send_t = 0.0;
    let mut zc = flate2::Compress::new(flate2::Compression::new(6), true);
    let mut zbuf = vec![0u8; 64 * 1024];
    let mut arriving: VecDeque<(f64, Vec<u8>)> = VecDeque::new();
    let mut keys_out: VecDeque<(f64, Vec<u8>)> = VecDeque::new();
    let half_rtt = o.rtt / 2000.0;
    let mut exited = false;

    macro_rules! check {
        ($name:expr) => {
            if let Some(log) = log.as_mut() {
                log.poll();
                let (exp, complete) = log.expected();
                let shown = term.shown();
                let (ok, why) = compare(exp.as_deref(), &shown);
                let why = if complete {
                    why
                } else {
                    format!("farleaf's last frame was still loading; {why}")
                };
                let name: String = $name;
                if o.verbose && !(ok && complete) {
                    println!("CHECK FAILED at {name}: {why}");
                }
                r.checks.push(Check {
                    name,
                    ok: ok && complete,
                    why,
                });
            }
        };
    }
    macro_rules! settle {
        ($p:expr, $quiet_at:expr, $timed_out:expr) => {{
            let p: Pending = $p;
            let lat = last_change.map(|lc: f64| (lc - p.t).max(0.0));
            let name = if p.n == 1 {
                p.name.clone()
            } else {
                format!("{}x{}", p.name, p.n)
            };
            let _ = $quiet_at;
            r.settles.push(Settle {
                name: name.clone(),
                latency: if $timed_out { None } else { lat },
            });
            check!(if $timed_out {
                format!("{name} (never settled)")
            } else {
                name
            });
        }};
    }
    macro_rules! press {
        ($bytes:expr) => {{
            let b: Vec<u8> = $bytes;
            let t = now();
            match pending.as_mut() {
                // A key before the last one settled: time the burst from its last key.
                Some(p) => {
                    p.t = t;
                    p.n += 1;
                }
                None => {
                    pending = Some(Pending {
                        t,
                        name: String::from_utf8_lossy(&b).into_owned(),
                        n: 1,
                        quiet: None,
                        frame: false,
                    })
                }
            }
            keys_out.push_back((t + half_rtt, b));
        }};
    }

    while now() < o.timeout {
        let t = now();
        let quiet_since = last_out.max(pending.as_ref().map_or(0.0, |p| p.t)) + o.rtt / 1000.0;
        if pending.is_some() && keys_out.is_empty() && arriving.is_empty() && burst.is_empty() {
            let p = pending.as_ref().expect("checked");
            let ready = !p.frame
                || log.as_mut().is_some_and(|l| {
                    l.poll();
                    l.frame.as_ref().is_some_and(|f| !f.1)
                });
            if ready && t - quiet_since > p.quiet.unwrap_or(o.settle_quiet) {
                settle!(pending.take().expect("checked"), quiet_since, false);
                next_step = t + 0.05;
            } else if t - p.t > o.step_timeout {
                settle!(pending.take().expect("checked"), t, true);
                next_step = t + 0.05;
            }
        }
        let mut actions = Vec::new();
        while script.front().is_some_and(|s| s.0 <= t) {
            actions.push(script.pop_front().expect("checked").1);
        }
        for a in actions {
            if let Step::Check(name) = a {
                check!(name);
            } else if let Some(keys) = act(
                a,
                &mut term,
                &mut text,
                fd,
                o,
                &mut pending,
                &mut next_step,
                &mut burst,
                t,
            ) {
                press!(keys);
            }
        }
        while pending.is_none() && burst.is_empty() && t >= next_step {
            let Some(a) = steps.pop_front() else { break };
            if let Step::Check(name) = a {
                check!(name);
            } else if let Some(keys) = act(
                a,
                &mut term,
                &mut text,
                fd,
                o,
                &mut pending,
                &mut next_step,
                &mut burst,
                t,
            ) {
                press!(keys);
            }
        }
        if use_steps && steps.is_empty() && pending.is_none() && burst.is_empty() {
            break;
        }
        while burst.front().is_some_and(|b| b.0 <= t) {
            let b = burst.pop_front().expect("checked").1;
            press!(b);
        }
        while keys_out.front().is_some_and(|k| k.0 <= t) {
            let k = keys_out.pop_front().expect("checked").1;
            if unsafe { libc::write(fd, k.as_ptr().cast(), k.len()) } < 0 {
                break;
            }
        }
        while arriving.front().is_some_and(|a| a.0 <= now()) {
            let data = arriving.pop_front().expect("checked").1;
            deliver(
                &data,
                &mut term,
                &mut rest,
                &mut r,
                now(),
                &mut last_out,
                &mut last_change,
                &mut seen_version,
            );
            for rep in term.replies.drain(..) {
                keys_out.push_back((now() + half_rtt, rep));
            }
        }
        let can_read = o.mbps == 0.0 || now() >= send_t;
        let wait = if can_read {
            0.003
        } else {
            (send_t - now()).clamp(0.0, 0.003)
        };
        if !can_read {
            std::thread::sleep(std::time::Duration::from_secs_f64(wait));
            continue;
        }
        if !readable(fd, wait) {
            continue;
        }
        // macOS ptys hand over at most 1 KB per read: gather up to 64 KB of
        // what is already waiting before accounting for it on the link.
        let mut data = Vec::new();
        let mut buf = [0u8; 65536];
        while data.len() < 65536 {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            data.extend_from_slice(&buf[..n as usize]);
            if !readable(fd, 0.0) {
                break;
            }
        }
        if data.is_empty() {
            exited = true;
            break;
        }
        if o.mbps > 0.0 {
            let n = if o.compress {
                let tc = Instant::now();
                let before = zc.total_out();
                for chunk in data.chunks(32768) {
                    let mut input = chunk;
                    loop {
                        let (i0, o0) = (zc.total_in(), zc.total_out());
                        let _ = zc.compress(input, &mut zbuf, flate2::FlushCompress::Sync);
                        input = &input[(zc.total_in() - i0) as usize..];
                        if input.is_empty() && (zc.total_out() - o0) < zbuf.len() as u64 {
                            break;
                        }
                    }
                }
                r.zlib_s += tc.elapsed().as_secs_f64();
                (zc.total_out() - before) as usize
            } else {
                data.len()
            };
            r.wire += n;
            send_t = send_t.max(now()) + n as f64 / (o.mbps * 1e6);
            arriving.push_back((send_t + half_rtt, data));
        } else {
            deliver(
                &data,
                &mut term,
                &mut rest,
                &mut r,
                now(),
                &mut last_out,
                &mut last_change,
                &mut seen_version,
            );
            for rep in term.replies.drain(..) {
                keys_out.push_back((now() + half_rtt, rep));
            }
        }
    }
    for (_, data) in std::mem::take(&mut arriving) {
        deliver(
            &data,
            &mut term,
            &mut rest,
            &mut r,
            now(),
            &mut last_out,
            &mut last_change,
            &mut seen_version,
        );
    }
    if let Some(p) = pending.take() {
        if !exited {
            settle!(p, now(), true);
        }
    }
    unsafe {
        libc::kill(pid, libc::SIGTERM);
        let mut status = 0;
        let mut got = libc::waitpid(pid, &mut status, libc::WNOHANG);
        if got == 0 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            got = libc::waitpid(pid, &mut status, libc::WNOHANG);
        }
        if got == pid && libc::WIFEXITED(status) {
            r.exit = Some(libc::WEXITSTATUS(status));
        }
        libc::close(fd);
    }
    if let Some(l) = log.as_mut() {
        l.poll();
        r.log = std::mem::take(&mut l.lines);
    }
    r.stats = term.stats.clone();
    r.screen = term.lines();
    r.elapsed = now();
    Ok(r)
}

#[allow(clippy::too_many_arguments)]
fn deliver(
    data: &[u8],
    term: &mut Term,
    rest: &mut Vec<u8>,
    r: &mut Results,
    t: f64,
    last_out: &mut f64,
    last_change: &mut Option<f64>,
    seen_version: &mut u64,
) {
    term.stats.bytes += data.len();
    *last_out = t;
    rest.extend_from_slice(data);
    *rest = term.feed(&std::mem::take(rest));
    if term.version != *seen_version {
        *seen_version = term.version;
        *last_change = Some(t);
    }
    if r.first_image.is_none() && !term.places.is_empty() {
        r.first_image = Some(t);
    }
    if std::env::var_os("FARLEAF_DEV_TRACE").is_some() {
        eprintln!("<- {:?}", String::from_utf8_lossy(&data[..data.len().min(300)]));
    }
}

/// Saves the screen as `path` (PNG) and its text next to it (.txt).
pub fn snapshot(term: &mut Term, text: &mut Option<Text>, path: &Path) {
    if text.is_none() {
        *text = Text::new(term.cw, term.ch);
    }
    let (w, h, fb) = term.render(text.as_mut());
    let _ = std::fs::write(path, crate::png::encode(w, h, &fb));
    let _ = std::fs::write(path.with_extension("txt"), term.lines().join("\n") + "\n");
}

/// Does `step`; returns the keys to type, if it is a key step.
#[allow(clippy::too_many_arguments)]
fn act(
    step: Step,
    term: &mut Term,
    text: &mut Option<Text>,
    fd: i32,
    o: &Opts,
    pending: &mut Option<Pending>,
    next_step: &mut f64,
    burst: &mut VecDeque<(f64, Vec<u8>)>,
    t: f64,
) -> Option<Vec<u8>> {
    match step {
        Step::Snap(name) => {
            snapshot(term, text, &o.out.join(format!("{name}.png")));
            if o.verbose {
                println!(
                    "snap {name}: {} placements, status: {:?}",
                    term.places.len(),
                    term.status()
                );
            }
        }
        Step::Resize(c, rows) => {
            term.resize(c, rows);
            let ws = winsize(c, rows, o.cell);
            unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
            if pending.is_none() {
                *pending = Some(Pending {
                    t,
                    name: format!("resize:{c}x{rows}"),
                    n: 1,
                    quiet: None,
                    frame: false,
                });
            }
        }
        Step::Call(mut f) => f(),
        Step::Wait(name, quiet) => {
            // Quiet is not enough while the program is still starting (a
            // shell in a herdr pane can take seconds): with a log, also wait
            // for a complete frame.
            *pending = Some(Pending {
                t,
                name,
                n: 1,
                quiet: Some(quiet),
                frame: o.flog.is_some(),
            });
        }
        Step::Sleep(s) => *next_step = t + s,
        Step::Burst(keys, interval) => {
            burst.extend(keys.into_iter().enumerate().map(|(i, k)| (t + i as f64 * interval, k)));
        }
        Step::Keys(b) => return Some(b),
        Step::Check(_) => {}
    }
    None
}
