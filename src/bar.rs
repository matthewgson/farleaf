//! The status line, drawn as lualine draws Neovim's in LazyVim: the mode in
//! a block of its color on the left, then the file, then the message; on the
//! right the keys of a command being typed (Vim's showcmd) and the view, how
//! far down, and the page. Colors: tokyonight moon, LazyVim's default; the
//! separators are lualine's powerline glyphs (Nerd Font; Ghostty has them
//! built in).

use std::fmt::Write;

pub type Rgb = (u8, u8, u8);

const BG: Rgb = (0x1e, 0x20, 0x30); // bg_statusline: section c
const GUTTER: Rgb = (0x3b, 0x42, 0x61); // fg_gutter: sections b and y
const DIM: Rgb = (0x82, 0x8b, 0xb8); // fg_dark: section c's text
pub const FG: Rgb = (0xc8, 0xd3, 0xf5); // fg: messages
const DARK: Rgb = (0x1b, 0x1d, 0x2b); // black: text on a mode's color
const SHOWCMD: Rgb = (0xc0, 0x99, 0xff); // Statement, as LazyVim colors noice's showcmd
const BUSY: Rgb = (0xff, 0x96, 0x6c); // orange

pub const HINT: Rgb = DIM;

const LEFT: char = '\u{e0b0}'; //
const RIGHT: char = '\u{e0b2}'; //
const THIN: char = '\u{e0b3}'; //

#[derive(Clone, Copy)]
pub struct Mode {
    pub name: &'static str,
    pub color: Rgb,
}

pub const NORMAL: Mode = Mode {
    name: "NORMAL",
    color: (0x82, 0xaa, 0xff),
};
pub const CURSOR: Mode = Mode {
    name: "CURSOR",
    color: (0xc3, 0xe8, 0x8d),
};
pub const VISUAL: Mode = Mode {
    name: "VISUAL",
    color: (0xc0, 0x99, 0xff),
};
pub const V_LINE: Mode = Mode {
    name: "V-LINE",
    color: (0xc0, 0x99, 0xff),
};
pub const COMMAND: Mode = Mode {
    name: "COMMAND",
    color: (0xff, 0xc7, 0x77),
};
pub const OUTLINE: Mode = Mode {
    name: "OUTLINE",
    color: (0x4f, 0xd6, 0xbe),
};
pub const PAGES: Mode = Mode {
    name: "PAGES",
    color: (0x4f, 0xd6, 0xbe),
};

pub struct Bar<'a> {
    pub mode: Mode,
    /// Section b: the file.
    pub file: &'a str,
    /// Section c: a message, a hint, or what is being typed after `:` `/`.
    pub msg: &'a str,
    pub msg_fg: Rgb,
    /// Cut `msg` at its start, not its end, when it does not fit (a prompt
    /// keeps the end, where the typing is).
    pub keep_end: bool,
    /// Keys typed so far of a command that waits for more, and the count.
    pub showcmd: &'a str,
    pub busy: bool,
    /// The view (fit, zoom, inverted).
    pub view: &'a str,
    /// Section y: Top, Bot, All or a percentage.
    pub progress: &'a str,
    /// Section z: the page.
    pub page: &'a str,
}

fn color(out: &mut String, fg: Rgb, bg: Rgb, bold: bool) {
    let _ = write!(
        out,
        "\x1b[0;{}38;2;{};{};{};48;2;{};{};{}m",
        if bold { "1;" } else { "" },
        fg.0,
        fg.1,
        fg.2,
        bg.0,
        bg.1,
        bg.2
    );
}

fn width(s: &str) -> usize {
    s.chars().count()
}

/// `s` cut to `n` columns, with … where it was cut.
fn cut(s: &str, n: usize, keep_end: bool) -> String {
    let w = width(s);
    if w <= n {
        return s.to_string();
    }
    if n == 0 {
        return String::new();
    }
    match keep_end {
        true => std::iter::once('…').chain(s.chars().skip(w - n + 1)).collect(),
        false => s.chars().take(n - 1).chain(std::iter::once('…')).collect(),
    }
}

