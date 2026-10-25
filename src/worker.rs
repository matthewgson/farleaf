//! The background threads.
//!
//! The render worker owns the document (MuPDF uses a document from one
//! thread at a time): it opens it, interprets each page once into a display
//! list, hands strips to the render pool in the order the main thread asks,
//! and runs searches. The pool's threads render strips from those display
//! lists in parallel, each straight into shared memory for a terminal on this
//! machine, or into a buffer they then compress to PNG or zlib. The writer
//! owns terminal output, so the main thread never blocks on a slow SSH link:
//! small frames (placements, the status line) jump ahead of queued image
//! uploads, and uploads nobody needs any more are dropped before they are sent.

use crate::encode::{self, Payload, ShmBuf, Transport};
use crate::mupdf::{Doc, Entry, Page};
use crate::term;
use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Identifies one rendering of one strip. `gen` changes with anything that
/// changes every page (size, zoom, inversion); `ver` with the page's search
/// highlights.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StripKey {
    pub gen: u32,
    pub page: u32,
    pub ver: u32,
    /// Strip (row band) and tile (column band) of the page; for a
    /// thumbnail, its size in cells.
    pub k: u32,
    pub t: u32,
    /// A thumbnail of the page, for the sidebar.
    pub thumb: bool,
}

pub struct StripJob {
    pub key: StripKey,
    pub id: u32,
    /// Bitmap size, and where the page sits in it (see Page::render).
    pub bw: u32,
    pub bh: u32,
    pub dx: i32,
    pub dy: i32,
    pub size_w: u32,
    pub size_h: u32,
    pub invert: bool,
    pub marks: Arc<Vec<[f32; 4]>>,
    pub transport: Transport,
    /// The color around the page, where the bitmap is bigger than it
    /// (thumbnails); white otherwise.
    pub bg: Option<[u8; 3]>,
}

pub enum Job {
    /// First page at or after `from` (before it, when `backward`), wrapping,
    /// that contains `needle`.
    Find {
        seq: u64,
        needle: String,
        from: usize,
        backward: bool,
    },
    Reload,
}

pub enum Event {
    /// The document opened (or reopened): its page sizes in points.
    Doc(Result<Vec<(f32, f32)>, String>),
    /// Its outline, read once the first screen is rendered.
    Outline(Vec<Entry>),
    Strip(StripKey, u32, Arc<Payload>),
    Found {
        seq: u64,
        hit: Option<(usize, Vec<Vec<[f32; 4]>>)>,
    },
    Sent(u32),
    WriteFailed(String),
}

#[derive(Default)]
struct Queue {
    strips: VecDeque<StripJob>,
    jobs: VecDeque<Job>,
    /// Strips being rendered or encoded right now.
    busy: HashSet<StripKey>,
}

type Shared = Arc<(Mutex<Queue>, Condvar)>;

pub struct Worker {
    q: Shared,
}

fn send(tx: &Sender<Event>, wake: RawFd, e: Event) {
    let _ = tx.send(e);
    term::wake(wake, b'E');
}

impl Worker {
    pub fn spawn(path: PathBuf, tx: Sender<Event>, wake: RawFd) -> Worker {
        let q: Shared = Arc::default();
        let q2 = q.clone();
        std::thread::Builder::new()
            .name("worker".into())
            .spawn(move || render_loop(path, q2, tx, wake))
            .expect("spawning the render thread");
        Worker { q }
    }

    /// Replaces the strip queue with `jobs` (most wanted first) and returns
    /// the strips being rendered or encoded right now, which are never
    /// queued again.
    pub fn set_strips(&self, jobs: Vec<StripJob>) -> HashSet<StripKey> {
        let mut q = self.q.0.lock().unwrap();
        let busy = q.busy.clone();
        q.strips = jobs.into_iter().filter(|j| !busy.contains(&j.key)).collect();
        self.q.1.notify_one();
        busy
    }

    pub fn push(&self, job: Job) {
        self.q.0.lock().unwrap().jobs.push_back(job);
        self.q.1.notify_one();
    }
}

