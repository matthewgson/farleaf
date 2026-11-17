//! Visual mode: a text cursor and a selection over a page's text, moved with
//! Vim's motions, yanked to the system clipboard.
//!
//! The text of a page comes from MuPDF (Page::text) as characters in reading
//! order, each with its box in fractions of the page, and is cut into lines.
//! A position is a page and a character index. Text arrives from the render
//! worker, so a motion that needs a page whose text has not come yet says
//! which one (`Err(page)`) and the caller asks for it.

use std::collections::HashMap;
use std::sync::Arc;

/// Flags of `Ch`: first character of a line / of a block (paragraph).
pub const LINE: u8 = 1;
pub const BLOCK: u8 = 2;

#[derive(Clone, Debug)]
pub struct Ch {
    pub c: char,
    pub flags: u8,
    /// x0, y0, x1, y1 as fractions of the page, origin top-left.
    pub r: [f32; 4],
}

/// Characters `start..end` of the page's text.
#[derive(Clone, Debug)]
pub struct Line {
    pub start: usize,
    pub end: usize,
    pub y0: f32,
    pub y1: f32,
}

#[derive(Default, Debug)]
pub struct Text {
    pub chars: Vec<Ch>,
    pub lines: Vec<Line>,
}

impl Text {
    pub fn new(mut chars: Vec<Ch>) -> Text {
        if let Some(c) = chars.first_mut() {
            c.flags |= LINE | BLOCK;
        }
        let mut lines: Vec<Line> = Vec::new();
        for (i, c) in chars.iter().enumerate() {
            if c.flags & LINE != 0 || lines.is_empty() {
                lines.push(Line {
                    start: i,
                    end: i,
                    y0: f32::MAX,
                    y1: f32::MIN,
                });
            }
            let l = lines.last_mut().unwrap();
            l.end = i + 1;
            // Spaces MuPDF makes up can have no height.
            if c.r[3] > c.r[1] {
                l.y0 = l.y0.min(c.r[1]);
                l.y1 = l.y1.max(c.r[3]);
            }
        }
        for l in &mut lines {
            if l.y0 > l.y1 {
                (l.y0, l.y1) = (0.0, 0.0);
            }
        }
        Text { chars, lines }
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn line_of(&self, i: usize) -> usize {
        self.lines.partition_point(|l| l.start <= i).saturating_sub(1)
    }

    pub fn mid_x(&self, i: usize) -> f32 {
        let r = self.chars[i].r;
        (r[0] + r[2]) / 2.0
    }

    /// The character of `line` nearest to the horizontal position `x`.
    fn at_x(&self, line: usize, x: f32) -> usize {
        let l = &self.lines[line];
        (l.start..l.end)
            .min_by(|&a, &b| {
                let d = |i: usize| {
                    let r = self.chars[i].r;
                    if x < r[0] {
                        r[0] - x
                    } else if x > r[2] {
                        x - r[2]
                    } else {
                        0.0
                    }
                };
                d(a).total_cmp(&d(b))
            })
            .unwrap_or(l.start)
    }

    /// The character nearest to the point (`fx`, `fy`): on the line whose
    /// height is closest to `fy`, the one closest across.
    pub fn nearest(&self, fx: f32, fy: f32) -> Option<usize> {
        let line = (0..self.lines.len()).min_by(|&a, &b| {
            let d = |i: usize| {
                let l = &self.lines[i];
                if fy < l.y0 {
                    l.y0 - fy
                } else if fy > l.y1 {
                    fy - l.y1
                } else {
                    0.0
                }
            };
            d(a).total_cmp(&d(b))
        })?;
        Some(self.at_x(line, fx))
    }

    /// The last character of the line that is not a space.
    fn last_solid(&self, line: usize) -> usize {
        let l = &self.lines[line];
        (l.start..l.end)
            .rev()
            .find(|&i| !self.chars[i].c.is_whitespace())
            .unwrap_or(l.start)
    }

    fn first_solid(&self, line: usize) -> usize {
        let l = &self.lines[line];
        (l.start..l.end)
            .find(|&i| !self.chars[i].c.is_whitespace())
            .unwrap_or(l.start)
    }
}

pub type Texts = HashMap<usize, Arc<Text>>;

/// A character: its page and index in the page's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pt {
    pub page: usize,
    pub i: usize,
}

