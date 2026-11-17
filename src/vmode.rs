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
/// spot on it, whether the spot is a particular place (else the cursor goes
/// to the start of its line), and whether the selection is by lines.
pub struct Entering {
    pub page: usize,
    pub fx: f32,
    pub fy: f32,
    pub exact: bool,
    pub line: bool,
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
        self.texts.clear();
        self.text_asked.clear();
    }

    pub fn visual_enter(&mut self, line: bool) {
        let (page, mut fx, mut fy, exact) = self.reading_spot();
        // On a search match, the cursor starts at its first character.
        if let Some((p, i)) = self.search.cur.filter(|c| c.0 == page) {
            if let Some(r) = self.search.hits.get(&p).and_then(|v| v.get(i)).and_then(|m| m.first()) {
                let mid = (r[1] + r[3]) / 2.0;
                if exact && r[0] <= fx && fx <= r[2] && (mid - fy).abs() < 0.01 {
                    (fx, fy) = (r[0] + 0.002, mid);
                }
            }
        }
        self.entering = Some(Entering {
            page,
            fx,
            fy,
            exact,
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
        let Some(mut i) = t.nearest(e.fx, e.fy) else {
            self.texts.remove(&page);
            return self.say("no text on this page");
        };
        if !e.exact {
            i = t.lines[t.line_of(i)].start;
        }
        self.visual = Some(Visual::new(Pt { page, i }, e.line, t.mid_x(i)));
        self.msg = None; // the mode shows in the status line, as in Vim
        self.visual_changed();
    }

    pub fn visual_exit(&mut self) {
        if let Some(v) = self.visual.take() {
            for p in v.painted.keys() {
                self.bump(*p);
            }
        }
        self.entering = None;
        self.texts.clear();
        self.text_asked.clear();
        self.dirty = true;
    }

    fn bump(&mut self, page: usize) {
        if let Some(v) = self.page_ver.get_mut(page) {
            *v = v.wrapping_add(1);
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
    pub fn visual_paint(&self, page: usize) -> (Vec<[f32; 4]>, Option<[f32; 4]>) {
        self.visual
            .as_ref()
            .map_or((Vec::new(), None), |v| v.rects(&self.texts, page))
    }

    /// The selection or cursor changed: draw the pages it touched again,
    /// have the text of the pages around the cursor ready, and keep the
    /// cursor on screen.
    fn visual_changed(&mut self) {
        let Some(mut v) = self.visual.take() else {
            return;
        };
        let mut now = HashMap::new();
        for page in v.pages() {
            let (sel, cur) = v.rects(&self.texts, page);
            let mut h = DefaultHasher::new();
            for r in sel.iter().chain(cur.iter()) {
                r.map(f32::to_bits).hash(&mut h);
            }
            cur.is_some().hash(&mut h);
            now.insert(page, h.finish());
        }
        for (page, h) in &now {
            if v.painted.get(page) != Some(h) {
                self.bump(*page);
            }
        }
        for page in v.painted.keys().filter(|p| !now.contains_key(p)) {
            self.bump(*page);
        }
        v.painted = now;
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

    /// Handles a key in visual mode. False: not a visual key, let the
    /// normal ones have it (zoom, invert, help, the prompts and so on).
    pub fn visual_key(&mut self, k: Key, n: Option<u32>) -> bool {
        let n1 = n.unwrap_or(1);
        match k {
            Key::Esc | Key::Ctrl('c') => self.visual_exit(),
            Key::Char('v') | Key::Char('V') => {
                let line = k == Key::Char('V');
                match self.visual.as_mut() {
                    Some(v) if v.line != line => v.line = line,
                    _ => {
                        self.visual_exit();
                        return true;
                    }
                }
                self.visual_changed();
            }
            Key::Char('o') | Key::Char('O') => {
                if let Some(v) = self.visual.as_mut() {
                    std::mem::swap(&mut v.anchor, &mut v.cursor);
                    if let Some(t) = self.texts.get(&v.cursor.page) {
                        v.goal = t.mid_x(v.cursor.i);
                    }
                }
                self.visual_changed();
            }
            Key::Char('y') => self.visual_yank(false),
            Key::Char('Y') => self.visual_yank(true),
            Key::Char('g') => self.pending = Some('g'),
            Key::Char('n') => self.search_next(!self.search.backward),
            Key::Char('N') => self.search_next(self.search.backward),
            Key::Ctrl('d') => self.visual_page(self.geom().rows as i64 / 2 * n1 as i64),
            Key::Ctrl('u') => self.visual_page(-(self.geom().rows as i64) / 2 * n1 as i64),
            Key::Ctrl('f') | Key::PageDown => self.visual_page(self.geom().rows as i64 * n1 as i64),
            Key::Ctrl('b') | Key::PageUp => self.visual_page(-(self.geom().rows as i64) * n1 as i64),
            Key::Char('z' | 'Z' | '+' | '=' | '-' | 'i' | '?' | '/' | ':' | 'r' | 'H' | 'L' | 'q') | Key::Ctrl('l') => {
                return false
            }
            _ => {
                let Some(v) = &self.visual else { return true };
                let (cur, goal) = (v.cursor, v.goal);
                let nav = self.nav();
                let mut vertical = false;
                let to: Res<Pt> = match k {
                    Key::Char('h') | Key::Left | Key::Backspace => nav.left(cur, n1),
                    Key::Char('l') | Key::Right | Key::Char(' ') => nav.right(cur, n1),
                    Key::Char('j') | Key::Down | Key::Ctrl('n') | Key::Ctrl('e') => {
                        vertical = true;
                        nav.vertical(cur, n1, true, goal)
                    }
                    Key::Char('k') | Key::Up | Key::Ctrl('p') | Key::Ctrl('y') => {
                        vertical = true;
                        nav.vertical(cur, n1, false, goal)
                    }
                    Key::Char('w') => nav.word_fwd(cur, n1),
                    Key::Char('b') => nav.word_back(cur, n1),
                    Key::Char('e') => nav.word_end(cur, n1),
                    Key::Char('0') | Key::Home => nav.line_start(cur),
                    Key::Char('^') => nav.first_solid(cur),
                    Key::Char('$') | Key::End => nav.line_end(cur),
                    Key::Char('}') => nav.block(cur, n1, true),
                    Key::Char('{') => nav.block(cur, n1, false),
                    _ => return true,
                };
                self.visual_move(to, vertical);
            }
        }
        true
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

    /// `y`: the selection to the system clipboard, and out of visual mode.
    /// With `lines`, as whole lines (Vim's `Y`).
    fn visual_yank(&mut self, lines: bool) {
        let Some(v) = self.visual.as_mut() else { return };
        v.line |= lines;
        let nav = Nav {
            texts: &self.texts,
            pages: self.sizes.len(),
        };
        match v.text(&nav) {
            Ok(text) => {
                let (n, line) = (text.chars().count(), v.line);
                self.visual_exit();
                if text.trim().is_empty() {
                    return self.say("nothing to copy");
                }
                self.copy(&text);
                match line {
                    true => self.say(&format!("{} lines copied", text.lines().count())),
                    false => self.say(&format!("{n} characters copied")),
                }
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
