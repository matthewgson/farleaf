//! The sidebar on the left, as in macOS Preview: the document's outline, or
//! a thumbnail of every page. Thumbnails are images like the pages' strips;
//! everything else here is text.

use crate::pdfium::Entry;
use std::io::Write;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Outline,
    Thumbs,
}

/// The sidebar's background, also painted around each thumbnail so its image
/// blends in.
pub const BG: [u8; 3] = [38, 38, 38];
const BG_SGR: &str = "48;2;38;38;38";
const TEXT: &str = "38;2;208;208;208";
const DIM: &str = "38;2;128;128;128";
const ACCENT: &str = "38;2;135;175;215";
const SELECTED_BG: &str = "48;2;42;80;128";
const CURRENT_BG: &str = "48;2;62;62;62";

/// Columns for the sidebar in a window `cols` wide with cells `cw` pixels
/// wide: about 400 pixels, never more than a third of the window. None if
/// the window is too narrow for one.
pub fn width(cols: u32, cw: u32) -> Option<u32> {
    let w = (400 / cw.max(1)).clamp(16, 36).min(cols / 3);
    (w >= 12).then_some(w)
}

/// Thumbnail sizes, in cells: as wide as the sidebar allows, unless that
/// makes them so tall that fewer than two and a half fit; then narrower.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Thumbs {
    /// The widest a thumbnail can be.
    pub cols: u32,
    /// Each page's thumbnail: columns, rows.
    pub size: Vec<(u32, u32)>,
}

/// One thumbnail on screen: its page, its top-left cell, and how many of its
/// rows show (fewer than all when it reaches past the bottom).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThumbAt {
    pub page: usize,
    pub row: u32,
    pub col: u32,
    pub shown: u32,
}

/// Rows above each thumbnail (its frame) and below it (frame, page number).
const ABOVE: u32 = 1;
const BELOW: u32 = 2;

impl Thumbs {
    /// For a sidebar `side` columns wide and `rows` tall (the header row
    /// included), with cells `cw`x`ch` pixels.
    pub fn new(sizes: &[(f32, f32)], side: u32, rows: u32, cw: u32, ch: u32) -> Thumbs {
        // Columns: padding, frame, the image, frame, padding, separator.
        let cols = side.saturating_sub(5).max(1);
        let (cw, ch) = (cw.max(1) as f32, ch.max(1) as f32);
        let fits = rows.saturating_sub(1 + ABOVE + BELOW).max(1);
        let max_rows = (rows.saturating_sub(1) * 2 / 5)
            .saturating_sub(ABOVE + BELOW)
            .max(3)
            .min(fits);
        let size = sizes
            .iter()
            .map(|&(pw, ph)| {
                let aspect = ph / pw.max(1.0);
                let r = ((cols as f32 * cw * aspect / ch).ceil() as u32).max(1);
                if r <= max_rows {
                    return (cols, r);
                }
                let c = ((max_rows as f32 * ch / aspect / cw).ceil() as u32).clamp(1, cols);
                (c, max_rows)
            })
            .collect();
        Thumbs { cols, size }
    }

    fn item(&self, page: usize) -> u32 {
        ABOVE + self.size[page].1 + BELOW
    }

    /// The thumbnails on screen when the list starts at page `first`, in a
    /// sidebar `rows` tall (row 0 is the header).
    pub fn visible(&self, first: usize, rows: u32) -> Vec<ThumbAt> {
        let mut out = Vec::new();
        let mut y = 1;
        for page in first..self.size.len() {
            if y >= rows {
                break;
            }
            let (c, r) = self.size[page];
            let row = y + ABOVE;
            let shown = r.min(rows.saturating_sub(row));
            if shown > 0 {
                let col = 2 + (self.cols - c) / 2;
                out.push(ThumbAt { page, row, col, shown });
            }
            y += self.item(page);
        }
        out
    }

