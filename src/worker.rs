//! The background threads.
//!
//! The render worker owns pdfium (which is not thread-safe): it opens the
//! document, renders strips in the order the main thread asks, and runs
//! searches. For a local terminal it renders straight into shared memory and
//! is done; for PNG and zlib, a pool of encoder threads compresses strips
//! while the worker renders the next ones. The writer owns terminal output,
//! so the main thread never blocks on a slow SSH link: small frames
//! (placements, the status line) jump ahead of queued image uploads, and
//! uploads nobody needs any more are dropped before they are sent.

use crate::encode::{self, Payload, ShmBuf, Transport};
use crate::pdfium::{Doc, Entry, Page};
use crate::term;
use std::collections::{HashMap, HashSet, VecDeque};
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
    q: Arc<(Mutex<Queue>, Condvar)>,
}

fn send(tx: &Sender<Event>, wake: RawFd, e: Event) {
    let _ = tx.send(e);
    term::wake(wake, b'E');
}

impl Worker {
    pub fn spawn(path: PathBuf, tx: Sender<Event>, wake: RawFd) -> Worker {
        let q: Arc<(Mutex<Queue>, Condvar)> = Arc::default();
        let q2 = q.clone();
        std::thread::Builder::new()
            .name("render".into())
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

fn render_loop(path: PathBuf, q: Arc<(Mutex<Queue>, Condvar)>, tx: Sender<Event>, wake: RawFd) {
    let open = || -> Result<Doc, String> {
        crate::pdfium::init()?;
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
    // Loaded pages, most recent last. Rendering several strips of one page
    // reuses its parsed content.
    let mut pages: Vec<(u32, Page)> = Vec::new();
    // The outline is read once the first screen is rendered and there is
    // nothing else to do, so the first screen never waits for it (a book's
    // can be long).
    let (mut outline_due, mut rendered) = (true, false);
    let mut pool: Option<Pool> = None;
    let mut cuts: HashMap<(u32, u32), Cut> = HashMap::new();

    enum Work {
        Job(Job),
        Strips(Vec<StripJob>),
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
                    let most = match (s.key.thumb, cuts.get(&(s.key.gen, s.key.page))) {
                        (true, _) | (false, None | Some(Cut::Single)) => 1,
                        (false, Some(Cut::Once(_))) => 2,
                        (false, Some(Cut::Bands)) => usize::MAX,
                    };
                    let band = take_band(&mut g, s, most);
                    break Work::Strips(band);
                }
                if outline_due && rendered {
                    break Work::Outline;
                }
                g = q.1.wait(g).unwrap();
            }
        };
        let band = match work {
            Work::Outline => {
                outline_due = false;
                let t = Instant::now();
                let outline = doc.outline(&doc.page_sizes());
                debug_log!("outline: {} entries in {:.1} ms", outline.len(), ms(t.elapsed()));
                send(&tx, wake, Event::Outline(outline));
                continue;
            }
            Work::Job(Job::Reload) => {
                match open() {
                    Ok(d) => {
                        pages.clear();
                        cuts.clear();
                        doc = d;
                        (outline_due, rendered) = (true, false);
                        send(&tx, wake, Event::Doc(Ok(doc.page_sizes())));
                    }
                    Err(e) => send(&tx, wake, Event::Doc(Err(e))),
                }
                continue;
            }
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
                        let found = doc.page(i)?.find(&needle, match_case);
                        (!found.is_empty()).then_some((i, found))
                    });
                send(&tx, wake, Event::Found { seq, hit });
                continue;
            }
            Work::Strips(band) => band,
        };
        rendered = true;
        let page = band[0].key.page;
        if !pages.iter().any(|(i, _)| *i == page) {
            if let Some(p) = doc.page(page as usize) {
                if pages.len() >= 3 {
                    pages.remove(0);
                }
                pages.push((page, p));
            }
        }
        let Some((_, p)) = pages.iter().find(|(i, _)| *i == page) else {
            let mut g = q.0.lock().unwrap();
            for s in &band {
                g.busy.remove(&s.key);
            }
            continue;
        };
        let t = Instant::now();
        let mut pieces: Vec<(StripJob, Vec<u8>)> = Vec::new();
        let (key, n, px) = (
            band[0].key,
            band.len(),
            band.iter().map(|s| s.bw as f64 * s.bh as f64).sum::<f64>(),
        );
        if let [s] = &band[..] {
            // One strip: a local terminal reads it where it was rendered.
            let len = s.bw as usize * s.bh as usize * 3;
            if s.transport == Transport::Shm {
                if let Some(mut buf) = ShmBuf::new(len) {
                    p.render_into(buf.pixels(), s.bw, s.bh, s.dx, s.dy, s.size_w, s.size_h);
                    let render = t.elapsed();
                    learn(&mut cuts, key, n, px, render);
                    finish_pixels(buf.pixels(), s);
                    log_strip(s, render, Duration::ZERO, 0, buf.pixels());
                    let payload = buf.payload(s.id, s.bw, s.bh);
                    send(&tx, wake, Event::Strip(s.key, s.id, Arc::new(payload)));
                    q.0.lock().unwrap().busy.remove(&s.key);
                    continue;
                }
            }
            let mut rgb = vec![0u8; len];
            p.render_into(&mut rgb, s.bw, s.bh, s.dx, s.dy, s.size_w, s.size_h);
            pieces.push((band.into_iter().next().expect("one strip"), rgb));
        } else {
            // Strips one below the other, in one call, then cut apart.
            let s0 = &band[0];
            let h: u32 = band.iter().map(|s| s.bh).sum();
            let row = s0.bw as usize * 3;
            let mut all = vec![0u8; row * h as usize];
            p.render_into(&mut all, s0.bw, h, s0.dx, s0.dy, s0.size_w, s0.size_h);
            let mut at = 0;
            for s in band {
                let len = row * s.bh as usize;
                pieces.push((s, all[at..at + len].to_vec()));
                at += len;
            }
        }
        learn(&mut cuts, key, n, px, t.elapsed());
        let render = t.elapsed() / pieces.len() as u32;
        for (s, mut rgb) in pieces {
            finish_pixels(&mut rgb, &s);
            if s.transport == Transport::Shm {
                if let Some(mut buf) = ShmBuf::new(rgb.len()) {
                    buf.pixels().copy_from_slice(&rgb);
                    log_strip(&s, render, Duration::ZERO, 0, &rgb);
                    let payload = buf.payload(s.id, s.bw, s.bh);
                    send(&tx, wake, Event::Strip(s.key, s.id, Arc::new(payload)));
                    q.0.lock().unwrap().busy.remove(&s.key);
                    continue;
                }
            }
            // Shared memory that failed falls back to PNG.
            let transport = if s.transport == Transport::Shm {
                Transport::Png
            } else {
                s.transport
            };
            let pool = pool.get_or_insert_with(|| Pool::spawn(q.clone(), tx.clone(), wake));
            let _ = pool.tx.send(Encode {
                job: s,
                rgb,
                render,
                transport,
            });
        }
    }
}

