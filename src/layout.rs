//! Where every page sits, in terminal cells, and how it is cut into images.
//!
//! Pages are stacked vertically (continuous scrolling) with a one-row gap.
//! Each page is rendered at cells x cell-pixels (times `scale`, which is 1
//! unless the screen is too big for the terminal's image memory), so the
//! terminal never rescales it. It is cut into horizontal strips, and a page
//! wider than the view also into vertical tiles; each piece is its own image.
//! Pieces keep every image under the terminals' limits (Ghostty and herdr
//! refuse images over 10000 px on a side; herdr drops any over 30 MiB once
//! base64-encoded), let a scroll or a pan re-place images instead of
//! re-sending them, and let the first pieces show before the rest are ready.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fit {
    Width,
    Page,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Geom {
    /// Columns and rows available for pages (the status line excluded).
    pub cols: u32,
    pub rows: u32,
    /// Cell size in pixels.
    pub cw: u32,
    pub ch: u32,
    /// Rendered pixels per screen pixel; below 1 only when a full screen of
    /// images would not fit in the terminal's image memory.
    pub scale: f32,
}

impl Geom {
    /// Rendered pixels in `n` columns, and in `n` rows.
    pub fn px_x(&self, n: u32) -> u32 {
        if self.scale == 1.0 {
            n * self.cw
        } else {
            (n as f32 * self.cw as f32 * self.scale).round() as u32
        }
    }

    pub fn px_y(&self, n: u32) -> u32 {
        if self.scale == 1.0 {
            n * self.ch
        } else {
            (n as f32 * self.ch as f32 * self.scale).round() as u32
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PageBox {
    /// First row in document coordinates, and height in rows.
    pub y: u32,
    pub rows: u32,
    /// Width in columns, and the column it starts at when narrower than the
    /// view (centered).
    pub cols: u32,
    pub x: u32,
    /// Size of the rendered page in pixels.
    pub px_w: u32,
    pub px_h: u32,
    /// Rows per strip and columns per tile (the last ones may be smaller).
    pub strip_rows: u32,
    pub tile_cols: u32,
}

pub struct Layout {
    pub pages: Vec<PageBox>,
    /// Rows in the whole document, gaps included.
    pub total: u32,
    pub geom: Geom,
}

pub const GAP: u32 = 1;
/// Raw RGB bytes per piece at most: far from herdr's 30 MiB limit.
const PIECE_BYTES: u64 = 4 << 20;
/// Pixels on a side at most: Ghostty and herdr refuse more than 10000.
pub const MAX_SIDE: u32 = 8192;

/// One visible part of a piece: its rows [a, b) and columns [c0, c1), shown
/// with its top-left at screen (`screen_row`, `screen_col`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Visible {
    pub page: usize,
    pub k: u32,
    pub t: u32,
    pub a: u32,
    pub b: u32,
    pub c0: u32,
    pub c1: u32,
    pub screen_row: u32,
    pub screen_col: u32,
}

/// Screen cells: rows [r0, r1), columns [c0, c1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub r0: u32,
    pub c0: u32,
    pub r1: u32,
    pub c1: u32,
}

impl Visible {
    /// The parts of this outside `hole` (up to four), so a popup drawn there
    /// shows: terminals draw images over text.
    pub fn around(&self, hole: Rect) -> Vec<Visible> {
        let (r0, r1) = (self.screen_row, self.screen_row + self.b - self.a);
        let (c0, c1) = (self.screen_col, self.screen_col + self.c1 - self.c0);
        if hole.r1 <= r0 || hole.r0 >= r1 || hole.c1 <= c0 || hole.c0 >= c1 {
            return vec![*self];
        }
        let (mr0, mr1) = (r0.max(hole.r0), r1.min(hole.r1));
        [
            (r0, mr0, c0, c1),
            (mr1, r1, c0, c1),
            (mr0, mr1, c0, hole.c0.min(c1)),
            (mr0, mr1, hole.c1.max(c0), c1),
        ]
        .into_iter()
        .filter(|&(top, bottom, left, right)| top < bottom && left < right)
        .map(|(top, bottom, left, right)| Visible {
            a: self.a + (top - r0),
            b: self.a + (bottom - r0),
            c0: self.c0 + (left - c0),
            c1: self.c0 + (right - c0),
            screen_row: top,
            screen_col: left,
            ..*self
        })
        .collect()
    }
}

impl Layout {
    pub fn new(sizes: &[(f32, f32)], geom: Geom, fit: Fit, zoom: f32) -> Layout {
        let rcw = geom.cw as f32 * geom.scale;
        let rch = geom.ch as f32 * geom.scale;
        let (vw, vh) = (geom.cols as f32 * rcw, geom.rows as f32 * rch);
        // A strip much taller than a few lines would mostly hang off the
        // screen, costing memory and time to send.
        let max_strip_rows = (geom.rows / 8).max(2);
        let mut y = 0;
        let pages = sizes
            .iter()
            .map(|&(pw, ph)| {
                let s = zoom
                    * match fit {
                        Fit::Width => vw / pw,
                        Fit::Page => (vw / pw).min(vh / ph),
                    };
                let px_w = ((pw * s).round() as u32).max(1);
                let px_h = ((ph * s).round() as u32).max(1);
                let cols = ((px_w as f32 / rcw).ceil() as u32).max(1);
                let rows = ((px_h as f32 / rch).ceil() as u32).max(1);
                // Tiles: the whole width when the page fits the view, half
                // the view when it is zoomed wider, so a pan re-places tiles.
                let mut tile_cols = if cols <= geom.cols { cols } else { geom.cols.div_ceil(2) };
                while tile_cols > 1 && geom.px_x(tile_cols) > MAX_SIDE {
                    tile_cols = tile_cols.div_ceil(2);
                }
                let row_bytes = geom.px_x(tile_cols) as u64 * 3 * geom.px_y(1).max(1) as u64;
                let mut strip_rows = ((PIECE_BYTES / row_bytes.max(1)) as u32).clamp(1, max_strip_rows);
                while strip_rows > 1 && geom.px_y(strip_rows) > MAX_SIDE {
                    strip_rows /= 2;
                }
                let b = PageBox {
                    y,
                    rows,
                    cols,
                    x: geom.cols.saturating_sub(cols) / 2,
                    px_w,
                    px_h,
                    strip_rows: strip_rows.min(rows),
                    tile_cols,
                };
                y += rows + GAP;
                b
            })
            .collect();
        Layout {
            pages,
            total: y.saturating_sub(GAP),
            geom,
        }
    }

