//! farleaf: a fast, minimal terminal PDF reader for Kitty-graphics terminals,
//! built to stay fast over SSH and inside herdr.

/// Appends a line to $FARLEAF_LOG, when set. For diagnosing, not for users.
macro_rules! debug_log {
    ($($t:tt)*) => {
        if let Some(path) = std::env::var_os("FARLEAF_LOG") {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let t = crate::START.get_or_init(std::time::Instant::now).elapsed();
                let line = format!("{:>8.1} {}\n", t.as_secs_f64() * 1e3, format_args!($($t)*));
                let _ = f.write_all(line.as_bytes());
            }
        }
    };
}

static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// CRC-32 of a strip's pixels, for the log only: dev/stress.py checks that
/// what the terminal shows is exactly what was rendered.
fn debug_crc(rgb: &[u8]) -> u32 {
    if std::env::var_os("FARLEAF_LOG").is_none() {
        return 0;
    }
    let mut c = flate2::Crc::new();
    c.update(rgb);
    c.sum()
}

mod bench;
mod encode;
mod layout;
mod mupdf;
mod sidebar;
mod term;
mod worker;

use encode::Transport;
use layout::{Fit, Geom, Layout, Rect, Visible};
use sidebar::{Kind, ThumbAt, Thumbs};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use term::{Input, Key, Parser, Tty};
use worker::{Event, Job, StripJob, StripKey, Worker, Writer};

/// Every key, for the help popup (`?`) and --help. A row without keys
/// starts a section.
const KEYS: &[(&str, &str)] = &[
    ("", "Move"),
    ("j k  Down Up", "scroll"),
    ("Ctrl-d  Ctrl-u", "half a screen down / up"),
    ("Space  Ctrl-f  PgDn", "a screen down"),
    ("Ctrl-b  PgUp", "a screen up"),
    ("l h  J K", "next / previous page"),
    ("gg  G", "first page / end"),
    ("12G  12gg  :12", "page 12"),
    ("", "View"),
    ("z  Z", "fit page / fit width"),
    ("+  -", "zoom in / out"),
    ("H L  Left Right", "pan when zoomed wider than the screen"),
    ("i", "invert colors"),
    ("", "Search"),
    ("/text Enter", "search (case matters if it has capitals)"),
    ("n  N", "next / previous match"),
    ("Esc  :noh", "clear the highlights"),
    ("", "Marks and jumps"),
    ("m{a-z}  '{a-z}", "set mark / go to mark"),
    ("''", "back to before the last jump"),
    ("Ctrl-o  Tab", "older / newer place in the jump list"),
    ("", "Sidebar"),
    ("o", "outline (contents); again to hide"),
    ("t", "page thumbnails; again to hide"),
    ("j k  Enter  Esc", "in the sidebar: move, go there, go back"),
    ("", "Other"),
    ("r  :e", "reload (automatic when the file changes)"),
    ("?  :help", "this list"),
    ("q  :q  Ctrl-c", "quit"),
];

const COUNT_NOTE: &str = "A count works before most keys: 5j, 3l, 12G.";

fn usage() -> String {
    let mut s = String::from("usage: farleaf [-p PAGE] FILE.pdf\n");
    for (keys, what) in KEYS {
        if keys.is_empty() {
            s += &format!("\n{}\n", what.to_lowercase());
        } else {
            s += &format!("  {keys:<20} {what}\n");
        }
    }
    s + &format!(
        "\n{COUNT_NOTE}\n\nenvironment\n\
         \x20 FARLEAF_TRANSPORT    force shm, png or zlib (default: probed)"
    )
}

/// Image bytes we let the terminal hold, as it stores them. Ghostty keeps up
/// to 320 MB per screen; herdr only 64 MiB per pane, and past that it evicts
/// images, the oldest unplaced ones first, which would leave blank strips.
const GHOSTTY_BUDGET: usize = 192 << 20;
const HERDR_BUDGET: usize = 56 << 20;

fn main() {
    START.get_or_init(Instant::now);
    let mut args = std::env::args().skip(1);
    let (mut file, mut page, mut bench) = (None, None, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => return println!("{}", usage()),
            "-V" | "--version" => return println!("farleaf {}", env!("CARGO_PKG_VERSION")),
            "-p" | "--page" => page = args.next().and_then(|n| n.parse::<usize>().ok()),
            "--bench" => bench = true,
            _ if file.is_none() && !a.starts_with('-') => file = Some(PathBuf::from(a)),
            _ => fail(&format!("unexpected argument {a:?}\n\n{}", usage())),
        }
    }
    let Some(file) = file else { fail(&usage()) };
    if !file.is_file() {
        fail(&format!("{}: no such file", file.display()));
    }
    let result = if bench { bench::run(&file) } else { run(file, page) };
    if let Err(e) = result {
        fail(&e);
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("farleaf: {msg}");
    std::process::exit(1)
}

struct Probe {
    graphics: bool,
    shm: bool,
    cell: Option<(u32, u32)>,
    version: Option<String>,
    early: Vec<Input>,
}

/// One round trip: can the terminal show Kitty images, can it read our shared
/// memory (only if it runs on this machine), how big is a cell, and what is it
/// (XTVERSION). DA1 goes last; every terminal answers it, so its reply marks
/// the end of the answers.
fn probe(tty: &mut Tty, parser: &mut Parser, try_shm: bool) -> Probe {
    let mut q = Vec::new();
    q.extend_from_slice(b"\x1b_Gi=1,a=q,t=d,f=24,s=1,v=1;AAAA\x1b\\");
    let shm = if try_shm { encode::shm_probe() } else { None };
    if let Some((_, cmd)) = &shm {
        q.extend_from_slice(cmd);
    }
    q.extend_from_slice(b"\x1b[16t\x1b[>0q\x1b[c");
    let _ = term::write_all(&mut tty.file, &q);

    let mut p = Probe {
        graphics: false,
        shm: false,
        cell: None,
        version: None,
        early: Vec::new(),
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 4096];
    'wait: while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now()).as_millis() as i32;
        if !term::poll(&[tty.fd()], left)[0] {
            continue; // timed out, or a signal (a resize, typically) cut the wait short
        }
        match term::read_some(tty.fd(), &mut buf) {
            Some(0) => break,
            Some(n) => parser.feed(&buf[..n]),
            None => continue,
        }
        while let Some(i) = parser.next(false) {
            match i {
                Input::Graphics(1, err) => p.graphics = err.is_none(),
                Input::Graphics(2, err) => p.shm = err.is_none(),
                Input::CellSize(w, h) if w > 0 && h > 0 => p.cell = Some((w, h)),
                Input::Version(v) => p.version = Some(v),
                Input::DeviceAttributes => break 'wait,
                other @ Input::Key(_) => p.early.push(other),
                _ => {}
            }
        }
    }
    if let Some((name, _)) = shm {
        unsafe { libc::shm_unlink(name.as_ptr()) };
    }
    debug_log!(
        "probe: graphics={} shm={} cell={:?} version={:?}",
        p.graphics,
        p.shm,
        p.cell,
        p.version
    );
    p
}