/// Image bytes rendered in one call at most. pdfium redoes some of the work
/// for an image on every call when it scales it up (a scanned page on a big
/// display, or zoomed), so several strips in one call can be much faster: a
/// scanned page 5700 px wide took 410 ms strip by strip, 130 ms eight strips
/// at a time and 100 ms whole. Text pages cost the same either way, and
/// single strips reach the terminal sooner (2026-10-24).
const BAND_BYTES: usize = 32 << 20;

/// How a page is best rendered, learned from its first calls.
#[derive(Clone, Copy)]
enum Cut {
    /// One strip took this long per pixel (ms); two at once come next.
    Once(f64),
    /// Two at once were not much cheaper per pixel: one strip at a time.
    Single,
    /// They were: as many as BAND_BYTES allows.
    Bands,
}

fn learn(cuts: &mut HashMap<(u32, u32), Cut>, key: StripKey, n: usize, px: f64, took: Duration) {
    if key.thumb {
        return;
    }
    if cuts.len() > 4096 {
        cuts.clear();
    }
    let per = ms(took) / px.max(1.0);
    let at = (key.gen, key.page);
    match (cuts.get(&at).copied(), n) {
        (None, 1) => {
            cuts.insert(at, Cut::Once(per));
        }
        (Some(Cut::Once(one)), 2) => {
            let cut = if per < one * 0.7 { Cut::Bands } else { Cut::Single };
            debug_log!(
                "page {}: {:.2} us/kpx alone, {:.2} two at once: {}",
                key.page + 1,
                one * 1e6,
                per * 1e6,
                if matches!(cut, Cut::Bands) {
                    "bands"
                } else {
                    "single strips"
                }
            );
            cuts.insert(at, cut);
        }
        _ => {}
    }
}