    pub fn strips(&self, page: usize) -> u32 {
        let p = &self.pages[page];
        p.rows.div_ceil(p.strip_rows)
    }

    pub fn tiles(&self, page: usize) -> u32 {
        let p = &self.pages[page];
        p.cols.div_ceil(p.tile_cols)
    }

    /// Rows in strip `k` of `page`.
    pub fn strip_height(&self, page: usize, k: u32) -> u32 {
        let p = &self.pages[page];
        p.strip_rows.min(p.rows - k * p.strip_rows)
    }

    /// Columns in tile `t` of `page`.
    pub fn tile_width(&self, page: usize, t: u32) -> u32 {
        let p = &self.pages[page];
        p.tile_cols.min(p.cols - t * p.tile_cols)
    }

    /// The bitmap of piece (`k`, `t`): its size, and where the page's
    /// top-left corner sits in it (see pdfium::Page::render).
    pub fn piece(&self, page: usize, k: u32, t: u32) -> (u32, u32, i32, i32) {
        let p = &self.pages[page];
        let g = &self.geom;
        let (r0, c0) = (k * p.strip_rows, t * p.tile_cols);
        let bw = g.px_x(c0 + self.tile_width(page, t)) - g.px_x(c0);
        let bh = g.px_y(r0 + self.strip_height(page, k)) - g.px_y(r0);
        (bw.max(1), bh.max(1), -(g.px_x(c0) as i32), -(g.px_y(r0) as i32))
    }

    /// Bytes the terminal holds for piece (`k`, `t`) at `bpp` bytes per pixel.
    pub fn piece_bytes(&self, page: usize, k: u32, t: u32, bpp: usize) -> usize {
        let (w, h, _, _) = self.piece(page, k, t);
        w as usize * h as usize * bpp
    }

