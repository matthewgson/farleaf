//! Visual mode in the app: entering and leaving it, its keys, painting the
//! selection, copying to the system clipboard, and going to the source.
//!
//! The clipboard is written with OSC 52, which is how Neovim does it over
//! SSH: Ghostty (or whatever terminal is in front) sets the clipboard of the
//! machine you sit at, and herdr passes it on. Nothing is installed on the
//! remote side.

use super::*;
use crate::visual::{Nav, Pt, Res, Visual};
use std::hash::{DefaultHasher, Hash, Hasher};

/// `v` pressed, waiting for the text of the page it starts on: the page, the
/// spot on it, where on it the cursor goes, and whether the selection is by
/// lines.
pub struct Entering {
    pub page: usize,
    pub fx: f32,
    pub fy: f32,
    pub start: Start,
    pub line: bool,
}

/// Where `v` puts the cursor, as bookokrat does, with a search first.
#[derive(Clone, Copy, PartialEq)]
pub enum Start {
    /// The character at the spot: a search match, or where the cursor was
    /// when visual mode was last left, if that is still on screen.
    Exact,
    /// The start of the line at the spot (a forward search without x).
    Line,
    /// The word at or after the spot on its line (a forward search: SyncTeX's
    /// x is about where the source line starts on the PDF line).
    Word,
    /// The start of the third line of text from the top of the screen
    /// (the spot): the first line whose top shows, two lines on.
    Top,
}

/// What visual mode paints on a page: the selected text's boxes (one per
/// line) and the cursor's character's box, in page fractions.
pub type Paint = (Vec<[f32; 4]>, Option<[f32; 4]>);

/// Where a motion takes the cursor, and how `y` + motion takes it: whole
/// lines (`j` `k`), or characters, with the one it lands on (`e` `$` `f`)
/// or without (`w` `b` `0` `F`).
struct Motion {
    to: Res<Pt>,
    lines: bool,
    incl: bool,
}

/// Longest text one yank puts on the clipboard.
const MAX_YANK: usize = 1 << 20;