/// `first`, and the queued strips right below it on its page (same tile,
/// next rows), `most` strips and BAND_BYTES at most: all to be rendered in
/// one call.
fn take_band(q: &mut Queue, first: StripJob, most: usize) -> Vec<StripJob> {
    let mut band = vec![first];
    let mut bytes = band[0].bw as usize * band[0].bh as usize * 3;
    while band.len() < most {
        let last = band.last().expect("not empty");
        // Only what is wanted next: the queue is in order of need, so look
        // past the other tiles of these rows, but no further than this page,
        // and stop at a strip of this tile that is wanted sooner.
        let mut next = None;
        for (i, j) in q.strips.iter().enumerate() {
            let (a, b) = (&j.key, &last.key);
            if a.thumb || (a.gen, a.page) != (b.gen, b.page) {
                break;
            }
            if a.t != b.t {
                continue;
            }
            let below = (a.ver, a.k) == (b.ver, b.k + 1)
                && (j.bw, j.dx, j.dy) == (last.bw, last.dx, last.dy - last.bh as i32)
                && (j.size_w, j.size_h, j.transport) == (last.size_w, last.size_h, last.transport);
            if below {
                next = Some(i);
            }
            break;
        }
        let Some(i) = next else { break };
        let size = q.strips[i].bw as usize * q.strips[i].bh as usize * 3;
        if bytes + size > BAND_BYTES {
            break;
        }
        bytes += size;
        let j = q.strips.remove(i).expect("found");
        q.busy.insert(j.key);
        band.push(j);
    }
    band
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
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

/// A strip rendered, waiting to be encoded.
struct Encode {
    job: StripJob,
    rgb: Vec<u8>,
    render: Duration,
    transport: Transport,
}

/// Encoder threads: PNG and zlib take longer than rendering (16-18 ms a page
/// against 7-9 ms, for a paper on half a 5K display), so several encode
/// while the worker renders the next strips.
struct Pool {
    tx: SyncSender<Encode>,
}

impl Pool {
    fn spawn(q: Shared, events: Sender<Event>, wake: RawFd) -> Pool {
        let n = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .saturating_sub(2)
            .clamp(1, 8);
        // A short queue: when encoders fall behind, the worker waits rather
        // than piling up rendered strips.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Encode>(n * 2);
        let rx: Arc<Mutex<Receiver<Encode>>> = Arc::new(Mutex::new(rx));
        for i in 0..n {
            let (rx, q, events) = (rx.clone(), q.clone(), events.clone());
            std::thread::Builder::new()
                .name(format!("encode{i}"))
                .spawn(move || loop {
                    let Ok(e) = rx.lock().unwrap().recv() else { return };
                    let t = Instant::now();
                    let payload = encode::transmit(e.transport, e.job.id, &e.rgb, e.job.bw, e.job.bh);
                    log_strip(&e.job, e.render, t.elapsed(), payload.bytes.len(), &e.rgb);
                    send(&events, wake, Event::Strip(e.job.key, e.job.id, Arc::new(payload)));
                    q.0.lock().unwrap().busy.remove(&e.job.key);
                })
                .expect("spawning an encoder thread");
        }
        debug_log!("{n} encoder threads");
        Pool { tx }
    }
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