fn render_loop(path: PathBuf, q: Shared, tx: Sender<Event>, wake: RawFd) {
    let open = || -> Result<Doc, String> {
        crate::mupdf::init()?;
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Doc::open(bytes).map_err(|e| format!("{}: {e}", path.display()))
    };
    let mut doc = match open() {
        Ok(d) => {
            send(&tx, wake, Event::Doc(Ok(d.page_sizes())));
            d
        }
        Err(e) => return send(&tx, wake, Event::Doc(Err(e))),
    };
    // Display lists of recent pages, most recent last: each page is
    // interpreted once, however many strips it is cut into.
    let mut pages: Vec<(u32, Arc<Page>)> = Vec::new();
    // The outline is read once the first screen is rendered and there is
    // nothing else to do, so the first screen never waits for it (a book's
    // can be long).
    let (mut outline_due, mut rendered) = (true, false);
    let pool = Pool::spawn(q.clone(), tx.clone(), wake);
    // Pages found heavy with images: their strips render on the pool.
    let mut heavy: HashSet<u32> = HashSet::new();

    enum Work {
        Job(Job),
        Strip(StripJob),
        Outline,
    }
    loop {
        let work = {
            let mut g = q.0.lock().unwrap();
            loop {
                if let Some(j) = g.jobs.pop_front() {
                    break Work::Job(j);
                }
                if let Some(s) = g.strips.pop_front() {
                    g.busy.insert(s.key);
                    break Work::Strip(s);
                }
                if outline_due && rendered {
                    break Work::Outline;
                }
                g = q.1.wait(g).unwrap();
            }
        };
        match work {
            Work::Outline => {
                outline_due = false;
                let t = Instant::now();
                let outline = doc.outline(&doc.page_sizes());
                debug_log!("outline: {} entries in {:.1} ms", outline.len(), ms(t.elapsed()));
                send(&tx, wake, Event::Outline(outline));
            }
            Work::Job(Job::Reload) => match open() {
                Ok(d) => {
                    pages.clear();
                    heavy.clear();
                    doc = d;
                    (outline_due, rendered) = (true, false);
                    send(&tx, wake, Event::Doc(Ok(doc.page_sizes())));
                }
                Err(e) => send(&tx, wake, Event::Doc(Err(e))),
            },
            Work::Job(Job::Find {
                seq,
                needle,
                from,
                backward,
            }) => {
                let n = doc.page_sizes().len();
                let match_case = needle.chars().any(char::is_uppercase);
                let hit = (0..n)
                    .map(|i| if backward { (from + n - i) % n } else { (from + i) % n })
                    .find_map(|i| {
                        let cached = pages.iter().find(|(p, _)| *p as usize == i).map(|(_, l)| l.clone());
                        let page = cached.or_else(|| doc.page(i).map(Arc::new))?;
                        let found = page.find(&needle, match_case);
                        (!found.is_empty()).then_some((i, found))
                    });
                send(&tx, wake, Event::Found { seq, hit });
            }
            Work::Strip(s) => {
                rendered = true;
                let n = s.key.page;
                let found = pages.iter().position(|(p, _)| *p == n);
                let page = match found {
                    Some(i) => Some(pages[i].1.clone()),
                    None => {
                        let t = Instant::now();
                        let page = doc.page(n as usize).map(Arc::new);
                        debug_log!("page {}: interpreted in {:.1} ms", n + 1, ms(t.elapsed()));
                        if let Some(p) = &page {
                            if pages.len() >= 8 {
                                pages.remove(0);
                            }
                            pages.push((n, p.clone()));
                        }
                        page
                    }
                };
                let Some(page) = page else {
                    q.0.lock().unwrap().busy.remove(&s.key);
                    continue;
                };
                // Pages heavy with images (a scan) render in parallel on the
                // pool. Text renders fastest here, one strip after another:
                // MuPDF's locks make threads wait on each other when each
                // strip is quick. Sends wait while every pool thread is busy,
                // so the queue the main thread rewrites stays where the
                // choosing is.
                if heavy.contains(&n) {
                    let _ = pool.tx.send(Task::Render { job: s, page });
                    continue;
                }
                let len = s.bw as usize * s.bh as usize * 3;
                let kpx = (s.bw as f64 * s.bh as f64 / 1e3).max(1.0);
                let t = Instant::now();
                let shm = (s.transport == Transport::Shm).then(|| ShmBuf::new(len)).flatten();
                let took = match shm {
                    Some(mut buf) => {
                        page.render_into(buf.pixels(), s.bw, s.bh, s.dx, s.dy, s.size_w, s.size_h);
                        let took = t.elapsed();
                        finish_pixels(buf.pixels(), &s);
                        log_strip(&s, took, Duration::ZERO, 0, buf.pixels());
                        let payload = buf.payload(s.id, s.bw, s.bh);
                        send(&tx, wake, Event::Strip(s.key, s.id, Arc::new(payload)));
                        q.0.lock().unwrap().busy.remove(&s.key);
                        took
                    }
                    None => {
                        let mut rgb = vec![0u8; len];
                        page.render_into(&mut rgb, s.bw, s.bh, s.dx, s.dy, s.size_w, s.size_h);
                        let took = t.elapsed();
                        finish_pixels(&mut rgb, &s);
                        let _ = pool.tx.send(Task::Encode {
                            job: s,
                            rgb,
                            render: took,
                        });
                        took
                    }
                };
                let cost = took.as_secs_f64() * 1e6 / kpx;
                if cost > HEAVY_US_PER_KPX {
                    debug_log!("page {}: heavy ({cost:.1} us/kpx), rendered in parallel", n + 1);
                    heavy.insert(n);
                }
            }
        }
    }
}