    /// The first page to list so that page `sel` shows whole, moving as
    /// little as possible from `first`.
    pub fn scroll(&self, first: usize, sel: usize, rows: u32) -> usize {
        if sel < first {
            return sel;
        }
        let room = rows.saturating_sub(1);
        let mut first = first;
        while first < sel && (first..=sel).map(|p| self.item(p)).sum::<u32>() > room {
            first += 1;
        }
        first
    }
}

/// The first outline entry to list so that `sel` shows with a few entries
/// of context, moving as little as possible from `first`.
pub fn outline_scroll(first: usize, sel: usize, len: usize, rows: u32) -> usize {
    let room = rows.saturating_sub(1) as usize;
    if len <= room {
        return 0;
    }
    let ctx = (room / 4).min(3);
    let mut first = first.min(len - room);
    if sel < first + ctx {
        first = sel.saturating_sub(ctx);
    } else if sel + ctx >= first + room {
        first = (sel + ctx + 1).saturating_sub(room).min(len - room);
    }
    first
}

/// The entry for the place being read (page `page`, `frac` of the way down):
/// the furthest entry at or before it. None if all are after it.
pub fn current_entry(entries: &[Entry], page: usize, frac: f32) -> Option<usize> {
    let mut best: Option<(usize, f32, usize)> = None;
    for (i, e) in entries.iter().enumerate() {
        let Some(p) = e.page else { continue };
        if (p, e.frac) > (page, frac + 1e-4) {
            continue;
        }
        if best.is_none_or(|(bp, bf, _)| (p, e.frac) >= (bp, bf)) {
            best = Some((p, e.frac, i));
        }
    }
    best.map(|(_, _, i)| i)
}

/// What the sidebar shows besides its thumbnails' images.
pub struct View<'a> {
    pub kind: Kind,
    /// Columns, the separator included, and rows (the header included).
    pub cols: u32,
    pub rows: u32,
    pub first: usize,
    /// The selected entry or page; highlighted brightly while the sidebar has
    /// the keys, dimly while it only follows the reading.
    pub sel: Option<usize>,
    pub focused: bool,
    pub outline: &'a [Entry],
    pub thumbs: &'a Thumbs,
    pub shown: &'a [ThumbAt],
}

impl View<'_> {
    /// The sidebar's text, every cell of it.
    pub fn draw(&self, out: &mut Vec<u8>) {
        let inner = self.cols.saturating_sub(1) as usize;
        let mut lines: Vec<Line> = (0..self.rows).map(|_| Line::blank(inner)).collect();
        let title = match self.kind {
            Kind::Outline => "Contents",
            Kind::Thumbs => "Pages",
        };
        let hstyle = if self.focused {
            format!("1;{ACCENT}")
        } else {
            DIM.to_string()
        };
        lines[0].put(1, title, &hstyle);
        match self.kind {
            Kind::Outline => self.outline_lines(&mut lines),
            Kind::Thumbs => self.thumb_lines(&mut lines),
        }
        for (r, line) in lines.iter().enumerate() {
            let _ = write!(out, "\x1b[{};1H", r + 1);
            line.write(out);
            let _ = write!(out, "\x1b[0;{BG_SGR};{DIM}m│");
        }
        out.extend_from_slice(b"\x1b[0m");
    }

    fn outline_lines(&self, lines: &mut [Line]) {
        let inner = self.cols.saturating_sub(1) as usize;
        if self.outline.is_empty() {
            if let Some(l) = lines.get_mut(2) {
                l.put(1, "This PDF has no outline.", DIM);
            }
            return;
        }
        for (i, e) in self.outline.iter().enumerate().skip(self.first) {
            let r = 1 + i - self.first;
            if r >= lines.len() {
                break;
            }
            let selected = self.sel == Some(i);
            let bg = match (selected, self.focused) {
                (true, true) => Some(SELECTED_BG),
                (true, false) => Some(CURRENT_BG),
                _ => None,
            };
            let num = e.page.map(|p| (p + 1).to_string()).unwrap_or_default();
            let indent = (2 * e.depth as usize).min(inner / 3);
            let room = inner.saturating_sub(2 + indent + num.len() + 1);
            let title = fit(&e.title, room);
            let line = &mut lines[r];
            if let Some(bg) = bg {
                line.fill(0, inner, bg);
            }
            let (tstyle, nstyle) = match (selected, e.depth) {
                (true, _) => ("1;38;2;240;240;240", TEXT),
                (false, 0) => (TEXT, DIM),
                _ => ("38;2;180;180;180", DIM),
            };
            line.put(1 + indent, &title, tstyle);
            line.put(inner.saturating_sub(1 + num.len()), &num, nstyle);
        }
    }

    fn thumb_lines(&self, lines: &mut [Line]) {
        let color = if self.focused { ACCENT } else { DIM };
        for t in self.shown {
            let (c, rows) = self.thumbs.size[t.page];
            let (c, rows, x) = (c as usize, rows as usize, t.col as usize - 1);
            let r0 = (t.row - ABOVE) as usize;
            let selected = self.sel == Some(t.page);
            if selected {
                let mut put = |r: usize, col: usize, s: &str| {
                    if let Some(l) = lines.get_mut(r) {
                        l.put(col, s, color);
                    }
                };
                put(r0, x, &format!("┌{}┐", "─".repeat(c)));
                for r in r0 + 1..r0 + 1 + rows {
                    put(r, x, "│");
                    put(r, x + 1 + c, "│");
                }
                put(r0 + 1 + rows, x, &format!("└{}┘", "─".repeat(c)));
            }
            let num = (t.page + 1).to_string();
            if let Some(l) = lines.get_mut(r0 + 2 + rows) {
                let style = if selected {
                    format!("1;{color}")
                } else {
                    DIM.to_string()
                };
                let mid = 2 + (self.thumbs.cols as usize).saturating_sub(num.len()) / 2;
                l.put(mid, &num, &style);
            }
        }
    }
}