/// The line for `cols` columns, to write at its start, and the column
/// (1-based) just after the message: where the cursor goes in a prompt.
pub fn render(b: &Bar, cols: usize) -> (String, usize) {
    let m = b.mode.color;
    // Section x: its pieces, with lualine's thin separator between them.
    let busy = if b.busy { "…" } else { "" };
    let mut x: Vec<(&str, Rgb, bool)> = [(b.showcmd, SHOWCMD, true), (busy, BUSY, true), (b.view, DIM, false)]
        .into_iter()
        .filter(|p| !p.0.is_empty())
        .collect();
    let mut file = b.file.to_string();
    let mut progress = b.progress;
    let mut page = b.page;
    let a = width(b.mode.name) + 3; // " NAME " and the separator
                                    // Drop what fits least, least needed first, until the message has room
                                    // for a few columns (the last column stays empty: no wrap).
    let need = |file: &str, x: &[(&str, Rgb, bool)], progress: &str, page: &str| {
        let w = |s: &str, extra: usize| if s.is_empty() { 0 } else { width(s) + extra };
        let xw: usize = x.iter().map(|p| width(p.0) + 3).sum::<usize>(); // " p " and a separator
        a + w(file, 3) + xw + w(progress, 3) + w(page, 3) + 2
    };
    let room = cols.saturating_sub(1);
    let min_msg = width(b.msg).min(12);
    let mut level = 0;
    while need(&file, &x, progress, page) + min_msg > room && level < 5 {
        match level {
            0 => x.retain(|p| p.1 != DIM), // the view
            1 => {
                // The file, cut to fit, or if even that is too tight, gone.
                let rest = need("", &x, progress, page) + min_msg + 3;
                file = match rest + 8 <= room {
                    true => cut(b.file, room - rest, false),
                    false => String::new(),
                };
            }
            2 => progress = "",
            3 => x.retain(|p| p.1 == SHOWCMD), // the busy mark
            _ => {
                x.clear();
                page = "";
            }
        }
        level += 1;
    }

    let mut out = String::new();
    if room < a + 1 {
        // Too narrow even for the mode: what of it fits.
        color(&mut out, DARK, m, true);
        let _ = write!(out, "{}\x1b[K\x1b[0m", cut(&format!(" {}", b.mode.name), room, false));
        return (out, room.min(cols));
    }
    let mut col = 0;
    // Section a: the mode.
    color(&mut out, DARK, m, true);
    let _ = write!(out, " {} ", b.mode.name);
    col += width(b.mode.name) + 2;
    let next = if file.is_empty() { BG } else { GUTTER };
    color(&mut out, m, next, false);
    out.push(LEFT);
    col += 1;
    if !file.is_empty() {
        color(&mut out, m, GUTTER, false);
        let _ = write!(out, " {file} ");
        color(&mut out, GUTTER, BG, false);
        out.push(LEFT);
        col += width(&file) + 3;
    }
    // The right side, to measure it.
    let mut right = String::new();
    let mut rw = 0;
    for (i, (s, fg, bold)) in x.iter().enumerate() {
        if i > 0 {
            color(&mut right, DIM, BG, false);
            right.push(THIN);
            rw += 1;
        }
        color(&mut right, *fg, BG, *bold);
        let _ = write!(right, " {s} ");
        rw += width(s) + 2;
    }
    if !x.is_empty() {
        right.push(' ');
        rw += 1;
    }
    let mut prev = BG;
    for (s, fg, bg, bold) in [(progress, m, GUTTER, false), (page, DARK, m, true)] {
        if s.is_empty() {
            continue;
        }
        color(&mut right, bg, prev, false);
        right.push(RIGHT);
        color(&mut right, fg, bg, bold);
        let _ = write!(right, " {s} ");
        rw += width(s) + 3;
        prev = bg;
    }
    // Section c: the message, cut to what is left.
    let msg = cut(b.msg, room.saturating_sub(col + rw + 2), b.keep_end);
    color(&mut out, b.msg_fg, BG, false);
    let _ = write!(out, " {msg}");
    col += 1 + width(&msg);
    let at = col + 1;
    let pad = room.saturating_sub(col + rw);
    let _ = write!(out, "{:pad$}{right}\x1b[K\x1b[0m", "");
    (out, at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The columns a line takes on the screen, escapes left out.
    fn shown(s: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for c in s.chars() {
            match (esc, c) {
                (false, '\x1b') => esc = true,
                (true, c) if c.is_ascii_alphabetic() => esc = false,
                (true, _) => {}
                (false, c) => out.push(c),
            }
        }
        out
    }

    fn bar<'a>(msg: &'a str, showcmd: &'a str) -> Bar<'a> {
        Bar {
            mode: NORMAL,
            file: "paper-with-a-rather-long-name.pdf",
            msg,
            msg_fg: FG,
            keep_end: false,
            showcmd,
            busy: false,
            view: "width 120%",
            progress: "42%",
            page: "12/34",
        }
    }

    #[test]
    fn fills_the_line_but_the_last_column() {
        for cols in [10, 20, 30, 45, 60, 80, 120, 200] {
            for msg in ["", "12 characters copied", &"x".repeat(300)] {
                let (s, at) = render(&bar(msg, "2y"), cols);
                let w = width(&shown(&s));
                assert!(w <= cols.saturating_sub(1), "{cols}: {w} {:?}", shown(&s));
                if cols >= 30 {
                    assert_eq!(w, cols - 1, "{cols}: {:?}", shown(&s));
                }
                assert!(at <= cols, "{cols}: {at}");
            }
        }
    }

    #[test]
    fn wide_shows_all_narrow_keeps_mode_and_showcmd() {
        let (s, _) = render(&bar("hello", "2y"), 120);
        let s = shown(&s);
        for part in [
            "NORMAL",
            "paper-with-a-rather-long-name.pdf",
            "hello",
            "2y",
            "width 120%",
            "42%",
            "12/34",
        ] {
            assert!(s.contains(part), "{part}: {s:?}");
        }
        let (s, _) = render(&bar("hello", "2y"), 30);
        let s = shown(&s);
        assert!(s.contains("NORMAL") && s.contains("2y"), "{s:?}");
        assert!(!s.contains("width"), "{s:?}");
    }

    #[test]
    fn a_prompt_keeps_its_end() {
        let mut b = bar("", "");
        let typed = format!("/{}end", "a".repeat(200));
        b.msg = &typed;
        b.keep_end = true;
        let (s, at) = render(&b, 80);
        let s = shown(&s);
        assert!(s.contains("aaend"), "{s:?}");
        // The cursor goes just after "end".
        let end = s.find("end").unwrap();
        assert_eq!(s[..end].chars().count() + 3 + 1, at);
    }
}