/// Microseconds per thousand pixels above which a page's strips render in
/// parallel: text takes about 1 (0.5 ms for a strip of 2489x252), a scanned
/// page 13-30, mostly decoding its image again for each strip.
const HEAVY_US_PER_KPX: f64 = 5.0;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Work for the pool: a strip of a heavy page to render (and compress), or a
/// strip the worker rendered, to compress.
enum Task {
    Render {
        job: StripJob,
        page: Arc<Page>,
    },
    Encode {
        job: StripJob,
        rgb: Vec<u8>,
        render: Duration,
    },
}

/// Pool threads, for what takes long: rendering pages heavy with images in
/// parallel, and compressing strips to PNG or zlib (which takes longer than
/// rendering text) while the worker renders the next ones.
struct Pool {
    tx: SyncSender<Task>,
}

impl Pool {
    fn spawn(q: Shared, events: Sender<Event>, wake: RawFd) -> Pool {
        let n = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .saturating_sub(2)
            .clamp(1, 8);
        let (tx, rx) = std::sync::mpsc::sync_channel::<Task>(n);
        let rx: Arc<Mutex<Receiver<Task>>> = Arc::new(Mutex::new(rx));
        for i in 0..n {
            let (rx, q, events) = (rx.clone(), q.clone(), events.clone());
            std::thread::Builder::new()
                .name(format!("pool{i}"))
                .spawn(move || loop {
                    let Ok(task) = rx.lock().unwrap().recv() else { return };
                    let (s, payload) = match task {
                        Task::Render { job, page } => {
                            let p = render(&page, &job);
                            (job, p)
                        }
                        Task::Encode { job, rgb, render } => {
                            let t = Instant::now();
                            let transport = match job.transport {
                                Transport::Shm => Transport::Png, // shared memory failed
                                t => t,
                            };
                            let p = encode::transmit(transport, job.id, &rgb, job.bw, job.bh);
                            log_strip(&job, render, t.elapsed(), p.bytes.len(), &rgb);
                            (job, p)
                        }
                    };
                    send(&events, wake, Event::Strip(s.key, s.id, Arc::new(payload)));
                    q.0.lock().unwrap().busy.remove(&s.key);
                })
                .expect("spawning a pool thread");
        }
        debug_log!("{n} pool threads");
        Pool { tx }
    }
}

/// Renders strip `s` and wraps it for the terminal.
fn render(page: &Page, s: &StripJob) -> Payload {
    let len = s.bw as usize * s.bh as usize * 3;
    let t = Instant::now();
    // A terminal on this machine reads the strip where it was rendered.
    if s.transport == Transport::Shm {
        if let Some(mut buf) = ShmBuf::new(len) {
            page.render_into(buf.pixels(), s.bw, s.bh, s.dx, s.dy, s.size_w, s.size_h);
            finish_pixels(buf.pixels(), s);
            log_strip(s, t.elapsed(), Duration::ZERO, 0, buf.pixels());
            return buf.payload(s.id, s.bw, s.bh);
        }
    }
    let mut rgb = vec![0u8; len];
    page.render_into(&mut rgb, s.bw, s.bh, s.dx, s.dy, s.size_w, s.size_h);
    finish_pixels(&mut rgb, s);
    let rendered = t.elapsed();
    // Shared memory that failed falls back to PNG.
    let transport = if s.transport == Transport::Shm {
        Transport::Png
    } else {
        s.transport
    };
    let payload = encode::transmit(transport, s.id, &rgb, s.bw, s.bh);
    log_strip(s, rendered, t.elapsed() - rendered, payload.bytes.len(), &rgb);
    payload
}

/// Search highlights, inversion, and the background around a thumbnail.
fn finish_pixels(rgb: &mut [u8], s: &StripJob) {
    highlight(rgb, s);
    if s.invert {
        rgb.iter_mut().for_each(|v| *v = 255 - *v);
    }
    if let Some(bg) = s.bg {
        surround(rgb, s, bg);
    }
}

fn log_strip(s: &StripJob, render: Duration, encode: Duration, bytes: usize, rgb: &[u8]) {
    debug_log!(
        "strip id={} p{} k{} t{} {}x{}: render {:.1} ms, encode {:.1} ms, {} KB, crc {:08x}",
        s.id,
        s.key.page + 1,
        s.key.k,
        s.key.t,
        s.bw,
        s.bh,
        ms(render),
        ms(encode),
        bytes / 1024,
        crate::debug_crc(rgb)
    );
}

