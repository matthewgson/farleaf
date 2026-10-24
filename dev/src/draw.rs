//! Drawing text into screenshots, in a monospace font: FARLEAF_DEV_FONT, or
//! else the first of JetBrains Mono, Menlo and DejaVu Sans Mono found.

use crate::screen::Cell;
use fontdue::{Font, FontSettings, Metrics};
use std::collections::HashMap;

pub const FG: [u8; 3] = [208, 208, 208];
pub const BG: [u8; 3] = [30, 30, 30];

pub struct Text {
    regular: Font,
    bold: Option<Font>,
    pub px: f32,
    ascent: f32,
    descent: f32,
    glyphs: HashMap<(char, bool, u32), (Metrics, Vec<u8>)>,
}

fn load(path: &str) -> Option<Font> {
    let bytes = std::fs::read(path).ok()?;
    Font::from_bytes(bytes, FontSettings::default()).ok()
}

impl Text {
    /// A font sized so a character is at most `cw` pixels wide (and fits
    /// `ch` pixels high). None if no font file is found.
    pub fn new(cw: u32, ch: u32) -> Option<Text> {
        let home = std::env::var("HOME").unwrap_or_default();
        let mut candidates = vec![
            (
                format!("{home}/Library/Fonts/JetBrainsMono-Regular.ttf"),
                format!("{home}/Library/Fonts/JetBrainsMono-Bold.ttf"),
            ),
            (
                "/Library/Fonts/JetBrainsMono-Regular.ttf".into(),
                "/Library/Fonts/JetBrainsMono-Bold.ttf".into(),
            ),
            ("/System/Library/Fonts/Menlo.ttc".into(), String::new()),
            (
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf".into(),
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf".into(),
            ),
        ];
        if let Ok(p) = std::env::var("FARLEAF_DEV_FONT") {
            candidates.insert(0, (p.clone(), p.replace("Regular", "Bold")));
        }
        let (regular, bold) = candidates
            .iter()
            .find_map(|(r, b)| Some((load(r)?, load(b).filter(|_| !b.is_empty()))))?;
        let mut px = ch as f32 * 0.8;
        while px > 6.0 && regular.metrics('M', px).advance_width > cw as f32 {
            px -= 0.5;
        }
        let lm = regular.horizontal_line_metrics(px)?;
        Some(Text {
            regular,
            bold,
            px,
            ascent: lm.ascent,
            descent: lm.descent,
            glyphs: HashMap::new(),
        })
    }

    /// Draws cell `cell` at (`x`, `y`), `cw`x`ch` pixels, into `fb` (RGB,
    /// `w` pixels wide).
    #[allow(clippy::too_many_arguments)]
    pub fn cell(&mut self, fb: &mut [u8], w: usize, x: i64, y: i64, cw: i64, ch: i64, cell: &Cell) {
        let a = cell.attr;
        let bg = a.bg.unwrap_or(BG);
        if a.bg.is_some() {
            fill(fb, w, x, y, cw, ch, bg);
        }
        if cell.ch == ' ' {
            return;
        }
        let mut fg = a.fg.unwrap_or(FG);
        if a.dim {
            fg = mix(bg, fg, 128);
        }
        if box_drawing(fb, w, x, y, cw, ch, cell.ch, fg) {
            return;
        }
        let baseline = y as f32 + (ch as f32 - (self.ascent - self.descent)) / 2.0 + self.ascent;
        self.glyph(fb, w, x as f32, baseline, cell.ch, a.bold, self.px, fg);
    }

    /// Draws `c` with its baseline at `baseline`, starting at `x`; returns
    /// its advance.
    #[allow(clippy::too_many_arguments)]
    pub fn glyph(
        &mut self,
        fb: &mut [u8],
        w: usize,
        x: f32,
        baseline: f32,
        c: char,
        bold: bool,
        px: f32,
        fg: [u8; 3],
    ) -> f32 {
        let font = match (&self.bold, bold) {
            (Some(b), true) => b,
            _ => &self.regular,
        };
        let key = (c, bold, (px * 4.0) as u32);
        let (m, bitmap) = &*self.glyphs.entry(key).or_insert_with(|| font.rasterize(c, px));
        let top = baseline.round() as i64 - (m.height as i64 + m.ymin as i64);
        let left = x.round() as i64 + m.xmin as i64;
        let h = fb.len() / 3 / w;
        for gy in 0..m.height {
            for gx in 0..m.width {
                let cov = bitmap[gy * m.width + gx];
                let (px_, py) = (left + gx as i64, top + gy as i64);
                if cov == 0 || px_ < 0 || py < 0 || px_ as usize >= w || py as usize >= h {
                    continue;
                }
                let i = (py as usize * w + px_ as usize) * 3;
                let under = [fb[i], fb[i + 1], fb[i + 2]];
                fb[i..i + 3].copy_from_slice(&mix(under, fg, cov));
            }
        }
        m.advance_width
    }