/// `Err(page)`: the text of that page has not arrived.
pub type Res<T> = Result<T, usize>;

#[derive(PartialEq, Clone, Copy)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Motions over the texts loaded so far.
pub struct Nav<'a> {
    pub texts: &'a Texts,
    pub pages: usize,
}

impl Nav<'_> {
    pub fn text(&self, page: usize) -> Res<&Text> {
        self.texts.get(&page).map(|t| &**t).ok_or(page)
    }

    fn ch(&self, p: Pt) -> Res<&Ch> {
        Ok(&self.text(p.page)?.chars[p.i])
    }

    /// The nearest page from `page` on (backwards if not `fwd`) that has
    /// text.
    fn page_with_text(&self, page: isize, fwd: bool) -> Res<Option<usize>> {
        let mut p = page;
        while p >= 0 && (p as usize) < self.pages {
            if !self.text(p as usize)?.is_empty() {
                return Ok(Some(p as usize));
            }
            p += if fwd { 1 } else { -1 };
        }
        Ok(None)
    }

    pub fn next(&self, p: Pt) -> Res<Option<Pt>> {
        if p.i + 1 < self.text(p.page)?.chars.len() {
            return Ok(Some(Pt {
                page: p.page,
                i: p.i + 1,
            }));
        }
        Ok(self
            .page_with_text(p.page as isize + 1, true)?
            .map(|page| Pt { page, i: 0 }))
    }

    pub fn prev(&self, p: Pt) -> Res<Option<Pt>> {
        if p.i > 0 {
            return Ok(Some(Pt {
                page: p.page,
                i: p.i - 1,
            }));
        }
        Ok(match self.page_with_text(p.page as isize - 1, false)? {
            Some(page) => Some(Pt {
                page,
                i: self.text(page)?.chars.len() - 1,
            }),
            None => None,
        })
    }

    pub fn right(&self, mut p: Pt, n: u32) -> Res<Pt> {
        for _ in 0..n {
            match self.next(p)? {
                Some(q) => p = q,
                None => break,
            }
        }
        Ok(p)
    }

    pub fn left(&self, mut p: Pt, n: u32) -> Res<Pt> {
        for _ in 0..n {
            match self.prev(p)? {
                Some(q) => p = q,
                None => break,
            }
        }
        Ok(p)
    }

    /// `n` lines down (or up), at the character nearest to `goal` across.
    pub fn vertical(&self, mut p: Pt, n: u32, down: bool, goal: f32) -> Res<Pt> {
        for _ in 0..n {
            let t = self.text(p.page)?;
            let l = t.line_of(p.i);
            let (page, line) = if down && l + 1 < t.lines.len() {
                (p.page, l + 1)
            } else if !down && l > 0 {
                (p.page, l - 1)
            } else {
                let step = if down { 1 } else { -1 };
                match self.page_with_text(p.page as isize + step, down)? {
                    Some(page) => (page, if down { 0 } else { self.text(page)?.lines.len() - 1 }),
                    None => break,
                }
            };
            p = Pt {
                page,
                i: self.text(page)?.at_x(line, goal),
            };
        }
        Ok(p)
    }

    pub fn line_start(&self, p: Pt) -> Res<Pt> {
        let t = self.text(p.page)?;
        Ok(Pt {
            page: p.page,
            i: t.lines[t.line_of(p.i)].start,
        })
    }

    pub fn first_solid(&self, p: Pt) -> Res<Pt> {
        let t = self.text(p.page)?;
        Ok(Pt {
            page: p.page,
            i: t.first_solid(t.line_of(p.i)),
        })
    }

    pub fn line_end(&self, p: Pt) -> Res<Pt> {
        let t = self.text(p.page)?;
        Ok(Pt {
            page: p.page,
            i: t.last_solid(t.line_of(p.i)),
        })
    }

    fn starts_line(&self, p: Pt) -> Res<bool> {
        Ok(self.ch(p)?.flags & LINE != 0)
    }

    /// Vim's `w`: the start of the next word. A line break ends a word.
    pub fn word_fwd(&self, mut p: Pt, n: u32) -> Res<Pt> {
        for _ in 0..n {
            let cls = class(self.ch(p)?.c);
            if cls != Class::Space {
                loop {
                    let Some(q) = self.next(p)? else { return Ok(p) };
                    let boundary = self.starts_line(q)? || class(self.ch(q)?.c) != cls;
                    p = q;
                    if boundary {
                        break;
                    }
                }
            }
            while class(self.ch(p)?.c) == Class::Space {
                let Some(q) = self.next(p)? else { return Ok(p) };
                p = q;
            }
        }
        Ok(p)
    }

    /// Vim's `b`: the start of this word, or of the one before.
    pub fn word_back(&self, mut p: Pt, n: u32) -> Res<Pt> {
        for _ in 0..n {
            let Some(q) = self.prev(p)? else { return Ok(p) };
            p = q;
            while class(self.ch(p)?.c) == Class::Space {
                let Some(q) = self.prev(p)? else { return Ok(p) };
                p = q;
            }
            let cls = class(self.ch(p)?.c);
            while !self.starts_line(p)? {
                let Some(q) = self.prev(p)? else { break };
                if class(self.ch(q)?.c) != cls {
                    break;
                }
                p = q;
            }
        }
        Ok(p)
    }

    /// Vim's `e`: the end of this word, or of the next.
    pub fn word_end(&self, mut p: Pt, n: u32) -> Res<Pt> {
        for _ in 0..n {
            let Some(q) = self.next(p)? else { return Ok(p) };
            p = q;
            while class(self.ch(p)?.c) == Class::Space {
                let Some(q) = self.next(p)? else { return Ok(p) };
                p = q;
            }
            let cls = class(self.ch(p)?.c);
            while let Some(q) = self.next(p)? {
                if self.starts_line(q)? || class(self.ch(q)?.c) != cls {
                    break;
                }
                p = q;
            }
        }
        Ok(p)
    }

    /// `}` / `{`: the start of the next / this-or-previous block.
    pub fn block(&self, mut p: Pt, n: u32, fwd: bool) -> Res<Pt> {
        for _ in 0..n {
            loop {
                let q = if fwd { self.next(p)? } else { self.prev(p)? };
                let Some(q) = q else { return Ok(p) };
                p = q;
                if self.ch(p)?.flags & BLOCK != 0 {
                    break;
                }
            }
        }
        Ok(p)
    }

    /// Pages `a..=b` of the texts are all loaded.
    pub fn missing(&self, a: usize, b: usize) -> Option<usize> {
        (a..=b).find(|p| !self.texts.contains_key(p))
    }
}