/// Marks search hits like a highlighter pen: multiply by yellow, so white
/// paper turns yellow and the ink stays dark.
fn highlight(rgb: &mut [u8], s: &StripJob) {
    let (w, h) = (s.bw as i64, s.bh as i64);
    for r in s.marks.iter() {
        let x0 = ((r[0] * s.size_w as f32) as i64 + s.dx as i64 - 1).clamp(0, w);
        let x1 = ((r[2] * s.size_w as f32).ceil() as i64 + s.dx as i64 + 1).clamp(0, w);
        let y0 = ((r[1] * s.size_h as f32) as i64 + s.dy as i64 - 1).clamp(0, h);
        let y1 = ((r[3] * s.size_h as f32).ceil() as i64 + s.dy as i64 + 1).clamp(0, h);
        for y in y0..y1 {
            for x in x0..x1 {
                let i = ((y * w + x) * 3) as usize;
                rgb[i + 2] = (rgb[i + 2] as u16 * 40 / 255) as u8;
                rgb[i + 1] = (rgb[i + 1] as u16 * 225 / 255) as u8;
            }
        }
    }
}

/// Paints `bg` where the bitmap reaches past the page.
fn surround(rgb: &mut [u8], s: &StripJob, bg: [u8; 3]) {
    let (w, h) = (s.bw as i64, s.bh as i64);
    let (x0, y0) = ((s.dx as i64).clamp(0, w), (s.dy as i64).clamp(0, h));
    let (x1, y1) = (
        (s.dx as i64 + s.size_w as i64).clamp(0, w),
        (s.dy as i64 + s.size_h as i64).clamp(0, h),
    );
    for (y, row) in rgb.as_chunks_mut::<3>().0.chunks_mut(w as usize).enumerate() {
        let y = y as i64;
        for (x, px) in row.iter_mut().enumerate() {
            let x = x as i64;
            if y < y0 || y >= y1 || x < x0 || x >= x1 {
                *px = bg;
            }
        }
    }
}

// --- writer ----------------------------------------------------------------------

#[derive(Default)]
struct WriteQueue {
    frames: VecDeque<Vec<u8>>,
    uploads: VecDeque<(u32, Arc<Payload>)>,
    stop: bool,
}

pub struct Writer {
    q: Arc<(Mutex<WriteQueue>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Writer {
    pub fn spawn(mut out: File, tx: Sender<Event>, wake: RawFd) -> Writer {
        let q: Arc<(Mutex<WriteQueue>, Condvar)> = Arc::default();
        let q2 = q.clone();
        let thread = std::thread::Builder::new()
            .name("writer".into())
            .spawn(move || loop {
                let (bytes, upload) = {
                    let mut g = q2.0.lock().unwrap();
                    loop {
                        if !g.frames.is_empty() {
                            let all: Vec<u8> = g.frames.drain(..).flatten().collect();
                            break (all, None);
                        }
                        if let Some((id, p)) = g.uploads.pop_front() {
                            break (Vec::new(), Some((id, p)));
                        }
                        if g.stop {
                            return;
                        }
                        g = q2.1.wait(g).unwrap();
                    }
                };
                let result = match &upload {
                    Some((_, p)) => {
                        p.mark_sent();
                        term::write_all(&mut out, &p.bytes)
                    }
                    None => term::write_all(&mut out, &bytes),
                };
                match result {
                    Ok(()) => {
                        if let Some((id, _)) = upload {
                            send(&tx, wake, Event::Sent(id));
                        }
                    }
                    Err(e) => return send(&tx, wake, Event::WriteFailed(e.to_string())),
                }
            })
            .expect("spawning the writer thread");
        Writer {
            q,
            thread: Some(thread),
        }
    }

    pub fn frame(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        self.q.0.lock().unwrap().frames.push_back(bytes);
        self.q.1.notify_one();
    }

    pub fn upload(&self, id: u32, p: Arc<Payload>) {
        self.q.0.lock().unwrap().uploads.push_back((id, p));
        self.q.1.notify_one();
    }

    /// Drops queued uploads for which `keep` says no; returns their ids.
    pub fn prune(&self, keep: impl Fn(u32) -> bool) -> Vec<u32> {
        let mut g = self.q.0.lock().unwrap();
        let mut dropped = Vec::new();
        g.uploads.retain(|(id, _)| {
            keep(*id) || {
                dropped.push(*id);
                false
            }
        });
        dropped
    }

    /// Writes `last` after anything already queued for display, then stops.
    /// Uploads still waiting are dropped.
    pub fn finish(mut self, last: Vec<u8>) {
        {
            let mut g = self.q.0.lock().unwrap();
            g.uploads.clear();
            g.frames.push_back(last);
            g.stop = true;
        }
        self.q.1.notify_one();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