    /// Draws `s` at size `px` with its baseline at `baseline`; returns its width.
    #[allow(clippy::too_many_arguments)]
    pub fn string(&mut self, fb: &mut [u8], w: usize, x: f32, baseline: f32, s: &str, px: f32, fg: [u8; 3]) -> f32 {
        let mut at = x;
        for c in s.chars() {
            at += self.glyph(fb, w, at, baseline, c, true, px, fg);
        }
        at - x
    }

    pub fn width(&self, s: &str, px: f32) -> f32 {
        let font = self.bold.as_ref().unwrap_or(&self.regular);
        s.chars().map(|c| font.metrics(c, px).advance_width).sum()
    }
}

/// `b` over `a` at coverage `t` (0-255).
fn mix(a: [u8; 3], b: [u8; 3], t: u8) -> [u8; 3] {
    let t = t as u32;
    [0, 1, 2].map(|i| ((a[i] as u32 * (255 - t) + b[i] as u32 * t) / 255) as u8)
}

pub fn fill(fb: &mut [u8], w: usize, x: i64, y: i64, cw: i64, ch: i64, color: [u8; 3]) {
    let h = (fb.len() / 3 / w) as i64;
    for yy in y.max(0)..(y + ch).min(h) {
        for xx in x.max(0)..(x + cw).min(w as i64) {
            let i = (yy as usize * w + xx as usize) * 3;
            fb[i..i + 3].copy_from_slice(&color);
        }
    }
}

/// Box-drawing characters as lines that meet across cells, as terminals
/// draw them.
#[allow(clippy::too_many_arguments)]
fn box_drawing(fb: &mut [u8], w: usize, x: i64, y: i64, cw: i64, ch: i64, c: char, color: [u8; 3]) -> bool {
    let arms = match c {
        '─' => "lr",
        '│' => "ud",
        '┌' => "rd",
        '┐' => "ld",
        '└' => "ru",
        '┘' => "lu",
        _ => return false,
    };
    let (mx, my) = (x + cw / 2, y + ch / 2);
    for a in arms.chars() {
        match a {
            'l' => fill(fb, w, x, my, cw / 2 + 1, 1, color),
            'r' => fill(fb, w, mx, my, cw - cw / 2, 1, color),
            'u' => fill(fb, w, mx, y, 1, ch / 2 + 1, color),
            _ => fill(fb, w, mx, my, 1, ch - ch / 2, color),
        }
    }
    true
}

/// A filled rectangle with rounded corners and a border.
#[allow(clippy::too_many_arguments)]
pub fn rounded_box(fb: &mut [u8], w: usize, rect: [i64; 4], radius: i64, border: i64, fill_c: [u8; 3], line: [u8; 3]) {
    let [x0, y0, x1, y1] = rect;
    let h = (fb.len() / 3 / w) as i64;
    for y in y0.max(0)..y1.min(h) {
        for x in x0.max(0)..x1.min(w as i64) {
            // Distance outside the rounded corner's circle, if in a corner.
            let cx = if x < x0 + radius {
                x0 + radius
            } else if x >= x1 - radius {
                x1 - radius - 1
            } else {
                x
            };
            let cy = if y < y0 + radius {
                y0 + radius
            } else if y >= y1 - radius {
                y1 - radius - 1
            } else {
                y
            };
            let d = (((x - cx).pow(2) + (y - cy).pow(2)) as f64).sqrt();
            if d > radius as f64 {
                continue;
            }
            let edge = x - x0 < border || x1 - 1 - x < border || y - y0 < border || y1 - 1 - y < border;
            let i = (y as usize * w + x as usize) * 3;
            let c = if edge || d > (radius - border) as f64 && (cx != x || cy != y) {
                line
            } else {
                fill_c
            };
            fb[i..i + 3].copy_from_slice(&c);
        }
    }
}