impl App {
    fn nav(&self) -> Nav<'_> {
        Nav {
            texts: &self.texts,
            pages: self.sizes.len(),
        }
    }

    /// Asks the worker for the text of `page`, unless it has it or has been asked.
    fn want_text(&mut self, page: usize) {
        if page < self.sizes.len() && !self.texts.contains_key(&page) && self.text_asked.insert(page) {
            self.worker.push(Job::Text(page));
        }
    }

    /// Forgets all of it (the document was reloaded).
    pub fn visual_reset(&mut self) {
        self.visual = None;
        self.entering = None;
        self.vpaint.clear();
        self.vlast = None;
        self.texts.clear();
        self.text_asked.clear();
    }

    /// Whether the page-fraction box `r` of `page` is all on screen.
    fn shows(&self, page: usize, r: [f32; 4]) -> bool {
        let rows = self.geom().rows;
        let (r0, r1) = (self.layout.row_in(page, r[1]), self.layout.row_in(page, r[3]));
        page < self.layout.pages.len() && r0 >= self.top && r1 < self.top + rows
    }

    /// `v` / `V`: where a forward search just went (its word, or its line);
    /// else, as bookokrat does, where the cursor was when visual mode was
    /// last left, if that is still on screen; else the search match on
    /// screen (its first character); else the start of the third line of
    /// text from the top of the screen.
    pub fn visual_enter(&mut self, line: bool) {
        let (page, fx, fy, exact) = self.reading_spot();
        let forward = self.spot.filter(|s| s.0 == self.top);
        let last = self.vlast.filter(|&(p, r)| self.shows(p, r));
        let (page, fx, fy, start) = if let Some((_, _, _, x)) = forward {
            match x {
                Some(x) => (page, x, fy, Start::Word),
                None => (page, fx, fy, Start::Line),
            }
        } else if let Some((p, r)) = last {
            (p, (r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0, Start::Exact)
        } else if exact {
            // reading_spot found the search match: its first character.
            let first = self
                .search
                .cur
                .and_then(|(p, i)| Some((p, *self.search.hits.get(&p)?.get(i)?.first()?)));
            match first {
                Some((p, r)) => (p, r[0] + 0.002, (r[1] + r[3]) / 2.0, Start::Exact),
                None => (page, fx, fy, Start::Exact),
            }
        } else {
            let p = self.layout.page_at(self.top);
            let b = self.layout.pages[p];
            let fy = self.layout.frac_at(p, 0.0, self.top.saturating_sub(b.y) as f32).1;
            (p, 0.0, fy, Start::Top)
        };
        self.entering = Some(Entering {
            page,
            fx,
            fy,
            start,
            line,
        });
        self.want_text(page);
        self.visual_begin();
    }

    /// Starts the selection once the text of the page is here.
    fn visual_begin(&mut self) {
        let Some(page) = self.entering.as_ref().map(|e| e.page) else {
            return;
        };
        let Some(t) = self.texts.get(&page).cloned() else {
            return;
        };
        let Some(e) = self.entering.take() else { return };
        let i = match e.start {
            Start::Top => {
                // The first row of text whose top shows, two rows on (if those show too).
                let rows = t.rows_from(e.fy - 0.001);
                let Some(&first) = rows.first() else {
                    // Nothing of this page's text shows: try the next page, if it does.
                    let next = page + 1;
                    if next < self.layout.pages.len() && self.layout.pages[next].y < self.top + self.geom().rows {
                        self.entering = Some(Entering {
                            page: next,
                            fy: 0.0,
                            ..e
                        });
                        self.want_text(next);
                        return self.visual_begin();
                    }
                    return self.visual_begin_at(page, &t, e.fx, e.fy, false, e.line);
                };
                let mut at = first;
                for &l in rows.iter().take(3) {
                    let r = [0.0, t.lines[l].y0, 1.0, t.lines[l].y1];
                    if self.shows(page, r) {
                        at = l;
                    }
                }
                t.lines[at].start
            }
            Start::Word => {
                let Some(i) = t.nearest(e.fx, e.fy) else {
                    return self.visual_begin_at(page, &t, e.fx, e.fy, false, e.line);
                };
                t.word_at(t.line_of(i), e.fx)
            }
            _ => return self.visual_begin_at(page, &t, e.fx, e.fy, e.start == Start::Exact, e.line),
        };
        self.visual_start(page, &t, i, e.line);
    }

    /// The selection starts at the character nearest to (`fx`, `fy`), or,
    /// unless `exact`, at the start of its line.
    fn visual_begin_at(&mut self, page: usize, t: &visual::Text, fx: f32, fy: f32, exact: bool, line: bool) {
        let Some(mut i) = t.nearest(fx, fy) else {
            self.texts.remove(&page);
            return self.say("no text on this page");
        };
        if !exact {
            i = t.lines[t.line_of(i)].start;
        }
        self.visual_start(page, t, i, line);
    }

    /// `v` puts just the cursor there; `V` selects its line at once.
    fn visual_start(&mut self, page: usize, t: &visual::Text, i: usize, line: bool) {
        let at = Pt { page, i };
        self.visual = Some(match line {
            true => Visual::new(at, true, t.mid_x(i)),
            false => Visual::cursor_at(at, t.mid_x(i)),
        });
        self.msg = None; // the mode shows in the status line, as in Vim
        self.visual_changed();
    }

    /// Leaves visual mode. The pages go back to the images they had before
    /// it (still held, usually), and the cursor's place is remembered for
    /// the next `v`.
    pub fn visual_exit(&mut self) {
        if let Some(v) = self.visual.take() {
            let r = self.texts.get(&v.cursor.page).and_then(|t| t.chars.get(v.cursor.i));
            self.vlast = r.map(|c| (v.cursor.page, c.r));
        }
        self.vpaint.clear();
        self.entering = None;
        self.texts.clear();
        self.text_asked.clear();
        self.dirty = true;
    }

    /// What visual mode paints on strip `k` of `page`, as a number added to
    /// the strip's version: 0 for nothing, else a hash of the boxes that
    /// touch it. So a cursor move draws again only the strips it changes,
    /// and leaving visual mode finds the plain strips still there.
    pub fn visual_ver(&self, page: usize, k: u32) -> u32 {
        let Some((sel, cur)) = self.vpaint.get(&page) else {
            return 0;
        };
        let (p, g) = (&self.layout.pages[page], &self.layout.geom);
        let r0 = k * p.strip_rows;
        let r1 = r0 + self.layout.strip_height(page, k);
        // In page pixels, with the pixel tint() adds around a box, and one more.
        let (y0, y1, h) = (g.px_y(r0) as f32 - 2.0, g.px_y(r1) as f32 + 2.0, p.px_h as f32);
        let mut hash = DefaultHasher::new();
        let mut any = false;
        let boxes = sel.iter().map(|r| (1u8, r)).chain(cur.iter().map(|r| (2u8, r)));
        for (kind, r) in boxes.filter(|(_, r)| r[3] * h > y0 && r[1] * h < y1) {
            any = true;
            kind.hash(&mut hash);
            r.map(f32::to_bits).hash(&mut hash);
        }
        match any {
            true => hash.finish() as u32 | 1,
            false => 0,
        }
    }

    /// The text of a page came from the worker.
    pub fn visual_text(&mut self, page: usize, text: Arc<visual::Text>) {
        self.text_asked.remove(&page);
        if self.visual.is_none() && self.entering.is_none() {
            return; // left visual mode meanwhile
        }
        self.texts.insert(page, text);
        if self.entering.as_ref().is_some_and(|e| e.page == page) {
            return self.visual_begin();
        }
        let aim = self.visual.as_ref().and_then(|v| v.aim).filter(|a| a.0 == page);
        if let Some((p, fx, fy, goal)) = aim {
            if let Some(v) = self.visual.as_mut() {
                v.aim = None;
            }
            return self.visual_aim(p, fx, fy, goal);
        }
        if self.visual.as_ref().is_some_and(|v| v.yank_wait) {
            self.visual_yank(false);
        }
        self.visual_changed();
    }

    /// What `page` is drawn with: the selected text's boxes, the cursor's.
    pub fn visual_paint(&self, page: usize) -> Paint {
        self.vpaint.get(&page).cloned().unwrap_or_default()
    }

    /// The selection or cursor changed: work out what each page it touches
    /// is painted with (the strips that change are drawn again, see
    /// `visual_ver`), have the text of the pages around the cursor ready,
    /// and keep the cursor on screen.
    fn visual_changed(&mut self) {
        let Some(v) = self.visual.take() else {
            return;
        };
        self.vpaint = v
            .pages()
            .map(|page| (page, v.rects(&self.texts, page)))
            .filter(|(_, (sel, cur))| !sel.is_empty() || cur.is_some())
            .collect();
        let at = v.cursor.page;
        self.visual = Some(v);
        for p in at.saturating_sub(1)..=at + 1 {
            self.want_text(p);
        }
        self.visual_reveal();
        self.dirty = true;
    }

    /// Scrolls (and pans) just enough to show the cursor, with a line to spare.
    fn visual_reveal(&mut self) {
        let Some(v) = &self.visual else { return };
        let Some(r) = self
            .texts
            .get(&v.cursor.page)
            .and_then(|t| t.chars.get(v.cursor.i))
            .map(|c| c.r)
        else {
            return;
        };
        let page = v.cursor.page;
        let g = self.geom();
        let (r0, r1) = (self.layout.row_in(page, r[1]), self.layout.row_in(page, r[3]));
        let margin = 1.min(g.rows.saturating_sub(r1 - r0 + 1) / 2);
        if r0 < self.top + margin {
            self.top = r0.saturating_sub(margin);
        } else if r1 + margin >= self.top + g.rows {
            self.top = r1 + margin + 1 - g.rows;
        }
        let b = self.layout.pages[page];
        if b.cols > g.cols {
            let (c0, c1) = (self.layout.col_in(page, r[0]), self.layout.col_in(page, r[2]));
            let left = self.layout.left_of(page, self.left);
            if c0 < left + 2 {
                self.left = c0.saturating_sub(4);
            } else if c1 + 4 >= left + g.cols {
                self.left = (c1 + 5).saturating_sub(g.cols);
            }
            self.left = self.left.min(b.cols - g.cols);
        }
        self.clamp();
    }

    /// Moves the cursor to the character nearest to (`fx`, `fy`) on `page`,
    /// or, if its text has not come, as soon as it has.
    fn visual_aim(&mut self, page: usize, fx: f32, fy: f32, set_goal: bool) {
        let Some(v) = self.visual.as_mut() else { return };
        let Some(t) = self.texts.get(&page) else {
            v.aim = Some((page, fx, fy, set_goal));
            return self.want_text(page);
        };
        if let Some(i) = t.nearest(fx, fy) {
            v.cursor = Pt { page, i };
            if set_goal {
                v.goal = t.mid_x(i);
            }
        }
        self.visual_changed();
    }

    /// A search went to a match: the cursor goes along.
    pub fn visual_follow(&mut self, page: usize, fx: f32, fy: f32) {
        if self.visual.is_some() {
            self.visual_aim(page, fx, fy, true);
        }
    }

    /// Handles a key in visual mode: with just the cursor (Vim's normal
    /// mode) or selecting (its visual mode). False: not a visual key, let
    /// the normal ones have it (zoom, help, the prompts and so on).
    pub fn visual_key(&mut self, k: Key, n: Option<u32>) -> bool {
        if matches!(k, Key::Esc | Key::Ctrl('c')) {
            self.visual_exit();
            return true;
        }
        let Some(v) = self.visual.as_mut() else { return true };
        if !v.pend.is_empty() {
            let pend = std::mem::take(&mut v.pend);
            // A count before the command and one inside it (`2y3w`) multiply.
            let n = match (v.pend_n.take(), n) {
                (Some(a), Some(b)) => Some(a.saturating_mul(b)),
                (a, b) => a.or(b),
            };
            self.dirty = true;
            self.visual_pending(&pend, k, n);
            return true;
        }
        let select = v.select;
        let n1 = n.unwrap_or(1);
        let rows = self.geom().rows as i64 * n1 as i64;
        match k {
            Key::Char('v') => self.visual_v(false),
            Key::Char('V') => self.visual_v(true),
            Key::Char('o' | 'O') if select => {
                if let Some(v) = self.visual.as_mut() {
                    std::mem::swap(&mut v.anchor, &mut v.cursor);
                    if let Some(t) = self.texts.get(&v.cursor.page) {
                        v.goal = t.mid_x(v.cursor.i);
                    }
                }
                self.visual_changed();
            }
            Key::Char('y') if select => self.visual_yank(false),
            Key::Char('Y') if select => self.visual_yank(true),
            Key::Char('Y') => self.visual_yank_lines(n1),
            Key::Char(c @ ('y' | 'g' | 'f' | 'F' | 't' | 'T')) => self.visual_pend(c, n),
            Key::Char(c @ ('i' | 'a')) => self.visual_pend(c, n),
            Key::Char(';') => self.visual_refind(n1, false),
            Key::Char(',') => self.visual_refind(n1, true),
            Key::Char('n') => self.search_next(!self.search.backward),
            Key::Char('N') => self.search_next(self.search.backward),
            Key::Char('H') => self.visual_screen(0),
            Key::Char('M') => self.visual_screen(1),
            Key::Char('L') => self.visual_screen(2),
            Key::Char('G') => self.visual_goto(n.map(|p| p as usize - 1)),
            Key::Ctrl('d') => self.visual_page(rows / 2),
            Key::Ctrl('u') => self.visual_page(-rows / 2),
            Key::Ctrl('f') | Key::PageDown => self.visual_page(rows),
            Key::Ctrl('b') | Key::PageUp => self.visual_page(-rows),
            Key::Char('z' | 'Z' | '+' | '=' | '-' | '?' | '/' | ':' | 'r' | 'q') | Key::Ctrl('l') => return false,
            _ => {
                let Some(v) = &self.visual else { return true };
                if let Some(m) = self.motion(k, n1, v.cursor, v.goal) {
                    self.visual_move(m.to, m.lines);
                }
            }
        }
        true
    }

    /// The cursor motions, for moving and for `y` + motion.
    fn motion(&self, k: Key, n: u32, cur: Pt, goal: f32) -> Option<Motion> {
        let nav = self.nav();
        let m = |to, incl| Some(Motion { to, lines: false, incl });
        let lines = |to| {
            Some(Motion {
                to,
                lines: true,
                incl: true,
            })
        };
        match k {
            Key::Char('h') | Key::Left | Key::Backspace => m(nav.left(cur, n), false),
            Key::Char('l') | Key::Right | Key::Char(' ') => m(nav.right(cur, n), false),
            Key::Char('j') | Key::Down | Key::Ctrl('n') | Key::Ctrl('e') => lines(nav.vertical(cur, n, true, goal)),
            Key::Char('k') | Key::Up | Key::Ctrl('p') | Key::Ctrl('y') => lines(nav.vertical(cur, n, false, goal)),
            Key::Char('w') => m(nav.word_fwd(cur, n, false), false),
            Key::Char('W') => m(nav.word_fwd(cur, n, true), false),
            Key::Char('b') => m(nav.word_back(cur, n, false), false),
            Key::Char('B') => m(nav.word_back(cur, n, true), false),
            Key::Char('e') => m(nav.word_end(cur, n, false), true),
            Key::Char('E') => m(nav.word_end(cur, n, true), true),
            Key::Char('0') | Key::Home => m(nav.line_start(cur), false),
            Key::Char('^') => m(nav.first_solid(cur), false),
            Key::Char('$') | Key::End => m(nav.line_end(cur), true),
            Key::Char('}') => m(nav.block(cur, n, true), false),
            Key::Char('{') => m(nav.block(cur, n, false), false),
            _ => None,
        }
    }

    /// The first key of a command that needs more (`f` and its character,
    /// `y` and its motion, `g`, `i`/`a` and an object), with its count.
    fn visual_pend(&mut self, c: char, n: Option<u32>) {
        if let Some(v) = self.visual.as_mut() {
            v.pend.push(c);
            v.pend_n = n;
        }
        self.dirty = true; // the status line shows it
    }

    /// The key that finishes (or goes on with) the command in `pend`.
    fn visual_pending(&mut self, pend: &str, k: Key, n: Option<u32>) {
        let Some(v) = &self.visual else { return };
        let (cur, goal) = (v.cursor, v.goal);
        let n1 = n.unwrap_or(1);
        let Key::Char(c) = k else { return };
        match (pend, c) {
            ("g", 'g') => self.visual_goto(Some(n.map_or(0, |p| p as usize - 1))),
            ("g", 'd') => self.visual_inverse(),
            ("f" | "F" | "t" | "T", _) => {
                let (fwd, till) = (matches!(pend, "f" | "t"), matches!(pend, "t" | "T"));
                if let Some(v) = self.visual.as_mut() {
                    v.last_find = Some((c, fwd, till));
                }
                self.visual_find(c, n1, fwd, till, false);
            }
            ("i" | "a", 'w' | 'W') => {
                if let Ok((a, b)) = self.nav().word_object(cur, pend == "a", c == 'W') {
                    // With just the cursor, `iw` starts selecting the word (as `viw`).
                    if let Some(v) = self.visual.as_mut() {
                        if !v.select {
                            (v.select, v.line) = (true, false);
                        }
                        (v.anchor, v.cursor) = (a, b);
                    }
                    self.visual_changed();
                }
            }
            ("y", 'y') => self.visual_yank_lines(n1),
            ("y", 'f' | 'F' | 't' | 'T' | 'i' | 'a') => {
                if let Some(v) = self.visual.as_mut() {
                    v.pend = format!("y{c}");
                    v.pend_n = n;
                }
            }
            ("y", _) => {
                if let Some(m) = self.motion(k, n1, cur, goal) {
                    self.yank_motion(cur, m);
                }
            }
            ("yi" | "ya", 'w' | 'W') => {
                if let Ok((a, b)) = self.nav().word_object(cur, pend == "ya", c == 'W') {
                    self.yank_span(a, b, false);
                }
            }
            ("yf" | "yF" | "yt" | "yT", _) => {
                let (fwd, till) = (matches!(pend, "yf" | "yt"), matches!(pend, "yt" | "yT"));
                if let Ok(Some(to)) = self.nav().find(cur, c, n1, fwd, till, false) {
                    // Forward finds take the character they land on, backward ones do not.
                    self.yank_motion(
                        cur,
                        Motion {
                            to: Ok(to),
                            lines: false,
                            incl: fwd,
                        },
                    );
                }
            }
            _ => {}
        }
    }

    /// `v` / `V`. With just the cursor: start selecting there (by
    /// characters / lines). Selecting: switch between the two; or, the same
    /// key again, stop selecting (back to just the cursor), or, if the
    /// selection has not grown from where it began, leave visual mode.
    fn visual_v(&mut self, line: bool) {
        let Some(v) = self.visual.as_mut() else { return };
        if !v.select {
            (v.select, v.line, v.anchor) = (true, line, v.cursor);
        } else if v.line != line {
            v.line = line;
        } else if v.anchor == v.cursor {
            return self.visual_exit();
        } else {
            (v.select, v.line, v.anchor) = (false, false, v.cursor);
        }
        self.visual_changed();
    }

    /// `f` `F` `t` `T` (and `;` `,`, with `again`): along the cursor's line.
    fn visual_find(&mut self, c: char, n: u32, fwd: bool, till: bool, again: bool) {
        let Some(v) = &self.visual else { return };
        match self.nav().find(v.cursor, c, n, fwd, till, again) {
            Ok(Some(p)) => self.visual_move(Ok(p), false),
            Ok(None) => {} // not on this line: stay, as Vim does
            Err(page) => self.visual_move(Err(page), false),
        }
    }

    /// `;` repeats the last `f` `F` `t` `T`, `,` the other way.
    fn visual_refind(&mut self, n: u32, reverse: bool) {
        let Some((c, fwd, till)) = self.visual.as_ref().and_then(|v| v.last_find) else {
            return;
        };
        self.visual_find(c, n, fwd != reverse, till, true);
    }

    /// `gg` / `G`: the start of page `page` (a count), else of the first
    /// page for `gg`, the last line of the last page for `G` (None).
    fn visual_goto(&mut self, page: Option<usize>) {
        let last = self.layout.pages.len().saturating_sub(1);
        match page {
            Some(p) => self.visual_aim(p.min(last), 0.0, 0.0, true),
            None => self.visual_aim(last, 0.0, 1.0, true),
        }
    }

    /// `H` `M` `L`: the line at the top, middle or bottom of the screen.
    fn visual_screen(&mut self, which: u32) {
        let rows = self.geom().rows;
        let row = self.top
            + match which {
                0 => 0,
                1 => rows / 2,
                _ => rows.saturating_sub(1),
            };
        let p = self.layout.page_at(row).min(self.layout.pages.len() - 1);
        let b = self.layout.pages[p];
        let fy = self.layout.frac_at(p, 0.0, row.saturating_sub(b.y) as f32 + 0.5).1;
        self.visual_aim(p, 0.0, fy, true);
    }

    fn visual_move(&mut self, to: Res<Pt>, vertical: bool) {
        match to {
            Ok(p) => {
                let goal = self.texts.get(&p.page).map(|t| t.mid_x(p.i));
                if let Some(v) = self.visual.as_mut() {
                    v.cursor = p;
                    if let Some(g) = goal.filter(|_| !vertical) {
                        v.goal = g;
                    }
                }
                self.visual_changed();
            }
            Err(page) => {
                self.want_text(page);
                self.say("loading text…");
            }
        }
    }

    /// Ctrl-d and friends: the view scrolls `rows`, and the cursor goes as
    /// far, staying at its place on the screen.
    fn visual_page(&mut self, rows: i64) {
        let Some(v) = &self.visual else { return };
        let (cur, goal) = (v.cursor, v.goal);
        let Some(r) = self.texts.get(&cur.page).map(|t| t.chars[cur.i].r) else {
            return;
        };
        let row = self.layout.row_in(cur.page, (r[1] + r[3]) / 2.0) as i64;
        let before = self.top as i64;
        self.scroll(rows);
        let moved = self.top as i64 - before;
        let last = self.layout.pages.last().map_or(0, |b| b.y + b.rows) as i64 - 1;
        let target = (row + if moved == 0 { rows } else { moved }).clamp(0, last.max(0)) as u32;
        let p = self.layout.page_at(target);
        let b = self.layout.pages[p];
        let (_, fy) = self.layout.frac_at(p, 0.0, target.saturating_sub(b.y) as f32 + 0.5);
        self.visual_aim(p, goal, fy, false);
    }

    /// `y`: the selection to the system clipboard; then, as in Vim, just
    /// the cursor, at the start of what was selected. With `lines`, as
    /// whole lines (Vim's `Y`).
    fn visual_yank(&mut self, lines: bool) {
        let Some(v) = self.visual.as_mut() else { return };
        v.line |= lines;
        let nav = Nav {
            texts: &self.texts,
            pages: self.sizes.len(),
        };
        match v.text(&nav) {
            Ok(text) => {
                let line = v.line;
                let start = v.ends().0;
                (v.select, v.line, v.yank_wait, v.cursor, v.anchor) = (false, false, false, start, start);
                self.copied(&text, line);
                self.visual_changed();
            }
            Err(_) => {
                // Pages between the ends have not come yet; yank when they have.
                v.yank_wait = true;
                let (a, b) = v.ends();
                for p in a.page..=b.page {
                    self.want_text(p);
                }
                self.say("loading text…");
            }
        }
    }

    /// `yy` and `Y` with just the cursor: `n` lines from the cursor's down.
    fn visual_yank_lines(&mut self, n: u32) {
        let Some(v) = &self.visual else { return };
        let (cur, goal) = (v.cursor, v.goal);
        let to = self.nav().vertical(cur, n.saturating_sub(1), true, goal);
        self.yank_motion(
            cur,
            Motion {
                to,
                lines: true,
                incl: true,
            },
        );
    }

    /// `y` + a motion: from the cursor to where the motion goes (not taking
    /// that character, for an exclusive motion), or the lines between.
    fn yank_motion(&mut self, cur: Pt, m: Motion) {
        let to = match m.to {
            Ok(p) => p,
            Err(page) => {
                self.want_text(page);
                return self.say("loading text… (try again)");
            }
        };
        let (a, b) = (cur.min(to), cur.max(to));
        if m.lines || m.incl {
            return self.yank_span(a, b, m.lines);
        }
        if a == b {
            return;
        }
        if let Ok(Some(b)) = self.nav().prev(b) {
            self.yank_span(a, b, false);
        }
    }

    /// Copies characters `a..=b` (by lines, their lines), the cursor staying.
    fn yank_span(&mut self, a: Pt, b: Pt, line: bool) {
        let mut sel = Visual::new(a, line, 0.0);
        sel.cursor = b;
        match sel.text(&self.nav()) {
            Ok(text) => self.copied(&text, line),
            Err(_) => {
                for p in a.page..=b.page {
                    self.want_text(p);
                }
                self.say("loading text… (try again)");
            }
        }
    }

    /// Puts `text` on the clipboard and says so.
    fn copied(&mut self, text: &str, line: bool) {
        if text.trim().is_empty() {
            return self.say("nothing to copy");
        }
        self.copy(text);
        let (n, what) = match line {
            true => (text.lines().count(), "line"),
            false => (text.chars().count(), "character"),
        };
        self.say(&format!("{n} {what}{} copied", if n == 1 { "" } else { "s" }));
    }

    /// Puts `text` on the clipboard of the machine the terminal is on, with
    /// OSC 52 (works over SSH and through herdr).
    fn copy(&mut self, text: &str) {
        let mut end = text.len().min(MAX_YANK);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut out = b"\x1b]52;c;".to_vec();
        encode::base64(&mut out, &text.as_bytes()[..end]);
        out.push(0x07);
        self.writer().frame(out);
        debug_log!("copied {end} bytes");
    }

    /// `gd` in visual mode: the source line of the cursor's character.
    pub fn visual_inverse(&mut self) {
        let Some(v) = &self.visual else { return };
        let c = v.cursor;
        let Some(r) = self.texts.get(&c.page).and_then(|t| t.chars.get(c.i)).map(|c| c.r) else {
            return;
        };
        self.inverse(c.page, (r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0);
    }
}
