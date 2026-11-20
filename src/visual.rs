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

/// The extent of some characters: top, bottom (of those with a height),
/// left of the first and right of the last with a width.
fn extent(chars: &[Ch]) -> (f32, f32, f32, f32) {
    let tall = chars.iter().filter(|c| c.r[3] > c.r[1]);
    let y0 = tall.clone().map(|c| c.r[1]).fold(f32::MAX, f32::min);
    let y1 = tall.map(|c| c.r[3]).fold(f32::MIN, f32::max);
    let wide = || chars.iter().filter(|c| c.r[2] > c.r[0]);
    let x0 = wide().next().map_or(0.0, |c| c.r[0]);
    let x1 = wide().map(|c| c.r[2]).fold(f32::MIN, f32::max);
    (y0, y1, x0, x1)
}

/// MuPDF cuts a line where words lie far apart: between the cells of a
/// table row, or in a tightly justified narrow column. Pieces that follow
/// one another on the same row, left to right, are joined back into one
/// line, with a space between them where there is a gap and none, as the
/// line looks on the page (so `j`, `V` and `yy` go by those lines).
fn join_rows(chars: Vec<Ch>) -> Vec<Ch> {
    let mut out: Vec<Ch> = Vec::with_capacity(chars.len());
    // The row so far: top, bottom, right edge.
    let mut row: Option<(f32, f32, f32)> = None;
    let mut i = 0;
    while i < chars.len() {
        let j = (i + 1..chars.len())
            .find(|&j| chars[j].flags & LINE != 0)
            .unwrap_or(chars.len());
        let piece = &chars[i..j];
        let (y0, y1, x0, x1) = extent(piece);
        let same_row = row.filter(|&(ry0, ry1, rx1)| {
            let overlap = ry1.min(y1) - ry0.max(y0);
            y1 > y0 && overlap > 0.5 * (ry1 - ry0).min(y1 - y0) && x0 >= rx1 - 0.002
        });
        match same_row {
            Some((ry0, ry1, rx1)) => {
                let spaced = out.last().is_some_and(|c| c.c.is_whitespace()) || piece[0].c.is_whitespace();
                if !spaced && x0 - rx1 > 0.001 {
                    out.push(Ch {
                        c: ' ',
                        flags: 0,
                        r: [rx1, ry0.max(y0), x0, ry1.min(y1)],
                    });
                }
                let mut first = piece[0].clone();
                first.flags &= !(LINE | BLOCK);
                out.push(first);
                out.extend_from_slice(&piece[1..]);
                row = Some((ry0.min(y0), ry1.max(y1), rx1.max(x1)));
            }
            None => {
                out.extend_from_slice(piece);
                row = (y1 > y0).then_some((y0, y1, x1));
            }
        }
        i = j;
    }
    out
}