    /// Where a visible part of a piece comes from in its bitmap: x, y, w, h.
    pub fn source(&self, v: &Visible) -> (u32, u32, u32, u32) {
        let p = &self.pages[v.page];
        let g = &self.geom;
        let (r0, c0) = (v.k * p.strip_rows, v.t * p.tile_cols);
        let (x0, y0) = (g.px_x(c0), g.px_y(r0));
        let (sx, sy) = (g.px_x(c0 + v.c0) - x0, g.px_y(r0 + v.a) - y0);
        (sx, sy, g.px_x(c0 + v.c1) - x0 - sx, g.px_y(r0 + v.b) - y0 - sy)
    }

    /// The page holding document row `row`; a row in a gap belongs to the
    /// page below it.
    pub fn page_at(&self, row: u32) -> usize {
        let i = self.pages.partition_point(|p| p.y + p.rows <= row);
        i.min(self.pages.len().saturating_sub(1))
    }

    pub fn max_top(&self) -> u32 {
        self.total.saturating_sub(self.geom.rows)
    }

    /// The first page column on screen, for a page wider than the view.
    pub fn left_of(&self, page: usize, left: u32) -> u32 {
        left.min(self.pages[page].cols.saturating_sub(self.geom.cols))
    }

    /// The tiles of `page` that are on screen when panned to `left`.
    pub fn tiles_in_view(&self, page: usize, left: u32) -> std::ops::Range<u32> {
        let p = &self.pages[page];
        if p.cols <= self.geom.cols {
            return 0..self.tiles(page);
        }
        let l = self.left_of(page, left);
        l / p.tile_cols..(l + self.geom.cols).div_ceil(p.tile_cols).min(self.tiles(page))
    }

    /// The document row of a point `frac` of the way down `page`.
    pub fn row_in(&self, page: usize, frac: f32) -> u32 {
        let p = &self.pages[page];
        p.y + ((frac * p.rows as f32) as u32).min(p.rows.saturating_sub(1))
    }

    /// The page column of a point `frac` of the way across `page`.
    pub fn col_in(&self, page: usize, frac: f32) -> u32 {
        let p = &self.pages[page];
        ((frac * p.cols as f32) as u32).min(p.cols.saturating_sub(1))
    }

    /// Everything on screen when the view starts at document row `top`,
    /// panned to column `left`.
    pub fn visible(&self, top: u32, left: u32) -> Vec<Visible> {
        let bottom = top + self.geom.rows;
        let mut out = Vec::new();
        let mut i = self.page_at(top);
        while i < self.pages.len() && self.pages[i].y < bottom {
            let p = &self.pages[i];
            let wide = p.cols > self.geom.cols;
            let l = self.left_of(i, left);
            for k in 0..self.strips(i) {
                let sy = p.y + k * p.strip_rows;
                let sh = self.strip_height(i, k);
                let (a, b) = (top.max(sy), bottom.min(sy + sh));
                if a >= b {
                    continue;
                }
                for t in self.tiles_in_view(i, left) {
                    let tx = t * p.tile_cols;
                    let tw = self.tile_width(i, t);
                    let (c0, c1, col) = if wide {
                        let (c0, c1) = (l.max(tx), (l + self.geom.cols).min(tx + tw));
                        if c0 >= c1 {
                            continue;
                        }
                        (c0 - tx, c1 - tx, c0 - l)
                    } else {
                        (0, tw, p.x + tx)
                    };
                    out.push(Visible {
                        page: i,
                        k,
                        t,
                        a: a - sy,
                        b: b - sy,
                        c0,
                        c1,
                        screen_row: a - top,
                        screen_col: col,
                    });
                }
            }
            i += 1;
        }
        out
    }

    /// Whether `other` cuts every page into the same images as this one, so
    /// the images already sent stay good (only where they go may differ).
    pub fn same_pieces(&self, other: &Layout) -> bool {
        let (a, b) = (&self.geom, &other.geom);
        (a.cw, a.ch, a.scale) == (b.cw, b.ch, b.scale)
            && self.pages.len() == other.pages.len()
            && self.pages.iter().zip(&other.pages).all(|(p, q)| {
                (p.px_w, p.px_h, p.rows, p.cols, p.strip_rows, p.tile_cols)
                    == (q.px_w, q.px_h, q.rows, q.cols, q.strip_rows, q.tile_cols)
            })
    }