fn run(path: PathBuf, start_page: Option<usize>) -> Result<(), String> {
    let (wake_r, wake_w) = term::wake_pipe().map_err(|e| e.to_string())?;
    let (tx, rx) = channel();
    let worker = Worker::spawn(path.clone(), tx.clone(), wake_w);

    let mut tty = Tty::open().map_err(|e| format!("cannot use the terminal: {e}"))?;
    std::panic::set_hook(Box::new(|info| {
        term::leave();
        eprintln!("farleaf: {info}");
    }));
    term::install_signals();
    let _ = term::write_all(&mut tty.file, b"\x1b[?1049h\x1b[?25l\x1b[2J");
    term::entered();

    // herdr says OK to the shared-memory probe but then shows no image sent
    // that way (0.9.3, tested 2026-10-14), so under herdr it is never used.
    // herdr's panes answer XTVERSION with "libghostty", which also gets
    // through SSH, unlike herdr's environment variables.
    let herdr_env =
        std::env::var("TERM_PROGRAM").is_ok_and(|v| v == "herdr") || std::env::var("HERDR_ENV").is_ok_and(|v| v == "1");
    let mut parser = Parser::default();
    let probe = probe(&mut tty, &mut parser, !herdr_env);
    let herdr = herdr_env || probe.version.as_deref().is_some_and(|v| v.starts_with("libghostty"));
    if !probe.graphics {
        term::leave();
        return Err(
            "this terminal does not show Kitty graphics (use Ghostty, kitty or WezTerm; \
                    inside herdr, keep its kitty_graphics setting on)"
                .into(),
        );
    }
    let transport = match std::env::var("FARLEAF_TRANSPORT").as_deref() {
        Ok("shm") => Transport::Shm,
        Ok("png") => Transport::Png,
        Ok("zlib") => Transport::Zlib,
        _ if herdr => Transport::Zlib,
        _ if probe.shm => Transport::Shm,
        _ => Transport::Png,
    };
    let budget = if herdr { HERDR_BUDGET } else { GHOSTTY_BUDGET };

    // The document has been loading on the worker while we probed.
    let sizes = loop {
        match rx.recv() {
            Ok(Event::Doc(Ok(sizes))) if !sizes.is_empty() => break sizes,
            Ok(Event::Doc(Ok(_))) => {
                term::leave();
                return Err(format!("{}: the PDF has no pages", path.display()));
            }
            Ok(Event::Doc(Err(e))) => {
                term::leave();
                return Err(e);
            }
            Ok(_) => continue,
            Err(_) => {
                term::leave();
                return Err("the render thread stopped".into());
            }
        }
    };
    let out = tty.file.try_clone().map_err(|e| e.to_string())?;
    let writer = Writer::spawn(out, tx, wake_w);
    let mut app = App::new(
        tty, wake_r, rx, worker, writer, transport, budget, path, sizes, probe.cell,
    );
    if let Some(p) = start_page {
        app.goto_page(p.saturating_sub(1), false);
    }
    for i in probe.early {
        app.input(i);
    }
    app.say(&format!(
        "{}: {} pages, {} transport. ? shows the keys",
        app.name,
        app.sizes.len(),
        transport.name()
    ));
    app.run();
    app.finish();
    term::leave();
    debug_log!(
        "exit: {}, max RSS {} MB",
        app.error.as_deref().unwrap_or("ok"),
        max_rss() >> 20
    );
    app.error.map_or(Ok(()), Err)
}

/// Peak resident memory in bytes.
fn max_rss() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let v = ru.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        v
    } else {
        v * 1024
    }
}

enum St {
    /// Asked of the render worker under this image id.
    Rendering(u32),
    /// Encoded and queued for upload; the terminal will hold this many bytes.
    Queued(usize),
    /// In the terminal, under this id; `seen` is the last frame it was on screen.
    Shown { id: u32, mem: usize, seen: u64 },
}

#[derive(Clone, Copy)]
struct Pos {
    page: usize,
    frac: f32,
}

/// The open sidebar: what it lists, the first item on screen, and, while it
/// has the keys, the selected item and where the view was before (Esc goes
/// back there).
struct Side {
    kind: Kind,
    first: usize,
    focus: Option<Focus>,
    /// The outline entry last gone to, and the view's top then: it stays the
    /// current one until the view moves.
    chosen: Option<(usize, u32)>,
}

struct Focus {
    sel: usize,
    origin: Pos,
    origin_left: u32,
}

#[derive(Default)]
struct Search {
    needle: String,
    backward: bool,
    seq: u64,
    /// The direction of the request in flight, and whether it is the first
    /// one of this search.
    asked: Option<(bool, bool)>,
    hits: HashMap<usize, Vec<Vec<[f32; 4]>>>,
    cur: Option<(usize, usize)>,
}

struct App {
    tty: Tty,
    wake_r: RawFd,
    rx: Receiver<Event>,
    worker: Worker,
    writer: Option<Writer>,
    transport: Transport,
    /// Image bytes the terminal may hold (see GHOSTTY_BUDGET).
    budget: usize,
    path: PathBuf,
    name: String,
    sizes: Vec<(f32, f32)>,
    outline: Vec<mupdf::Entry>,
    /// The outline has been read (it comes after the first screen).
    outline_ready: bool,
    side: Option<Side>,
    /// Thumbnail sizes for the sidebar (set by relayout).
    thumbs: Thumbs,
    /// The sidebar text last drawn, so unchanged text is not sent again.
    side_drawn: Vec<u8>,
    screen: term::Size,
    cell: (u32, u32),
    cell_queried: bool,
    /// Render resolution relative to the screen (see Geom::scale).
    scale: f32,
    fit: Fit,
    zoom: f32,
    invert: bool,
    layout: Layout,
    /// Changes when the pages are cut into images differently.
    gen: u32,
    /// Changes when what the pages look like changes (inversion, reload):
    /// thumbnails from before are stale too.
    content_gen: u32,
    top: u32,
    left: u32,
    strips: HashMap<StripKey, St>,
    by_id: HashMap<u32, StripKey>,
    next_id: u32,
    /// Bytes of the images queued for, or held by, the terminal.
    term_mem: usize,
    on_screen: HashSet<StripKey>,
    /// Pieces the terminal refused, and how often: retried, then given up on.
    failures: HashMap<StripKey, u32>,
    tick: u64,
    /// On screen: (image, placement) -> row, col, source x y w h, cols, rows.
    placed: HashMap<(u32, u32), [u32; 8]>,
    /// The key list popup, open at this scroll position.
    help: Option<usize>,
    /// Where a closed popup was, to blank.
    erase: Option<Rect>,
    to_free: Vec<u32>,
    wanted: Vec<StripKey>,
    page_ver: Vec<u32>,
    parser: Parser,
    count: Option<u32>,
    pending: Option<char>,
    prompt: Option<(char, String)>,
    search: Search,
    marks: HashMap<char, Pos>,
    jumps: Vec<Pos>,
    jump_at: usize,
    msg: Option<(String, Instant)>,
    dirty: bool,
    clear: bool,
    stamp: Option<(SystemTime, u64)>,
    last_poll: Option<(SystemTime, u64)>,
    next_poll: Instant,
    quit: bool,
    error: Option<String>,
    /// When the last key arrived, until the screen it asked for is complete.
    key_at: Option<Instant>,
}