/// The selection: from `anchor` to `cursor` (either may come first), by
/// characters or, `line`, by whole lines.
#[derive(Clone, Debug)]
pub struct Visual {
    pub anchor: Pt,
    pub cursor: Pt,
    pub line: bool,
    /// The horizontal position `j` and `k` keep, as a fraction of the page.
    pub goal: f32,
    /// A yank is waiting for the text of the pages it spans.
    pub yank_wait: bool,
    /// What each page was last drawn with (a hash of its boxes), to draw it
    /// again when that changes.
    pub painted: HashMap<usize, u64>,
    /// The cursor is to go to this point (page, x, y, and whether `j` `k`
    /// keep that x) once the page's text has come.
    pub aim: Option<(usize, f32, f32, bool)>,
}

impl Visual {
    pub fn new(at: Pt, line: bool, goal: f32) -> Visual {
        Visual {
            anchor: at,
            cursor: at,
            line,
            goal,
            yank_wait: false,
            painted: HashMap::new(),
            aim: None,
        }
    }

    pub fn ends(&self) -> (Pt, Pt) {
        (self.anchor.min(self.cursor), self.anchor.max(self.cursor))
    }

    /// The pages the selection or the cursor touches.
    pub fn pages(&self) -> std::ops::RangeInclusive<usize> {
        let (a, b) = self.ends();
        a.page..=b.page
    }