    /// The most image memory a screen can take at `bpp` bytes per pixel:
    /// the view, plus the strips and tiles that stick out past its edges.
    pub fn worst_screen_bytes(&self, bpp: usize) -> usize {
        let g = &self.geom;
        self.pages
            .iter()
            .map(|p| {
                let w = if p.cols > g.cols {
                    (g.cols + p.tile_cols).min(p.cols)
                } else {
                    p.cols
                };
                let h = g.rows + 2 * (p.strip_rows - 1);
                g.px_x(w) as usize * g.px_y(h) as usize * bpp
            })
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LETTER: (f32, f32) = (612.0, 792.0);

    fn geom(cols: u32, rows: u32, cw: u32, ch: u32) -> Geom {
        Geom {
            cols,
            rows,
            cw,
            ch,
            scale: 1.0,
        }
    }

    /// Every cell of every page is in exactly one piece, and no piece is
    /// over the limits.
    fn check_pieces(l: &Layout) {
        for i in 0..l.pages.len() {
            let p = l.pages[i];
            let rows: u32 = (0..l.strips(i)).map(|k| l.strip_height(i, k)).sum();
            let cols: u32 = (0..l.tiles(i)).map(|t| l.tile_width(i, t)).sum();
            assert_eq!((rows, cols), (p.rows, p.cols));
            for k in 0..l.strips(i) {
                for t in 0..l.tiles(i) {
                    let (w, h, _, _) = l.piece(i, k, t);
                    assert!(w <= MAX_SIDE && h <= MAX_SIDE, "{w}x{h}");
                    assert!(l.piece_bytes(i, k, t, 3) as u64 <= PIECE_BYTES.max(w as u64 * 3 * l.geom.px_y(1) as u64));
                }
            }
        }
    }

    #[test]
    fn fit_width_fills_the_view_and_pieces_stay_small() {
        let g = geom(131, 48, 19, 42);
        let l = Layout::new(&[LETTER; 3], g, Fit::Width, 1.0);
        let p = l.pages[0];
        assert_eq!((p.cols, p.x, p.px_w), (131, 0, 2489));
        assert_eq!(p.rows, 3221u32.div_ceil(42));
        assert_eq!(l.tiles(0), 1);
        assert_eq!(l.pages[1].y, p.rows + GAP);
        check_pieces(&l);
    }

    #[test]
    fn fit_page_centers_and_fits() {
        let g = geom(200, 48, 19, 42);
        let l = Layout::new(&[LETTER], g, Fit::Page, 1.0);
        let p = l.pages[0];
        assert!(p.rows <= 48 && p.cols < 200);
        assert_eq!(p.x, (200 - p.cols) / 2);
        check_pieces(&l);
    }

    #[test]
    fn visible_spans_a_page_boundary() {
        let g = geom(100, 40, 10, 20);
        let l = Layout::new(&[LETTER; 2], g, Fit::Width, 1.0);
        let top = l.pages[1].y - 10;
        let v = l.visible(top, 0);
        assert_eq!(v.first().unwrap().page, 0);
        assert_eq!(v.last().unwrap().page, 1);
        let shown: u32 = v.iter().map(|s| s.b - s.a).sum();
        assert_eq!(shown, 40 - GAP);
        assert_eq!(l.page_at(l.pages[0].y + l.pages[0].rows), 1); // the gap row
    }

    #[test]
    fn zoomed_pages_are_tiled_and_tiles_cover_the_view() {
        // A 5700 px wide view zoomed 4x: 22800 px, far over the 10000 px limit.
        let g = geom(300, 55, 19, 42);
        let l = Layout::new(&[LETTER; 2], g, Fit::Width, 4.0);
        assert!(l.tiles(0) > 1);
        check_pieces(&l);
        for left in [0, 7, 150, 151, l.pages[0].cols - 300, 99_999] {
            let v = l.visible(l.pages[0].y + 3, left);
            // Each screen row is covered once across the full width.
            for row in 0..g.rows {
                let cols: u32 = v
                    .iter()
                    .filter(|s| s.screen_row <= row && row < s.screen_row + s.b - s.a)
                    .map(|s| s.c1 - s.c0)
                    .sum();
                assert_eq!(cols, g.cols, "left {left} row {row}");
            }
        }
    }

    #[test]
    fn a_hole_leaves_exactly_the_cells_around_it() {
        let v = Visible {
            page: 0,
            k: 2,
            t: 0,
            a: 1,
            b: 9,
            c0: 3,
            c1: 50,
            screen_row: 10,
            screen_col: 5,
        };
        let cells = |p: &Visible| {
            let mut out = Vec::new();
            for r in p.screen_row..p.screen_row + p.b - p.a {
                for c in p.screen_col..p.screen_col + p.c1 - p.c0 {
                    out.push((r, c, p.a + r - p.screen_row, p.c0 + c - p.screen_col));
                }
            }
            out
        };
        for hole in [
            Rect {
                r0: 12,
                c0: 20,
                r1: 15,
                c1: 30,
            }, // inside
            Rect {
                r0: 0,
                c0: 0,
                r1: 100,
                c1: 100,
            }, // covers it
            Rect {
                r0: 0,
                c0: 0,
                r1: 5,
                c1: 5,
            }, // misses it
            Rect {
                r0: 8,
                c0: 40,
                r1: 12,
                c1: 99,
            }, // over a corner
        ] {
            let inside =
                |&(r, c, _, _): &(u32, u32, u32, u32)| hole.r0 <= r && r < hole.r1 && hole.c0 <= c && c < hole.c1;
            let mut want: Vec<_> = cells(&v).into_iter().filter(|x| !inside(x)).collect();
            let mut got: Vec<_> = v.around(hole).iter().flat_map(cells).collect();
            want.sort();
            got.sort();
            assert_eq!(got, want, "{hole:?}");
        }
    }

    #[test]
    fn a_narrower_view_keeps_fit_page_pieces_but_not_fit_width_ones() {
        // Fit page on a wide window: the page is as tall as the view either way.
        let wide = Layout::new(&[LETTER; 2], geom(300, 55, 19, 42), Fit::Page, 1.0);
        let less = Layout::new(&[LETTER; 2], geom(270, 55, 19, 42), Fit::Page, 1.0);
        assert!(wide.same_pieces(&less));
        let wide = Layout::new(&[LETTER; 2], geom(300, 55, 19, 42), Fit::Width, 1.0);
        let less = Layout::new(&[LETTER; 2], geom(270, 55, 19, 42), Fit::Width, 1.0);
        assert!(!wide.same_pieces(&less));
    }

    #[test]
    fn huge_cells_and_tall_pages_stay_under_the_side_limit() {
        // A receipt-like page, very tall, in a view with large cells.
        let g = geom(40, 12, 48, 110);
        let l = Layout::new(&[(200.0, 20_000.0)], g, Fit::Width, 4.0);
        check_pieces(&l);
    }

    #[test]
    fn scaled_pieces_tile_the_page_without_gaps() {
        let mut g = geom(150, 55, 19, 42);
        g.scale = 0.83;
        let l = Layout::new(&[LETTER; 2], g, Fit::Width, 2.0);
        check_pieces(&l);
        let p = l.pages[0];
        // Neighbouring pieces meet exactly: one's end is the next one's start.
        let (w0, _, dx0, _) = l.piece(0, 0, 0);
        let (_, _, dx1, _) = l.piece(0, 0, 1);
        assert_eq!(-dx0 + w0 as i32, -dx1);
        let (_, h0, _, dy0) = l.piece(0, 0, 0);
        let (_, _, _, dy1) = l.piece(0, 1, 0);
        assert_eq!(-dy0 + h0 as i32, -dy1);
        assert!(p.px_w <= g.px_x(p.cols));
        let v = l.visible(3, 10);
        for s in &v {
            let (sx, sy, sw, sh) = l.source(s);
            let (bw, bh, _, _) = l.piece(s.page, s.k, s.t);
            assert!(sx + sw <= bw && sy + sh <= bh);
        }
    }
}