fn file_stamp(path: &PathBuf) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        tty: Tty,
        wake_r: RawFd,
        rx: Receiver<Event>,
        worker: Worker,
        writer: Writer,
        transport: Transport,
        budget: usize,
        path: PathBuf,
        sizes: Vec<(f32, f32)>,
        cell: Option<(u32, u32)>,
    ) -> App {
        let screen = tty.size();
        let cell_queried = cell.is_some();
        let cell = cell.unwrap_or_else(|| cell_from(screen).unwrap_or((10, 20)));
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut app = App {
            tty,
            wake_r,
            rx,
            worker,
            writer: Some(writer),
            transport,
            budget,
            stamp: file_stamp(&path),
            last_poll: None,
            next_poll: Instant::now(),
            path,
            name,
            page_ver: vec![0; sizes.len()],
            layout: Layout::new(
                &sizes,
                Geom {
                    cols: 1,
                    rows: 1,
                    cw: 1,
                    ch: 1,
                    scale: 1.0,
                },
                Fit::Width,
                1.0,
            ),
            sizes,
            outline: Vec::new(),
            outline_ready: false,
            side: None,
            thumbs: Thumbs::default(),
            side_drawn: Vec::new(),
            screen,
            cell,
            cell_queried,
            scale: 1.0,
            fit: Fit::Width,
            zoom: 1.0,
            invert: false,
            gen: 0,
            content_gen: 0,
            top: 0,
            left: 0,
            strips: HashMap::new(),
            by_id: HashMap::new(),
            // 1 and 2 are the probe's.
            next_id: 16,
            term_mem: 0,
            on_screen: HashSet::new(),
            failures: HashMap::new(),
            tick: 0,
            placed: HashMap::new(),
            help: None,
            erase: None,
            to_free: Vec::new(),
            wanted: Vec::new(),
            parser: Parser::default(),
            count: None,
            pending: None,
            prompt: None,
            search: Search::default(),
            marks: HashMap::new(),
            jumps: Vec::new(),
            jump_at: 0,
            msg: None,
            dirty: true,
            clear: false,
            quit: false,
            error: None,
            key_at: None,
        };
        app.relayout(true);
        app.clear = false;
        app
    }

    fn writer(&self) -> &Writer {
        self.writer.as_ref().expect("writer runs until finish()")
    }

    /// Columns the sidebar takes (0 when closed); the pages get the rest.
    fn side_cols(&self) -> u32 {
        match self.side {
            Some(_) => sidebar::width(self.screen.cols, self.cell.0).unwrap_or(0),
            None => 0,
        }
    }

    fn geom(&self) -> Geom {
        Geom {
            cols: self.screen.cols.saturating_sub(self.side_cols()).max(1),
            rows: self.screen.rows.saturating_sub(1).max(1),
            cw: self.cell.0.max(1),
            ch: self.cell.1.max(1),
            scale: self.scale,
        }
    }

    fn run(&mut self) {
        let mut buf = vec![0u8; 1 << 16];
        let fd = self.tty.fd();
        while !self.quit {
            self.update();
            let timeout = if self.parser.pending() { 25 } else { 500 };
            let ready = term::poll(&[fd, self.wake_r], timeout);
            if ready[0] {
                match term::read_some(fd, &mut buf) {
                    Some(0) => break, // the terminal went away
                    Some(n) => self.parser.feed(&buf[..n]),
                    None => {}
                }
            }
            let quiet = !ready[0] && !ready[1];
            while let Some(i) = self.parser.next(quiet) {
                self.input(i);
            }
            if ready[1] {
                let mut w = [0u8; 64];
                while let Some(n @ 1..) = term::read_some(self.wake_r, &mut w) {
                    for &b in &w[..n] {
                        match b {
                            b'W' => self.resized(),
                            b'T' => self.quit = true,
                            _ => {}
                        }
                    }
                }
            }
            while let Ok(e) = self.rx.try_recv() {
                self.event(e);
            }
            self.housekeeping();
        }
    }

    fn finish(&mut self) {
        let mut out = Vec::new();
        for st in self.strips.values() {
            if let St::Shown { id, .. } = st {
                encode::free(&mut out, *id);
            }
        }
        if let Some(w) = self.writer.take() {
            w.finish(out);
        }
    }

    fn input(&mut self, i: Input) {
        match i {
            Input::Key(k) => {
                debug_log!("key {k:?}");
                self.key_at = Some(Instant::now());
                self.key(k)
            }
            Input::CellSize(w, h) if w > 0 && h > 0 => {
                self.cell_queried = true;
                if (w, h) != self.cell {
                    self.cell = (w, h);
                    self.relayout(false);
                }
            }
            Input::Graphics(id, Some(err)) => self.refused(id, &err),
            _ => {}
        }
    }

    /// The terminal reported an error for image `id`: it refused it, or it
    /// evicted it and so could not place it. Forget it, so it is rendered and
    /// sent again; after three failures, give up on that piece and say why.
    fn refused(&mut self, id: u32, err: &str) {
        let Some(key) = self.by_id.remove(&id) else {
            return; // from before a relayout
        };
        debug_log!(
            "terminal error for image {id} (p{} k{} t{}): {err}",
            key.page + 1,
            key.k,
            key.t
        );
        if let Some(St::Queued(mem) | St::Shown { mem, .. }) = self.strips.remove(&key) {
            self.term_mem -= mem;
        }
        self.placed.retain(|k, _| k.0 != id);
        self.to_free.push(id);
        let n = self.failures.entry(key).or_insert(0);
        *n += 1;
        if *n == 3 {
            self.say(&format!("the terminal refused an image: {err}"));
        }
        self.wanted.clear(); // so update() asks for it again
        self.dirty = true;
    }

    fn event(&mut self, e: Event) {
        match e {
            Event::Strip(key, id, payload) => {
                debug_log!("strip ready p{} k{} t{} id {id}", key.page + 1, key.k, key.t);
                // Anything else is superseded; dropping it unlinks any shared memory.
                if matches!(self.strips.get(&key), Some(St::Rendering(r)) if *r == id) {
                    self.make_room(payload.mem);
                    self.strips.insert(key, St::Queued(payload.mem));
                    self.term_mem += payload.mem;
                    self.writer().upload(id, payload);
                }
            }
            Event::Sent(id) => {
                debug_log!("sent {id}");
                let Some(key) = self.by_id.get(&id).copied() else {
                    self.to_free.push(id); // uploaded after a relayout forgot it
                    self.dirty = true;
                    return;
                };
                if let Some(st) = self.strips.get_mut(&key) {
                    if let St::Queued(mem) = *st {
                        *st = St::Shown {
                            id,
                            mem,
                            seen: self.tick,
                        };
                        if self.on_screen.contains(&key) {
                            self.dirty = true;
                        }
                    }
                }
            }
            Event::Doc(Ok(sizes)) if !sizes.is_empty() => {
                self.sizes = sizes;
                self.page_ver = vec![0; self.sizes.len()];
                self.search.hits.clear();
                self.search.cur = None;
                if let Some(f) = self.side.as_mut().and_then(|s| s.focus.as_mut()) {
                    f.sel = 0;
                }
                self.relayout(true);
                self.say("reloaded");
            }
            Event::Doc(Ok(_)) => self.say("reload: the PDF has no pages"),
            Event::Outline(outline) => {
                debug_log!("outline: {} entries", outline.len());
                self.outline = outline;
                self.outline_ready = true;
                let len = self.outline.len();
                if let Some(side) = self.side.as_mut().filter(|s| s.kind == Kind::Outline) {
                    if let Some(f) = side.focus.as_mut() {
                        f.sel = f.sel.min(len.saturating_sub(1));
                    }
                    self.dirty = true;
                }
            }
            Event::Doc(Err(e)) => self.say(&format!("reload failed: {e}")),
            Event::Found { seq, hit } => self.found(seq, hit),
            Event::WriteFailed(e) => {
                self.error = Some(format!("lost the terminal: {e}"));
                self.quit = true;
            }
        }
    }

    fn housekeeping(&mut self) {
        if self
            .msg
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(4))
        {
            self.msg = None;
            self.dirty = true;
        }
        if Instant::now() >= self.next_poll {
            self.next_poll = Instant::now() + Duration::from_millis(500);
            // Reload once the file has stopped changing for one poll, so a
            // LaTeX run is read when it is done, not halfway.
            let now = file_stamp(&self.path);
            if now.is_some() && now != self.stamp && now == self.last_poll {
                self.stamp = now;
                self.worker.push(Job::Reload);
            }
            self.last_poll = now;
        }
    }

    fn resized(&mut self) {
        self.screen = self.tty.size();
        if !self.cell_queried {
            if let Some(c) = cell_from(self.screen) {
                self.cell = c;
            }
        }
        // The font may have changed size too; ask again.
        self.writer().frame(b"\x1b[16t".to_vec());
        if sidebar::width(self.screen.cols, self.cell.0).is_none() {
            self.side = None;
        }
        self.relayout(false);
    }

    fn say(&mut self, m: &str) {
        self.msg = Some((m.to_string(), Instant::now()));
        self.dirty = true;
    }

    // --- position ----------------------------------------------------------------

    fn pos(&self) -> Pos {
        let p = self.layout.page_at(self.top);
        let b = self.layout.pages[p];
        Pos {
            page: p,
            frac: (self.top as f32 - b.y as f32) / b.rows as f32,
        }
    }

    fn set_pos(&mut self, pos: Pos) {
        let p = pos.page.min(self.layout.pages.len() - 1);
        let b = self.layout.pages[p];
        self.top = (b.y as f32 + pos.frac * b.rows as f32).round().max(0.0) as u32;
        self.clamp();
    }

    fn clamp(&mut self) {
        self.top = self.top.min(self.layout.max_top());
        self.dirty = true;
    }

    fn scroll(&mut self, rows: i64) {
        self.top = (self.top as i64 + rows).clamp(0, self.layout.max_top() as i64) as u32;
        self.dirty = true;
    }

    fn goto_page(&mut self, p: usize, jump: bool) {
        if jump {
            self.push_jump();
        }
        self.top = self.layout.pages[p.min(self.layout.pages.len() - 1)].y;
        self.clamp();
    }

    fn page_step(&mut self, n: i64) {
        let cur = self.layout.page_at(self.top);
        let target = if n < 0 && self.top > self.layout.pages[cur].y {
            cur as i64 + n + 1 // first go to the top of the current page
        } else {
            cur as i64 + n
        };
        self.goto_page(target.clamp(0, self.layout.pages.len() as i64 - 1) as usize, false);
    }

    fn pan(&mut self, n: i64) {
        let g = self.geom();
        let cols = self.layout.pages[self.layout.page_at(self.top)].cols;
        let max = cols.saturating_sub(g.cols) as i64;
        self.left = (self.left as i64 + n * (g.cols as i64 / 8).max(1)).clamp(0, max) as u32;
        self.dirty = true;
    }

    fn pannable(&self) -> bool {
        self.layout.pages[self.layout.page_at(self.top)].cols > self.geom().cols
    }

    fn push_jump(&mut self) {
        self.push_jump_at(self.pos());
    }

    fn push_jump_at(&mut self, pos: Pos) {
        self.jumps.truncate(self.jump_at);
        self.jumps.push(pos);
        if self.jumps.len() > 100 {
            self.jumps.remove(0);
        }
        self.jump_at = self.jumps.len();
    }

    fn set_view(&mut self, fit: Fit, zoom: f32) {
        let zoom = zoom.clamp(0.25, 4.0);
        if fit != self.fit || (zoom - self.zoom).abs() > 1e-3 {
            self.fit = fit;
            self.zoom = zoom;
            self.relayout(false);
        }
    }

    /// The view changed size or shape, or (`content_changed`) the pages look
    /// different: lay out again, keeping the reading position, and forget the
    /// images that no longer fit. Pages cut into the same pieces as before
    /// keep theirs (a narrower view in fit-page mode, say), and so do
    /// thumbnails of the same size, unless the content changed.
    fn relayout(&mut self, content_changed: bool) {
        let pos = self.pos();
        if content_changed {
            self.content_gen = self.content_gen.wrapping_add(1);
        }
        // Full resolution, unless one screen of images (with the pieces that
        // stick out past its edges, and the sidebar) would not fit in the
        // terminal's image memory with room to spare: then render less
        // sharply, rather than have the terminal evict images that are on screen.
        let bpp = self.transport.bpp();
        let side_bytes = (self.side_cols() * self.cell.0) as usize * (self.geom().rows * self.cell.1) as usize * bpp;
        let limit = (self.budget / 8 * 7).saturating_sub(side_bytes);
        self.scale = 1.0;
        let mut layout = Layout::new(&self.sizes, self.geom(), self.fit, self.zoom);
        for _ in 0..4 {
            let worst = layout.worst_screen_bytes(bpp);
            if worst <= limit {
                break;
            }
            self.scale = (self.scale * (limit as f32 / worst as f32).sqrt() * 0.98).max(0.2);
            layout = Layout::new(&self.sizes, self.geom(), self.fit, self.zoom);
        }
        let keep = !content_changed && self.layout.same_pieces(&layout);
        self.layout = layout;
        debug_log!(
            "layout {}x{} cells of {}x{} px, scale {:.2}, {:?} {:.0}%, screen needs up to {} MB{}",
            self.geom().cols,
            self.geom().rows,
            self.cell.0,
            self.cell.1,
            self.scale,
            self.fit,
            self.zoom * 100.0,
            self.layout.worst_screen_bytes(bpp) >> 20,
            if keep { ", images kept" } else { "" }
        );
        self.thumbs = match sidebar::width(self.screen.cols, self.cell.0) {
            Some(w) => Thumbs::new(&self.sizes, w, self.geom().rows, self.cell.0, self.cell.1),
            None => Thumbs::default(),
        };
        self.set_pos(pos);
        let max_left = self
            .layout
            .pages
            .iter()
            .map(|p| p.cols)
            .max()
            .unwrap_or(0)
            .saturating_sub(self.geom().cols);
        self.left = self.left.min(max_left);
        if !keep {
            self.gen = self.gen.wrapping_add(1);
        }
        let stale: Vec<StripKey> = self
            .strips
            .keys()
            .filter(|k| match k.thumb {
                true => (k.page as usize) >= self.sizes.len() || **k != self.thumb_key(k.page as usize),
                false => k.gen != self.gen,
            })
            .copied()
            .collect();
        for k in stale {
            match self.strips.remove(&k) {
                Some(St::Shown { id, mem, .. }) => {
                    self.to_free.push(id);
                    self.term_mem -= mem;
                }
                Some(St::Queued(mem)) => self.term_mem -= mem,
                _ => {}
            }
        }
        let strips = &self.strips;
        self.by_id.retain(|_, k| strips.contains_key(k));
        if let Some(w) = &self.writer {
            let by_id = &self.by_id;
            w.prune(|id| by_id.contains_key(&id));
        }
        self.placed.clear();
        self.side_drawn.clear();
        self.wanted.clear();
        self.on_screen.clear();
        self.failures.clear();
        self.clear = true;
        self.dirty = true;
    }

    // --- keys ------------------------------------------------------------------------

    fn key(&mut self, k: Key) {
        if self.prompt.is_some() {
            return self.prompt_key(k);
        }
        if self.help.is_some() {
            return self.help_key(k);
        }
        if let Some(p) = self.pending.take() {
            return self.pending_key(p, k);
        }
        if let Key::Char(c @ '0'..='9') = k {
            if c != '0' || self.count.is_some() {
                self.count = Some((self.count.unwrap_or(0) * 10 + c.to_digit(10).unwrap()).min(99_999));
                return;
            }
        }
        let n = self.count.take();
        if self.side.as_ref().is_some_and(|s| s.focus.is_some()) {
            return self.side_key(k, n);
        }
        let n1 = n.unwrap_or(1) as i64;
        let rows = self.geom().rows as i64;
        let line = (rows / 12).max(2);
        match k {
            Key::Char('j') | Key::Down | Key::Ctrl('e') | Key::Ctrl('n') => self.scroll(n1 * line),
            Key::Char('k') | Key::Up | Key::Ctrl('y') | Key::Ctrl('p') => self.scroll(-n1 * line),
            Key::Ctrl('d') => self.scroll(n1 * rows / 2),
            Key::Ctrl('u') => self.scroll(-n1 * rows / 2),
            Key::Ctrl('f') | Key::PageDown | Key::Char(' ') => self.scroll(n1 * rows),
            Key::Ctrl('b') | Key::PageUp | Key::Backspace => self.scroll(-n1 * rows),
            Key::Char('l') | Key::Char('J') => self.page_step(n1),
            Key::Char('h') | Key::Char('K') => self.page_step(-n1),
            Key::Right if self.pannable() => self.pan(n1),
            Key::Left if self.pannable() => self.pan(-n1),
            Key::Right => self.page_step(n1),
            Key::Left => self.page_step(-n1),
            Key::Char('L') => self.pan(n1),
            Key::Char('H') => self.pan(-n1),
            Key::Char('g') => {
                self.pending = Some('g');
                self.count = n;
            }
            Key::Char('G') | Key::End => match n {
                Some(p) => self.goto_page(p as usize - 1, true),
                None => {
                    self.push_jump();
                    self.top = self.layout.max_top();
                    self.dirty = true;
                }
            },
            Key::Home => self.goto_page(0, true),
            Key::Char('z') => self.set_view(Fit::Page, 1.0),
            Key::Char('Z') => self.set_view(Fit::Width, 1.0),
            Key::Char('+') | Key::Char('=') => self.set_view(self.fit, self.zoom * 1.25f32.powi(n1 as i32)),
            Key::Char('-') => self.set_view(self.fit, self.zoom / 1.25f32.powi(n1 as i32)),
            Key::Char('i') => {
                self.invert = !self.invert;
                self.relayout(true);
            }
            Key::Char('o') => self.side_open(Kind::Outline),
            Key::Char('t') => self.side_open(Kind::Thumbs),
            Key::Char('r') => {
                self.worker.push(Job::Reload);
                self.say("reloading…");
            }
            Key::Char(c @ ('/' | ':')) => {
                self.prompt = Some((c, String::new()));
                self.dirty = true;
            }
            Key::Char('?') => self.open_help(),
            Key::Char('n') => self.search_next(!self.search.backward),
            Key::Char('N') => self.search_next(self.search.backward),
            Key::Char(c @ ('m' | '\'' | '`')) => self.pending = Some(c),
            Key::Ctrl('o') => {
                if self.jump_at > 0 {
                    if self.jump_at == self.jumps.len() {
                        self.jumps.push(self.pos());
                    }
                    self.jump_at -= 1;
                    self.set_pos(self.jumps[self.jump_at]);
                }
            }
            Key::Tab => {
                if self.jump_at + 1 < self.jumps.len() {
                    self.jump_at += 1;
                    self.set_pos(self.jumps[self.jump_at]);
                }
            }
            Key::Ctrl('l') => {
                self.placed.clear();
                self.side_drawn.clear();
                self.clear = true;
                self.dirty = true;
            }
            Key::Char('q') | Key::Ctrl('c') => self.quit = true,
            Key::Esc => {
                self.msg = None;
                self.clear_highlights();
            }
            _ => {}
        }
    }

    fn pending_key(&mut self, p: char, k: Key) {
        let n = self.count.take();
        match (p, k) {
            ('g', Key::Char('g')) => self.goto_page(n.map_or(0, |n| n as usize - 1), true),
            ('m', Key::Char(c)) if c.is_ascii_alphabetic() => {
                self.marks.insert(c, self.pos());
                self.say(&format!("mark {c}"));
            }
            ('\'' | '`', Key::Char('\'' | '`')) => {
                let here = self.pos();
                if let Some(last) = self.jumps.pop() {
                    self.set_pos(last);
                }
                self.jumps.push(here);
                self.jump_at = self.jumps.len();
            }
            ('\'' | '`', Key::Char(c)) => match self.marks.get(&c).copied() {
                Some(pos) => {
                    self.push_jump();
                    self.set_pos(pos);
                }
                None => self.say(&format!("no mark {c}")),
            },
            _ => {}
        }
    }

    fn prompt_key(&mut self, k: Key) {
        self.dirty = true;
        let Some((kind, buf)) = self.prompt.as_mut() else {
            return;
        };
        match k {
            Key::Esc | Key::Ctrl('c') => self.prompt = None,
            Key::Backspace => {
                if buf.pop().is_none() {
                    self.prompt = None;
                }
            }
            Key::Ctrl('u') => buf.clear(),
            Key::Char(c) => buf.push(c),
            Key::Enter => {
                let (kind, text) = (*kind, std::mem::take(buf));
                self.prompt = None;
                match kind {
                    '/' => self.search_start(text, false),
                    _ => self.command(text.trim()),
                }
            }
            _ => {}
        }
    }

    fn command(&mut self, cmd: &str) {
        if let Ok(n) = cmd.parse::<usize>() {
            return self.goto_page(n.max(1) - 1, true);
        }
        match cmd {
            "q" | "q!" | "quit" | "wq" | "x" => self.quit = true,
            "noh" | "nohlsearch" => self.clear_highlights(),
            "h" | "help" => self.open_help(),
            "e" | "reload" => {
                self.worker.push(Job::Reload);
                self.say("reloading…");
            }
            "" => {}
            _ => self.say(&format!("not a command: {cmd}")),
        }
    }

    // --- the key list popup --------------------------------------------------------------

    fn open_help(&mut self) {
        self.help = Some(0);
        self.dirty = true;
    }

    /// Where the popup goes: centered over the pages, as big as its text
    /// allows. None if the screen is too small for it.
    fn help_rect(&self) -> Option<Rect> {
        let (cols, rows) = (self.screen.cols, self.screen.rows.saturating_sub(1));
        let w = (help_width() as u32 + 2).min(cols);
        let h = (help_lines().len() as u32 + 2).min(rows);
        if w < 12 || h < 4 {
            return None;
        }
        let (r0, c0) = ((rows - h) / 2, (cols - w) / 2);
        Some(Rect {
            r0,
            c0,
            r1: r0 + h,
            c1: c0 + w,
        })
    }

    fn help_key(&mut self, k: Key) {
        let Some(rect) = self.help_rect() else {
            self.help = None;
            return;
        };
        let page = (rect.r1 - rect.r0 - 2) as usize;
        let max = help_lines().len().saturating_sub(page);
        let at = self.help.unwrap_or(0);
        self.help = match k {
            Key::Char('?' | 'q') | Key::Esc | Key::Enter => {
                self.erase = Some(rect);
                None
            }
            Key::Ctrl('c') => {
                self.quit = true;
                None
            }
            Key::Char('j') | Key::Down | Key::Ctrl('e' | 'n') => Some(at + 1),
            Key::Char('k') | Key::Up | Key::Ctrl('y' | 'p') => Some(at.saturating_sub(1)),
            Key::Ctrl('d' | 'f') | Key::Char(' ') | Key::PageDown => Some(at + page / 2),
            Key::Ctrl('u' | 'b') | Key::PageUp | Key::Backspace => Some(at.saturating_sub(page / 2)),
            Key::Char('g') | Key::Home => Some(0),
            Key::Char('G') | Key::End => Some(max),
            _ => Some(at),
        }
        .map(|s| s.min(max));
        self.dirty = true;
    }

    fn draw_help(&self, out: &mut Vec<u8>, rect: Rect, scroll: usize) {
        const BORDER: &str = "\x1b[0;38;5;244;48;5;235m";
        const TEXT: &str = "\x1b[0;38;5;252;48;5;235m";
        const KEY: &str = "\x1b[0;1;38;5;179;48;5;235m";
        const HEAD: &str = "\x1b[0;1;38;5;110;48;5;235m";
        let iw = (rect.c1 - rect.c0 - 2) as usize;
        let ih = (rect.r1 - rect.r0 - 2) as usize;
        let lines = help_lines();
        let kw = KEYS.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        let rule = |out: &mut Vec<u8>, row: u32, left: &str, label: &str, right: &str| {
            let label: String = label.chars().take(iw).collect();
            let fill = iw - label.chars().count();
            let _ = write!(
                out,
                "\x1b[{};{}H{BORDER}{left}{}{label}{}{right}",
                row + 1,
                rect.c0 + 1,
                "─".repeat(fill / 2),
                "─".repeat(fill - fill / 2)
            );
        };
        rule(out, rect.r0, "┌", " Keys: ? or Esc to close ", "┐");
        for i in 0..ih {
            let parts: Vec<(&str, String)> = match lines.get(scroll + i) {
                Some(HelpLine::Head(t)) => vec![(HEAD, format!(" {t}"))],
                Some(HelpLine::Row(k, t)) => vec![(KEY, format!("   {k:<kw$}")), (TEXT, format!("  {t}"))],
                Some(HelpLine::Note(t)) => vec![(TEXT, format!(" {t}"))],
                Some(HelpLine::Blank) | None => vec![],
            };
            let _ = write!(out, "\x1b[{};{}H{BORDER}│", rect.r0 + i as u32 + 2, rect.c0 + 1);
            let mut left = iw;
            for (style, text) in parts {
                let text: String = text.chars().take(left).collect();
                left -= text.chars().count();
                let _ = write!(out, "{style}{text}");
            }
            let _ = write!(out, "{TEXT}{:left$}{BORDER}│", "");
        }
        let more = lines.len() > ih;
        let label = if more {
            format!(
                " j k to scroll, {}-{} of {} ",
                scroll + 1,
                (scroll + ih).min(lines.len()),
                lines.len()
            )
        } else {
            String::new()
        };
        rule(out, rect.r1 - 1, "└", &label, "┘");
        out.extend_from_slice(b"\x1b[0m");
    }

    // --- the sidebar ---------------------------------------------------------------

    /// `o` or `t`: open that sidebar with the keys, or give it the keys if it
    /// is open, or hide it if it has them already.
    fn side_open(&mut self, kind: Kind) {
        if kind == Kind::Outline && !self.outline_ready {
            return self.say("still reading the outline, a moment");
        }
        if kind == Kind::Outline && self.outline.is_empty() {
            return self.say("this PDF has no outline");
        }
        match &self.side {
            Some(s) if s.kind == kind && s.focus.is_some() => return self.side_close(),
            Some(_) => {}
            None => {
                if sidebar::width(self.screen.cols, self.cell.0).is_none() {
                    return self.say("the window is too narrow for the sidebar");
                }
                self.side = Some(Side {
                    kind,
                    first: 0,
                    focus: None,
                    chosen: None,
                });
                self.relayout(false);
            }
        }
        let sel = match kind {
            Kind::Thumbs => self.layout.page_at(self.top),
            Kind::Outline => self.current_entry().unwrap_or(0),
        };
        // Switching from the other list keeps the place to go back to.
        let (origin, origin_left) = match self.side.as_ref().and_then(|s| s.focus.as_ref()) {
            Some(f) => (f.origin, f.origin_left),
            None => (self.pos(), self.left),
        };
        if let Some(side) = self.side.as_mut() {
            if side.kind != kind {
                side.first = 0;
            }
            side.kind = kind;
            side.focus = Some(Focus {
                sel,
                origin,
                origin_left,
            });
        }
        self.dirty = true;
    }

    fn side_close(&mut self) {
        self.side_leave(true);
        self.side = None;
        self.relayout(false);
    }

    /// The sidebar gives the keys back: at the selected place (`go`), or
    /// back where the view was before.
    fn side_leave(&mut self, go: bool) {
        let Some(side) = self.side.as_mut() else { return };
        let Some(f) = side.focus.take() else { return };
        if go && side.kind == Kind::Outline {
            side.chosen = Some((f.sel, self.top));
        }
        if go {
            let here = self.pos();
            if here.page != f.origin.page || (here.frac - f.origin.frac).abs() > 1e-3 {
                self.push_jump_at(f.origin);
            }
        } else {
            self.set_pos(f.origin);
            self.left = f.origin_left;
        }
        self.dirty = true;
    }

    fn side_key(&mut self, k: Key, n: Option<u32>) {
        let Some(side) = &self.side else { return };
        let sel = side.focus.as_ref().map_or(0, |f| f.sel);
        let rows = self.geom().rows;
        let (len, screenful) = match side.kind {
            Kind::Outline => (self.outline.len(), rows.saturating_sub(1) as usize),
            Kind::Thumbs => (self.sizes.len(), self.thumbs.visible(side.first, rows).len()),
        };
        let (n1, half, full) = (n.unwrap_or(1) as usize, (screenful / 2).max(1), screenful.max(1));
        let to = match k {
            Key::Char('j') | Key::Down | Key::Ctrl('e' | 'n') => Some(sel + n1),
            Key::Char('k') | Key::Up | Key::Ctrl('y' | 'p') => Some(sel.saturating_sub(n1)),
            Key::Ctrl('d') => Some(sel + n1 * half),
            Key::Ctrl('u') => Some(sel.saturating_sub(n1 * half)),
            Key::Ctrl('f') | Key::PageDown | Key::Char(' ') => Some(sel + n1 * full),
            Key::Ctrl('b') | Key::PageUp | Key::Backspace => Some(sel.saturating_sub(n1 * full)),
            Key::Char('g') | Key::Home => Some(0),
            Key::Char('G') | Key::End => Some(n.map_or(usize::MAX, |n| (n as usize).saturating_sub(1))),
            _ => None,
        };
        if let Some(to) = to {
            return self.side_select(to.min(len.saturating_sub(1)));
        }
        match k {
            Key::Enter | Key::Char('l') | Key::Right => self.side_leave(true),
            Key::Esc | Key::Char('h') | Key::Left => self.side_leave(false),
            Key::Char('q') => self.side_close(),
            Key::Char('o') => self.side_open(Kind::Outline),
            Key::Char('t') => self.side_open(Kind::Thumbs),
            Key::Char('?') => self.open_help(),
            Key::Ctrl('c') => self.quit = true,
            _ => {
                // Any other key is for the pages.
                self.side_leave(true);
                self.count = n;
                self.key(k);
            }
        }
    }

    /// Selects item `sel` of the sidebar and shows it, as Preview does.
    fn side_select(&mut self, sel: usize) {
        let Some(side) = self.side.as_mut() else { return };
        let kind = side.kind;
        if let Some(f) = side.focus.as_mut() {
            f.sel = sel;
        }
        match kind {
            Kind::Thumbs => {
                if let Some(b) = self.layout.pages.get(sel) {
                    self.top = b.y;
                }
            }
            Kind::Outline => {
                if let Some(&mupdf::Entry {
                    page: Some(p), frac, ..
                }) = self.outline.get(sel)
                {
                    if p < self.layout.pages.len() {
                        self.top = self.layout.row_in(p, frac).saturating_sub(1);
                    }
                }
            }
        }
        self.clamp();
    }

    /// The outline entry of the place being read: the one last gone to, if
    /// the view has not moved since, else the last one in the top quarter of
    /// the screen or above it.
    fn current_entry(&self) -> Option<usize> {
        if let Some((i, top)) = self.side.as_ref().and_then(|s| s.chosen) {
            if top == self.top && i < self.outline.len() {
                return Some(i);
            }
        }
        let row = self.top + self.layout.geom.rows / 4;
        let p = self.layout.page_at(row);
        let b = self.layout.pages[p];
        let frac = row.saturating_sub(b.y) as f32 / b.rows.max(1) as f32;
        sidebar::current_entry(&self.outline, p, frac)
    }

    /// The sidebar's highlighted item: the selection while it has the keys,
    /// else the page or section being read.
    fn side_sel(&self) -> Option<usize> {
        let side = self.side.as_ref()?;
        if let Some(f) = &side.focus {
            return Some(f.sel);
        }
        match side.kind {
            Kind::Thumbs => Some(self.layout.page_at(self.top)),
            Kind::Outline => self.current_entry(),
        }
    }

    /// Scrolls the sidebar to its highlighted item; returns the thumbnails
    /// now on screen.
    fn side_sync(&mut self) -> Vec<ThumbAt> {
        let rows = self.geom().rows;
        let sel = self.side_sel();
        let Some(side) = self.side.as_mut() else {
            return Vec::new();
        };
        match side.kind {
            Kind::Outline => {
                side.first = sidebar::outline_scroll(side.first, sel.unwrap_or(0), self.outline.len(), rows);
                Vec::new()
            }
            Kind::Thumbs => {
                if let Some(sel) = sel {
                    side.first = self.thumbs.scroll(side.first, sel, rows);
                }
                self.thumbs.visible(side.first, rows)
            }
        }
    }

    fn thumb_key(&self, page: usize) -> StripKey {
        let g = &self.layout.geom;
        StripKey {
            gen: self.content_gen,
            page: page as u32,
            ver: 0,
            k: g.px_x(self.thumbs.size.get(page).map_or(1, |s| s.0)),
            t: g.px_y(self.thumbs.size.get(page).map_or(1, |s| s.1)),
            thumb: true,
        }
    }

    // --- search ------------------------------------------------------------------------

    fn search_start(&mut self, needle: String, backward: bool) {
        if !needle.is_empty() {
            self.clear_highlights();
            self.search.needle = needle;
        }
        if self.search.needle.is_empty() {
            return;
        }
        self.search.backward = backward;
        self.search.cur = None;
        let from = self.layout.page_at(self.top);
        self.find(from, !backward, true);
    }

    fn search_next(&mut self, forward: bool) {
        if self.search.needle.is_empty() {
            return;
        }
        let n = self.layout.pages.len();
        match self.search.cur {
            Some((p, i)) => {
                let count = self.search.hits.get(&p).map_or(0, Vec::len);
                if forward && i + 1 < count {
                    self.goto_match(p, i + 1);
                } else if !forward && i > 0 {
                    self.goto_match(p, i - 1);
                } else {
                    self.find(if forward { (p + 1) % n } else { (p + n - 1) % n }, forward, false);
                }
            }
            None => self.find(self.layout.page_at(self.top), forward, true),
        }
    }

    fn find(&mut self, from: usize, forward: bool, first: bool) {
        self.search.seq += 1;
        self.search.asked = Some((forward, first));
        self.worker.push(Job::Find {
            seq: self.search.seq,
            needle: self.search.needle.clone(),
            from,
            backward: !forward,
        });
        self.say(&format!("searching for {}…", self.search.needle));
    }

    fn found(&mut self, seq: u64, hit: Option<(usize, Vec<Vec<[f32; 4]>>)>) {
        let Some((forward, first)) = self.search.asked.filter(|_| seq == self.search.seq) else {
            return;
        };
        self.search.asked = None;
        let Some((p, matches)) = hit else {
            return self.say(&format!("not found: {}", self.search.needle));
        };
        let row_of = |m: &Vec<[f32; 4]>| self.layout.row_in(p, m[0][1]);
        let i = if forward {
            // A new search starts at the first hit at or below the top of the screen.
            let from = if first && p == self.layout.page_at(self.top) {
                self.top
            } else {
                0
            };
            matches.iter().position(|m| row_of(m) >= from).unwrap_or(0)
        } else {
            matches.len() - 1
        };
        if !self.search.hits.contains_key(&p) {
            self.page_ver[p] = self.page_ver[p].wrapping_add(1);
        }
        self.search.hits.insert(p, matches);
        self.goto_match(p, i);
    }

    fn goto_match(&mut self, p: usize, i: usize) {
        let Some(m) = self.search.hits.get(&p).and_then(|v| v.get(i)) else {
            return;
        };
        let r = m[0];
        let count = self.search.hits[&p].len();
        let b = self.layout.pages[p];
        let g = self.geom();
        self.search.cur = Some((p, i));
        self.push_jump();
        self.top = self.layout.row_in(p, r[1]).saturating_sub(g.rows / 3);
        if b.cols > g.cols {
            let col = self.layout.col_in(p, r[0]);
            self.left = col.saturating_sub(g.cols / 4).min(b.cols - g.cols);
        }
        self.clamp();
        self.say(&format!(
            "{}: {}/{} on page {}",
            self.search.needle,
            i + 1,
            count,
            p + 1
        ));
    }

    fn clear_highlights(&mut self) {
        for p in self.search.hits.keys() {
            self.page_ver[*p] = self.page_ver[*p].wrapping_add(1);
        }
        if !self.search.hits.is_empty() {
            self.dirty = true;
        }
        self.search.hits.clear();
        self.search.cur = None;
    }

    // --- rendering -----------------------------------------------------------------------

    fn key_of(&self, page: usize, k: u32, t: u32) -> StripKey {
        StripKey {
            gen: self.gen,
            page: page as u32,
            ver: self.page_ver[page],
            k,
            t,
            thumb: false,
        }
    }

    /// What should be in the terminal, most wanted first: what is on screen
    /// (pages and thumbnails), then the rest of the pages on screen (nearest
    /// strips first), the next two pages and the previous one, so a scroll or
    /// a page turn usually finds its images already there, then the
    /// thumbnails just off the sidebar. It stops where the terminal's image
    /// memory would run out; past that, prefetching would only evict.
    fn wanted_keys(&self, vis: &[Visible], shown: &[ThumbAt]) -> Vec<StripKey> {
        let bpp = self.transport.bpp();
        let size = |k: &StripKey| match k.thumb {
            true => k.k as usize * k.t as usize * bpp,
            false => self.layout.piece_bytes(k.page as usize, k.k, k.t, bpp),
        };
        let mut out = Vec::new();
        let mut have = HashSet::new();
        let mut bytes = 0;
        let on_screen = vis
            .iter()
            .map(|v| self.key_of(v.page, v.k, v.t))
            .chain(shown.iter().map(|t| self.thumb_key(t.page)));
        for key in on_screen {
            if have.insert(key) {
                bytes += size(&key);
                out.push(key);
            }
        }
        let mut more = Vec::new();
        if let (Some(first), Some(last)) = (vis.first(), vis.last()) {
            let (top, bottom) = (self.top, self.top + self.layout.geom.rows);
            let mut near: Vec<(u32, usize, u32)> = Vec::new();
            for p in first.page..=last.page {
                let b = self.layout.pages[p];
                for k in 0..self.layout.strips(p) {
                    let y0 = b.y + k * b.strip_rows;
                    let y1 = y0 + self.layout.strip_height(p, k);
                    // Below the screen counts a little nearer than above it.
                    let d = if y0 >= bottom {
                        (y0 - bottom) * 2
                    } else {
                        top.saturating_sub(y1) * 3
                    };
                    near.push((d, p, k));
                }
            }
            near.sort_unstable();
            let n = self.layout.pages.len();
            let pages = (last.page + 1..n).take(2).chain(first.page.checked_sub(1));
            let rest = pages.flat_map(|p| (0..self.layout.strips(p)).map(move |k| (0, p, k)));
            for (_, p, k) in near.into_iter().chain(rest) {
                more.extend(self.layout.tiles_in_view(p, self.left).map(|t| self.key_of(p, k, t)));
            }
        }
        if let (Some(a), Some(b)) = (shown.first(), shown.last()) {
            let pages = (b.page + 1..self.sizes.len()).take(3).chain(a.page.checked_sub(1));
            more.extend(pages.map(|p| self.thumb_key(p)));
        }
        for key in more {
            if have.contains(&key) {
                continue;
            }
            let s = size(&key);
            if bytes + s > self.budget {
                break;
            }
            bytes += s;
            have.insert(key);
            out.push(key);
        }
        out
    }

    fn job(&mut self, key: StripKey, id: u32) -> StripJob {
        if key.thumb {
            // The page, fit and centered in the thumbnail's cells.
            let (pw, ph) = self.sizes[key.page as usize];
            let (bw, bh) = (key.k.max(1), key.t.max(1));
            let s = (bw as f32 / pw).min(bh as f32 / ph);
            let (sw, sh) = (
                ((pw * s).round() as u32).clamp(1, bw),
                ((ph * s).round() as u32).clamp(1, bh),
            );
            return StripJob {
                key,
                id,
                bw,
                bh,
                dx: ((bw - sw) / 2) as i32,
                dy: ((bh - sh) / 2) as i32,
                size_w: sw,
                size_h: sh,
                invert: self.invert,
                marks: Arc::new(Vec::new()),
                transport: self.transport,
                bg: Some(sidebar::BG),
            };
        }
        let b = self.layout.pages[key.page as usize];
        let (bw, bh, dx, dy) = self.layout.piece(key.page as usize, key.k, key.t);
        let marks = self
            .search
            .hits
            .get(&(key.page as usize))
            .map(|m| m.iter().flatten().copied().collect())
            .unwrap_or_default();
        StripJob {
            key,
            id,
            bw,
            bh,
            dx,
            dy,
            size_w: b.px_w,
            size_h: b.px_h,
            invert: self.invert,
            marks: Arc::new(marks),
            transport: self.transport,
            bg: None,
        }
    }

    fn update(&mut self) {
        let vis = self.layout.visible(self.top, self.left);
        let shown = self.side_sync();
        let want = self.wanted_keys(&vis, &shown);
        if want != self.wanted {
            self.on_screen = vis
                .iter()
                .map(|v| self.key_of(v.page, v.k, v.t))
                .chain(shown.iter().map(|t| self.thumb_key(t.page)))
                .collect();
            let mut jobs = Vec::new();
            for &key in &want {
                if self.failures.get(&key).is_some_and(|&n| n >= 3) {
                    continue;
                }
                let id = match self.strips.get(&key) {
                    None => {
                        let id = self.next_id;
                        self.next_id = self.next_id.wrapping_add(1).max(16);
                        self.strips.insert(key, St::Rendering(id));
                        self.by_id.insert(id, key);
                        id
                    }
                    Some(St::Rendering(id)) => *id,
                    Some(_) => continue,
                };
                jobs.push(self.job(key, id));
            }
            let busy = self.worker.set_strips(jobs);
            let wanted: HashSet<StripKey> = want.iter().copied().collect();
            let by_id = &mut self.by_id;
            self.strips.retain(|k, st| match st {
                St::Rendering(id) if !wanted.contains(k) && !busy.contains(k) => {
                    by_id.remove(id);
                    false
                }
                _ => true,
            });
            let keep = |id: u32| self.by_id.get(&id).is_some_and(|k| wanted.contains(k));
            for id in self.writer().prune(keep) {
                if let Some(k) = self.by_id.remove(&id) {
                    if let Some(St::Queued(mem)) = self.strips.remove(&k) {
                        self.term_mem -= mem;
                    }
                }
            }
            self.wanted = want;
        }
        if self.dirty {
            self.frame(&vis, &shown);
        }
    }

    /// Frees images the terminal holds until `need` more bytes fit in the
    /// budget: unwanted ones first, longest unseen first, then the least
    /// wanted. Never what is on screen. The deletions go out before the
    /// upload that needs the room (the writer sends frames first).
    fn make_room(&mut self, need: usize) {
        if self.term_mem + need <= self.budget {
            return;
        }
        let rank: HashMap<StripKey, usize> = self.wanted.iter().enumerate().map(|(i, k)| (*k, i)).collect();
        let mut victims: Vec<(bool, i64, StripKey)> = self
            .strips
            .iter()
            .filter_map(|(k, st)| match st {
                St::Shown { seen, .. } if !self.on_screen.contains(k) => Some(match rank.get(k) {
                    None => (false, *seen as i64, *k),
                    Some(&r) => (true, -(r as i64), *k),
                }),
                _ => None,
            })
            .collect();
        victims.sort_unstable_by_key(|&(wanted, order, _)| (wanted, order));
        let mut out = Vec::new();
        for (_, _, k) in victims {
            if self.term_mem + need <= self.budget {
                break;
            }
            if let Some(St::Shown { id, mem, .. }) = self.strips.remove(&k) {
                self.by_id.remove(&id);
                self.placed.retain(|k, _| k.0 != id);
                encode::free(&mut out, id);
                self.term_mem -= mem;
            }
        }
        // Evicted pieces that are still wanted are asked for again the next
        // time the wanted list changes.
        self.writer().frame(out);
    }

    fn frame(&mut self, vis: &[Visible], shown: &[ThumbAt]) {
        self.tick += 1;
        let mut out = Vec::with_capacity(4096);
        out.extend_from_slice(b"\x1b[?2026h");
        if self.clear {
            out.extend_from_slice(b"\x1b_Ga=d,d=a,q=2\x1b\\\x1b[2J");
            self.clear = false;
        }
        for id in self.to_free.drain(..) {
            encode::free(&mut out, id);
        }
        if self.erase.is_some() {
            self.side_drawn.clear(); // the popup was over it, maybe
        }
        if let Some(r) = self.erase.take() {
            for row in r.r0..r.r1 {
                let _ = write!(
                    out,
                    "\x1b[{};{}H\x1b[0m{:w$}",
                    row + 1,
                    r.c0 + 1,
                    "",
                    w = (r.c1 - r.c0) as usize
                );
            }
        }
        if let Some(side) = &self.side {
            let mut text = Vec::new();
            sidebar::View {
                kind: side.kind,
                cols: self.side_cols(),
                rows: self.geom().rows,
                first: side.first,
                sel: self.side_sel(),
                focused: side.focus.is_some(),
                outline: &self.outline,
                thumbs: &self.thumbs,
                shown,
            }
            .draw(&mut text);
            if text != self.side_drawn {
                out.extend_from_slice(&text);
                self.side_drawn = text;
            }
        }
        let popup = self.help.and_then(|s| Some((self.help_rect()?, s)));
        let mut now: HashMap<(u32, u32), [u32; 8]> = HashMap::new();
        let mut missing = false;
        // Pages right of the sidebar, thumbnails in it; each split around
        // the popup, if one is open, so its text shows.
        let x0 = self.side_cols();
        let pages = vis.iter().map(|v| {
            let key = self.key_of(v.page, v.k, v.t);
            let at = Visible {
                screen_col: v.screen_col + x0,
                ..*v
            };
            (key, at)
        });
        let thumbs = shown.iter().map(|t| {
            let at = Visible {
                page: t.page,
                k: 0,
                t: 0,
                a: 0,
                b: t.shown,
                c0: 0,
                c1: self.thumbs.size[t.page].0,
                screen_row: t.row,
                screen_col: t.col,
            };
            (self.thumb_key(t.page), at)
        });
        let pieces: Vec<(StripKey, Visible)> = pages.chain(thumbs).collect();
        let g = self.layout.geom;
        for (key, v) in pieces {
            let Some(St::Shown { id, seen, .. }) = self.strips.get_mut(&key) else {
                missing = true;
                continue;
            };
            *seen = self.tick;
            let parts = match popup {
                Some((hole, _)) => v.around(hole),
                None => vec![v],
            };
            for (n, v) in parts.iter().enumerate() {
                let (sx, sy, sw, sh) = match key.thumb {
                    true => (
                        g.px_x(v.c0),
                        g.px_y(v.a),
                        g.px_x(v.c1) - g.px_x(v.c0),
                        g.px_y(v.b) - g.px_y(v.a),
                    ),
                    false => self.layout.source(v),
                };
                let at = [v.screen_row, v.screen_col, sx, sy, sw, sh, v.c1 - v.c0, v.b - v.a];
                now.insert((*id, n as u32 + 1), at);
            }
        }
        for &(id, p) in self.placed.keys() {
            if !now.contains_key(&(id, p)) {
                encode::unplace(&mut out, id, p);
            }
        }
        for (&(id, p), at) in &now {
            if self.placed.get(&(id, p)) != Some(at) {
                encode::place(&mut out, id, p, at);
            }
        }
        if let Some((rect, scroll)) = popup {
            self.draw_help(&mut out, rect, scroll);
        }
        if std::env::var_os("FARLEAF_LOG").is_some() {
            let mut list: Vec<String> = now
                .iter()
                .map(|((id, _), at)| format!("{id}:{}", at.map(|v| v.to_string()).join(":")))
                .collect();
            list.sort();
            debug_log!("frame {} missing={} {}", self.tick, u8::from(missing), list.join(" "));
        }
        self.placed = now;
        if !missing {
            if let Some(t) = self.key_at.take() {
                debug_log!("screen complete {:.1} ms after key", t.elapsed().as_secs_f64() * 1e3);
            }
        }
        self.status(&mut out, missing);
        out.extend_from_slice(b"\x1b[?2026l");
        self.writer().frame(out);
        self.dirty = false;
    }

    fn status(&self, out: &mut Vec<u8>, loading: bool) {
        let cols = self.screen.cols as usize;
        let _ = write!(out, "\x1b[{};1H\x1b[0m\x1b[2K", self.screen.rows.max(1));
        if let Some((kind, buf)) = &self.prompt {
            let text: String = format!("{kind}{buf}")
                .chars()
                .rev()
                .take(cols.saturating_sub(1))
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let _ = write!(out, "{text}\x1b[?25h");
            return;
        }
        let page = self.layout.page_at(self.top) + 1;
        let view = match (self.fit, (self.zoom * 100.0).round() as u32) {
            (Fit::Width, 100) => "width".to_string(),
            (Fit::Page, 100) => "page".to_string(),
            (Fit::Width, z) => format!("width {z}%"),
            (Fit::Page, z) => format!("page {z}%"),
        };
        let mut right = format!("{page}/{}  {view}", self.layout.pages.len());
        if self.invert {
            right.push_str("  inv");
        }
        if loading {
            right.push_str("  …");
        }
        if let Some(c) = self.count {
            right = format!("{c}  {right}");
        }
        let hint = match &self.side {
            Some(Side {
                kind, focus: Some(_), ..
            }) => Some(match kind {
                Kind::Outline => "contents: j k to move, Enter to go, Esc to go back, o to hide",
                Kind::Thumbs => "pages: j k to move, Enter to go, Esc to go back, t to hide",
            }),
            _ => None,
        };
        let left = match (&self.msg, hint) {
            (Some((m, _)), _) => m.as_str(),
            (None, Some(h)) => h,
            (None, None) => self.name.as_str(),
        };
        let room = cols.saturating_sub(right.chars().count() + 2);
        let left: String = left.chars().take(room).collect();
        let pad = cols.saturating_sub(left.chars().count() + right.chars().count() + 1);
        let _ = write!(out, "\x1b[?25l{left}{:pad$}\x1b[2m{right}\x1b[0m", "");
    }
}

enum HelpLine {
    Head(&'static str),
    Row(&'static str, &'static str),
    Note(&'static str),
    Blank,
}

/// The popup's lines: KEYS with a blank line between sections, and a note.
fn help_lines() -> Vec<HelpLine> {
    let mut out = Vec::new();
    for &(keys, what) in KEYS {
        if keys.is_empty() {
            if !out.is_empty() {
                out.push(HelpLine::Blank);
            }
            out.push(HelpLine::Head(what));
        } else {
            out.push(HelpLine::Row(keys, what));
        }
    }
    out.push(HelpLine::Blank);
    out.push(HelpLine::Note(COUNT_NOTE));
    out
}

/// Columns the popup's text needs, inside its border.
fn help_width() -> usize {
    let kw = KEYS.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let row = KEYS.iter().map(|(_, t)| 3 + kw + 2 + t.len()).max().unwrap_or(0);
    row.max(1 + COUNT_NOTE.len()) + 1
}

/// Cell size from the window's pixel size, when the terminal reports one.
fn cell_from(s: term::Size) -> Option<(u32, u32)> {
    (s.px_w > 0 && s.px_h > 0 && s.cols > 0 && s.rows > 0).then(|| (s.px_w / s.cols, s.px_h / s.rows))
}
