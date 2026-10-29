//! The fake terminal's state: text cells with their colors, and Kitty
//! graphics images and placements. It enforces what Ghostty enforces, so a
//! test fails where Ghostty would show nothing: images over 10000 px on a
//! side are refused, and stored images are evicted beyond the storage limit
//! (unplaced ones first, oldest first). Errors are answered unless the
//! command said q=2.

use crate::draw::Text;
use crate::png;
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};

pub type Kv = BTreeMap<String, String>;

fn get<'a>(kv: &'a Kv, k: &str) -> Option<&'a str> {
    kv.get(k).map(String::as_str)
}

fn num(kv: &Kv, k: &str) -> i64 {
    get(kv, k).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// A cell's colors and weight; None is the terminal's default color.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Attr {
    pub fg: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
    pub bold: bool,
    pub dim: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cell {
    pub ch: char,
    pub attr: Attr,
}

const BLANK: Cell = Cell {
    ch: ' ',
    attr: Attr {
        fg: None,
        bg: None,
        bold: false,
        dim: false,
    },
};

pub struct Image {
    pub kv: Kv,
    raw: Option<Vec<u8>>,
    pub w: u32,
    pub h: u32,
    pub mem: usize,
    gen: u64,
    rgb: Option<Vec<u8>>,
    crc: Option<u32>,
}

impl Image {
    /// The pixels as packed RGB, decoded the first time they are needed, so
    /// reading stays as fast as a real terminal's.
    pub fn decoded(&mut self) -> &[u8] {
        if self.rgb.is_none() {
            let raw = self.raw.take().unwrap_or_default();
            let raw = if get(&self.kv, "o") == Some("z") {
                let mut out = Vec::new();
                let _ = flate2::read::ZlibDecoder::new(&raw[..]).read_to_end(&mut out);
                out
            } else {
                raw
            };
            let rgb = match get(&self.kv, "f").unwrap_or("32") {
                "100" => png::decode(&raw).map(|(_, _, rgb)| rgb).unwrap_or_default(),
                "24" => raw,
                _ => png::to_rgb(&raw, 4),
            };
            let want = self.w as usize * self.h as usize * 3;
            let mut rgb = rgb;
            rgb.resize(want, 0);
            self.rgb = Some(rgb);
        }
        self.rgb.as_deref().unwrap_or_default()
    }

    pub fn checksum(&mut self) -> u32 {
        if self.crc.is_none() {
            let mut c = flate2::Crc::new();
            c.update(self.decoded());
            self.crc = Some(c.sum());
        }
        self.crc.unwrap_or(0)
    }
}

pub struct Place {
    pub row: i64,
    pub col: i64,
    pub kv: Kv,
}

/// One placement as shown: image checksum, row, col, cols, rows, and the
/// source rectangle x, y, w, h.
pub type Shown = [i64; 9];

#[derive(Default, Clone, Debug)]
pub struct Stats {
    pub bytes: usize,
    pub uploads: usize,
    pub upload_bytes: usize,
    pub places: usize,
    pub deletes: usize,
    pub frames: usize,
    pub refused: usize,
    pub evicted: usize,
    pub missing_places: usize,
    pub peak_mem: usize,
}

struct Loading {
    kv: Kv,
    data: Vec<u8>,
}

pub struct Term {
    pub cols: u32,
    pub rows: u32,
    pub cw: u32,
    pub ch: u32,
    shm: bool,
    storage_limit: usize,
    max_dim: u32,
    xtversion: String,
    pub images: HashMap<u32, Image>,
    pub places: BTreeMap<(u32, u32), Place>,
    loading: Option<Loading>,
    cur: (i64, i64),
    saved: (i64, i64),
    pub grid: Vec<Vec<Cell>>,
    attr: Attr,
    pub stats: Stats,
    pub replies: Vec<Vec<u8>>,
    apc: Option<Vec<u8>>,
    gen: u64,
    pub mem: usize,
    /// Bumped whenever what the screen shows changed (see `feed`).
    pub version: u64,
    view: u64,
    /// Inside a synchronized update (mode 2026).
    sync: bool,
    /// Whether text changes count as screen changes (not under herdr, which
    /// redraws its own text now and then).
    pub text_counts: bool,
}

impl Term {
    pub fn new(cols: u32, rows: u32, cw: u32, ch: u32, shm: bool, storage_limit: usize, xtversion: &str) -> Term {
        Term {
            cols,
            rows,
            cw,
            ch,
            shm,
            storage_limit,
            max_dim: 10000,
            xtversion: xtversion.to_string(),
            images: HashMap::new(),
            places: BTreeMap::new(),
            loading: None,
            cur: (0, 0),
            saved: (0, 0),
            grid: vec![vec![BLANK; cols as usize]; rows as usize],
            attr: Attr::default(),
            stats: Stats::default(),
            replies: Vec::new(),
            apc: None,
            gen: 0,
            mem: 0,
            version: 0,
            view: 0,
            sync: false,
            text_counts: true,
        }
    }

    pub fn resize(&mut self, cols: u32, rows: u32) {
        self.cols = cols;
        self.rows = rows;
        self.grid = vec![vec![BLANK; cols as usize]; rows as usize];
    }

    fn reply(&mut self, kv: &Kv, msg: &str) {
        let q = get(kv, "q").unwrap_or("0");
        if let Some(i) = get(kv, "i") {
            if q == "0" || (q == "1" && msg != "OK") {
                let extra = get(kv, "p").map(|p| format!(",p={p}")).unwrap_or_default();
                self.replies
                    .push(format!("\x1b_Gi={i}{extra};{msg}\x1b\\").into_bytes());
            }
        }
    }

    fn graphics(&mut self, ctl: &str, payload: &[u8]) {
        let kv: Kv = ctl
            .split(',')
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if let Some(mut job) = self.loading.take() {
            if kv.keys().all(|k| k == "m" || k == "q") {
                job.data.extend_from_slice(payload);
                if get(&kv, "m").unwrap_or("0") == "0" {
                    self.finish(job.kv, None, job.data.len(), job.data);
                } else {
                    self.loading = Some(job);
                }
                return;
            }
            self.stats.refused += 1; // a new command cut the transfer off: it is lost
        }
        match get(&kv, "a").unwrap_or("t") {
            "q" => {
                let ok = match get(&kv, "t").unwrap_or("d") {
                    "d" => true,
                    "s" => self.shm && read_shm(&kv, payload).is_some(),
                    "f" | "t" => read_file(&kv, payload).is_some(),
                    _ => false,
                };
                self.reply(&kv, if ok { "OK" } else { "ENOTSUP:no" });
            }
            "t" | "T" => {
                let medium = get(&kv, "t").unwrap_or("d").to_string();
                if matches!(medium.as_str(), "s" | "f" | "t") {
                    let raw = if medium == "s" {
                        read_shm(&kv, payload)
                    } else {
                        read_file(&kv, payload)
                    };
                    match raw {
                        Some(raw) => self.finish(kv, Some(raw), payload.len(), Vec::new()),
                        None => self.reply(&kv, "EBADF:cannot read the medium"),
                    }
                    return;
                }
                if get(&kv, "m") == Some("1") {
                    self.loading = Some(Loading {
                        kv,
                        data: payload.to_vec(),
                    });
                } else {
                    let n = payload.len();
                    self.finish(kv, None, n, payload.to_vec());
                }
            }
            "p" => {
                self.stats.places += 1;
                let i = num(&kv, "i") as u32;
                if !self.images.contains_key(&i) {
                    self.stats.missing_places += 1;
                    return self.reply(&kv, "ENOENT:no such image");
                }
                let p = num(&kv, "p") as u32;
                let (row, col) = self.cur;
                self.reply(&kv, "OK");
                self.places.insert((i, p), Place { row, col, kv });
            }
            "d" => {
                self.stats.deletes += 1;
                let d = get(&kv, "d").unwrap_or("a");
                match d {
                    "a" | "A" => {
                        self.unplace(|_| true);
                        if d == "A" {
                            let all: Vec<u32> = self.images.keys().copied().collect();
                            self.drop_images(&all);
                        }
                    }
                    "i" | "I" => {
                        let (i, p) = (num(&kv, "i") as u32, num(&kv, "p") as u32);
                        self.unplace(|k| k.0 == i && (p == 0 || k.1 == p));
                        if d == "I" && p == 0 {
                            self.drop_images(&[i]);
                        }
                    }
                    // Images with ids from x to y.
                    "r" | "R" => {
                        let (lo, hi) = (num(&kv, "x") as u32, num(&kv, "y") as u32);
                        self.unplace(|k| (lo..=hi).contains(&k.0));
                        if d == "R" {
                            let ids: Vec<u32> = self.images.keys().copied().filter(|i| (lo..=hi).contains(i)).collect();
                            self.drop_images(&ids);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn drop_images(&mut self, ids: &[u32]) {
        for i in ids {
            if let Some(img) = self.images.remove(i) {
                self.mem -= img.mem;
            }
        }
    }

    /// An image has arrived: `raw` from a medium, or else the base64 `data`.
    fn finish(&mut self, kv: Kv, raw: Option<Vec<u8>>, payload_len: usize, data: Vec<u8>) {
        let raw = raw.unwrap_or_else(|| base64_decode(&data));
        self.stats.uploads += 1;
        self.stats.upload_bytes += payload_len;
        let f = get(&kv, "f").unwrap_or("32");
        let (w, h, bpp) = if f == "100" {
            match png::info(&raw) {
                Some((w, h)) => (w as i64, h as i64, 4), // Ghostty keeps decoded PNGs as RGBA
                None => return self.reply(&kv, "EBADPNG:not a PNG"),
            }
        } else {
            (num(&kv, "s"), num(&kv, "v"), if f == "24" { 3 } else { 4 })
        };
        let max = self.max_dim as i64;
        if w <= 0 || h <= 0 || w > max || h > max {
            self.stats.refused += 1;
            return self.reply(&kv, "EINVAL:dimensions too large");
        }
        let mem = (w * h * bpp) as usize;
        let i = num(&kv, "i") as u32;
        self.drop_images(&[i]); // a re-sent id replaces the image and its placements
        self.unplace(|k| k.0 == i);
        if mem > self.storage_limit {
            self.stats.refused += 1;
            return self.reply(&kv, "ENOMEM:image too big");
        }
        self.evict((self.mem + mem).saturating_sub(self.storage_limit));
        self.gen += 1;
        let reply_kv = kv.clone();
        self.images.insert(
            i,
            Image {
                kv,
                raw: Some(raw),
                w: w as u32,
                h: h as u32,
                mem,
                gen: self.gen,
                rgb: None,
                crc: None,
            },
        );
        self.mem += mem;
        self.stats.peak_mem = self.stats.peak_mem.max(self.mem);
        // a=T: transmit and place at the cursor (farleaf sends a=t, then a=p).
        if get(&reply_kv, "a") == Some("T") {
            self.stats.places += 1;
            let (row, col) = self.cur;
            let p = num(&reply_kv, "p") as u32;
            self.places.insert(
                (i, p),
                Place {
                    row,
                    col,
                    kv: reply_kv.clone(),
                },
            );
        }
        // An image arriving changes nothing on screen until it is placed.
        self.reply(&reply_kv, "OK");
    }

    /// Removes the placements `which` selects.
    fn unplace(&mut self, which: impl Fn(&(u32, u32)) -> bool) {
        self.places.retain(|k, _| !which(k));
    }

    /// Ghostty's policy: unplaced images first, then placed ones, oldest first.
    fn evict(&mut self, mut need: usize) {
        if need == 0 {
            return;
        }
        let placed: std::collections::HashSet<u32> = self.places.keys().map(|k| k.0).collect();
        let mut order: Vec<(bool, u64, u32)> = self
            .images
            .iter()
            .map(|(&i, img)| (placed.contains(&i), img.gen, i))
            .collect();
        order.sort_unstable();
        for (_, _, i) in order {
            if need == 0 {
                break;
            }
            need = need.saturating_sub(self.images[&i].mem);
            self.stats.evicted += 1;
            self.unplace(|k| k.0 == i);
            self.drop_images(&[i]);
        }
    }

    /// What is on screen, one entry per placement. A placement below the
    /// text (z < 0) that an image above it covers entirely cannot be seen
    /// (farleaf's warm-up placements under herdr) and is left out.
    pub fn shown(&mut self) -> Vec<Shown> {
        let seen = self.visible();
        seen.into_iter()
            .filter_map(|(i, s)| {
                let img = self.images.get_mut(&i)?;
                Some([img.checksum() as i64, s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
            })
            .collect()
    }

    /// The placements that can be seen, as (image, [row, col, cols, rows,
    /// x, y, w, h]): all but those below the text that an image above
    /// covers.
    fn visible(&self) -> Vec<(u32, [i64; 8])> {
        let mut all = Vec::new();
        for ((i, _), pl) in &self.places {
            let Some(img) = self.images.get(i) else { continue };
            let (sx, sy) = (num(&pl.kv, "x"), num(&pl.kv, "y"));
            let sw = Some(num(&pl.kv, "w")).filter(|&v| v != 0).unwrap_or(img.w as i64 - sx);
            let sh = Some(num(&pl.kv, "h")).filter(|&v| v != 0).unwrap_or(img.h as i64 - sy);
            let c = Some(num(&pl.kv, "c"))
                .filter(|&v| v != 0)
                .unwrap_or((sw + self.cw as i64 - 1) / self.cw as i64);
            let r = Some(num(&pl.kv, "r"))
                .filter(|&v| v != 0)
                .unwrap_or((sh + self.ch as i64 - 1) / self.ch as i64);
            all.push((num(&pl.kv, "z"), *i, [pl.row, pl.col, c, r, sx, sy, sw, sh]));
        }
        let covers = |a: &[i64; 8], b: &[i64; 8]| {
            a[0] <= b[0] && a[1] <= b[1] && a[0] + a[3] >= b[0] + b[3] && a[1] + a[2] >= b[1] + b[2]
        };
        all.iter()
            .filter(|(z, _, s)| *z >= 0 || !all.iter().any(|(z2, _, o)| *z2 >= 0 && covers(o, s)))
            .map(|&(_, i, s)| (i, s))
            .collect()
    }

    /// What the screen shows, as a hash: the images that can be seen, where
    /// (and which transmission of each), and the text if it counts.
    fn view_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for (i, s) in self.visible() {
            (i, self.images.get(&i).map(|img| img.gen), s).hash(&mut h);
        }
        if self.text_counts {
            self.grid.hash(&mut h);
        }
        h.finish()
    }

    /// The screen as packed RGB: the text (if a font is given), then the
    /// images over it, as terminals draw them.
    pub fn render(&mut self, text: Option<&mut Text>) -> (u32, u32, Vec<u8>) {
        let (cw, ch) = (self.cw as i64, self.ch as i64);
        let (w, h) = (self.cols as i64 * cw, self.rows as i64 * ch);
        let mut fb = vec![30u8; (w * h * 3) as usize];
        if let Some(text) = text {
            for (r, row) in self.grid.iter().enumerate() {
                for (c, cell) in row.iter().enumerate() {
                    text.cell(&mut fb, w as usize, c as i64 * cw, r as i64 * ch, cw, ch, cell);
                }
            }
        }
        // Lower z first, so what is above covers it.
        let mut order: Vec<(i64, (u32, u32))> = self.places.iter().map(|(k, pl)| (num(&pl.kv, "z"), *k)).collect();
        order.sort_unstable();
        for (_, k) in order {
            let (i, pl) = (&k.0, &self.places[&k]);
            let Some(img) = self.images.get_mut(i) else { continue };
            let (iw, ih) = (img.w as i64, img.h as i64);
            let rgb = img.decoded();
            let (sx, sy) = (num(&pl.kv, "x"), num(&pl.kv, "y"));
            let sw = Some(num(&pl.kv, "w")).filter(|&v| v != 0).unwrap_or(iw - sx);
            let sh = Some(num(&pl.kv, "h")).filter(|&v| v != 0).unwrap_or(ih - sy);
            let dw = Some(num(&pl.kv, "c") * cw).filter(|&v| v != 0).unwrap_or(sw);
            let dh = Some(num(&pl.kv, "r") * ch).filter(|&v| v != 0).unwrap_or(sh);
            for dy in 0..dh {
                let y = pl.row * ch + dy;
                if !(0..h).contains(&y) {
                    continue;
                }
                let syy = sy + dy * sh / dh;
                for dx in 0..dw {
                    let x = pl.col * cw + dx;
                    if !(0..w).contains(&x) {
                        continue;
                    }
                    let s = ((syy * iw + sx + dx * sw / dw) * 3) as usize;
                    let d = ((y * w + x) * 3) as usize;
                    if let Some(px) = rgb.get(s..s + 3) {
                        fb[d..d + 3].copy_from_slice(px);
                    }
                }
            }
        }
        (w as u32, h as u32, fb)
    }

    /// The screen's text, row by row.
    pub fn lines(&self) -> Vec<String> {
        self.grid
            .iter()
            .map(|r| r.iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
            .collect()
    }

    pub fn status(&self) -> String {
        self.lines().pop().unwrap_or_default()
    }

    /// Takes in terminal output; returns what could not be used yet (a
    /// sequence cut off by the end of `data`), to be given again with more.
    pub fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        let rest = self.feed_some(data);
        // What looks the same after being redrawn or placed again is no
        // change, and inside a synchronized update nothing is shown yet.
        if !self.sync {
            let h = self.view_hash();
            if h != self.view {
                self.view = h;
                self.version += 1;
            }
        }
        rest
    }

    fn feed_some(&mut self, data: &[u8]) -> Vec<u8> {
        // An unfinished APC (a big image arriving in small reads) is kept, and
        // only the new bytes are searched for its end.
        let data = match self.apc.take() {
            Some(mut apc) => {
                let from = apc.len().saturating_sub(1);
                apc.extend_from_slice(data);
                if find(&apc[from..], b"\x1b\\").is_none() {
                    self.apc = Some(apc);
                    return Vec::new();
                }
                apc
            }
            None => data.to_vec(),
        };
        let n = data.len();
        let mut i = 0;
        while i < n {
            let c = data[i];
            if c == 0x1b {
                if i + 1 >= n {
                    return data[i..].to_vec();
                }
                match data[i + 1] {
                    b'_' => {
                        let Some(j) = find(&data[i..], b"\x1b\\") else {
                            self.apc = Some(data[i..].to_vec());
                            return Vec::new();
                        };
                        let body = &data[i + 2..i + j];
                        if let Some(rest) = body.strip_prefix(b"G") {
                            let (ctl, payload) = match rest.iter().position(|&b| b == b';') {
                                Some(k) => (&rest[..k], &rest[k + 1..]),
                                None => (rest, &rest[rest.len()..]),
                            };
                            self.graphics(&String::from_utf8_lossy(ctl), payload);
                        }
                        i += j + 2;
                    }
                    b'[' => {
                        let mut k = i + 2;
                        while k < n && matches!(data[k], b'0'..=b'9' | b';' | b'?' | b'>' | b'=') {
                            k += 1;
                        }
                        let params = String::from_utf8_lossy(&data[i + 2..k]).into_owned();
                        while k < n && (0x20..=0x2f).contains(&data[k]) {
                            k += 1;
                        }
                        if k >= n {
                            return data[i..].to_vec();
                        }
                        if (0x40..=0x7e).contains(&data[k]) {
                            self.csi(&params, data[k] as char);
                        }
                        i = k + 1;
                    }
                    b'7' => {
                        self.saved = self.cur;
                        i += 2;
                    }
                    b'8' => {
                        self.cur = self.saved;
                        i += 2;
                    }
                    _ => i += 2,
                }
                continue;
            }
            if c >= 0x20 {
                let len = match c {
                    0..=0x7f => 1,
                    0x80..=0xdf => 2,
                    0xe0..=0xef => 3,
                    _ => 4,
                };
                if i + len > n {
                    return data[i..].to_vec(); // a character split across reads
                }
                let ch = std::str::from_utf8(&data[i..i + len])
                    .ok()
                    .and_then(|s| s.chars().next())
                    .unwrap_or('\u{fffd}');
                let (r, col) = self.cur;
                if r >= 0 && col >= 0 && r < self.rows as i64 && col < self.cols as i64 {
                    self.grid[r as usize][col as usize] = Cell { ch, attr: self.attr };
                }
                self.cur = (r, col + 1);
                i += len;
                continue;
            }
            i += 1;
        }
        Vec::new()
    }

    fn csi(&mut self, params: &str, fin: char) {
        let nums: Vec<i64> = if params.is_empty() {
            Vec::new()
        } else {
            params
                .trim_start_matches(['?', '>'])
                .split(';')
                .map(|x| x.parse().unwrap_or(0))
                .collect()
        };
        let first = nums.first().copied();
        match fin {
            'H' => {
                let r = first.filter(|&v| v > 0).unwrap_or(1);
                let c = nums.get(1).copied().filter(|&v| v > 0).unwrap_or(1);
                self.cur = (r - 1, c - 1);
            }
            'J' if first == Some(2) => {
                self.grid = vec![vec![BLANK; self.cols as usize]; self.rows as usize];
            }
            'K' => {
                let r = (self.cur.0.max(0) as usize).min(self.rows as usize - 1);
                self.grid[r] = vec![BLANK; self.cols as usize];
            }
            't' if first == Some(16) => {
                self.replies
                    .push(format!("\x1b[6;{};{}t", self.ch, self.cw).into_bytes());
            }
            'q' if params.starts_with('>') && !self.xtversion.is_empty() => {
                self.replies
                    .push(format!("\x1bP>|{}\x1b\\", self.xtversion).into_bytes());
            }
            'c' if !params.starts_with('>') => self.replies.push(b"\x1b[?62;22c".to_vec()),
            // Status and cursor reports: other viewers (bookokrat) wait for
            // them at startup, as every real terminal answers.
            'n' if params == "5" => self.replies.push(b"\x1b[0n".to_vec()),
            'n' if params == "6" => self
                .replies
                .push(format!("\x1b[{};{}R", self.cur.0 + 1, self.cur.1 + 1).into_bytes()),
            'h' if params == "?2026" => {
                self.stats.frames += 1;
                self.sync = true;
            }
            'l' if params == "?2026" => self.sync = false,
            'm' if !params.starts_with(['?', '>']) => {
                self.attr = sgr(self.attr, if nums.is_empty() { &[0] } else { &nums });
            }
            _ => {}
        }
    }
}

/// The 256-color palette, xterm's.
pub fn color256(n: i64) -> [u8; 3] {
    const BASE: [[u8; 3]; 16] = [
        [0, 0, 0],
        [205, 49, 49],
        [13, 188, 121],
        [229, 229, 16],
        [36, 114, 200],
        [188, 63, 188],
        [17, 168, 205],
        [229, 229, 229],
        [102, 102, 102],
        [241, 76, 76],
        [35, 209, 139],
        [245, 245, 67],
        [59, 142, 234],
        [214, 112, 214],
        [41, 184, 219],
        [255, 255, 255],
    ];
    let n = n.clamp(0, 255) as usize;
    if n < 16 {
        return BASE[n];
    }
    if n < 232 {
        const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let n = n - 16;
        return [STEPS[n / 36], STEPS[n / 6 % 6], STEPS[n % 6]];
    }
    let v = (8 + (n - 232) * 10) as u8;
    [v, v, v]
}

/// The attributes after SGR parameters `nums`.
fn sgr(mut a: Attr, nums: &[i64]) -> Attr {
    let mut i = 0;
    while i < nums.len() {
        match nums[i] {
            0 => a = Attr::default(),
            1 => a.bold = true,
            2 => a.dim = true,
            22 => {
                a.bold = false;
                a.dim = false;
            }
            n @ (38 | 48) => {
                let c = match nums.get(i + 1) {
                    Some(5) if i + 2 < nums.len() => {
                        i += 2;
                        Some(color256(nums[i]))
                    }
                    Some(2) if i + 4 < nums.len() => {
                        let c = [nums[i + 2] as u8, nums[i + 3] as u8, nums[i + 4] as u8];
                        i += 4;
                        Some(c)
                    }
                    _ => None,
                };
                if n == 38 {
                    a.fg = c;
                } else {
                    a.bg = c;
                }
            }
            39 => a.fg = None,
            49 => a.bg = None,
            n @ (30..=37 | 90..=97) => a.fg = Some(color256(if n < 90 { n - 30 } else { n - 82 })),
            n @ (40..=47 | 100..=107) => a.bg = Some(color256(if n < 100 { n - 40 } else { n - 92 })),
            _ => {}
        }
        i += 1;
    }
    a
}

pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Base64, fast: herdr re-sends whole screens of raw pixels this way, and a
/// slow decoder here would show up as latency that is not farleaf's.
pub fn base64_decode(data: &[u8]) -> Vec<u8> {
    const BAD: u8 = 255;
    static TABLE: std::sync::OnceLock<[u8; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [BAD; 256];
        for (i, &c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
            .iter()
            .enumerate()
        {
            t[c as usize] = i as u8;
        }
        t
    });
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let (quads, _) = data.as_chunks::<4>();
    let mut done = 0;
    for q in quads {
        let [a, b, c, d] = q.map(|x| t[x as usize]);
        if (a | b | c | d) == BAD {
            break; // padding or anything unusual: the careful way below
        }
        let n = (a as u32) << 18 | (b as u32) << 12 | (c as u32) << 6 | d as u32;
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
        done += 4;
    }
    let (mut acc, mut bits) = (0u32, 0);
    for &c in &data[done..] {
        let v = t[c as usize];
        if v == BAD {
            continue;
        }
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn medium_path(payload: &[u8]) -> String {
    String::from_utf8_lossy(&base64_decode(payload)).into_owned()
}

/// t=s: POSIX shared memory, which the terminal unlinks after reading.
fn read_shm(kv: &Kv, payload: &[u8]) -> Option<Vec<u8>> {
    let name = std::ffi::CString::new(medium_path(payload)).ok()?;
    unsafe {
        let fd = libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0);
        if fd < 0 {
            return None;
        }
        let mut st: libc::stat = std::mem::zeroed();
        let size = if libc::fstat(fd, &mut st) == 0 {
            st.st_size as usize
        } else {
            0
        };
        let want = get(kv, "S").and_then(|v| v.parse().ok()).unwrap_or(size).min(size);
        let mut out = None;
        if want > 0 {
            let p = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_SHARED, fd, 0);
            if p != libc::MAP_FAILED {
                out = Some(std::slice::from_raw_parts(p as *const u8, want).to_vec());
                libc::munmap(p, size);
            }
        }
        libc::close(fd);
        libc::shm_unlink(name.as_ptr());
        out
    }
}

/// t=f (a file) and t=t (a temporary file, deleted once read).
fn read_file(kv: &Kv, payload: &[u8]) -> Option<Vec<u8>> {
    let path = medium_path(payload);
    let mut f = std::fs::File::open(&path).ok()?;
    f.seek(SeekFrom::Start(num(kv, "O").max(0) as u64)).ok()?;
    let mut data = Vec::new();
    match get(kv, "S").and_then(|v| v.parse::<u64>().ok()) {
        Some(n) => f.take(n).read_to_end(&mut data).ok()?,
        None => f.read_to_end(&mut data).ok()?,
    };
    if get(kv, "t") == Some("t") && path.contains("tty-graphics-protocol") {
        let _ = std::fs::remove_file(&path);
    }
    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        assert_eq!(base64_decode(b"Zm9vYmFy"), b"foobar");
        assert_eq!(base64_decode(b"Zm8="), b"fo");
    }

    #[test]
    fn text_colors_and_a_placed_image() {
        let mut t = Term::new(10, 3, 2, 4, false, 1 << 20, "ghostty 1.3.1");
        let rest = t.feed(b"\x1b[2;3H\x1b[1;38;2;1;2;3mhi\x1b_Ga=t,f=24,s=2,v=1,i=5,q=1;AAAAAAAA\x1b\\\x1b[1;1H\x1b_Ga=p,i=5,p=1,q=1\x1b\\\xe2");
        assert_eq!(rest, b"\xe2"); // half a character, kept for later
        assert_eq!(t.grid[1][2].ch, 'h');
        assert_eq!(t.grid[1][3].attr.fg, Some([1, 2, 3]));
        assert!(t.grid[1][3].attr.bold);
        assert_eq!(t.shown().len(), 1);
        assert!(t.replies.is_empty()); // q=1: no reply on success
    }

    #[test]
    fn only_what_can_be_seen_changes_the_screen() {
        let mut t = Term::new(10, 3, 2, 4, false, 1 << 20, "ghostty 1.3.1");
        let img = |i| format!("\x1b_Ga=t,f=24,s=2,v=1,i={i},q=1;AAAAAAAA\x1b\\");
        t.feed(format!("{}{}\x1b[1;1H\x1b_Ga=p,i=5,p=1,c=4,r=2,q=1\x1b\\", img(5), img(6)).as_bytes());
        let v = t.version;
        // Placed again as it was, and another image hidden under it.
        t.feed(b"\x1b[1;1H\x1b_Ga=p,i=5,p=1,c=4,r=2,q=1\x1b\\\x1b_Ga=p,i=6,p=9,c=1,r=1,z=-1,q=1\x1b\\");
        assert_eq!((t.version, t.shown().len()), (v, 1));
        // Nothing shows until a synchronized update ends; then image 6 does.
        t.feed(b"\x1b[?2026h\x1b_Ga=d,d=i,i=5,p=1,q=2\x1b\\");
        assert_eq!(t.version, v);
        t.feed(b"\x1b[?2026l");
        assert_eq!((t.version, t.shown().len()), (v + 1, 1));
    }
}