    /// What to paint on `page`: the boxes of the selected text (one per
    /// line) and, if the cursor is on this page, its character's box.
    pub fn rects(&self, texts: &Texts, page: usize) -> (Vec<[f32; 4]>, Option<[f32; 4]>) {
        let Some(t) = texts.get(&page) else {
            return (Vec::new(), None);
        };
        let (a, b) = self.ends();
        let mut out = Vec::new();
        if (a.page..=b.page).contains(&page) && !t.is_empty() {
            let from = if page == a.page { a.i } else { 0 };
            let to = if page == b.page { b.i } else { t.chars.len() - 1 };
            for l in &t.lines {
                if l.end <= from || l.start > to {
                    continue;
                }
                let (s, e) = if self.line {
                    (l.start, l.end)
                } else {
                    (l.start.max(from), l.end.min(to + 1))
                };
                let mut r = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
                for c in t.chars[s..e].iter().filter(|c| c.r[2] > c.r[0]) {
                    r = [r[0].min(c.r[0]), r[1].min(c.r[1]), r[2].max(c.r[2]), r[3].max(c.r[3])];
                }
                if r[0] <= r[2] {
                    out.push(r);
                }
            }
        }
        let cursor = (self.cursor.page == page)
            .then(|| t.chars.get(self.cursor.i).map(|c| c.r))
            .flatten()
            .map(|mut r| {
                r[2] = r[2].max(r[0] + 0.004);
                r
            });
        (out, cursor)
    }