impl Text {
    pub fn new(mut chars: Vec<Ch>) -> Text {
        if let Some(c) = chars.first_mut() {
            c.flags |= LINE | BLOCK;
        }
        let chars = join_rows(chars);
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

    /// The rows of text as they look on the page, top to bottom, from the
    /// height `fy` down. Lines side by side (a running head MuPDF cuts in
    /// pieces, the cells of a table row, two columns) make one row, given
    /// by its leftmost line.
    pub fn rows_from(&self, fy: f32) -> Vec<usize> {
        let x = |l: usize| self.chars[self.lines[l].start].r[0];
        let mut lines: Vec<usize> = (0..self.lines.len())
            .filter(|&l| self.lines[l].y1 > self.lines[l].y0 && self.lines[l].y0 >= fy)
            .collect();
        lines.sort_by(|&a, &b| self.lines[a].y0.total_cmp(&self.lines[b].y0));
        // Each row: the middle height of its first line, and its leftmost line.
        let mut rows: Vec<(f32, usize)> = Vec::new();
        for l in lines {
            let (y0, y1) = (self.lines[l].y0, self.lines[l].y1);
            match rows.last_mut() {
                Some((mid, left)) if y0 < *mid => {
                    if x(l) < x(*left) {
                        *left = l;
                    }
                }
                _ => rows.push(((y0 + y1) / 2.0, l)),
            }
        }
        rows.into_iter().map(|r| r.1).collect()
    }

    pub fn line_of(&self, i: usize) -> usize {
        self.lines.partition_point(|l| l.start <= i).saturating_sub(1)
    }

    pub fn mid_x(&self, i: usize) -> f32 {
        let r = self.chars[i].r;
        (r[0] + r[2]) / 2.0
    }

    /// The first character of the word of `line` that ends past `x`: a
    /// forward search's x is about where its source line starts on the PDF
    /// line, in the space before a word or on it. Past the last word: its
    /// first character.
    pub fn word_at(&self, line: usize, x: f32) -> usize {
        let Line { start, end, .. } = self.lines[line];
        let solid = |i: usize| !self.chars[i].c.is_whitespace();
        let mut i = (start..end)
            .find(|&i| solid(i) && self.chars[i].r[2] > x)
            .or_else(|| (start..end).rev().find(|&i| solid(i)))
            .unwrap_or(start);
        while i > start && solid(i - 1) {
            i -= 1;
        }
        i
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

/// The kind of a character, for word motions; with `big` (Vim's WORDs:
/// `W` `B` `E` `iW`) all but spaces are one kind.
fn class(c: char, big: bool) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if big || c.is_alphanumeric() || c == '_' {
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

    /// Vim's `w` (`W`): the start of the next word. A line break ends a word.
    pub fn word_fwd(&self, mut p: Pt, n: u32, big: bool) -> Res<Pt> {
        for _ in 0..n {
            let cls = class(self.ch(p)?.c, big);
            if cls != Class::Space {
                loop {
                    let Some(q) = self.next(p)? else { return Ok(p) };
                    let boundary = self.starts_line(q)? || class(self.ch(q)?.c, big) != cls;
                    p = q;
                    if boundary {
                        break;
                    }
                }
            }
            while class(self.ch(p)?.c, big) == Class::Space {
                let Some(q) = self.next(p)? else { return Ok(p) };
                p = q;
            }
        }
        Ok(p)
    }

    /// Vim's `b` (`B`): the start of this word, or of the one before.
    pub fn word_back(&self, mut p: Pt, n: u32, big: bool) -> Res<Pt> {
        for _ in 0..n {
            let Some(q) = self.prev(p)? else { return Ok(p) };
            p = q;
            while class(self.ch(p)?.c, big) == Class::Space {
                let Some(q) = self.prev(p)? else { return Ok(p) };
                p = q;
            }
            let cls = class(self.ch(p)?.c, big);
            while !self.starts_line(p)? {
                let Some(q) = self.prev(p)? else { break };
                if class(self.ch(q)?.c, big) != cls {
                    break;
                }
                p = q;
            }
        }
        Ok(p)
    }

    /// Vim's `e` (`E`): the end of this word, or of the next.
    pub fn word_end(&self, mut p: Pt, n: u32, big: bool) -> Res<Pt> {
        for _ in 0..n {
            let Some(q) = self.next(p)? else { return Ok(p) };
            p = q;
            while class(self.ch(p)?.c, big) == Class::Space {
                let Some(q) = self.next(p)? else { return Ok(p) };
                p = q;
            }
            let cls = class(self.ch(p)?.c, big);
            while let Some(q) = self.next(p)? {
                if self.starts_line(q)? || class(self.ch(q)?.c, big) != cls {
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

    /// Vim's `f` `F` `t` `T`: the `n`th `c` on this line after (`fwd`) or
    /// before `p`, or, with `till`, the character just short of it. None
    /// if there are not that many. `again` (a `;` or `,` repeating a `t`
    /// or `T`) skips a `c` right next to `p`, which would not move it.
    pub fn find(&self, p: Pt, c: char, n: u32, fwd: bool, till: bool, again: bool) -> Res<Option<Pt>> {
        let t = self.text(p.page)?;
        let l = &t.lines[t.line_of(p.i)];
        let skip = usize::from(till && again);
        let hits = |i: &usize| t.chars[*i].c == c;
        let hit = match fwd {
            true => (p.i + 1 + skip..l.end).filter(hits).nth(n.max(1) as usize - 1),
            false => (l.start..p.i.saturating_sub(skip))
                .rev()
                .filter(hits)
                .nth(n.max(1) as usize - 1),
        };
        Ok(hit.map(|j| Pt {
            page: p.page,
            i: match (till, fwd) {
                (false, _) => j,
                (true, true) => j - 1,
                (true, false) => j + 1,
            },
        }))
    }

    /// Vim's `iw` / `aw` (`iW` / `aW` with `big`): the word, or the run of
    /// spaces, at `p` on its line; `around` takes the spaces after it too
    /// (or, if none follow, those before it), or for spaces the word after.
    pub fn word_object(&self, p: Pt, around: bool, big: bool) -> Res<(Pt, Pt)> {
        let t = self.text(p.page)?;
        let l = &t.lines[t.line_of(p.i)];
        let cls = |i: usize| class(t.chars[i].c, big);
        let run_back = |mut i: usize, c: Class| {
            while i > l.start && cls(i - 1) == c {
                i -= 1;
            }
            i
        };
        let run_fwd = |mut i: usize, c: Class| {
            while i + 1 < l.end && cls(i + 1) == c {
                i += 1;
            }
            i
        };
        let c = cls(p.i);
        let (mut a, mut b) = (run_back(p.i, c), run_fwd(p.i, c));
        if around {
            if c == Class::Space {
                if b + 1 < l.end {
                    b = run_fwd(b + 1, cls(b + 1));
                }
            } else if b + 1 < l.end && cls(b + 1) == Class::Space {
                b = run_fwd(b + 1, Class::Space);
            } else if a > l.start && cls(a - 1) == Class::Space {
                a = run_back(a - 1, Class::Space);
            }
        }
        Ok((Pt { page: p.page, i: a }, Pt { page: p.page, i: b }))
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
    /// The cursor is to go to this point (page, x, y, and whether `j` `k`
    /// keep that x) once the page's text has come.
    pub aim: Option<(usize, f32, f32, bool)>,
    /// Text is being selected (Vim's visual mode); else there is only the
    /// cursor (its normal mode), and `anchor` is the cursor.
    pub select: bool,
    /// Keys of a command not finished yet (`f`, `y`, `yi`, `g`, ...), and
    /// the count typed before it.
    pub pend: String,
    pub pend_n: Option<u32>,
    /// The last `f` `F` `t` `T`, for `;` and `,`: the character, forward,
    /// till.
    pub last_find: Option<(char, bool, bool)>,
}

impl Visual {
    /// A selection starting at `at` (by lines, `line`).
    pub fn new(at: Pt, line: bool, goal: f32) -> Visual {
        Visual {
            anchor: at,
            cursor: at,
            line,
            goal,
            yank_wait: false,
            aim: None,
            select: true,
            pend: String::new(),
            pend_n: None,
            last_find: None,
        }
    }

    /// Just a cursor at `at`, nothing selected.
    pub fn cursor_at(at: Pt, goal: f32) -> Visual {
        Visual {
            select: false,
            ..Visual::new(at, false, goal)
        }
    }

    pub fn ends(&self) -> (Pt, Pt) {
        match self.select {
            true => (self.anchor.min(self.cursor), self.anchor.max(self.cursor)),
            false => (self.cursor, self.cursor),
        }
    }

    /// The next key is a character to find (after `f` `F` `t` `T`), not a
    /// command or a count.
    pub fn wants_char(&self) -> bool {
        self.pend.ends_with(['f', 'F', 't', 'T'])
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
        if self.select && (a.page..=b.page).contains(&page) && !t.is_empty() {
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
        let w = |i, n| nav.word_fwd(at(0, i), n, false).unwrap().i;
        assert_eq!((w(0, 1), w(3, 1), w(4, 1), w(9, 1), w(9, 2)), (3, 4, 9, 12, 14));
        assert_eq!(w(13, 1), 14);
        let b = |i, n| nav.word_back(at(0, i), n, false).unwrap().i;
        assert_eq!((b(9, 1), b(4, 1), b(3, 1), b(12, 1), b(14, 2)), (4, 3, 0, 9, 9));
        let e = |i, n| nav.word_end(at(0, i), n, false).unwrap().i;
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
    fn rows_join_lines_side_by_side() {
        // A running head in two pieces (right one first, as MuPDF may give
        // it), then two lines of body text.
        let mut chars = Vec::new();
        for (x, y, c) in [(0.8, 0.05, 'R'), (0.1, 0.05, 'L'), (0.1, 0.1, 'a'), (0.1, 0.15, 'b')] {
            chars.push(Ch {
                c,
                flags: LINE,
                r: [x, y, x + 0.01, y + 0.04],
            });
        }
        let t = Text::new(chars);
        assert_eq!(t.rows_from(0.0), vec![1, 2, 3]);
        assert_eq!(t.rows_from(0.08), vec![2, 3]);
        assert_eq!(Text::default().rows_from(0.0), Vec::<usize>::new());
    }

    #[test]
    fn pieces_of_a_row_join() {
        // Two cells of a table row, MuPDF lines of their own, then a row below.
        let ch = |c, x: f32, y: f32, first| Ch {
            c,
            flags: if first { LINE | BLOCK } else { 0 },
            r: [x, y, x + 0.01, y + 0.04],
        };
        let t = Text::new(vec![
            ch('a', 0.1, 0.1, true),
            ch('b', 0.11, 0.1, false),
            ch('c', 0.5, 0.1, true),
            ch('d', 0.51, 0.1, false),
            ch('e', 0.1, 0.2, true),
        ]);
        let s: String = t.chars.iter().map(|c| c.c).collect();
        assert_eq!(s, "ab cde");
        assert_eq!(t.lines.len(), 2);
        assert_eq!((t.lines[0].start, t.lines[0].end), (0, 5));
        assert_eq!(t.chars[3].flags, 0); // c no longer starts a line or block
    }

    #[test]
    fn find_on_the_line() {
        let ts = texts(vec![page(&["a.b.c.d", "x.y"])]);
        let nav = Nav { texts: &ts, pages: 1 };
        let f = |i, c, n, fwd, till, again| nav.find(at(0, i), c, n, fwd, till, again).unwrap().map(|p| p.i);
        assert_eq!(f(0, '.', 1, true, false, false), Some(1)); // f.
        assert_eq!(f(0, '.', 2, true, false, false), Some(3)); // 2f.
        assert_eq!(f(0, 'c', 1, true, true, false), Some(3)); // tc
        assert_eq!(f(2, '.', 1, true, true, false), Some(2)); // t. right before a .: stays
        assert_eq!(f(2, '.', 1, true, true, true), Some(4)); // ; after t. moves on
        assert_eq!(f(6, 'b', 1, false, false, false), Some(2)); // Fb
        assert_eq!(f(6, 'b', 1, false, true, false), Some(3)); // Tb
        assert_eq!(f(0, 'x', 1, true, false, false), None); // not past the line
        assert_eq!(f(0, '.', 4, true, false, false), None);
    }

    #[test]
    fn forward_search_lands_on_a_word() {
        let t = page(&["one two three", "four"]);
        let x = |k: f32| 0.1 + 0.01 * k; // the left of character k
        assert_eq!(t.word_at(0, x(0.0)), 0); // the line's start
        assert_eq!(t.word_at(0, x(3.5)), 4); // in the space: the next word
        assert_eq!(t.word_at(0, x(5.5)), 4); // inside "two": its start
        assert_eq!(t.word_at(0, x(30.0)), 8); // past the end: the last word
        assert_eq!(t.word_at(0, 0.0), 0); // left of the line
        assert_eq!(t.word_at(1, x(2.0)), 13); // the next line: from its start
    }

    #[test]
    fn word_objects() {
        let ts = texts(vec![page(&["one two, three"])]);
        let nav = Nav { texts: &ts, pages: 1 };
        let o = |i, around, big| {
            let (a, b) = nav.word_object(at(0, i), around, big).unwrap();
            (a.i, b.i)
        };
        assert_eq!(o(5, false, false), (4, 6)); // iw on "two"
        assert_eq!(o(5, true, false), (3, 6)); // aw: "two" is followed by ",": the space before
        assert_eq!(o(5, true, true), (4, 8)); // aW: "two," and the space
        assert_eq!(o(1, true, false), (0, 3)); // aw: "one "
        assert_eq!(o(12, true, false), (8, 13)); // aw at the end: " three"
        assert_eq!(o(3, false, false), (3, 3)); // iw on a space
    }

    #[test]
    fn cursor_alone_selects_nothing() {
        let ts = texts(vec![page(&["abc", "def"])]);
        let mut v = Visual::cursor_at(at(0, 1), 0.0);
        v.cursor = at(0, 4);
        let (sel, cur) = v.rects(&ts, 0);
        assert!(sel.is_empty());
        assert!(cur.is_some());
        assert_eq!(v.ends(), (at(0, 4), at(0, 4)));
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