/// Columns a character takes: two for wide (East Asian) ones, none for
/// combining marks.
fn char_width(c: char) -> usize {
    match c as u32 {
        0x0300..=0x036F | 0x200B..=0x200F | 0xFE00..=0xFE0F => 0,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// `s` cut to `w` columns, with an ellipsis if it was longer.
fn fit(s: &str, w: usize) -> String {
    let total: usize = s.chars().map(char_width).sum();
    if total <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if used + cw + 1 > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    if w > 0 {
        out.push('…');
    }
    out
}

/// One row of the sidebar: a character and a style per cell.
struct Line {
    cells: Vec<(String, String)>,
}

impl Line {
    fn blank(w: usize) -> Line {
        Line {
            cells: vec![(" ".to_string(), String::new()); w],
        }
    }

    /// Writes `s` from column `col` in `style` (SGR parameters), keeping any
    /// background the cells already have.
    fn put(&mut self, col: usize, s: &str, style: &str) {
        let mut x = col;
        for c in s.chars() {
            let w = char_width(c);
            if w == 0 {
                if let Some(cell) = x.checked_sub(1).and_then(|i| self.cells.get_mut(i)) {
                    cell.0.push(c);
                }
                continue;
            }
            if x + w > self.cells.len() {
                break;
            }
            let bg = background(&self.cells[x].1);
            self.cells[x] = (c.to_string(), join(style, bg));
            if w == 2 {
                // The wide character covers this cell too.
                self.cells[x + 1] = (String::new(), String::new());
            }
            x += w;
        }
    }

    fn fill(&mut self, from: usize, to: usize, bg: &str) {
        for cell in self.cells.iter_mut().take(to).skip(from) {
            cell.1 = bg.to_string();
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        let mut cur: Option<&str> = None;
        for (c, style) in &self.cells {
            if c.is_empty() {
                continue;
            }
            if cur != Some(style.as_str()) {
                let _ = write!(out, "\x1b[0;{BG_SGR}");
                if !style.is_empty() {
                    let _ = write!(out, ";{style}");
                }
                out.push(b'm');
                cur = Some(style);
            }
            out.extend_from_slice(c.as_bytes());
        }
    }
}

/// The background part ("48;...") of a style, if any.
fn background(style: &str) -> &str {
    style.find("48;").map_or("", |i| &style[i..])
}

fn join(fg: &str, bg: &str) -> String {
    match (fg.is_empty(), bg.is_empty()) {
        (_, true) => fg.to_string(),
        (true, false) => bg.to_string(),
        (false, false) => format!("{fg};{bg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str, depth: u32, page: usize, frac: f32) -> Entry {
        Entry {
            title: title.into(),
            depth,
            page: Some(page),
            frac,
        }
    }

    #[test]
    fn thumbs_scroll_to_show_the_selected_page_whole() {
        let t = Thumbs::new(&[(612.0, 792.0); 40], 24, 50, 19, 42);
        assert_eq!(t.cols, 19);
        for sel in [0, 1, 5, 39, 12, 3] {
            for first in [0, 7, 39] {
                let f = t.scroll(first, sel, 50);
                let v = t.visible(f, 50);
                let at = v.iter().find(|a| a.page == sel).expect("selected page on screen");
                assert_eq!(at.shown, t.size[sel].1, "sel {sel} first {first}");
                assert!(at.row + at.shown + BELOW <= 50);
            }
        }
    }

    #[test]
    fn thumbs_fit_and_keep_their_shape() {
        // Full-width thumbnails would be too tall for this window: narrower ones,
        // two and a half to a screen.
        let (cw, ch) = (12, 26);
        let t = Thumbs::new(&[(612.0, 792.0), (792.0, 612.0)], 30, 40, cw, ch);
        assert!(t
            .size
            .iter()
            .all(|&(c, r)| c <= t.cols && 5 * (r + ABOVE + BELOW) <= 2 * 39));
        assert!(t.size[0].0 < t.cols);
        let (c, r) = t.size[0];
        let shape = (r * ch) as f32 / (c * cw) as f32;
        assert!((shape - 792.0 / 612.0).abs() < 0.15, "{shape}");
        assert_eq!(t.visible(0, 40)[0].col, 2 + (t.cols - c) / 2);
        assert_eq!(t.size[1].0, t.cols); // landscape: full width
    }

    #[test]
    fn thumbs_fit_a_tiny_window() {
        let t = Thumbs::new(&[(612.0, 792.0), (792.0, 612.0)], 16, 8, 19, 42);
        assert!(t.size.iter().all(|&(_, r)| r + ABOVE + BELOW < 8));
        assert_eq!(t.scroll(0, 1, 8), 1);
    }

    #[test]
    fn outline_scroll_keeps_context() {
        assert_eq!(outline_scroll(0, 5, 10, 20), 0);
        let f = outline_scroll(0, 30, 100, 21);
        assert!(f <= 30 && 30 + 3 < f + 20);
        assert_eq!(outline_scroll(f, 30, 100, 21), f); // already in view: no move
        assert_eq!(outline_scroll(50, 51, 100, 21), 48);
        assert_eq!(outline_scroll(0, 99, 100, 21), 80);
    }

    #[test]
    fn current_entry_is_the_last_one_reached() {
        let o = vec![
            entry("1 Intro", 0, 0, 0.1),
            entry("2 Model", 0, 2, 0.5),
            entry("2.1 Setup", 1, 2, 0.7),
            entry("3 Results", 0, 5, 0.0),
        ];
        assert_eq!(current_entry(&o, 0, 0.0), None);
        assert_eq!(current_entry(&o, 1, 0.9), Some(0));
        assert_eq!(current_entry(&o, 2, 0.6), Some(1));
        assert_eq!(current_entry(&o, 2, 0.7), Some(2));
        assert_eq!(current_entry(&o, 9, 0.0), Some(3));
    }

    #[test]
    fn titles_fit_their_columns() {
        assert_eq!(fit("Results", 10), "Results");
        assert_eq!(fit("Identification strategy", 10), "Identific…");
        assert_eq!(fit("数据和方法", 6), "数据…");
    }
}