    /// The selected text, ready for the clipboard. By characters, the lines
    /// of a paragraph run together (a word broken by a hyphen is mended)
    /// and paragraphs are on lines of their own; by lines, every line is
    /// its own, and the last ends in a newline, as a Vim line yank does.
    pub fn text(&self, nav: &Nav) -> Res<String> {
        let (a, b) = self.ends();
        if let Some(p) = nav.missing(a.page, b.page) {
            return Err(p);
        }
        // Each selected line's text, and whether it starts a paragraph.
        let mut segs: Vec<(String, bool)> = Vec::new();
        for page in a.page..=b.page {
            let t = nav.text(page)?;
            if t.is_empty() {
                continue;
            }
            let from = if page == a.page { a.i } else { 0 };
            let to = if page == b.page { b.i } else { t.chars.len() - 1 };
            for l in &t.lines {
                if l.end <= from || l.start > to {
                    continue;
                }
                let (s, e) = if self.line {
                    (l.start, l.end)
                } else {
                    (l.start.max(from), l.end.min(to + 1))
                };
                let text: String = t.chars[s..e]
                    .iter()
                    .map(|c| if c.c.is_control() || c.c == '\u{a0}' { ' ' } else { c.c })
                    .collect();
                let fresh = s == l.start && t.chars[l.start].flags & BLOCK != 0;
                segs.push((text, fresh));
            }
        }
        let mut out = String::new();
        let last = segs.len().saturating_sub(1);
        for (k, (text, fresh)) in segs.iter().enumerate() {
            let text = if k == last { text.as_str() } else { text.trim_end() };
            if k == 0 {
                out.push_str(text);
            } else if self.line || *fresh {
                out.push('\n');
                out.push_str(text);
            } else {
                let text = text.trim_start();
                let mended = {
                    let mut rev = out.chars().rev();
                    matches!(rev.next(), Some('-' | '\u{2010}' | '\u{ad}'))
                        && rev.next().is_some_and(char::is_alphabetic)
                        && text.chars().next().is_some_and(char::is_lowercase)
                };
                if mended {
                    out.pop();
                } else {
                    out.push(' ');
                }
                out.push_str(text);
            }
        }
        if self.line {
            out.push('\n');
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page of `lines`, each a line of text; a line is a paragraph of its
    /// own if it starts with '¶'. Every character is 0.01 wide.
    fn page(lines: &[&str]) -> Text {
        let mut chars = Vec::new();
        for (n, l) in lines.iter().enumerate() {
            let (l, block) = match l.strip_prefix('¶') {
                Some(r) => (r, true),
                None => (*l, false),
            };
            for (k, c) in l.chars().enumerate() {
                let x = 0.1 + 0.01 * k as f32;
                let y = 0.1 + 0.05 * n as f32;
                let flags = if k == 0 {
                    LINE | if block { BLOCK } else { 0 }
                } else {
                    0
                };
                chars.push(Ch {
                    c,
                    flags,
                    r: [x, y, x + 0.01, y + 0.04],
                });
            }
        }
        Text::new(chars)
    }

    fn texts(pages: Vec<Text>) -> Texts {
        pages.into_iter().enumerate().map(|(i, t)| (i, Arc::new(t))).collect()
    }

    fn at(page: usize, i: usize) -> Pt {
        Pt { page, i }
    }

    #[test]
    fn horizontal_crosses_lines_and_pages() {
        let ts = texts(vec![page(&["ab", "cd"]), page(&[]), page(&["ef"])]);
        let nav = Nav { texts: &ts, pages: 3 };
        assert_eq!(nav.right(at(0, 0), 1), Ok(at(0, 1)));
        assert_eq!(nav.right(at(0, 1), 1), Ok(at(0, 2)));
        assert_eq!(nav.right(at(0, 3), 1), Ok(at(2, 0))); // the empty page is skipped
        assert_eq!(nav.right(at(2, 1), 5), Ok(at(2, 1)));
        assert_eq!(nav.left(at(2, 0), 1), Ok(at(0, 3)));
        assert_eq!(nav.left(at(0, 0), 1), Ok(at(0, 0)));
    }

    #[test]
    fn missing_text_is_asked_for() {
        let ts = texts(vec![page(&["ab"])]);
        let nav = Nav { texts: &ts, pages: 3 };
        assert_eq!(nav.right(at(0, 1), 1), Err(1));
        assert_eq!(nav.left(at(0, 0), 1), Ok(at(0, 0)));
    }

    #[test]
    fn lines_keep_the_column() {
        let ts = texts(vec![page(&["abcdef", "gh", "ijklmn"]), page(&["opqrst"])]);
        let nav = Nav { texts: &ts, pages: 2 };
        let goal = 0.1 + 0.055; // the middle of the 6th character
        assert_eq!(nav.vertical(at(0, 5), 1, true, goal), Ok(at(0, 7))); // "h", the nearest
        assert_eq!(nav.vertical(at(0, 5), 2, true, goal), Ok(at(0, 13)));
        assert_eq!(nav.vertical(at(0, 13), 1, true, goal), Ok(at(1, 5)));
        assert_eq!(nav.vertical(at(1, 5), 2, false, goal), Ok(at(0, 7)));
        assert_eq!(nav.vertical(at(0, 2), 1, false, goal), Ok(at(0, 2))); // nothing above
    }

    #[test]
    fn words() {
        let ts = texts(vec![page(&["foo.bar  baz", "qux"])]);
        let nav = Nav { texts: &ts, pages: 1 };
        let w = |i, n| nav.word_fwd(at(0, i), n).unwrap().i;
        assert_eq!((w(0, 1), w(3, 1), w(4, 1), w(9, 1), w(9, 2)), (3, 4, 9, 12, 14));
        assert_eq!(w(13, 1), 14);
        let b = |i, n| nav.word_back(at(0, i), n).unwrap().i;
        assert_eq!((b(9, 1), b(4, 1), b(3, 1), b(12, 1), b(14, 2)), (4, 3, 0, 9, 9));
        let e = |i, n| nav.word_end(at(0, i), n).unwrap().i;
        assert_eq!((e(0, 1), e(2, 1), e(3, 1), e(4, 1), e(6, 1)), (2, 3, 6, 6, 11));
    }

    #[test]
    fn line_ends() {
        let ts = texts(vec![page(&["  ab  ", "cd"])]);
        let nav = Nav { texts: &ts, pages: 1 };
        assert_eq!(nav.line_start(at(0, 3)), Ok(at(0, 0)));
        assert_eq!(nav.first_solid(at(0, 3)), Ok(at(0, 2)));
        assert_eq!(nav.line_end(at(0, 3)), Ok(at(0, 3)));
        assert_eq!(nav.line_end(at(0, 7)), Ok(at(0, 7)));
    }

    #[test]
    fn blocks() {
        let ts = texts(vec![page(&["¶one", "two", "¶three"])]);
        let nav = Nav { texts: &ts, pages: 1 };
        assert_eq!(nav.block(at(0, 1), 1, true), Ok(at(0, 6)));
        assert_eq!(nav.block(at(0, 8), 1, false), Ok(at(0, 6)));
        assert_eq!(nav.block(at(0, 6), 1, false), Ok(at(0, 0)));
    }

    #[test]
    fn yank_runs_a_paragraph_together() {
        let ts = texts(vec![page(&["¶The quick bro-", "wn fox jumps", "over.", "¶Next one."])]);
        let nav = Nav { texts: &ts, pages: 1 };
        let mut v = Visual::new(at(0, 4), false, 0.0);
        v.cursor = at(0, 36);
        assert_eq!(v.text(&nav).unwrap(), "quick brown fox jumps over.\nNext o");
        // Backwards is the same selection.
        std::mem::swap(&mut v.anchor, &mut v.cursor);
        assert_eq!(v.text(&nav).unwrap(), "quick brown fox jumps over.\nNext o");
        v.line = true;
        assert_eq!(
            v.text(&nav).unwrap(),
            "The quick bro-\nwn fox jumps\nover.\nNext one.\n"
        );
    }

    #[test]
    fn yank_waits_for_text() {
        let ts = texts(vec![page(&["ab"]), page(&["cd"])]);
        let nav = Nav { texts: &ts, pages: 4 };
        let mut v = Visual::new(at(0, 1), false, 0.0);
        v.cursor = at(3, 0);
        assert_eq!(v.text(&nav), Err(2));
    }

    #[test]
    fn rects_one_per_line() {
        let ts = texts(vec![page(&["abc", "def", "ghi"])]);
        let mut v = Visual::new(at(0, 1), false, 0.0);
        v.cursor = at(0, 7);
        let (sel, cur) = v.rects(&ts, 0);
        assert_eq!(sel.len(), 3);
        assert!((sel[0][0] - 0.11).abs() < 1e-6 && (sel[0][2] - 0.13).abs() < 1e-6);
        assert!((sel[2][0] - 0.1).abs() < 1e-6 && (sel[2][2] - 0.12).abs() < 1e-6);
        assert_eq!(cur.map(|r| (r[1] * 1000.0).round()), Some(200.0));
        assert_eq!(v.rects(&ts, 1), (Vec::new(), None));
        v.line = true;
        assert!((v.rects(&ts, 0).0[0][0] - 0.1).abs() < 1e-6);
    }

    #[test]
    fn nearest_char() {
        let t = page(&["abc", "def"]);
        assert_eq!(t.nearest(0.115, 0.12), Some(1));
        assert_eq!(t.nearest(0.5, 0.2), Some(5));
        assert_eq!(t.nearest(0.0, 0.9), Some(3));
        assert_eq!(Text::default().nearest(0.0, 0.0), None);
    }
}
