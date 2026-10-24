//! The two background threads.
//!
//! The render worker owns pdfium: it opens the document, renders and encodes
//! strips in the order the main thread asks, and runs searches. The writer
//! owns terminal output, so the main thread never blocks on a slow SSH link:
//! small frames (placements, the status line) jump ahead of queued image
//! uploads, and uploads nobody needs any more are dropped before they are sent.

use crate::encode::{self, Payload, Transport};
use crate::pdfium::{Doc, Entry, Page};
use crate::term;
use std::collections::VecDeque;
use std::fs::File;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};

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

/// What the main thread needs of a document: page sizes in points, and the
/// outline.
pub struct DocInfo {
    pub sizes: Vec<(f32, f32)>,
    pub outline: Vec<Entry>,
}

impl DocInfo {
    fn of(doc: &Doc) -> DocInfo {
        let sizes = doc.page_sizes();
        let outline = doc.outline(&sizes);
        DocInfo { sizes, outline }
    }
}

pub enum Event {
    Doc(Result<DocInfo, String>),
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
    busy: Option<StripKey>,
}

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
    /// the key being rendered right now, which is never re-queued.
    pub fn set_strips(&self, jobs: Vec<StripJob>) -> Option<StripKey> {
        let mut q = self.q.0.lock().unwrap();
        let busy = q.busy;
        q.strips = jobs.into_iter().filter(|j| Some(j.key) != busy).collect();
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
            send(&tx, wake, Event::Doc(Ok(DocInfo::of(&d))));
            d
        }
        Err(e) => return send(&tx, wake, Event::Doc(Err(e))),
    };
    // Loaded pages, most recent last. Rendering several strips of one page
    // reuses its parsed content.
    let mut pages: Vec<(u32, Page)> = Vec::new();

    loop {
        let (job, strip) = {
            let mut g = q.0.lock().unwrap();
            loop {
                if let Some(j) = g.jobs.pop_front() {
                    break (Some(j), None);
                }
                if let Some(s) = g.strips.pop_front() {
                    g.busy = Some(s.key);
                    break (None, Some(s));
                }
                g = q.1.wait(g).unwrap();
            }
        };
        match job {
            Some(Job::Reload) => match open() {
                Ok(d) => {
                    pages.clear();
                    doc = d;
                    send(&tx, wake, Event::Doc(Ok(DocInfo::of(&doc))));
                }
                Err(e) => send(&tx, wake, Event::Doc(Err(e))),
            },
            Some(Job::Find {
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
            }
            None => {}
        }
        if let Some(s) = strip {
            let page = s.key.page;
            if !pages.iter().any(|(i, _)| *i == page) {
                if let Some(p) = doc.page(page as usize) {
                    if pages.len() >= 3 {
                        pages.remove(0);
                    }
                    pages.push((page, p));
                }
            }
            if let Some((_, p)) = pages.iter().find(|(i, _)| *i == page) {
                let t0 = std::time::Instant::now();
                let mut rgb = p.render(s.bw, s.bh, s.dx, s.dy, s.size_w, s.size_h);
                let t1 = t0.elapsed();
                highlight(&mut rgb, &s);
                if s.invert {
                    rgb.iter_mut().for_each(|v| *v = 255 - *v);
                }
                if let Some(bg) = s.bg {
                    surround(&mut rgb, &s, bg);
                }
                let payload = encode::transmit(s.transport, s.id, &rgb, s.bw, s.bh);
                debug_log!(
                    "strip id={} p{} k{} t{} {}x{}: render {:.1} ms, encode {:.1} ms, {} KB, crc {:08x}",
                    s.id,
                    page + 1,
                    s.key.k,
                    s.key.t,
                    s.bw,
                    s.bh,
                    t1.as_secs_f64() * 1e3,
                    (t0.elapsed() - t1).as_secs_f64() * 1e3,
                    payload.bytes.len() / 1024,
                    crate::debug_crc(&rgb)
                );
                send(&tx, wake, Event::Strip(s.key, s.id, Arc::new(payload)));
            }
            q.0.lock().unwrap().busy = None;
        }
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
